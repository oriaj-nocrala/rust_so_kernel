// kernel/src/gpu/copy.rs
//
// Phase 6c of docs/gpu/gpu-plan.md: a GPFIFO channel on the Ampere copy engine
// and a measured copy, `gpu=copy`. It extends `gpu=vaspace` (`vaspace.rs` calls
// `prepare`/`map` before it writes the page tables and `run` after RM accepted
// them). Everything is at boot with IF=0 and bounded waits, like the rest of
// the GSP work.
//
// What happens:
//   1. `prepare`: a method buffer (system memory, RM wants it) and two
//      COPY_BYTES buffers of system pages, `src` filled with a pattern and
//      `back` zero;
//   2. `map`: our page tables get the channel's VRAM (GPFIFO, push buffer,
//      fence), a VRAM destination buffer and the two system buffers;
//   3. `run`: the channel through RM (ALLOC 0xc56f, BIND, GPFIFO_SCHEDULE, the
//      copy object 0xc7b5, the work-submit token), then two copies, each a
//      GPFIFO entry + push buffer + USERD GP_PUT + the doorbell, waited on
//      through a semaphore the copy engine releases: src -> VRAM, VRAM -> back.
//      `back == src` proves the page tables, the channel and the engine.
//
// Phase 6d adds `bench.rs` after a successful round trip: bandwidth by size, 2 MiB
// pages, a CPU write-combined baseline, CPU work overlapped with a copy, the PCIe
// link's state. Its buffers (2 MiB pages, a host fence page) are made here.
//
// VRAM is written through PRAMIN (small pieces only; the data goes by the copy
// engine). The logic is `nvgpu::chan`, tested on the host against nouveau's RPCs.

use alloc::string::String;
use core::fmt::Write;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use nvgpu::chan::{self, ChanAlloc};
use nvgpu::evo::Pramin;
use nvgpu::mmu::{Flags, MapError, PageTables, Target};
use nvgpu::rm;
use nvgpu::Mmio;

use super::gsp::Rm;
use super::Bar0;
use crate::memory::dma::{DmaBuf, DmaPages};

/// What the GPU may address (the same 40-bit limit as the GSP's buffers).
const DMA_MASK: u64 = (1 << 40) - 1;

/// The size of each copy.
pub const COPY_BYTES: u64 = 4 << 20;
const COPY_PAGES: usize = (COPY_BYTES / 0x1000) as usize;

/// `NVC361_NOTIFY_CHANNEL_PENDING` (`clc361.h:33`, offset 0x90) in the usermode
/// window of the Ampere VFN: `0xb80000 + 0x30000` (`vfn/ga100.c:41,49`;
/// Volta's was `0x810000`, `vfn/gv100.c:28`). The value is the work-submit
/// token (`tu102_chan_start`, `engine/fifo/tu102.c:46`).
const DOORBELL: u32 = 0xb8_0000 + 0x3_0000 + 0x90;
/// The channel id nouveau's CE channel has in the trace (handle `0xf1f00002`,
/// privileged, USERD slot 2).
const CHID: u32 = 2;
const FENCE_TIMEOUT_MS: u64 = 1000;

// 0 = not run, 1 = OK, 2 = failed
static STATE: AtomicU32 = AtomicU32::new(0);
static CID: AtomicU32 = AtomicU32::new(0);
static TOKEN: AtomicU32 = AtomicU32::new(0);
static UP_KBPS: AtomicU64 = AtomicU64::new(0);
static DOWN_KBPS: AtomicU64 = AtomicU64::new(0);
static MISMATCH: AtomicU64 = AtomicU64::new(0);

fn stop(r: &mut String, why: core::fmt::Arguments) {
    STATE.store(2, Ordering::Relaxed);
    let _ = writeln!(r, "copy: STOP: {}", why);
}

/// The host memory the channel and the copies use.
pub(super) struct Buffers {
    mthdbuf: DmaBuf,
    src: DmaPages,
    back: DmaPages,
    /// 6d: contiguous, 2 MiB-aligned copies of `src`/`back`, mapped with 2 MiB pages.
    pub(super) hsrc: DmaBuf,
    pub(super) hback: DmaBuf,
    /// 6d: a fence page in host memory (the copy engine writes it, the CPU polls it from cache).
    pub(super) hfence: DmaBuf,
    /// 6d: a framebuffer-sized host buffer (`chan::FRAME_BYTES`, contiguous).
    pub(super) fsrc: DmaBuf,
}

/// The pattern of `src`: word `k` of page `i`.
pub(super) fn pattern(page: usize, k: usize) -> u32 {
    ((page as u32) << 10 | k as u32).wrapping_mul(0x9e37_79b1) ^ 0x5bd1_e995
}

