// kernel/src/gpu/bench.rs
//
// Phase 6d of docs/gpu/gpu-plan.md: what the copy engine is worth, measured on
// the channel `copy.rs` brought up (`gpu=copy`, IF=0 at boot, single CPU). Every
// number is printed as a `bench:` line of /proc/gpu and summarised in one
// `gpu_bench:` line of /proc/kdebug.
//
//   1. link      the GPU's PCIe link (speed/width, capability and status) before
//                and after the traffic: the link is the ceiling both the copy
//                engine and the CPU's write-combined stores hit.
//   2. sweep     system -> VRAM by size (4 KiB .. 4 MiB, 4 KiB pages), 9 runs
//                each, the fence in host memory (the CPU polls it from cache:
//                no BAR0 read in the timing).
//   3. batch     16 x 4 MiB in ONE submission: the sustained rate, 4 KiB and
//                2 MiB pages, and the 2 MiB path's data checked end to end.
//   4. cpu       the same 4 MiB by `copy_nonoverlapping` + sfence into BAR1
//                mapped write-combined: what `Framebuffer::flush` does.
//   4b. frames   rectangles of a 1920x1080 framebuffer (pitch 8192, 4 bytes a
//                pixel) from a 16 MiB host buffer into 16 MiB of VRAM: the copy
//                engine's 2D copy against the CPU row by row, from a 32x32 patch
//                to the whole screen, to find where the engine starts to win.
//   5. overlap   CPU arithmetic while a 64 MiB batch runs: how much of the CPU
//                the copy leaves free.
//
// The data of every copy that could be wrong is compared afterwards (the 2 MiB
// path, the batch): a fast copy that moves the wrong bytes proves nothing.

use alloc::string::String;
use core::fmt::Write;

use nvgpu::chan;
use nvgpu::evo::Pramin;
use nvgpu::Mmio;

use super::copy::{self, Buffers, Submitter};
use super::gsp::Rm;
use super::Bar0;
use crate::memory::dma::DmaBuf;

const SIZES: [u32; 5] = [4 << 10, 64 << 10, 256 << 10, 1 << 20, 4 << 20];
const RUNS: usize = 9;
const BATCH: usize = 16;
const MB: u64 = 1 << 20;
/// The pages of the 4 MiB copies (`copy::COPY_BYTES / 4 KiB`).
const COPY_PAGES_BENCH: usize = (copy::COPY_BYTES / 0x1000) as usize;

static SUMMARY: spin::Once<String> = spin::Once::new();

/// `/proc/kdebug` line (empty until the bench ran).
pub fn render_kdebug() -> String {
    SUMMARY.get().cloned().unwrap_or_default()
}

fn us(ticks: u64) -> u64 {
    ticks * 1_000_000 / crate::cpu::tsc::freq_hz()
}

/// MB/s (MiB) for `bytes` in `ticks`.
fn mbps(bytes: u64, ticks: u64) -> u64 {
    (bytes as u128 * crate::cpu::tsc::freq_hz() as u128 / MB as u128 / ticks.max(1) as u128) as u64
}

fn median(v: &mut [u64]) -> u64 {
    v.sort_unstable();
    v[v.len() / 2]
}

/// The PCIe link: (current speed, current width, max speed, max width) from the
/// capability (`LnkSta` at +0x12, `LnkCap` at +0x0c; speed 1/2/3/4 = 2.5/5/8/16 GT/s).
fn link(pci: &super::gsp::PciInfo) -> Option<(u8, u8, u8, u8)> {
    let cfg = crate::pci::config_space(pci.bus, pci.device, pci.function);
    let mut ptr = cfg[0x34] as usize;
    for _ in 0..48 {
        if ptr < 0x40 || ptr + 0x14 > 256 {
            return None;
        }
        if cfg[ptr] == 0x10 {
            let sta = u16::from_le_bytes([cfg[ptr + 0x12], cfg[ptr + 0x13]]);
            let cap = u32::from_le_bytes([cfg[ptr + 0x0c], cfg[ptr + 0x0d], cfg[ptr + 0x0e], cfg[ptr + 0x0f]]);
            return Some(((sta & 0xf) as u8, ((sta >> 4) & 0x3f) as u8, (cap & 0xf) as u8, ((cap >> 4) & 0x3f) as u8));
        }
        ptr = cfg[ptr + 1] as usize;
    }
    None
}

