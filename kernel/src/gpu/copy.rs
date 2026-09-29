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

/// `NV_USERMODE_NOTIFY_CHANNEL_PENDING` (`clc361.h:33`) in the usermode window
/// (`vfn/gv100.c:28`: BAR0 `0x810000`): the value is the work-submit token.
const DOORBELL: u32 = 0x81_0000 + 0x90;
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
}

/// The pattern of `src`: word `k` of page `i`.
fn pattern(page: usize, k: usize) -> u32 {
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
    Some(Buffers { mthdbuf, src, back })
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
        Ok(())
    }
}

/// Write `words` to VRAM at `at` through PRAMIN.
fn vram_write(p: &mut Pramin, at: u64, words: &[u32]) {
    for (i, w) in words.iter().enumerate() {
        p.wr32(at + 4 * i as u64, *w);
    }
}

fn ms_ticks(ms: u64) -> u64 {
    crate::cpu::tsc::freq_hz() / 1000 * ms
}

/// One submitted copy's result.
struct Done {
    ticks: u64,
}

struct Submitter<'a> {
    regs: &'a Bar0,
    token: u32,
    slot: u32,
    push_at: u32,
}

impl Submitter<'_> {
    /// Put `push` in the push buffer, a GPFIFO entry after it, advance GP_PUT,
    /// ring the doorbell and wait for the copy engine to release `payload` in
    /// the fence word.
    fn submit(&mut self, p: &mut Pramin, push: &[u32], payload: u32) -> Result<Done, String> {
        let len = (push.len() * 4) as u32;
        assert!(self.push_at + len <= 0x1000 && self.slot < chan::GPFIFO_ENTRIES);
        vram_write(p, chan::PUSH_VRAM + self.push_at as u64, push);
        p.wr32(chan::FENCE_VRAM, 0);
        let e = chan::gp_entry(chan::PUSH_VA + self.push_at as u64, len);
        vram_write(p, chan::GPFIFO_VRAM + 8 * self.slot as u64, &[e as u32, (e >> 32) as u32]);
        self.slot += 1;
        self.push_at += len;
        p.wr32(chan::USERD_VRAM + chan::USERD_GP_PUT, self.slot);
        // everything reached VRAM before the doorbell
        let _ = p.rd32(chan::USERD_VRAM + chan::USERD_GP_PUT);
        core::sync::atomic::fence(Ordering::SeqCst);
        let t0 = crate::cpu::tsc::read();
        self.regs.wr32(DOORBELL, self.token);
        let limit = ms_ticks(FENCE_TIMEOUT_MS);
        loop {
            if p.rd32(chan::FENCE_VRAM) == payload {
                return Ok(Done { ticks: crate::cpu::tsc::read().wrapping_sub(t0) });
            }
            if crate::cpu::tsc::read().wrapping_sub(t0) > limit {
                return Err(alloc::format!(
                    "the fence never reached {:#x} in {} ms: fence {:#x}, USERD GPGet {} GPPut {}, Get {:#x}",
                    payload,
                    FENCE_TIMEOUT_MS,
                    p.rd32(chan::FENCE_VRAM),
                    p.rd32(chan::USERD_VRAM + chan::USERD_GP_GET),
                    p.rd32(chan::USERD_VRAM + chan::USERD_GP_PUT),
                    p.rd32(chan::USERD_VRAM + 0x44)
                ));
            }
        }
    }
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
        engine_type: chan::ENGINE_COPY0,
        gpfifo_va: chan::GPFIFO_VA,
        gpfifo_bytes: chan::GPFIFO_ENTRIES * 8,
        inst: chan::INST_VRAM,
        userd: chan::USERD_VRAM,
        mthdbuf: bufs.mthdbuf.bus_addr(),
        vaspace: rm::H_VASPACE,
    };
    let ch = chan::h_chan(CHID);
    let result = (|| -> Result<u32, String> {
        let reply = rm.alloc(rm::H_DEVICE, ch, chan::CLASS_GPFIFO, &chan::alloc_params(&alloc)).map_err(|e| alloc::format!("ALLOC channel: {}", e))?;
        let cid = chan::cid_from_reply(&reply).ok_or_else(|| String::from("ALLOC channel: short reply"))?;
        CID.store(cid, Ordering::Relaxed);
        let _ = writeln!(r, "copy: channel {:#x} (class {:#x}, engine {:#x}) allocated: RM's channel id {}", ch, chan::CLASS_GPFIFO, chan::ENGINE_COPY0, cid);
        rm.control(ch, chan::CTRL_BIND, &chan::bind_params(chan::ENGINE_COPY0)).map_err(|e| alloc::format!("BIND: {}", e))?;
        rm.control(ch, chan::CTRL_GPFIFO_SCHEDULE, &chan::schedule_params()).map_err(|e| alloc::format!("GPFIFO_SCHEDULE: {}", e))?;
        rm.alloc(ch, chan::H_COPY, chan::CLASS_COPY, &chan::copy_params(chan::ENGINE_COPY0)).map_err(|e| alloc::format!("ALLOC copy object: {}", e))?;
        let t = rm.control(ch, chan::CTRL_GET_WORK_SUBMIT_TOKEN, &chan::token_request_params()).map_err(|e| alloc::format!("GET_WORK_SUBMIT_TOKEN: {}", e))?;
        chan::token_from_params(&t).ok_or_else(|| String::from("GET_WORK_SUBMIT_TOKEN: short reply"))
    })();
    let token = match result {
        Ok(t) => t,
        Err(e) => {
            p.restore();
            return stop(r, format_args!("{}", e));
        }
    };
    TOKEN.store(token, Ordering::Relaxed);
    let _ = writeln!(r, "copy: channel scheduled, copy object {:#x} allocated, work-submit token {:#x}", chan::H_COPY, token);

    let mut sub = Submitter { regs, token, slot: 0, push_at: 0 };
    // src (system) -> VRAM
    let up = sub.submit(&mut p, &chan::copy_push(chan::SRC_VA, chan::DST_VA, COPY_BYTES as u32, chan::FENCE_VA, 1), 1);
    let up = match up {
        Ok(d) => d,
        Err(e) => {
            p.restore();
            let events = rm.drain(50);
            return stop(r, format_args!("copy up: {}; RM events {:x?}", e, events));
        }
    };
    // VRAM -> back (system)
    let down = sub.submit(&mut p, &chan::copy_push(chan::DST_VA, chan::BACK_VA, COPY_BYTES as u32, chan::FENCE_VA, 2), 2);
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
    // The channel, the buffers and the mappings stay: RM and the GPU own them now.
    core::mem::forget(bufs);
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