pub(super) fn prepare(r: &mut String) -> Option<Buffers> {
    let mthdbuf = match DmaBuf::alloc(chan::MTHDBUF_SIZE as usize, DMA_MASK) {
        Ok(b) => b,
        Err(e) => {
            stop(r, format_args!("method buffer: {:?}", e));
            return None;
        }
    };
    let (src, back) = match (DmaPages::alloc(COPY_PAGES, DMA_MASK), DmaPages::alloc(COPY_PAGES, DMA_MASK)) {
        (Ok(a), Ok(b)) => (a, b),
        (a, b) => {
            stop(r, format_args!("{} KiB of system pages: {:?} {:?}", COPY_BYTES / 1024 * 2, a.err(), b.err()));
            return None;
        }
    };
    for i in 0..COPY_PAGES {
        let mut page = [0u8; 0x1000];
        for (k, w) in page.chunks_exact_mut(4).enumerate() {
            w.copy_from_slice(&pattern(i, k).to_le_bytes());
        }
        src.page(i).copy_in(0, &page);
    }
    let (hsrc, hback, hfence) = match (DmaBuf::alloc(COPY_BYTES as usize, DMA_MASK), DmaBuf::alloc(COPY_BYTES as usize, DMA_MASK), DmaBuf::alloc(0x1000, DMA_MASK)) {
        (Ok(a), Ok(b), Ok(c)) => (a, b, c),
        (a, b, c) => {
            stop(r, format_args!("contiguous buffers: {:?} {:?} {:?}", a.err(), b.err(), c.err()));
            return None;
        }
    };
    // Same pattern as `src` (page i, word k), written in place.
    for i in 0..COPY_PAGES {
        let mut page = [0u8; 0x1000];
        for (k, w) in page.chunks_exact_mut(4).enumerate() {
            w.copy_from_slice(&pattern(i, k).to_le_bytes());
        }
        hsrc.copy_in(i * 0x1000, &page);
    }
    let fsrc = match DmaBuf::alloc(chan::FRAME_BYTES as usize, DMA_MASK) {
        Ok(b) => b,
        Err(e) => {
            stop(r, format_args!("{} MiB frame buffer: {:?}", chan::FRAME_BYTES >> 20, e));
            return None;
        }
    };
    Some(Buffers { mthdbuf, src, back, hsrc, hback, hfence, fsrc })
}

impl Buffers {
    /// Everything the channel and the copies address, in our VA space.
    pub(super) fn map(&self, pt: &mut PageTables) -> Result<(), MapError> {
        let f = Flags::default();
        pt.map_range(chan::GPFIFO_VA, chan::GPFIFO_VRAM, chan::GPFIFO_ENTRIES as u64 * 8, Target::Vram, f)?;
        pt.map_range(chan::PUSH_VA, chan::PUSH_VRAM, 0x1000, Target::Vram, f)?;
        pt.map_range(chan::FENCE_VA, chan::FENCE_VRAM, 0x1000, Target::Vram, f)?;
        pt.map_range(chan::DST_VA, chan::VRAM_DST, COPY_BYTES, Target::Vram, f)?;
        for i in 0..COPY_PAGES {
            let off = (i * 0x1000) as u64;
            pt.map(chan::SRC_VA + off, self.src.page(i).bus_addr(), Target::Host, f)?;
            pt.map(chan::BACK_VA + off, self.back.page(i).bus_addr(), Target::Host, f)?;
        }
        // 6d: the host fence, and the contiguous buffers through 2 MiB pages
        // (the VRAM destination again, at a second VA)
        pt.map(chan::HFENCE_VA, self.hfence.bus_addr(), Target::Host, f)?;
        pt.map_huge_range(chan::HUGE_DST_VA, chan::VRAM_DST, COPY_BYTES, Target::Vram, f)?;
        pt.map_huge_range(chan::HUGE_SRC_VA, self.hsrc.bus_addr(), COPY_BYTES, Target::Host, f)?;
        pt.map_huge_range(chan::HUGE_BACK_VA, self.hback.bus_addr(), COPY_BYTES, Target::Host, f)?;
        pt.map_huge_range(chan::FRAME_SRC_VA, self.fsrc.bus_addr(), chan::FRAME_BYTES, Target::Host, f)?;
        pt.map_huge_range(chan::FRAME_DST_VA, chan::FRAME_VRAM, chan::FRAME_BYTES, Target::Vram, f)?;
        Ok(())
    }
}

/// Write `words` to VRAM at `at` through PRAMIN.
pub(super) fn vram_write(p: &mut Pramin, at: u64, words: &[u32]) {
    for (i, w) in words.iter().enumerate() {
        p.wr32(at + 4 * i as u64, *w);
    }
}

/// Where the channel's rings are written: PRAMIN at boot (BAR0's window, reads work), BAR1
/// write-combined at run time (no shared window; reads of VRAM through BAR1 fail after GSP-RM,
/// so it is write-only).
pub(super) trait VramIo {
    fn wr32(&mut self, vram: u64, v: u32);
    /// Everything written so far is on its way to VRAM, in order, before the caller's next store
    /// (the doorbell).
    fn flush(&mut self);
}

impl VramIo for Pramin<'_> {
    fn wr32(&mut self, vram: u64, v: u32) {
        Pramin::wr32(self, vram, v)
    }
    fn flush(&mut self) {
        let _ = self.rd32(chan::USERD_VRAM + chan::USERD_GP_PUT);
    }
}

/// A write-combined BAR1 mapping of the channel's VRAM page block `[VRAM_CHAN, +64 KiB)`.
struct Bar1Io {
    base: *mut u8,
}