fn gts(speed: u8) -> &'static str {
    match speed {
        1 => "2.5",
        2 => "5",
        3 => "8",
        4 => "16",
        5 => "32",
        _ => "?",
    }
}

fn link_text(l: Option<(u8, u8, u8, u8)>) -> String {
    match l {
        Some((s, w, cs, cw)) => alloc::format!("{} GT/s x{} (capability {} GT/s x{})", gts(s), w, gts(cs), cw),
        None => String::from("unknown"),
    }
}

#[inline(never)]
fn work(mut x: u64, n: u32) -> u64 {
    for _ in 0..n {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    }
    core::hint::black_box(x)
}

/// A pattern check over a contiguous buffer: number of words that differ.
fn diff_words(buf: &DmaBuf, len: usize) -> u64 {
    let mut bad = 0;
    let mut page = [0u8; 0x1000];
    for i in 0..len / 0x1000 {
        buf.read(i * 0x1000, &mut page);
        for (k, w) in page.chunks_exact(4).enumerate() {
            if u32::from_le_bytes(w.try_into().unwrap()) != copy::pattern(i, k) {
                bad += 1;
            }
        }
    }
    bad
}

fn sfence() {
    // SAFETY: a store fence has no memory effect beyond ordering.
    unsafe { core::arch::asm!("sfence", options(nostack, preserves_flags)) };
}

/// The screen the frame-shaped copies model: 1920x1080 at 4 bytes a pixel in a
/// pitch of 8192 bytes (the GOP framebuffer of the ASUS, 8.4 MiB).
const PITCH: u32 = 8192;
const FRAME_W: u32 = 1920;
const FRAME_H: u32 = 1080;

fn fpat(word: usize) -> u32 {
    (word as u32).wrapping_mul(0x9e37_79b1) ^ 0x1234_abcd
}

/// Rectangles from a 32x32 patch to the whole screen: the copy engine (one 2D
/// copy, host fence) against the CPU (`copy_nonoverlapping` row by row + sfence,
/// what `Framebuffer::copy_out` does). Returns the full-frame MB/s of each and
/// the smallest rectangle (in bytes) where the engine's median beat the CPU's.
fn frames(r: &mut String, p: &mut Pramin, sub: &mut Submitter, bufs: &Buffers, pci: Option<&super::gsp::PciInfo>) -> Result<(u64, u64, u64), String> {
    let Some(pci) = pci.filter(|p| p.bar1 != 0) else {
        let _ = writeln!(r, "bench: frames skipped: no BAR1");
        return Ok((0, 0, 0));
    };
    // SAFETY: VRAM [FRAME_VRAM, +16 MiB) belongs to this bring-up only (`chan` layout); the
    // copy engine is idle whenever the CPU touches it (every fence was waited on).
    let Some(v) = (unsafe { crate::memory::mmio::map(x86_64::PhysAddr::new(pci.bar1 + chan::FRAME_VRAM), chan::FRAME_BYTES as usize) }) else {
        let _ = writeln!(r, "bench: frames skipped: cannot map BAR1");
        return Ok((0, 0, 0));
    };
    let wc = crate::memory::memtype::set_pat_index_range(v.as_u64(), chan::FRAME_BYTES, hal::memtype::PAT_WC_INDEX).is_ok();
    let src = bufs.fsrc.virt();
    let words = chan::FRAME_BYTES as usize / 4;
    for i in 0..words {
        // SAFETY: inside the 16 MiB DMA buffer.
        unsafe { core::ptr::write_volatile((src as *mut u32).add(i), fpat(i)) };
    }
    // a clean destination, so untouched padding reads 0
    // SAFETY: inside the mapping.
    unsafe { core::ptr::write_bytes(v.as_u64() as *mut u8, 0, chan::FRAME_BYTES as usize) };
    sfence();

    let mut seq = 5000u32;
    let mut ce_full = 0;
    let mut cpu_full = 0;
    let mut cross = 0;
    let _ = writeln!(r, "bench: rectangles of a {}x{} screen (pitch {}), {} ({} runs each, median):", FRAME_W, FRAME_H, PITCH, if wc { "VRAM through BAR1 write-combined" } else { "VRAM through BAR1 NOT write-combined" }, RUNS);
    for (w, h) in [(32u32, 32u32), (64, 64), (128, 128), (256, 256), (512, 512), (640, 400), (1024, 768), (FRAME_W, FRAME_H)] {
        let line = w * 4;
        let bytes = line as u64 * h as u64;
        let mut ce = [0u64; RUNS];
        for x in ce.iter_mut() {
            seq += 1;
            let push = chan::copy_rect_push(chan::FRAME_SRC_VA, chan::FRAME_DST_VA, PITCH, PITCH, line, h, chan::HFENCE_VA, seq);
            *x = sub.submit_host(p, &push, bufs.hfence.virt() as *mut u32, seq).map_err(|e| alloc::format!("{}x{}: {}", w, h, e))?.ticks;
        }
        let mut cpu = [0u64; RUNS];
        for x in cpu.iter_mut() {
            let t0 = crate::cpu::tsc::read();
            for row in 0..h as usize {
                let off = row * PITCH as usize;
                // SAFETY: rows inside the 16 MiB source and the mapping (h * PITCH <= 16 MiB), disjoint memories.
                unsafe { core::ptr::copy_nonoverlapping(src.add(off) as *const u8, (v.as_u64() as *mut u8).add(off), line as usize) };
            }
            sfence();
            *x = crate::cpu::tsc::read().wrapping_sub(t0);
        }
        let (ce_m, cpu_m) = (median(&mut ce), median(&mut cpu));
        let wins = ce_m < cpu_m;
        if wins && cross == 0 {
            cross = bytes;
        }
        if (w, h) == (FRAME_W, FRAME_H) {
            ce_full = mbps(bytes, ce_m);
            cpu_full = mbps(bytes, cpu_m);
        }
        let _ = writeln!(
            r,
            "bench:   {:>4}x{:<4} {:>8} KiB: copy engine {:>5} us ({:>5} MB/s), CPU {:>5} us ({:>5} MB/s){}",
            w,
            h,
            bytes / 1024,
            us(ce_m),
            mbps(bytes, ce_m),
            us(cpu_m),
            mbps(bytes, cpu_m),
            if wins { "  <- engine faster" } else { "" }
        );
    }
    // Verify through PRAMIN (BAR0's window on VRAM, the path the rungs already trust)
    // and through BAR1, on every 32nd row and the last: the words of each visible
    // line and of the padding after it. Reads of VRAM are uncached, ~1 us a word.
    let rows: alloc::vec::Vec<usize> = (0..FRAME_H as usize).step_by(32).chain(core::iter::once(FRAME_H as usize - 1)).collect();
    let words_per_row = PITCH as usize / 4;
    let bar1 = |i: usize| -> u32 {
        // SAFETY: inside the mapping (i < 16 MiB / 4).
        unsafe { core::ptr::read_volatile((v.as_u64() as *const u32).add(i)) }
    };
    // (visible words that differ from `want`, padding words that differ from `pad`, first difference)
    let mut check = |rd: &mut dyn FnMut(usize) -> u32, want: &dyn Fn(usize) -> u32, pad: u32| -> (u64, u64, Option<(usize, u32, u32)>) {
        let (mut bad, mut pad_bad, mut first) = (0u64, 0u64, None);
        for &row in &rows {
            for k in 0..words_per_row {
                let i = row * words_per_row + k;
                let got = rd(i);
                let exp = if k < FRAME_W as usize { want(i) } else { pad };
                if got != exp {
                    if k < FRAME_W as usize { bad += 1 } else { pad_bad += 1 }
                    first.get_or_insert((i, exp, got));
                }
            }
        }
        (bad, pad_bad, first)
    };
    let base = chan::FRAME_VRAM;
    // 1. does the CPU's full-frame copy really reach VRAM? Scribble the sampled rows (through PRAMIN, so a
    // copy that did nothing cannot pass on data an earlier engine copy left there), copy once more, check.
    for &row in &rows {
        for k in 0..words_per_row {
            p.wr32(chan::FRAME_VRAM + 4 * (row * words_per_row + k) as u64, 0xa5a5_a5a5);
        }
    }
    for row in 0..FRAME_H as usize {
        let off = row * PITCH as usize;
        // SAFETY: rows inside the 16 MiB source and the mapping, disjoint memories.
        unsafe { core::ptr::copy_nonoverlapping(src.add(off) as *const u8, (v.as_u64() as *mut u8).add(off), (FRAME_W * 4) as usize) };
    }
    sfence();
    let (b, pb, f) = check(&mut |i| p.rd32(base + 4 * i as u64), &fpat, 0xa5a5_a5a5);
    let _ = writeln!(r, "bench: the CPU's full-frame copy over scribbled rows, read back through PRAMIN: {} visible / {} padding words differ (first {:x?})", b, pb, f);
    let (b1, pb1, f1) = check(&mut |i| bar1(i), &fpat, 0xa5a5_a5a5);
    let _ = writeln!(r, "bench: the same, read back through BAR1: {} visible / {} padding words differ (first {:x?})", b1, pb1, f1);
    if b != 0 || pb != 0 {
        return Err(alloc::format!("the CPU's BAR1 stores did not reach VRAM ({} visible, {} padding words differ through PRAMIN)", b, pb));
    }
    // 2. the engine: scribble the sampled rows through PRAMIN, copy, read back
    for &row in &rows {
        for k in 0..words_per_row {
            p.wr32(base + 4 * (row * words_per_row + k) as u64, 0xa5a5_a5a5);
        }
    }
    seq += 1;
    let push = chan::copy_rect_push(chan::FRAME_SRC_VA, chan::FRAME_DST_VA, PITCH, PITCH, FRAME_W * 4, FRAME_H, chan::HFENCE_VA, seq);
    sub.submit_host(p, &push, bufs.hfence.virt() as *mut u32, seq)?;
    let (b, pb, f) = check(&mut |i| p.rd32(base + 4 * i as u64), &fpat, 0xa5a5_a5a5);
    let _ = writeln!(r, "bench: the engine's full-screen copy over scribbled rows, read back through PRAMIN: {} visible words differ, {} padding words were touched (first {:x?})", b, pb, f);
    let _ = writeln!(r, "bench: BAR1 read probe after GSP-RM: frame region words 0..3 {:#010x} {:#010x} {:#010x}, the GOP framebuffer's first word {:#010x}", bar1(0), bar1(1), bar1(2), {
        // SAFETY: BAR1's first word (VRAM 0, the GOP framebuffer); a scratch mapping of one page.
        match unsafe { crate::memory::mmio::map(x86_64::PhysAddr::new(pci.bar1), 0x1000) } {
            Some(g) => unsafe { core::ptr::read_volatile(g.as_u64() as *const u32) },
            None => 0,
        }
    });
    let (b1, pb1, f1) = check(&mut |i| bar1(i), &fpat, 0xa5a5_a5a5);
    let _ = writeln!(r, "bench: the same, read back through BAR1: {} visible / {} padding (first {:x?})", b1, pb1, f1);
    if b != 0 || pb != 0 {
        return Err(alloc::format!("2D copy moved the wrong data ({} visible, {} padding, through PRAMIN)", b, pb));
    }
    Ok((ce_full, cpu_full, cross))
}