// SAFETY: a device mapping used under `RUNTIME`'s lock.
unsafe impl Send for Bar1Io {}

const CHAN_BLOCK: u64 = 0x1_0000;

impl VramIo for Bar1Io {
    fn wr32(&mut self, vram: u64, v: u32) {
        assert!(vram >= chan::VRAM_CHAN && vram + 4 <= chan::VRAM_CHAN + CHAN_BLOCK);
        // SAFETY: inside the mapping made in `install`.
        unsafe { core::ptr::write_volatile(self.base.add((vram - chan::VRAM_CHAN) as usize) as *mut u32, v) };
    }
    fn flush(&mut self) {
        sfence();
    }
}

pub(super) fn sfence() {
    // SAFETY: a store fence has no memory effect beyond ordering.
    unsafe { core::arch::asm!("sfence", options(nostack, preserves_flags)) };
}

pub(super) fn ms_ticks(ms: u64) -> u64 {
    crate::cpu::tsc::freq_hz() / 1000 * ms
}

/// One submitted copy's result.
pub(super) struct Done {
    ticks: u64,
    /// The copy only completed after RM's own token was rung as well.
    second_token: bool,
}

pub(super) struct Submitter<'a> {
    regs: &'a Bar0,
    /// The doorbell value we computed, then (rung after 300 ms without progress) RM's own.
    tokens: [u32; 2],
    slot: u32,
    push_at: u32,
}

/// What one submission through a host fence cost.
pub(super) struct HostDone {
    /// Writing the push buffer, the GPFIFO entry and GP_PUT (BAR0 through PRAMIN), before the doorbell.
    pub queue_ticks: u64,
    /// The doorbell to the CPU seeing the fence.
    pub ticks: u64,
    /// The TSC when the doorbell was rung.
    pub t0: u64,
}

impl Submitter<'_> {
    pub(super) fn new<'a>(regs: &'a Bar0, tokens: [u32; 2]) -> Submitter<'a> {
        Submitter { regs, tokens, slot: 0, push_at: 0 }
    }

    /// Put `push` in the push buffer and a GPFIFO entry after it, and advance
    /// GP_PUT; both rings wrap when full (every submission is waited for, so the
    /// GPU is never behind by more than one).
    fn queue(&mut self, p: &mut dyn VramIo, push: &[u32]) {
        let len = (push.len() * 4) as u32;
        assert!(len <= 0x1000);
        if self.push_at + len > 0x1000 {
            self.push_at = 0;
        }
        for (i, w) in push.iter().enumerate() {
            p.wr32(chan::PUSH_VRAM + self.push_at as u64 + 4 * i as u64, *w);
        }
        let e = chan::gp_entry(chan::PUSH_VA + self.push_at as u64, len);
        let at = chan::GPFIFO_VRAM + 8 * (self.slot % chan::GPFIFO_ENTRIES) as u64;
        p.wr32(at, e as u32);
        p.wr32(at + 4, (e >> 32) as u32);
        self.slot = (self.slot + 1) % chan::GPFIFO_ENTRIES;
        self.push_at += len;
        p.wr32(chan::USERD_VRAM + chan::USERD_GP_PUT, self.slot);
        // everything reached VRAM before the doorbell
        p.flush();
        core::sync::atomic::fence(Ordering::SeqCst);
    }

    /// Queue `push` without ringing the doorbell (the caller does, with [`ring`](Self::ring)).
    pub(super) fn queue_only(&mut self, p: &mut dyn VramIo, push: &[u32]) {
        self.queue(p, push);
    }

    pub(super) fn ring(&self) {
        self.regs.wr32(DOORBELL, self.tokens[0]);
    }

    /// Like [`submit`](Self::submit) but the copy engine releases `payload` in the
    /// host fence page `hfence` (`chan::HFENCE_VA`), which the CPU polls from cache.
    pub(super) fn submit_host(&mut self, p: &mut dyn VramIo, push: &[u32], hfence: *mut u32, payload: u32) -> Result<HostDone, String> {
        let t_q = crate::cpu::tsc::read();
        // SAFETY: `hfence` is a live 4 KiB DMA page of ours.
        unsafe { core::ptr::write_volatile(hfence, 0) };
        self.queue(p, push);
        let t0 = crate::cpu::tsc::read();
        let queue_ticks = t0.wrapping_sub(t_q);
        self.ring();
        let limit = ms_ticks(FENCE_TIMEOUT_MS * 2);
        loop {
            // SAFETY: as above.
            if unsafe { core::ptr::read_volatile(hfence) } == payload {
                return Ok(HostDone { queue_ticks, ticks: crate::cpu::tsc::read().wrapping_sub(t0), t0 });
            }
            if crate::cpu::tsc::read().wrapping_sub(t0) > limit {
                return Err(alloc::format!("the host fence never reached {:#x} in {} ms (page holds {:#x})", payload, FENCE_TIMEOUT_MS * 2, unsafe { core::ptr::read_volatile(hfence) }));
            }
        }
    }

    /// Put `push` in the push buffer, a GPFIFO entry after it, advance GP_PUT,
    /// ring the doorbell and wait for the copy engine to release `payload` in
    /// the fence word.
    fn submit(&mut self, p: &mut Pramin, push: &[u32], payload: u32) -> Result<Done, String> {
        p.wr32(chan::FENCE_VRAM, 0);
        self.queue(p, push);
        let t0 = crate::cpu::tsc::read();
        self.regs.wr32(DOORBELL, self.tokens[0]);
        let limit = ms_ticks(FENCE_TIMEOUT_MS);
        let mut second = self.tokens[1] == self.tokens[0];
        loop {
            if p.rd32(chan::FENCE_VRAM) == payload {
                return Ok(Done { ticks: crate::cpu::tsc::read().wrapping_sub(t0), second_token: second && self.tokens[1] != self.tokens[0] });
            }
            if !second && crate::cpu::tsc::read().wrapping_sub(t0) > ms_ticks(300) {
                second = true;
                self.regs.wr32(DOORBELL, self.tokens[1]);
            }
            if crate::cpu::tsc::read().wrapping_sub(t0) > limit {
                return Err(alloc::format!(
                    "the fence never reached {:#x} in {} ms: fence {:#x}, USERD GPGet {} GPPut {}, Get {:#x} Reference {:#x} TopLevelGet {:#x} GetHi {:#x}; 0x2100 {:#x}, 0xb65000 {:#x}",
                    payload,
                    FENCE_TIMEOUT_MS,
                    p.rd32(chan::FENCE_VRAM),
                    p.rd32(chan::USERD_VRAM + chan::USERD_GP_GET),
                    p.rd32(chan::USERD_VRAM + chan::USERD_GP_PUT),
                    p.rd32(chan::USERD_VRAM + 0x44),
                    p.rd32(chan::USERD_VRAM + 0x48),
                    p.rd32(chan::USERD_VRAM + 0x58),
                    p.rd32(chan::USERD_VRAM + 0x60),
                    self.regs.rd32(0x2100),
                    self.regs.rd32(0xb6_5000)
                ));
            }
        }
    }
}

/// One rung of the ladder: submit, wait, report.
fn rung(r: &mut String, rm: &mut Rm, p: &mut Pramin, sub: &mut Submitter, name: &str, push: &[u32], payload: u32) -> Option<Done> {
    match sub.submit(p, push, payload) {
        Ok(d) => {
            let _ = writeln!(
                r,
                "copy: rung {}: OK in {} us{}",
                name,
                d.ticks * 1_000_000 / crate::cpu::tsc::freq_hz(),
                if d.second_token { " (only after RM's own token was rung too)" } else { "" }
            );
            Some(d)
        }
        Err(e) => {
            let events = rm.drain(50);
            stop(r, format_args!("rung {}: {}; RM events {:x?}", name, e, events));
            None
        }
    }
}

/// Fill 4 KiB of VRAM with `f(word index)`.
fn vram_fill(p: &mut Pramin, at: u64, f: impl Fn(usize) -> u32) {
    for k in 0..1024 {
        p.wr32(at + 4 * k as u64, f(k));
    }
}

/// The first of 4 KiB of VRAM that differs from `f`: (index, wanted, got).
fn vram_diff(p: &mut Pramin, at: u64, f: impl Fn(usize) -> u32) -> Option<(usize, u32, u32)> {
    (0..1024).find_map(|k| {
        let got = p.rd32(at + 4 * k as u64);
        (got != f(k)).then(|| (k, f(k), got))
    })
}

/// KiB/s for `bytes` in `ticks` of the TSC.
fn kbps(bytes: u64, ticks: u64) -> u64 {
    let hz = crate::cpu::tsc::freq_hz();
    // bytes / (ticks / hz) / 1024
    (bytes as u128 * hz as u128 / 1024 / ticks.max(1) as u128) as u64
}