pub(super) fn run(r: &mut String, regs: &Bar0, rm: &mut Rm, sub: &mut Submitter, bufs: &Buffers) {
    let mut p = Pramin::new(regs);
    let pci = super::gsp::pci_info();
    let mut out = String::new();
    let mut seq = 100u32;
    let mut next = || {
        seq += 1;
        seq
    };
    macro_rules! fail {
        ($what:expr, $e:expr) => {{
            p.restore();
            let events = rm.drain(50);
            let _ = writeln!(r, "bench: STOP: {}: {}; RM events {:x?}", $what, $e, events);
            return;
        }};
    }

    // 1. the link, before
    let link0 = pci.and_then(link);
    let _ = writeln!(r, "bench: PCIe link before the traffic: {}", link_text(link0));

    // 1b. which tree vector does a finished copy raise? Launch with INTERRUPT_TYPE NON_BLOCKING and see
    // what leaf bit appears (twice: the same one both times). RM's and the registers' own idea of the
    // vector, for comparison (`ga100_ce_nonstall`: 0x104424 + inst * 0x80, `engine/ce/ga100.c:40`).
    let _ = writeln!(
        r,
        "bench: vector registers: 0xb66880 {:#x} 0xb66884 {:#x}; CE0..3 nonstall/stall (0x104424/0x10442c + inst * 0x80): {}",
        regs.rd32(0xb6_6880),
        regs.rd32(0xb6_6884),
        (0..4u32).map(|i| alloc::format!("{:#x}/{:#x}", regs.rd32(0x10_4424 + i * 0x80), regs.rd32(0x10_442c + i * 0x80))).collect::<alloc::vec::Vec<_>>().join(" ")
    );
    let mut failed = None;
    let mut found = [None, None];
    for (k, f) in found.iter_mut().enumerate() {
        let payload = 7000 + k as u32;
        let push = chan::with_interrupt(chan::copy_push(chan::SRC_VA, chan::DST_VA, 4096, chan::HFENCE_VA, payload));
        *f = super::intr::discover(r, regs, &super::intr::CE, &mut || {
            if let Err(e) = sub.submit_host(&mut p, &push, bufs.hfence.virt() as *mut u32, payload) {
                failed = Some(e);
            }
        });
        if failed.is_some() {
            break;
        }
    }
    if let Some(e) = failed {
        fail!("copy with an interrupt", e);
    }
    let _ = writeln!(r, "bench: the copy engine's interrupt vector: {:?} then {:?}", found[0], found[1]);

    // 2. the sweep, 4 KiB pages, host fence
    let mut sweep = [0u64; SIZES.len()];
    let mut sweep_us = [0u64; SIZES.len()];
    let mut submit_us = 0;
    for (i, &size) in SIZES.iter().enumerate() {
        let mut ticks = [0u64; RUNS];
        let mut queue = [0u64; RUNS];
        for run in 0..RUNS {
            let payload = next();
            let push = chan::copy_push(chan::SRC_VA, chan::DST_VA, size, chan::HFENCE_VA, payload);
            match sub.submit_host(&mut p, &push, bufs.hfence.virt() as *mut u32, payload) {
                Ok(d) => {
                    ticks[run] = d.ticks;
                    queue[run] = d.queue_ticks;
                }
                Err(e) => fail!(alloc::format!("sweep {} KiB", size / 1024), e),
            }
        }
        submit_us = us(median(&mut queue));
        let med = median(&mut ticks);
        let best = ticks.iter().copied().min().unwrap_or(0);
        sweep[i] = mbps(size as u64, med);
        sweep_us[i] = us(med);
        let _ = writeln!(
            r,
            "bench: sweep {:>5} KiB, 4 KiB pages: median {} us ({} MB/s), best {} us ({} MB/s); queueing the work {} us",
            size / 1024,
            us(med),
            sweep[i],
            us(best),
            mbps(size as u64, best),
            submit_us
        );
    }

    // 3. batches
    let batch = |va_src: u64, va_dst: u64, n: usize, payload: u32| -> alloc::vec::Vec<u32> {
        let mut w = alloc::vec::Vec::new();
        for k in 0..n {
            w.extend(chan::copy_push(va_src, va_dst, copy::COPY_BYTES as u32, chan::HFENCE_VA, payload - (n - 1 - k) as u32));
        }
        w
    };
    let mut run_batch = |sub: &mut Submitter, p: &mut Pramin, src: u64, dst: u64| -> Result<u64, String> {
        let mut t = [0u64; 5];
        for x in t.iter_mut() {
            let payload = next() + BATCH as u32;
            let push = batch(src, dst, BATCH, payload);
            *x = sub.submit_host(p, &push, bufs.hfence.virt() as *mut u32, payload)?.ticks;
        }
        Ok(median(&mut t))
    };
    let small = match run_batch(sub, &mut p, chan::SRC_VA, chan::DST_VA) {
        Ok(t) => t,
        Err(e) => fail!("batch, 4 KiB pages", e),
    };
    let small_mbps = mbps(BATCH as u64 * copy::COPY_BYTES, small);
    let _ = writeln!(r, "bench: {} x 4 MiB in one submission, 4 KiB pages: {} us, {} MB/s", BATCH, us(small), small_mbps);
    let huge = match run_batch(sub, &mut p, chan::HUGE_SRC_VA, chan::HUGE_DST_VA) {
        Ok(t) => t,
        Err(e) => fail!("batch, 2 MiB pages", e),
    };
    let huge_mbps = mbps(BATCH as u64 * copy::COPY_BYTES, huge);
    let _ = writeln!(r, "bench: {} x 4 MiB in one submission, 2 MiB pages: {} us, {} MB/s", BATCH, us(huge), huge_mbps);
    // one 4 MiB copy each way through the 2 MiB pages, and the data checked
    let mut single = [0u64; RUNS];
    for x in single.iter_mut() {
        let payload = next();
        let push = chan::copy_push(chan::HUGE_SRC_VA, chan::HUGE_DST_VA, copy::COPY_BYTES as u32, chan::HFENCE_VA, payload);
        match sub.submit_host(&mut p, &push, bufs.hfence.virt() as *mut u32, payload) {
            Ok(d) => *x = d.ticks,
            Err(e) => fail!("2 MiB pages, one copy", e),
        }
    }
    let huge_single = mbps(copy::COPY_BYTES, median(&mut single));
    let payload = next();
    let back = chan::copy_push(chan::HUGE_DST_VA, chan::HUGE_BACK_VA, copy::COPY_BYTES as u32, chan::HFENCE_VA, payload);
    if let Err(e) = sub.submit_host(&mut p, &back, bufs.hfence.virt() as *mut u32, payload) {
        fail!("2 MiB pages, back", e);
    }
    let bad = diff_words(&bufs.hback, copy::COPY_BYTES as usize);
    let _ = writeln!(r, "bench: 4 MiB one copy, 2 MiB pages: {} MB/s; round trip through them compared: {} words differ", huge_single, bad);
    if bad != 0 {
        p.restore();
        let _ = writeln!(r, "bench: STOP: the 2 MiB pages moved the wrong data");
        return;
    }

    // 4. the CPU into BAR1, write-combined (what Framebuffer::flush does)
    let mut cpu_mbps = 0;
    let mut cpu_ticks_4m = 0;
    match pci {
        Some(pci) if pci.bar1 != 0 => {
            // SAFETY: BAR1 is the VRAM aperture; [VRAM_DST, +4 MiB) is VRAM only this
            // bring-up uses (`chan` layout), and the CE is idle (every fence was waited on).
            match unsafe { crate::memory::mmio::map(x86_64::PhysAddr::new(pci.bar1 + chan::VRAM_DST), copy::COPY_BYTES as usize) } {
                Some(v) => {
                    let wc = crate::memory::memtype::set_pat_index_range(v.as_u64(), copy::COPY_BYTES, hal::memtype::PAT_WC_INDEX).is_ok();
                    // scribble the words the check will read (through PRAMIN), so stores that never
                    // land cannot pass on data the engine's copies left in the same place
                    for pg in 0..COPY_PAGES_BENCH {
                        p.wr32(chan::VRAM_DST + (pg * 0x1000 + (pg % 1024) * 4) as u64, 0xa5a5_a5a5);
                    }
                    let mut t = [0u64; RUNS];
                    for x in t.iter_mut() {
                        let t0 = crate::cpu::tsc::read();
                        // SAFETY: the source is our 4 MiB DMA buffer, the destination the mapping above, disjoint.
                        unsafe { core::ptr::copy_nonoverlapping(bufs.hsrc.virt() as *const u8, v.as_u64() as *mut u8, copy::COPY_BYTES as usize) };
                        sfence();
                        *x = crate::cpu::tsc::read().wrapping_sub(t0);
                    }
                    cpu_ticks_4m = median(&mut t);
                    cpu_mbps = mbps(copy::COPY_BYTES, cpu_ticks_4m);
                    // did the stores reach VRAM? 1024 words, one per page, read through PRAMIN
                    let bad = (0..COPY_PAGES_BENCH)
                        .filter(|&pg| p.rd32(chan::VRAM_DST + (pg * 0x1000 + (pg % 1024) * 4) as u64) != copy::pattern(pg, pg % 1024))
                        .count();
                    let _ = writeln!(r, "bench: CPU copy_nonoverlapping + sfence, 4 MiB into BAR1 ({}): median {} us, {} MB/s; {} of {} sampled words did not reach VRAM (read through PRAMIN)", if wc { "write-combined" } else { "NOT write-combined" }, us(cpu_ticks_4m), cpu_mbps, bad, COPY_PAGES_BENCH);
                    if bad != 0 {
                        cpu_mbps = 0;
                    }
                }
                None => {
                    let _ = writeln!(r, "bench: CPU baseline skipped: cannot map BAR1");
                }
            }
        }
        _ => {
            let _ = writeln!(r, "bench: CPU baseline skipped: no BAR1");
        }
    }

    // 4b. rectangles of a framebuffer
    let (frame_ce_full, frame_cpu_full, cross) = match frames(r, &mut p, sub, bufs, pci) {
        Ok(x) => x,
        Err(e) => fail!("frame-shaped copies", e),
    };

    // 5. CPU work overlapped with a 64 MiB batch
    let t0 = crate::cpu::tsc::read();
    let mut x = 1u64;
    x = work(x, 200_000);
    let per_iter_x1000 = (crate::cpu::tsc::read().wrapping_sub(t0)).max(1) * 1000 / 200_000; // ticks per 1000 iterations
    let payload = next() + BATCH as u32;
    let push = batch(chan::HUGE_SRC_VA, chan::HUGE_DST_VA, BATCH, payload);
    let t_q = crate::cpu::tsc::read();
    // SAFETY: a live DMA page of ours.
    unsafe { core::ptr::write_volatile(bufs.hfence.virt() as *mut u32, 0) };
    sub.queue_only(&mut p, &push);
    let t_go = crate::cpu::tsc::read();
    sub.ring();
    let mut iters = 0u64;
    let limit = copy::ms_ticks(2000);
    let elapsed = loop {
        x = work(x, 200);
        iters += 200;
        // SAFETY: as above.
        if unsafe { core::ptr::read_volatile(bufs.hfence.virt() as *const u32) } == payload {
            break crate::cpu::tsc::read().wrapping_sub(t_go);
        }
        if crate::cpu::tsc::read().wrapping_sub(t_go) > limit {
            p.restore();
            let _ = writeln!(r, "bench: STOP: overlap run: the fence never came");
            return;
        }
    };
    core::hint::black_box(x);
    let free_permille = (iters * per_iter_x1000 / 1000 * 1000 / elapsed.max(1)).min(1000);
    let cpu_alone = if cpu_mbps > 0 { BATCH as u64 * copy::COPY_BYTES / MB * 1_000_000 / cpu_mbps } else { 0 };
    let _ = writeln!(
        r,
        "bench: overlap: a 64 MiB batch took {} us while the CPU computed {} iterations: {}.{} % of the CPU was free (queueing {} us); the CPU alone needs {} us for the same bytes",
        us(elapsed),
        iters,
        free_permille / 10,
        free_permille % 10,
        us(t_go.wrapping_sub(t_q)),
        cpu_alone
    );
    p.restore();

    // 6. the link, after
    let link1 = pci.and_then(link);
    let _ = writeln!(r, "bench: PCIe link after the traffic: {}", link_text(link1));

    let l = |x: Option<(u8, u8, u8, u8)>| x.map(|(s, w, _, _)| alloc::format!("{}x{}", gts(s), w)).unwrap_or_else(|| String::from("?"));
    let _ = write!(
        out,
        "gpu_bench: state=ok sweep_4k_mbps={} sweep_64k_mbps={} sweep_256k_mbps={} sweep_1m_mbps={} sweep_4m_mbps={} sweep_4k_us={} batch4k_mbps={} batch2m_mbps={} huge_single_mbps={} cpu_wc_mbps={} cpu_wc_4m_us={} frame_ce_mbps={} frame_cpu_mbps={} crossover_bytes={} free_permille={} queue_us={} link_before={} link_after={}",
        sweep[0],
        sweep[1],
        sweep[2],
        sweep[3],
        sweep[4],
        sweep_us[0],
        small_mbps,
        huge_mbps,
        huge_single,
        cpu_mbps,
        us(cpu_ticks_4m),
        frame_ce_full,
        frame_cpu_full,
        cross,
        free_permille,
        submit_us,
        l(link0),
        l(link1)
    );
    SUMMARY.call_once(|| out);
    let _ = writeln!(r, "bench: OK");
}