pub(super) fn run(r: &mut String, regs: &Bar0, rm: &mut Rm, bufs: Buffers) {
    let mut p = Pramin::new(regs);
    // A clean start: RM writes the instance block (RAMFC), the GPU the rest.
    for off in (0..0x1_0000u64).step_by(4) {
        p.wr32(chan::VRAM_CHAN + off, 0);
    }
    let alloc = ChanAlloc {
        chid: CHID,
        privileged: true,
        engine_type: chan::ENGINE_COPY2,
        gpfifo_va: chan::GPFIFO_VA,
        gpfifo_bytes: chan::GPFIFO_ENTRIES * 8,
        inst: chan::INST_VRAM,
        userd: chan::USERD_VRAM,
        mthdbuf: bufs.mthdbuf.bus_addr(),
        vaspace: rm::H_VASPACE,
    };
    let ch = chan::h_chan(CHID);
    let result = (|| -> Result<(u32, u32, u32, &'static str), String> {
        let reply = rm.alloc(rm::H_DEVICE, ch, chan::CLASS_GPFIFO, &chan::alloc_params(&alloc)).map_err(|e| alloc::format!("ALLOC channel: {}", e))?;
        let cid = chan::cid_from_reply(&reply).ok_or_else(|| String::from("ALLOC channel: short reply"))?;
        CID.store(cid, Ordering::Relaxed);
        let _ = writeln!(r, "copy: channel {:#x} (class {:#x}, engine {:#x}) allocated: RM's channel id {}", ch, chan::CLASS_GPFIFO, chan::ENGINE_COPY2, cid);
        rm.control(ch, chan::CTRL_BIND, &chan::bind_params(chan::ENGINE_COPY2)).map_err(|e| alloc::format!("BIND: {}", e))?;
        rm.control(ch, chan::CTRL_GPFIFO_SCHEDULE, &chan::schedule_params()).map_err(|e| alloc::format!("GPFIFO_SCHEDULE: {}", e))?;
        rm.alloc(ch, chan::H_COPY, chan::CLASS_COPY, &chan::copy_params(chan::ENGINE_COPY2)).map_err(|e| alloc::format!("ALLOC copy object: {}", e))?;
        // GSP's own token is for runlist 0 (boot #102/#103); the doorbell wants the
        // channel's real runlist: ask RM's device table which one CE2 is on.
        let rm_token = rm
            .control(ch, chan::CTRL_GET_WORK_SUBMIT_TOKEN, &chan::token_request_params())
            .map_err(|e| alloc::format!("GET_WORK_SUBMIT_TOKEN: {}", e))
            .and_then(|t| chan::token_from_params(&t).ok_or_else(|| String::from("GET_WORK_SUBMIT_TOKEN: short reply")))?;
        let table = rm.control(rm::H_SUBDEVICE, chan::CTRL_FIFO_GET_DEVICE_INFO_TABLE, &chan::device_info_params());
        let (runlist, how) = match &table {
            Ok(t) => match chan::runlist_for_engine(t, chan::ENGINE_COPY2) {
                Some(r) => (r, "RM's device table"),
                None => (1, "the trace (engine not in RM's table)"),
            },
            Err(e) => {
                let _ = writeln!(r, "copy: GET_DEVICE_INFO_TABLE on our subdevice: {}", e);
                (1, "the trace (RM refused the table)")
            }
        };
        Ok((chan::doorbell_token(runlist, CHID), rm_token, runlist, how))
    })();
    let (token, rm_token, runlist, how) = match result {
        Ok(t) => t,
        Err(e) => {
            p.restore();
            return stop(r, format_args!("{}", e));
        }
    };
    TOKEN.store(token, Ordering::Relaxed);
    let _ = writeln!(
        r,
        "copy: channel scheduled, copy object {:#x} allocated; CE2 is on runlist {} ({}); doorbell token {:#x} (runlist << 16 | chid {}), RM's own token {:#x}",
        chan::H_COPY, runlist, how, token, CHID, rm_token
    );

    let mut sub = Submitter::new(regs, [token, rm_token]);

    // The bring-up ladder: each rung adds one thing, and the first that fails is
    // named (boots #102-#104 got as far as the GPU reading the GPFIFO entry).
    // 1. a bare semaphore release: the channel runs, the fence page is writable.
    if rung(r, rm, &mut p, &mut sub, "semaphore release", &chan::release_push(chan::FENCE_VA, 1), 1).is_none() {
        p.restore();
        return;
    }
    // 2. VRAM -> VRAM, 4 KiB: the copy engine and our VRAM mappings.
    let (v_src, v_dst) = (chan::VRAM_DST, chan::VRAM_DST + 0x10_0000);
    vram_fill(&mut p, v_src, |k| pattern(0, k));
    vram_fill(&mut p, v_dst, |_| 0);
    if rung(r, rm, &mut p, &mut sub, "VRAM -> VRAM 4 KiB", &chan::copy_push(chan::DST_VA, chan::DST_VA + 0x10_0000, 0x1000, chan::FENCE_VA, 2), 2).is_none() {
        p.restore();
        return;
    }
    if let Some((k, want, got)) = vram_diff(&mut p, v_dst, |k| pattern(0, k)) {
        p.restore();
        return stop(r, format_args!("rung VRAM -> VRAM 4 KiB: the fence came but word {} is {:#x}, wanted {:#x}", k, got, want));
    }
    // 3. system -> VRAM, 4 KiB: the system-memory mapping and the PCIe path.
    let v_dst = chan::VRAM_DST + 0x20_0000;
    vram_fill(&mut p, v_dst, |_| 0);
    if rung(r, rm, &mut p, &mut sub, "system -> VRAM 4 KiB", &chan::copy_push(chan::SRC_VA, chan::DST_VA + 0x20_0000, 0x1000, chan::FENCE_VA, 3), 3).is_none() {
        p.restore();
        return;
    }
    if let Some((k, want, got)) = vram_diff(&mut p, v_dst, |k| pattern(0, k)) {
        p.restore();
        return stop(r, format_args!("rung system -> VRAM 4 KiB: the fence came but word {} is {:#x}, wanted {:#x}", k, got, want));
    }
    // 4. the measured round trip, COPY_BYTES each way.
    let up = sub.submit(&mut p, &chan::copy_push(chan::SRC_VA, chan::DST_VA, COPY_BYTES as u32, chan::FENCE_VA, 4), 4);
    let up = match up {
        Ok(d) => d,
        Err(e) => {
            p.restore();
            let events = rm.drain(50);
            return stop(r, format_args!("copy up: {}; RM events {:x?}", e, events));
        }
    };
    let down = sub.submit(&mut p, &chan::copy_push(chan::DST_VA, chan::BACK_VA, COPY_BYTES as u32, chan::FENCE_VA, 5), 5);
    p.restore();
    let down = match down {
        Ok(d) => d,
        Err(e) => {
            let events = rm.drain(50);
            return stop(r, format_args!("copy down: {}; RM events {:x?}", e, events));
        }
    };

    // Did the data survive the round trip?
    let mut bad = 0u64;
    let mut first = None;
    for i in 0..COPY_PAGES {
        let mut page = [0u8; 0x1000];
        bufs.back.page(i).read(0, &mut page);
        for (k, w) in page.chunks_exact(4).enumerate() {
            let got = u32::from_le_bytes(w.try_into().unwrap());
            if got != pattern(i, k) {
                bad += 1;
                first.get_or_insert((i * 0x1000 + k * 4, pattern(i, k), got));
            }
        }
    }
    MISMATCH.store(bad, Ordering::Relaxed);
    let (u, d) = (kbps(COPY_BYTES, up.ticks), kbps(COPY_BYTES, down.ticks));
    UP_KBPS.store(u, Ordering::Relaxed);
    DOWN_KBPS.store(d, Ordering::Relaxed);
    if up.second_token || down.second_token {
        let _ = writeln!(r, "copy: NOTE: a copy needed RM's own token (up {}, down {})", up.second_token, down.second_token);
    }
    let _ = writeln!(
        r,
        "copy: {} KiB system -> VRAM in {} us ({} MB/s), VRAM -> system in {} us ({} MB/s)",
        COPY_BYTES / 1024,
        up.ticks * 1_000_000 / crate::cpu::tsc::freq_hz(),
        u / 1024,
        down.ticks * 1_000_000 / crate::cpu::tsc::freq_hz(),
        d / 1024
    );
    if let Some((off, want, got)) = first {
        return stop(r, format_args!("{} words differ after the round trip; first at byte {:#x}: wanted {:#x}, read {:#x}", bad, off, want, got));
    }
    STATE.store(1, Ordering::Relaxed);
    let _ = writeln!(r, "copy: OK: {} KiB went system -> VRAM -> system through the copy engine and came back identical", COPY_BYTES / 1024);
    // Phase 6d: the measurements, on the same channel, then the channel for run-time use.
    super::bench::run(r, regs, rm, &mut sub, &bufs);
    install(r, regs, &sub, &bufs);
    // The channel, the buffers and the mappings stay: RM and the GPU own them now.
    core::mem::forget(bufs);
}

// ---- run time (phase 6d) ----------------------------------------------------------------

/// The channel after the boot: the tokens, the rings' positions, a write-combined BAR1 view of
/// the channel's VRAM for the CPU to write GPFIFO/push/USERD, and the host fence page.
struct Runtime {
    regs: Bar0,
    tokens: [u32; 2],
    slot: u32,
    push_at: u32,
    io: Bar1Io,
    hfence: usize,
    /// A run-time test faulted the channel on purpose (`selftest_fault`): RM tore it down.
    dead: bool,
}

static RUNTIME: crate::sync::Mutex<Option<Runtime>> = crate::sync::Mutex::new(None);

fn install(r: &mut String, regs: &Bar0, sub: &Submitter, bufs: &Buffers) {
    let Some(pci) = super::gsp::pci_info().filter(|p| p.bar1 != 0) else {
        let _ = writeln!(r, "copy: run-time channel not installed: no BAR1");
        return;
    };
    // SAFETY: BAR1 is the VRAM aperture; [VRAM_CHAN, +64 KiB) is the channel's own memory
    // (`chan` layout), written from here on only through this mapping and PRAMIN at boot.
    let Some(v) = (unsafe { crate::memory::mmio::map(x86_64::PhysAddr::new(pci.bar1 + chan::VRAM_CHAN), CHAN_BLOCK as usize) }) else {
        let _ = writeln!(r, "copy: run-time channel not installed: cannot map BAR1");
        return;
    };
    let wc = crate::memory::memtype::set_pat_index_range(v.as_u64(), CHAN_BLOCK, hal::memtype::PAT_WC_INDEX).is_ok();
    // Do stores through this mapping land at the VRAM address they name? The last words of each page of
    // the block (unused by a channel that has queued fewer than 500 entries), written through BAR1 and read back through PRAMIN.
    let mut io = Bar1Io { base: v.as_u64() as *mut u8 };
    let mut p = Pramin::new(regs);
    let _ = writeln!(r, "copy: BAR block registers now: {}", super::gsp::bar_regs_text(&super::gsp::bar_regs_now(regs)));
    let mut bad = 0;
    for page in 0..(CHAN_BLOCK / 0x1000) {
        let at = chan::VRAM_CHAN + page * 0x1000 + 0xff0;
        io.wr32(at, 0xc0de_0000 | page as u32);
    }
    io.flush();
    for page in 0..(CHAN_BLOCK / 0x1000) {
        let at = chan::VRAM_CHAN + page * 0x1000 + 0xff0;
        let got = p.rd32(at);
        if got != 0xc0de_0000 | page as u32 {
            bad += 1;
            let _ = writeln!(r, "copy: BAR1 self-check: VRAM {:#x} holds {:#x}, wrote {:#x}", at, got, 0xc0de_0000u32 | page as u32);
        }
    }
    p.restore();
    let _ = writeln!(r, "copy: BAR1 self-check: {} of {} pages of the channel block did not take the store", bad, CHAN_BLOCK / 0x1000);
    if bad != 0 {
        let _ = writeln!(r, "copy: run-time channel NOT installed: BAR1 does not reach VRAM");
        return;
    }
    *RUNTIME.lock() = Some(Runtime {
        regs: Bar0 { base: regs.base, len: regs.len },
        tokens: sub.tokens,
        slot: sub.slot,
        push_at: sub.push_at,
        io,
        hfence: bufs.hfence.virt() as usize,
        dead: false,
    });
    let _ = writeln!(r, "copy: run-time channel installed: rings written through BAR1 ({}), fence in host memory", if wc { "write-combined" } else { "NOT write-combined" });
}

static IRQ_REPORT: spin::Mutex<String> = spin::Mutex::new(String::new());

fn median(v: &mut [u64]) -> u64 {
    v.sort_unstable();
    v[v.len() / 2]
}

/// `/dev/dispctl` `copy irq <runs>`: `runs` copies of 4 KiB, 1 MiB and 4 MiB launched with the
/// non-stall interrupt at run time (IF=1, rings written through BAR1, host fence); for each, when
/// the CPU saw the fence by polling and when the MSI handler saw the interrupt, both from the
/// doorbell. Interrupts that never came are counted.
pub fn selftest_irq(runs: usize) -> Result<(), &'static str> {
    let runs = runs.clamp(1, 1000);
    let mut g = RUNTIME.lock();
    let rt = g.as_mut().ok_or("no run-time channel")?;
    if rt.dead {
        return Err("the channel was faulted on purpose");
    }
    let hz = crate::cpu::tsc::freq_hz();
    let ns = |t: u64| (t as u128 * 1_000_000_000 / hz as u128) as u64;
    let was_on = x86_64::instructions::interrupts::are_enabled();
    if !was_on {
        x86_64::instructions::interrupts::enable();
    }
    let irq0 = super::intr::CE.count();
    let mut out = String::new();
    let _ = write!(out, "gpu_copyirq: runs={}", runs);
    let mut failed = None;
    let mut payload = 20_000u32;
    'sizes: for (name, size) in [("4k", 4u32 << 10), ("1m", 1 << 20), ("4m", 4 << 20)] {
        let mut poll = alloc::vec![0u64; runs];
        let mut irq = alloc::vec![0u64; runs];
        let mut queue = alloc::vec![0u64; runs];
        let mut missed = 0;
        for k in 0..runs {
            payload += 1;
            let push = chan::with_interrupt(chan::copy_push(chan::SRC_VA, chan::DST_VA, size, chan::HFENCE_VA, payload));
            let c0 = super::intr::CE.count();
            let mut sub = Submitter::new(&rt.regs, rt.tokens);
            sub.slot = rt.slot;
            sub.push_at = rt.push_at;
            let d = sub.submit_host(&mut rt.io, &push, rt.hfence as *mut u32, payload);
            rt.slot = sub.slot;
            rt.push_at = sub.push_at;
            let d = match d {
                Ok(d) => d,
                Err(e) => {
                    // What is in VRAM now (PRAMIN: read once, only for this report)
                    let mut p = Pramin::new(&rt.regs);
                    let slot_prev = (rt.slot + chan::GPFIFO_ENTRIES - 1) % chan::GPFIFO_ENTRIES;
                    let gp = [p.rd32(chan::GPFIFO_VRAM + 8 * slot_prev as u64), p.rd32(chan::GPFIFO_VRAM + 8 * slot_prev as u64 + 4)];
                    let expect = chan::gp_entry(chan::PUSH_VA, 0x1000);
                    let diag = alloc::format!(
                        "{}; slot {} push_at {}; GPFIFO[{}] = {:#x}/{:#x}; USERD GPGet {} GPPut {}; fence(VRAM) {:#x}; push words {:x?}; entry for VA {:#x} would be {:#x}; 0x2100 {:#x}",
                        e,
                        rt.slot,
                        rt.push_at,
                        slot_prev,
                        gp[0],
                        gp[1],
                        p.rd32(chan::USERD_VRAM + chan::USERD_GP_GET),
                        p.rd32(chan::USERD_VRAM + chan::USERD_GP_PUT),
                        p.rd32(chan::FENCE_VRAM),
                        (0..8).map(|i| p.rd32(chan::PUSH_VRAM + 4 * i)).collect::<alloc::vec::Vec<_>>(),
                        chan::PUSH_VA,
                        expect,
                        rt.regs.rd32(0x2100)
                    );
                    p.restore();
                    failed = Some(diag);
                    break 'sizes;
                }
            };
            poll[k] = d.ticks;
            queue[k] = d.queue_ticks;
            let t_wait = crate::cpu::tsc::read();
            loop {
                if super::intr::CE.count() > c0 {
                    irq[k] = super::intr::CE.last_tsc().wrapping_sub(d.t0);
                    break;
                }
                if crate::cpu::tsc::read().wrapping_sub(t_wait) > ms_ticks(20) {
                    missed += 1;
                    break;
                }
                core::hint::spin_loop();
            }
        }
        let got: alloc::vec::Vec<u64> = irq.iter().copied().filter(|&t| t != 0).collect();
        let mut got = got;
        let irq_ns = if got.is_empty() { 0 } else { ns(median(&mut got)) };
        let _ = write!(out, " {}_queue_ns={} {}_poll_ns={} {}_irq_ns={} {}_missed={}", name, ns(median(&mut queue)), name, ns(median(&mut poll)), name, irq_ns, name, missed);
    }
    if !was_on {
        x86_64::instructions::interrupts::disable();
    }
    let _ = write!(out, " irqs={} vector={:?}", super::intr::CE.count() - irq0, super::intr::CE.vector());
    if let Some(e) = &failed {
        let _ = write!(out, " FAILED=\"{}\"", e);
    }
    *IRQ_REPORT.lock() = out;
    if failed.is_some() { Err("a copy did not complete") } else { Ok(()) }
}

static FAULT_REPORT: spin::Mutex<String> = spin::Mutex::new(String::new());

/// `/dev/dispctl` `copy fault`: a copy whose destination is not mapped, at run time. The copy
/// engine's MMU fault makes RM reset the channel (RC) and tell the host: this shows what reaches
/// the CPU (interrupt vectors, the events in RM's status queue). Kills the channel: run it last.
pub fn selftest_fault() -> Result<(), &'static str> {
    use nvgpu::vblank as vb;
    let mut g = RUNTIME.lock();
    let rt = g.as_mut().ok_or("no run-time channel")?;
    if rt.dead {
        return Err("the channel was faulted already");
    }
    let hz = crate::cpu::tsc::freq_hz();
    let was_on = x86_64::instructions::interrupts::are_enabled();
    if !was_on {
        x86_64::instructions::interrupts::enable();
    }
    // nothing of ours is mapped just below DST_VA
    const HOLE_VA: u64 = chan::DST_VA - 0x100_0000;
    let payload = 30_000u32;
    let push = chan::with_interrupt(chan::copy_push(chan::SRC_VA, HOLE_VA, 4096, chan::HFENCE_VA, payload));
    let ce0 = super::intr::CE.count();
    let ev0 = super::gsp::events_seen();
    let before = vb::leaf_stats(&rt.regs);
    let mut sub = Submitter::new(&rt.regs, rt.tokens);
    sub.slot = rt.slot;
    sub.push_at = rt.push_at;
    // SAFETY: the host fence page is ours.
    unsafe { core::ptr::write_volatile(rt.hfence as *mut u32, 0) };
    sub.queue_only(&mut rt.io, &push);
    let t0 = crate::cpu::tsc::read();
    sub.ring();
    rt.slot = sub.slot;
    rt.push_at = sub.push_at;
    rt.dead = true;
    // wait: the fence must NOT arrive; RM's events (polled by the vblank handler and by us) may
    let mut fence_at = None;
    let mut first_event_us = None;
    while crate::cpu::tsc::read().wrapping_sub(t0) < ms_ticks(1500) {
        // SAFETY: as above.
        if fence_at.is_none() && unsafe { core::ptr::read_volatile(rt.hfence as *const u32) } == payload {
            fence_at = Some(crate::cpu::tsc::read().wrapping_sub(t0));
        }
        if first_event_us.is_none() && super::gsp::events_seen() > ev0 {
            first_event_us = Some(crate::cpu::tsc::read().wrapping_sub(t0) * 1_000_000 / hz);
        }
        core::hint::spin_loop();
    }
    let after = vb::leaf_stats(&rt.regs);
    let new = vb::new_vectors(&before, &after);
    if !was_on {
        x86_64::instructions::interrupts::disable();
    }
    let out = alloc::format!(
        "gpu_copyfault: fence_arrived={:?} ce_irqs={} rm_events_new={} first_event_us={:?} new_vectors={:?} leaves_after={:x?}",
        fence_at,
        super::intr::CE.count() - ce0,
        super::gsp::events_seen() - ev0,
        first_event_us,
        new,
        after
    );
    *FAULT_REPORT.lock() = out;
    Ok(())
}

pub fn render_fault_kdebug() -> String {
    FAULT_REPORT.lock().clone()
}

/// `/proc/kdebug` line of the last run-time self-test.
pub fn render_irq_kdebug() -> String {
    IRQ_REPORT.lock().clone()
}

/// `/proc/kdebug` line (empty when the level was not asked for).
pub fn render_kdebug() -> String {
    match STATE.load(Ordering::Relaxed) {
        0 => String::new(),
        s => alloc::format!(
            "gpu_copy: state={} cid={} token={:#x} up_kbps={} down_kbps={} mismatch={}",
            if s == 1 { "ok" } else { "failed" },
            CID.load(Ordering::Relaxed),
            TOKEN.load(Ordering::Relaxed),
            UP_KBPS.load(Ordering::Relaxed),
            DOWN_KBPS.load(Ordering::Relaxed),
            MISMATCH.load(Ordering::Relaxed)
        ),
    }
}
