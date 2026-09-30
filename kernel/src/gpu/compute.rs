// kernel/src/gpu/compute.rs
//
// Phase 7a of docs/gpu/gpu-plan.md: a GR channel with the Ampere compute class, `gpu=compute`. It
// extends `gpu=copy` (`vaspace.rs` calls `prepare`/`map` before the page tables are written and `run`
// after the copy channel). Everything is at boot with IF=0 and bounded waits.
//
// With GSP-RM the host owns the memory of a GR channel's context. What happens:
//   1. `prepare`: `GET_CONTEXT_BUFFERS_INFO` says how big each context buffer is; `nvgpu::gr::plan`
//      lays them out in VRAM and in our VA space; the ones RM initialises are zeroed (PRAMIN);
//   2. `map`: the tables get the context buffers (privileged, each with the page size nouveau uses: 4 KiB, 64 KiB or 2 MiB), the
//      channel's GPFIFO and push buffer, a VRAM destination page and a host page (data + semaphores);
//   3. `run`: the *golden* channel (ALLOC 0xc56f on GR0, `PROMOTE_CTX` with every buffer, ALLOC 0xc797
//      so RM builds the golden context image, then freed) exactly as nouveau does it at init
//      (`r535_gr_oneinit`); then the real channel (ALLOC, BIND, SCHEDULE, `PROMOTE_CTX` with its own
//      buffers and the golden's globals, ALLOC 0xc7c0) and a ladder of pushes, each waited on through a
//      semaphore in host memory:
//        (1) `SET_REPORT_SEMAPHORE` release (the GR pipeline runs, the class is bound);
//        (2) an inline-to-memory write of 64 bytes into host memory, verified by the CPU;
//        (3) an inline write of 4 KiB into VRAM, verified through PRAMIN;
//        (4) phase 7b: a shader. `nvgpu::qmd` builds the QMD, `gr::dispatch_push` launches it (the memory windows,
//            `SEND_PCAS_A`, `SEND_SIGNALING_PCAS2_B`, then `WAIT_FOR_IDLE` and a semaphore); the SASS is
//            `nvgpu/fixtures/shader-fill-sm86.bin` (8 CTAs x 32 threads, thread i stores `qmd::fill_word(i)` at `out[i]`), its
//            code, QMD and constant buffer 0 live in host memory. Verified by the CPU: every word the grid should write, and that
//            the words past the grid are still the scribble the page started with, and the grid's own semaphore (`RELEASE0`);
//        (5) the same launch with the output in VRAM (the page rung 3 verified), verified by the GPU: a second shader
//            (`nvgpu/fixtures/shader-copy-sm86.bin`) reads that page (through the L2) into the host page, which the CPU reads;
//        (6) a measurement: which ways of reading that VRAM page through PRAMIN show the SM's stores to the CPU (plain, after
//            a wait, after the L2 flush by MMIO or by `MEM_OP`, or with a system-scope store): `vram_cpu_view` in /proc/kdebug.
//
// The oracle for the golden sequence is the trace (`nvgpu/fixtures/rm-ph7-gr-*`); the channel's own
// promote and the pushes have none and follow nouveau's source and NVIDIA's class headers
// (`nvgpu/src/gr.rs`); the launch follows Mesa's `nak/hw_runner.rs`, which runs on nouveau's channels.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use nvgpu::chan::{self, ChanAlloc};
use nvgpu::evo::Pramin;
use nvgpu::gr::{self, CtxBuf, Plan};
use nvgpu::mmu::{Flags, MapError, PageTables, Target};
use nvgpu::qmd;
use nvgpu::rm;
use nvgpu::Mmio;

use super::gsp::Rm;
use super::Bar0;
use crate::memory::dma::DmaBuf;

/// What the GPU may address (the same 40-bit limit as the GSP's buffers).
const DMA_MASK: u64 = (1 << 40) - 1;

/// `NVC361_NOTIFY_CHANNEL_PENDING` in the Ampere usermode window (see `copy.rs`).
const DOORBELL: u32 = 0xb8_0000 + 0x3_0000 + 0x90;
const CHID: u32 = 1;
const FENCE_TIMEOUT_MS: u64 = 2000;

// 0 = not run, 1 = OK, 2 = failed
static STATE: AtomicU32 = AtomicU32::new(0);
static RUNGS: AtomicU32 = AtomicU32::new(0);
static GOLDEN_MS: AtomicU64 = AtomicU64::new(0);
static CHAN_MS: AtomicU64 = AtomicU64::new(0);
static TOKEN: AtomicU32 = AtomicU32::new(0);
static CTX_KIB: AtomicU64 = AtomicU64::new(0);
static LAUNCH_HOST_US: AtomicU64 = AtomicU64::new(0);
static LAUNCH_VRAM_US: AtomicU64 = AtomicU64::new(0);
static GRID_RELEASED: AtomicU32 = AtomicU32::new(0);
/// Which way of reading the VRAM page first showed the SM's stores to the CPU: 0 none, then `CPU_VIEWS`.
static CPU_VIEW: AtomicU32 = AtomicU32::new(0);
const CPU_VIEWS: [&str; 6] = ["none", "plain", "wait", "mmio_flush", "memop_flush", "stwt"];

fn stop(r: &mut String, why: core::fmt::Arguments) {
    STATE.store(2, Ordering::Relaxed);
    let _ = writeln!(r, "compute: STOP: {}", why);
}

/// What `prepare` made and `map`/`run` use.
pub(super) struct Compute {
    bufs: Vec<CtxBuf>,
    plan: Plan,
    /// The method buffers (system memory, RM wants one per channel).
    mthd_golden: DmaBuf,
    mthd_chan: DmaBuf,
    /// The host page the pushes write data into (page 0) and release semaphores in (page 1).
    host: DmaBuf,
    /// The launch's host pages (`gr::KERN_*`): shader, QMD, constant buffer 0, output, semaphores.
    kern: DmaBuf,
}

/// Zero `len` bytes of VRAM at `at` through PRAMIN.
fn vram_zero(p: &mut Pramin, at: u64, len: u64) {
    for off in (0..len).step_by(4) {
        p.wr32(at + off, 0);
    }
}

pub(super) fn prepare(r: &mut String, regs: &Bar0, rm: &mut Rm) -> Option<Compute> {
    let info = match rm.control(rm::H_SUBDEVICE, gr::CTRL_GET_CONTEXT_BUFFERS_INFO, &gr::ctx_buffers_request()) {
        Ok(p) => p,
        Err(e) => {
            stop(r, format_args!("GET_CONTEXT_BUFFERS_INFO on our subdevice: {}", e));
            return None;
        }
    };
    let Some(bufs) = gr::ctx_buffers(&info) else {
        stop(r, format_args!("GET_CONTEXT_BUFFERS_INFO: a reply of {} bytes holds no buffer table", info.len()));
        return None;
    };
    let plan = gr::plan(&bufs);
    let mut total = 0u64;
    for (i, b) in bufs.iter().enumerate() {
        let _ = writeln!(
            r,
            "compute: ctx buffer {:2}: {:#9x} bytes, page 2^{}, align 2^{}{}{}{}: VRAM {:#x}, VA {:#x}",
            b.id,
            b.size,
            b.page,
            b.align,
            if b.global { ", global" } else { "" },
            if b.init { ", init" } else { "" },
            if b.ro { ", ro" } else { "" },
            plan.golden[i].pa,
            plan.golden[i].va
        );
        total += gr::mapped_len(b);
    }
    CTX_KIB.store(total / 1024, Ordering::Relaxed);

    let (mthd_golden, mthd_chan, host, kern) = match (
        DmaBuf::alloc(chan::MTHDBUF_SIZE as usize, DMA_MASK),
        DmaBuf::alloc(chan::MTHDBUF_SIZE as usize, DMA_MASK),
        DmaBuf::alloc((gr::HOST_PAGES * 0x1000) as usize, DMA_MASK),
        DmaBuf::alloc((gr::KERN_PAGES * 0x1000) as usize, DMA_MASK),
    ) {
        (Ok(a), Ok(b), Ok(c), Ok(d)) => (a, b, c, d),
        (a, b, c, d) => {
            stop(r, format_args!("host buffers: {:?} {:?} {:?} {:?}", a.err(), b.err(), c.err(), d.err()));
            return None;
        }
    };

    // A clean start: the channels' blocks, and the buffers RM initialises (nouveau allocates them
    // zeroed, `nvkm_memory_new(.., clear = init)`); the golden set and the channel's own.
    let t0 = crate::cpu::tsc::read();
    let mut p = Pramin::new(regs);
    vram_zero(&mut p, gr::VRAM_GOLDEN, 0x2_0000);
    let mut zeroed = 0u64;
    for (i, b) in bufs.iter().enumerate() {
        if b.init {
            vram_zero(&mut p, plan.golden[i].pa, gr::mapped_len(b));
            zeroed += gr::mapped_len(b);
            if !b.global {
                vram_zero(&mut p, plan.chan[i].pa, gr::mapped_len(b));
                zeroed += gr::mapped_len(b);
            }
        }
    }
    p.restore();
    let _ = writeln!(
        r,
        "compute: {} context buffers, {} KiB of VRAM at {:#x}..{:#x} (VA {:#x}..{:#x}); {} KiB zeroed in {} ms",
        bufs.len(),
        total / 1024,
        gr::VRAM_CTX,
        plan.vram_end,
        gr::VA_CTX,
        plan.va_end,
        zeroed / 1024,
        super::ms_since(t0)
    );
    Some(Compute { bufs, plan, mthd_golden, mthd_chan, host, kern })
}

impl Compute {
    /// The context buffers and the channel's pages in our VA space.
    pub(super) fn map(&self, pt: &mut PageTables) -> Result<(), MapError> {
        for m in gr::mappings(&self.bufs, &self.plan) {
            // gf100_vmm_map_v0 { priv = 1, ro = ctxbuf.ro } (`gr.c:106-109`)
            let f = Flags { privileged: true, read_only: m.ro, kind: 0 };
            match m.page {
                21 => pt.map_huge_range(m.va, m.pa, m.len, Target::Vram, f)?,
                16 => pt.map_big_range(m.va, m.pa, m.len, Target::Vram, f)?,
                _ => pt.map_range(m.va, m.pa, m.len, Target::Vram, f)?,
            }
        }
        let f = Flags::default();
        pt.map_range(gr::GPFIFO_VA, gr::CHAN_GPFIFO, gr::GPFIFO_ENTRIES as u64 * 8, Target::Vram, f)?;
        pt.map_range(gr::PUSH_VA, gr::CHAN_PUSH, 0x1000, Target::Vram, f)?;
        pt.map_range(gr::VDST_VA, gr::CHAN_DST, 0x1000, Target::Vram, f)?;
        for i in 0..gr::HOST_PAGES {
            pt.map(gr::HOST_VA + i * 0x1000, self.host.bus_addr() + i * 0x1000, Target::Host, f)?;
        }
        for i in 0..gr::KERN_PAGES {
            pt.map(gr::KERN_VA + i * 0x1000, self.kern.bus_addr() + i * 0x1000, Target::Host, f)?;
        }
        Ok(())
    }
}

/// One channel's rings and doorbell.
struct Sub<'a> {
    regs: &'a Bar0,
    token: u32,
    slot: u32,
    push_at: u32,
}

impl Sub<'_> {
    /// The push buffer, a GPFIFO entry, GP_PUT, the doorbell; then wait for `payload` at the host
    /// semaphore. Returns the microseconds it took, or what the GPU state says when it never came.
    fn submit(&mut self, p: &mut Pramin, host: &DmaBuf, sem_off: u64, push: &[u32], payload: u32) -> Result<u64, String> {
        let len = (push.len() * 4) as u32;
        assert!(len as usize <= gr::PUSH_BYTES);
        if self.push_at + len > 0x1000 {
            self.push_at = 0;
        }
        // the semaphore starts at 0, so a stale value cannot pass for the release
        host.copy_in(sem_off as usize, &0u32.to_le_bytes());
        for (i, w) in push.iter().enumerate() {
            p.wr32(gr::CHAN_PUSH + self.push_at as u64 + 4 * i as u64, *w);
        }
        let e = chan::gp_entry(gr::PUSH_VA + self.push_at as u64, len);
        let at = gr::CHAN_GPFIFO + 8 * (self.slot % gr::GPFIFO_ENTRIES) as u64;
        p.wr32(at, e as u32);
        p.wr32(at + 4, (e >> 32) as u32);
        self.slot = (self.slot + 1) % gr::GPFIFO_ENTRIES;
        self.push_at += len;
        p.wr32(gr::CHAN_USERD + chan::USERD_GP_PUT, self.slot);
        // everything reached VRAM before the doorbell
        let _ = p.rd32(gr::CHAN_USERD + chan::USERD_GP_PUT);
        core::sync::atomic::fence(Ordering::SeqCst);
        let t0 = crate::cpu::tsc::read();
        self.regs.wr32(DOORBELL, self.token);
        let sem = unsafe { host.virt().add(sem_off as usize) } as *const u32;
        let limit = super::copy::ms_ticks(FENCE_TIMEOUT_MS);
        loop {
            // SAFETY: the host buffer is a live DMA allocation of ours, at least `sem_off + 4` bytes long.
            if unsafe { core::ptr::read_volatile(sem) } == payload {
                return Ok(crate::cpu::tsc::read().wrapping_sub(t0) * 1_000_000 / crate::cpu::tsc::freq_hz());
            }
            if crate::cpu::tsc::read().wrapping_sub(t0) > limit {
                return Err(alloc::format!(
                    "the semaphore never reached {:#x} in {} ms (holds {:#x}); USERD GPGet {} GPPut {}, Get {:#x} TopLevelGet {:#x}; 0x2100 {:#x}",
                    payload,
                    FENCE_TIMEOUT_MS,
                    unsafe { core::ptr::read_volatile(sem) },
                    p.rd32(gr::CHAN_USERD + chan::USERD_GP_GET),
                    p.rd32(gr::CHAN_USERD + chan::USERD_GP_PUT),
                    p.rd32(gr::CHAN_USERD + 0x44),
                    p.rd32(gr::CHAN_USERD + 0x58),
                    self.regs.rd32(0x2100)
                ));
            }
        }
    }
}

/// A distinct word per position, so a misplaced or stale word shows.
fn pattern(i: usize) -> u32 {
    (i as u32).wrapping_mul(0x9e37_79b1) ^ 0xc0de_0000
}

/// Write back the L2's dirty lines and flush the FB (`gf100_ltc_flush`: write 1 to `0x70010` and wait for bits 1:0 to clear;
/// `NV_UFLUSH_FB_FLUSH` `0x70000` the same). Returns what happened, for the log.
fn l2_flush(regs: &Bar0) -> String {
    let wait = |reg: u32| -> Option<u64> {
        regs.wr32(reg, 1);
        let t0 = crate::cpu::tsc::read();
        let limit = super::copy::ms_ticks(2000);
        loop {
            if regs.rd32(reg) & 3 == 0 {
                return Some(crate::cpu::tsc::read().wrapping_sub(t0) * 1_000_000 / crate::cpu::tsc::freq_hz());
            }
            if crate::cpu::tsc::read().wrapping_sub(t0) > limit {
                return None;
            }
        }
    };
    let l2 = wait(0x70010);
    let fb = wait(0x70000);
    alloc::format!(
        "0x70010 {}, 0x70000 {}",
        l2.map(|us| alloc::format!("done in {} us", us)).unwrap_or_else(|| alloc::format!("still {:#x} after 2 s", regs.rd32(0x70010))),
        fb.map(|us| alloc::format!("done in {} us", us)).unwrap_or_else(|| alloc::format!("still {:#x} after 2 s", regs.rd32(0x70000)))
    )
}

/// Does the output page hold what the fill grid writes? `word(i)` reads word `i` of its 1024. `None` if so, else the first
/// difference, in words.
fn check_output(word: &mut dyn FnMut(usize) -> u32, want: fn(u32) -> u32) -> Option<String> {
    let first: Vec<u32> = (0..qmd::FILL_WORDS as usize).map(|i| word(i)).collect();
    if let Some(i) = (0..first.len()).find(|&i| first[i] != want(i as u32)) {
        let right = (0..first.len()).filter(|&j| first[j] == want(j as u32)).count();
        return Some(alloc::format!("word {} is {:#x}, wanted {:#x} ({} of the first {} are right)", i, first[i], want(i as u32), right, first.len()));
    }
    for i in qmd::FILL_WORDS as usize..1024 {
        let (got, was) = (word(i), qmd::scribble_word(i as u32));
        if got != was {
            return Some(alloc::format!("word {} (past the grid) is {:#x}, it was {:#x}: the grid wrote outside its range", i, got, was));
        }
    }
    None
}

/// Rungs 4 to 6: one launch of `shader` (code, registers) with constant buffer 0 `cbuf0`. Uploads the shader, constant buffer 0 and the QMD
/// (host memory, `gr::KERN_*`), pushes the launch and waits for the class's semaphore (`payload`, after `WAIT_FOR_IDLE`); the
/// grid's own release (`grid_payload`) is read afterwards. Returns the microseconds and whether the grid's release arrived.
fn launch(sub: &mut Sub, p: &mut Pramin, c: &Compute, shader: (&[u8], u8), cbuf0: &[u8], payload: u32, grid_payload: u32) -> Result<(u64, bool), String> {
    c.kern.copy_in(gr::KERN_SHADER_OFF as usize, shader.0);
    c.kern.copy_in(gr::KERN_CB0_OFF as usize, cbuf0);
    let launch = qmd::Launch {
        program: gr::KERN_VA + gr::KERN_SHADER_OFF,
        registers: shader.1,
        grid: [qmd::FILL_CTAS, 1, 1],
        block: [qmd::FILL_BLOCK, 1, 1],
        smem: 0,
        local: 0,
        cbuf0: (gr::KERN_VA + gr::KERN_CB0_OFF, qmd::FILL_CBUF0_BYTES),
        release: Some((gr::KERN_VA + gr::KERN_SEM_OFF + gr::KERN_SEM_GRID, grid_payload)),
    };
    c.kern.copy_in(gr::KERN_QMD_OFF as usize, &qmd::bytes(&qmd::build(&launch)));
    // neither semaphore can hold its value from before
    c.kern.copy_in((gr::KERN_SEM_OFF + gr::KERN_SEM_GRID) as usize, &0u32.to_le_bytes());
    let push = gr::dispatch_push(gr::KERN_VA + gr::KERN_QMD_OFF, gr::KERN_VA + gr::KERN_SEM_OFF, payload);
    let us = sub.submit(p, &c.kern, gr::KERN_SEM_OFF, &push, payload)?;
    let mut w = [0u8; 4];
    c.kern.read((gr::KERN_SEM_OFF + gr::KERN_SEM_GRID) as usize, &mut w);
    Ok((us, u32::from_le_bytes(w) == grid_payload))
}

pub(super) fn run(r: &mut String, regs: &Bar0, rm: &mut Rm, c: Compute) {
    let Some(gv) = golden(r, rm, regs, &c) else { return };
    let _ = writeln!(r, "compute: going on with the golden context from variant \"{}\"", gv.name);
    let t0 = crate::cpu::tsc::read();
    let mut p = Pramin::new(regs);
    let result = (|| -> Result<u32, String> {
        let alloc = ChanAlloc {
            chid: CHID,
            privileged: false,
            engine_type: gr::ENGINE_GR0,
            gpfifo_va: gr::GPFIFO_VA,
            gpfifo_bytes: gr::GPFIFO_ENTRIES * 8,
            inst: gr::CHAN_INST,
            userd: gr::CHAN_USERD,
            mthdbuf: c.mthd_chan.bus_addr(),
            vaspace: rm::H_VASPACE,
        };
        let ch = chan::h_chan(CHID);
        let reply = rm.alloc(rm::H_DEVICE, ch, chan::CLASS_GPFIFO, &chan::alloc_params(&alloc)).map_err(|e| alloc::format!("ALLOC GR channel: {}", e))?;
        let cid = chan::cid_from_reply(&reply).ok_or_else(|| String::from("ALLOC GR channel: short reply"))?;
        let _ = writeln!(r, "compute: GR channel {:#x} (class {:#x}, engine {}, chid {}) allocated: RM's channel id {}", ch, chan::CLASS_GPFIFO, gr::ENGINE_GR0, CHID, cid);
        rm.control(ch, chan::CTRL_BIND, &chan::bind_params(gr::ENGINE_GR0)).map_err(|e| alloc::format!("BIND: {}", e))?;
        rm.control(ch, chan::CTRL_GPFIFO_SCHEDULE, &chan::schedule_params()).map_err(|e| alloc::format!("GPFIFO_SCHEDULE: {}", e))?;
        // r535_gr_chan_new: the channel's own buffers and the golden's globals, promoted before the object
        let e = gr::entries(&c.bufs, false, &c.plan.chan);
        rm.control(rm::H_SUBDEVICE, gr::CTRL_PROMOTE_CTX, &gr::promote_params(rm::H_CLIENT, ch, &e)).map_err(|e| alloc::format!("PROMOTE_CTX for the channel: {}", e))?;
        let _ = writeln!(r, "compute: BIND, GPFIFO_SCHEDULE and PROMOTE_CTX ({} entries) accepted", e.len());
        rm.alloc(ch, gr::H_COMPUTE, gr::CLASS_COMPUTE, &[]).map_err(|e| alloc::format!("ALLOC compute object {:#x}: {}", gr::CLASS_COMPUTE, e))?;
        let rm_token = rm
            .control(ch, chan::CTRL_GET_WORK_SUBMIT_TOKEN, &chan::token_request_params())
            .map_err(|e| alloc::format!("GET_WORK_SUBMIT_TOKEN: {}", e))
            .and_then(|t| chan::token_from_params(&t).ok_or_else(|| String::from("GET_WORK_SUBMIT_TOKEN: short reply")))?;
        let table = rm.control(rm::H_SUBDEVICE, chan::CTRL_FIFO_GET_DEVICE_INFO_TABLE, &chan::device_info_params());
        let (runlist, how) = match &table {
            Ok(t) => match chan::runlist_for_engine(t, gr::ENGINE_GR0) {
                Some(r) => (r, "RM's device table"),
                None => (0, "the trace (GR0 not in RM's table)"),
            },
            Err(e) => {
                let _ = writeln!(r, "compute: GET_DEVICE_INFO_TABLE on our subdevice: {}", e);
                (0, "the trace (RM refused the table)")
            }
        };
        let token = chan::doorbell_token(runlist, CHID);
        let _ = writeln!(r, "compute: compute object {:#x} allocated; GR0 is on runlist {} ({}); doorbell token {:#x}, RM's own {:#x}", gr::CLASS_COMPUTE, runlist, how, token, rm_token);
        Ok(token)
    })();
    let token = match result {
        Ok(t) => t,
        Err(e) => {
            p.restore();
            let events = rm.drain(50);
            return stop(r, format_args!("{}; RM events {:x?}", e, events));
        }
    };
    let ms = super::ms_since(t0);
    CHAN_MS.store(ms, Ordering::Relaxed);
    TOKEN.store(token, Ordering::Relaxed);
    let _ = writeln!(r, "compute: the channel and its compute object took {} ms", ms);

    let mut sub = Sub { regs, token, slot: 0, push_at: 0 };
    let sem_va = gr::HOST_VA + gr::HOST_SEM_OFF;

    // Rung 1: the GR pipeline releases a semaphore.
    match sub.submit(&mut p, &c.host, gr::HOST_SEM_OFF, &gr::report_semaphore_push(sem_va, 0x1111), 0x1111) {
        Ok(us) => {
            let _ = writeln!(r, "compute: rung semaphore release: OK in {} us", us);
            RUNGS.store(1, Ordering::Relaxed);
        }
        Err(e) => {
            p.restore();
            let events = rm.drain(50);
            return stop(r, format_args!("rung semaphore release: {}; RM events {:x?}", e, events));
        }
    }

    // Rung 2: an inline write of 64 bytes into host memory (scribbled first, read back by the CPU).
    let data: Vec<u32> = (0..16).map(pattern).collect();
    let scribble: Vec<u8> = (0..64).map(|i| 0xa5 ^ i as u8).collect();
    c.host.copy_in(gr::HOST_DATA_OFF as usize, &scribble);
    match sub.submit(&mut p, &c.host, gr::HOST_SEM_OFF, &gr::inline_write_push(gr::HOST_VA + gr::HOST_DATA_OFF, &data, sem_va, 0x2222), 0x2222) {
        Ok(us) => {
            let mut got = [0u8; 64];
            c.host.read(gr::HOST_DATA_OFF as usize, &mut got);
            let bad = got.chunks_exact(4).enumerate().find(|(i, w)| u32::from_le_bytes((*w).try_into().unwrap()) != data[*i]);
            if let Some((i, w)) = bad {
                p.restore();
                return stop(r, format_args!("rung inline write to host memory: the semaphore came but word {} is {:#x}, wanted {:#x}", i, u32::from_le_bytes(w.try_into().unwrap()), data[i]));
            }
            let _ = writeln!(r, "compute: rung inline write 64 B -> host memory: OK in {} us, 16 words verified", us);
            RUNGS.store(2, Ordering::Relaxed);
        }
        Err(e) => {
            p.restore();
            let events = rm.drain(50);
            return stop(r, format_args!("rung inline write to host memory: {}; RM events {:x?}", e, events));
        }
    }

    // Rung 3: 4 KiB into VRAM, in inline writes of `INLINE_CHUNK_WORDS` (a whole page does not fit the push buffer),
    // each waited on; read back through PRAMIN (a different path from the writer's).
    let data: Vec<u32> = (0..1024).map(|i| pattern(i + 100)).collect();
    vram_zero(&mut p, gr::CHAN_DST, 0x1000);
    let mut us = 0;
    for (k, chunk) in data.chunks(gr::INLINE_CHUNK_WORDS).enumerate() {
        let payload = 0x3330 + k as u32;
        let dst = gr::VDST_VA + (k * gr::INLINE_CHUNK_WORDS * 4) as u64;
        match sub.submit(&mut p, &c.host, gr::HOST_SEM_OFF, &gr::inline_write_push(dst, chunk, sem_va, payload), payload) {
            Ok(t) => us += t,
            Err(e) => {
                p.restore();
                let events = rm.drain(50);
                return stop(r, format_args!("rung inline write to VRAM, chunk {}: {}; RM events {:x?}", k, e, events));
            }
        }
    }
    let bad = (0..1024).find_map(|i| {
        let got = p.rd32(gr::CHAN_DST + 4 * i as u64);
        (got != data[i]).then_some((i, got))
    });
    if let Some((i, got)) = bad {
        p.restore();
        return stop(r, format_args!("rung inline write to VRAM: the semaphores came but word {} is {:#x}, wanted {:#x}", i, got, data[i]));
    }
    let _ = writeln!(r, "compute: rung inline write 4 KiB -> VRAM: OK in {} us ({} pushes), 1024 words verified through PRAMIN", us, 1024 / gr::INLINE_CHUNK_WORDS);
    RUNGS.store(3, Ordering::Relaxed);

    // Rung 4 (phase 7b): the fill shader, output in host memory. The whole output page is scribbled first: the grid must
    // write exactly its 256 words and leave the other 768 alone.
    let out_host = gr::KERN_VA + gr::KERN_OUT_OFF;
    let scribble: Vec<u8> = (0..1024u32).flat_map(|i| qmd::scribble_word(i).to_le_bytes()).collect();
    c.kern.copy_in(gr::KERN_OUT_OFF as usize, &scribble);
    match launch(&mut sub, &mut p, &c, (qmd::FILL, qmd::FILL_REGISTERS), &qmd::fill_cbuf0(out_host), 0x4444, 0x4242) {
        Ok((us, grid)) => {
            LAUNCH_HOST_US.store(us, Ordering::Relaxed);
            GRID_RELEASED.store(grid as u32, Ordering::Relaxed);
            let mut host_word = |i: usize| {
                let mut w = [0u8; 4];
                c.kern.read(gr::KERN_OUT_OFF as usize + 4 * i, &mut w);
                u32::from_le_bytes(w)
            };
            if let Some(why) = check_output(&mut host_word, qmd::fill_word) {
                // did the words arrive late (the semaphore overtook the stores), or never?
                let t0 = crate::cpu::tsc::read();
                let mut late = false;
                while crate::cpu::tsc::read().wrapping_sub(t0) < super::copy::ms_ticks(20) {
                    if check_output(&mut host_word, qmd::fill_word).is_none() {
                        late = true;
                        break;
                    }
                }
                p.restore();
                let events = rm.drain(50);
                return stop(r, format_args!("rung shader -> host memory: the semaphore came ({} us, grid release {}) but {}; {}; RM events {:x?}", us, if grid { "seen" } else { "NOT seen" }, why, if late { "the words did arrive within 20 ms (the release overtook the stores)" } else { "still wrong after 20 ms" }, events));
            }
            let _ = writeln!(r, "compute: rung shader -> host memory: OK in {} us, {} words written by {} CTAs verified, the other {} untouched, the grid's own release {}", us, qmd::FILL_WORDS, qmd::FILL_CTAS, 1024 - qmd::FILL_WORDS, if grid { "seen" } else { "NOT seen" });
            RUNGS.store(4, Ordering::Relaxed);
        }
        Err(e) => {
            p.restore();
            let events = rm.drain(50);
            return stop(r, format_args!("rung shader -> host memory: {}; RM events {:x?}", e, events));
        }
    }

    // Rung 5: the same launch writing into VRAM (the page rung 3 wrote, scribbled through PRAMIN first), and the GPU's own view of
    // it: a second shader reads that page (through the L2, `ld.global.cg`) and stores it in the host page, which the CPU reads.
    // Boots #137 and #138: the semaphores come and the GPU reads the words back, but the CPU, reading through PRAMIN, sees only
    // the scribble (even after 0x70010/0x70000). The GPU's readback is the verification of the store; rung 6 then tries the ways the
    // CPU could see it, and logs each, as a measurement.
    for i in 0..1024u32 {
        p.wr32(gr::CHAN_DST + 4 * i as u64, qmd::scribble_word(i));
    }
    let vram_us = match launch(&mut sub, &mut p, &c, (qmd::FILL, qmd::FILL_REGISTERS), &qmd::fill_cbuf0(gr::VDST_VA), 0x5555, 0x4243) {
        Ok((us, grid)) => {
            LAUNCH_VRAM_US.store(us, Ordering::Relaxed);
            let _ = writeln!(r, "compute: shader -> VRAM launched: the semaphore came in {} us, the grid's own release {}", us, if grid { "seen" } else { "NOT seen" });
            us
        }
        Err(e) => {
            p.restore();
            let events = rm.drain(50);
            return stop(r, format_args!("rung shader -> VRAM: {}; RM events {:x?}", e, events));
        }
    };
    c.kern.copy_in(gr::KERN_OUT_OFF as usize, &scribble);
    match launch(&mut sub, &mut p, &c, (qmd::COPY, qmd::COPY_REGISTERS), &qmd::copy_cbuf0(out_host, gr::VDST_VA), 0x6666, 0x4244) {
        Ok((us, grid)) => {
            let mut host_word = |i: usize| {
                let mut w = [0u8; 4];
                c.kern.read(gr::KERN_OUT_OFF as usize + 4 * i, &mut w);
                u32::from_le_bytes(w)
            };
            if let Some(why) = check_output(&mut host_word, qmd::fill_word) {
                p.restore();
                let events = rm.drain(50);
                return stop(r, format_args!("rung GPU read of the VRAM page: {} (the shader -> VRAM launch took {} us); RM events {:x?}", why, vram_us, events));
            }
            let _ = writeln!(r, "compute: rung shader -> VRAM, read back by the GPU: OK in {} us: a second shader reads the page and the host page holds the {} words, the other {} untouched (grid release {})", us, qmd::FILL_WORDS, 1024 - qmd::FILL_WORDS, if grid { "seen" } else { "NOT seen" });
            RUNGS.store(5, Ordering::Relaxed);
        }
        Err(e) => {
            p.restore();
            let events = rm.drain(50);
            return stop(r, format_args!("rung GPU read of the VRAM page: {}; RM events {:x?}", e, events));
        }
    }

    // Rung 6 (a measurement, not a verdict): how many of the 256 words does the CPU read through PRAMIN, plain and after each
    // thing that could make the stores visible? Stops at the first way that shows all of them.
    let count = |p: &mut Pramin, want: fn(u32) -> u32| -> usize { (0..qmd::FILL_WORDS).filter(|&i| p.rd32(gr::CHAN_DST + 4 * i as u64) == want(i)).count() };
    let mut view = 0u32;
    let n = count(&mut p, qmd::fill_word);
    let _ = writeln!(r, "compute: CPU view of the VRAM stores: plain PRAMIN read {} of {}", n, qmd::FILL_WORDS);
    if n == qmd::FILL_WORDS as usize {
        view = 1;
    }
    if view == 0 {
        let t0 = crate::cpu::tsc::read();
        while crate::cpu::tsc::read().wrapping_sub(t0) < super::copy::ms_ticks(20) {}
        let n = count(&mut p, qmd::fill_word);
        let _ = writeln!(r, "compute: CPU view: after 20 ms {} of {}", n, qmd::FILL_WORDS);
        if n == qmd::FILL_WORDS as usize {
            view = 2;
        }
    }
    if view == 0 {
        let flush = l2_flush(regs);
        let n = count(&mut p, qmd::fill_word);
        let _ = writeln!(r, "compute: CPU view: after the MMIO L2 flush ({}) {} of {}", flush, n, qmd::FILL_WORDS);
        if n == qmd::FILL_WORDS as usize {
            view = 3;
        }
    }
    if view == 0 {
        let push = gr::l2_flush_push(gr::KERN_VA + gr::KERN_SEM_OFF, 0x7777);
        match sub.submit(&mut p, &c.kern, gr::KERN_SEM_OFF, &push, 0x7777) {
            Ok(us) => {
                let n = count(&mut p, qmd::fill_word);
                let _ = writeln!(r, "compute: CPU view: after MEM_OP L2_FLUSH_DIRTY through the channel ({} us) {} of {}", us, n, qmd::FILL_WORDS);
                if n == qmd::FILL_WORDS as usize {
                    view = 4;
                }
            }
            Err(e) => {
                let _ = writeln!(r, "compute: CPU view: MEM_OP L2_FLUSH_DIRTY through the channel: {}", e);
            }
        }
    }
    if view == 0 {
        // a system-scope store (`STG.E.STRONG.SYS`), different words so an old dirty line cannot pass for it
        for i in 0..1024u32 {
            p.wr32(gr::CHAN_DST + 4 * i as u64, qmd::scribble_word(i));
        }
        match launch(&mut sub, &mut p, &c, (qmd::FILLWT, qmd::FILLWT_REGISTERS), &qmd::fill_cbuf0(gr::VDST_VA), 0x8888, 0x4245) {
            Ok((us, grid)) => {
                let n = count(&mut p, qmd::fillwt_word);
                let _ = writeln!(r, "compute: CPU view: a system-scope store shader ({} us, grid release {}): {} of {} (its words are the fill words xor {:#x})", us, if grid { "seen" } else { "NOT seen" }, n, qmd::FILL_WORDS, qmd::FILLWT_XOR);
                if n == qmd::FILL_WORDS as usize {
                    view = 5;
                }
            }
            Err(e) => {
                let _ = writeln!(r, "compute: CPU view: the system-scope store shader: {}", e);
            }
        }
    }
    CPU_VIEW.store(view, Ordering::Relaxed);
    if view != 0 {
        RUNGS.store(6, Ordering::Relaxed);
    }
    let _ = writeln!(r, "compute: CPU view of the SM's stores to VRAM through PRAMIN: {}", CPU_VIEWS[view as usize]);

    p.restore();
    STATE.store(1, Ordering::Relaxed);
    let _ = writeln!(r, "compute: OK: the GR pipeline ran a semaphore release, two inline writes and a compute shader (to host memory and to VRAM) through the compute class");
    // The channel, its memory and the mappings stay: RM and the GPU own them now.
    core::mem::forget(c);
}

/// One way of asking for the golden context. Boot #126 (`gpu=compute`, the first run) got `NV_ERR_INVALID_ARGUMENT` from
/// the golden `PROMOTE_CTX` with the entries nouveau's trace has, at our addresses, in our externally owned VA space, and
/// GSP-RM's logs cannot be decoded here, so the differences from the trace are tried one at a time in the same boot
/// (each on a fresh golden channel, freed afterwards) and every result is logged.
#[derive(Clone, Copy)]
struct Variant {
    name: &'static str,
    /// nouveau's golden channel lives in a VA space RM manages (`r535_mmu_vaspace_new(.., false)`, with the
    /// server-reserved PDEs copied from three page-directory pages of the host), not in ours, which is externally
    /// owned; the buffers' VAs are then the trace's.
    rm_vas: bool,
    /// A real GPFIFO (our own VA and size) instead of the trace's shell at offset 0.
    gpfifo: bool,
    /// `NVA06F_CTRL_CMD_BIND` to GR0 before the promote (a real channel does it right after the ALLOC; the trace's
    /// golden one does not).
    bind: bool,
    /// The context buffers' VAs moved below 4 GiB (RM's range starts at 64 MiB; nouveau's are below 32 MiB).
    low_va: bool,
    /// Only these buffers (a diagnostic subset: no 3D object afterwards).
    only: Option<&'static [u16]>,
    edit: Edit,
}

/// What a diagnostic does to the entries it keeps (kept for the next round of variants).
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Edit {
    None,
    /// `bInitialize` off (no physical address, no size).
    NoInit,
    /// `bNonmapped` on (no VA), keeping the initialisation.
    Nonmapped,
}

const V0: Variant = Variant { name: "", rm_vas: false, gpfifo: true, bind: false, low_va: false, only: None, edit: Edit::None };

/// The golden `PROMOTE_CTX` got `NV_ERR_INVALID_ARGUMENT` for every set holding MAIN with a VA (boots #126-#134) in our
/// externally owned VA space, with 4 KiB or 64 KiB pages, high or low VAs; MAIN nonmapped and PATCH were accepted. With a
/// VA that no table mapped, MAIN made RM walk it: MMU_FAULT_QUEUED, NOCAT records and RC_TRIGGERED (boot #130). The one
/// attempt in a VA space RM manages (#128) was not nouveau's: its page directories were three zeroed pages, so nothing
/// was mapped there. Nothing follows the baseline: a refused promote can leave GSP-RM mute for the rest of the boot.
const VARIANTS: [Variant; 0] = [];

/// nouveau's golden sequence (`r535_gr_oneinit`) with nothing changed but the VRAM addresses and the client: a VA space
/// RM manages over our own tables, the buffers mapped there at the trace's VAs, the trace's GPFIFO shell.
const BASELINE: Option<Variant> = Some(Variant { name: "nouveau's golden VA space, over our tables with the buffers mapped", rm_vas: true, gpfifo: false, ..V0 });

/// 256 MiB: above RM's range base (64 MiB) and below 4 GiB.
const LOW_VA: u64 = 0x1000_0000;

/// nouveau's golden context init (`r535_gr_oneinit`, `gr.c:250-315`): a channel on GR0, every context
/// buffer promoted, a 3D object allocated on it (RM builds the golden context image), everything freed.
/// Returns the variant that worked.
fn golden(r: &mut String, rm: &mut Rm, regs: &Bar0, c: &Compute) -> Option<Variant> {
    let t0 = crate::cpu::tsc::read();
    let mut result = None;
    for v in BASELINE.iter().chain(VARIANTS.iter()) {
        // the subsets are diagnostics: once a complete golden context exists there is nothing left to learn
        if result.is_some() {
            break;
        }
        match try_golden(r, rm, regs, c, v) {
            Ok(done) => {
                let _ = writeln!(r, "compute: golden variant \"{}\": {}", v.name, if done { "OK, the golden context is built" } else { "PROMOTE_CTX accepted (a subset: nothing more tried)" });
                if done {
                    result = Some(*v);
                }
            }
            Err(e) => {
                let events = rm.drain(20);
                let _ = writeln!(r, "compute: golden variant \"{}\": {}; RM events {:x?}", v.name, e, events);
                for (f, bytes) in super::gsp::take_events().iter().take(8) {
                    // words first (the numbers), then the text in it
                    let mut words = String::new();
                    for w in bytes.chunks(8).take(12) {
                        let mut a = [0u8; 8];
                        a[..w.len()].copy_from_slice(w);
                        let _ = write!(words, "{:x} ", u64::from_le_bytes(a));
                    }
                    let text: String = bytes.iter().map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' }).collect();
                    // RM asserts about GFW_BOOT_PROGRESS all the time, with or without our calls: not news
                    if text.contains("GFW_BOOT_PROGRESS") {
                        continue;
                    }
                    let _ = writeln!(r, "compute:   event {:#x} ({} bytes) words {}", f, bytes.len(), words);
                    let _ = writeln!(r, "compute:   event {:#x} text {}", f, text);
                }
            }
        }
    }
    let ms = super::ms_since(t0);
    match result {
        Some(v) => {
            GOLDEN_MS.store(ms, Ordering::Relaxed);
            let _ = writeln!(r, "compute: golden context built with variant \"{}\" in {} ms in all", v.name, ms);
            Some(v)
        }
        None => {
            stop(r, format_args!("golden context: no variant was accepted (see the golden variant lines)"));
            None
        }
    }
}

/// nouveau's golden VMM as page tables of ours: the context buffers at the trace's VAs (`gr::trace_placement`) in a
/// tree of their own in the golden block, written through PRAMIN and read back. Returns the root, PD2 and PD1 that
/// `COPY_SERVER_RESERVED_PDES` names: the channel's page directory is then this tree, as nouveau's is its VMM's.
/// Boot #128 handed RM three zeroed pages instead, so no buffer was mapped in the space its channel used.
fn golden_tables(r: &mut String, regs: &Bar0, c: &Compute) -> Result<[u64; 3], String> {
    let at = gr::trace_placement(&c.bufs, &c.plan.golden).ok_or_else(|| String::from("a context buffer has no VA in the trace"))?;
    let mut pt = PageTables::new(gr::GOLDEN_TABLES, gr::GOLDEN_TABLES_MAX, Target::Vram);
    for m in gr::golden_mappings(&c.bufs, &at) {
        let f = Flags { privileged: true, read_only: m.ro, kind: 0 };
        match m.page {
            21 => pt.map_huge_range(m.va, m.pa, m.len, Target::Vram, f),
            16 => pt.map_big_range(m.va, m.pa, m.len, Target::Vram, f),
            _ => pt.map_range(m.va, m.pa, m.len, Target::Vram, f),
        }
        .map_err(|e| alloc::format!("golden tables: buffer {} at VA {:#x}: {:?}", m.id, m.va, e))?;
    }
    let dirs = pt.directories(0).ok_or_else(|| String::from("golden tables: no directory chain under VA 0"))?;
    let mut p = Pramin::new(regs);
    // the whole pool, so a table the tree does not use is zero, not a stale one
    vram_zero(&mut p, gr::GOLDEN_TABLES, (gr::GOLDEN_TABLES_MAX * 0x1000) as u64);
    let mut bad = None;
    'write: for (pa, img) in pt.images() {
        for (i, w) in img.chunks_exact(4).enumerate() {
            p.wr32(pa + (i * 4) as u64, u32::from_le_bytes(w.try_into().unwrap()));
        }
        for (i, w) in img.chunks_exact(4).enumerate() {
            let want = u32::from_le_bytes(w.try_into().unwrap());
            let got = p.rd32(pa + (i * 4) as u64);
            if got != want {
                bad = Some((pa + (i * 4) as u64, want, got));
                break 'write;
            }
        }
    }
    p.restore();
    if let Some((a, want, got)) = bad {
        return Err(alloc::format!("golden table word at VRAM {:#x}: wrote {:#x}, read back {:#x}", a, want, got));
    }
    let _ = writeln!(r, "compute: golden VA space: {} tables at VRAM {:#x}, levels {:#x} {:#x} {:#x}, buffers at the trace's VAs", pt.len(), gr::GOLDEN_TABLES, dirs[0], dirs[1], dirs[2]);
    Ok(dirs)
}

/// One attempt: `Ok(true)` when the golden context was built (and everything freed), `Ok(false)` when a
/// subset was promoted (freed too), `Err` with the step that RM refused.
fn try_golden(r: &mut String, rm: &mut Rm, regs: &Bar0, c: &Compute, v: &Variant) -> Result<bool, String> {
    let vas = if v.rm_vas { gr::H_VASPACE_GOLDEN } else { rm::H_VASPACE };
    if v.rm_vas {
        // nouveau's golden VMM: a VA space RM manages over page directories of ours, into which RM puts its reserved
        // range (`r535_mmu_vaspace_new`, `vmm.c:57-138`); the buffers are mapped in those same tables
        let dirs = golden_tables(r, regs, c)?;
        rm.alloc(rm::H_DEVICE, vas, rm::FERMI_VASPACE_A, &gr::golden_vaspace_params()).map_err(|e| alloc::format!("ALLOC the RM-managed VA space: {}", e))?;
        if let Err(e) = rm.control(vas, gr::CTRL_COPY_SERVER_RESERVED_PDES, &gr::copy_server_reserved_pdes_params(dirs)) {
            let _ = rm.free(vas);
            return Err(alloc::format!("COPY_SERVER_RESERVED_PDES: {}", e));
        }
    }
    let outcome = golden_channel(r, rm, c, v, vas);
    if v.rm_vas {
        if let Err(e) = rm.free(vas) {
            let _ = writeln!(r, "compute: FREE golden VA space: {}", e);
        }
    }
    outcome
}

fn golden_channel(r: &mut String, rm: &mut Rm, c: &Compute, v: &Variant, vas: u32) -> Result<bool, String> {
    let alloc = ChanAlloc {
        chid: 1, // nouveau's `rsvd_chids`
        privileged: true,
        engine_type: gr::ENGINE_GR0,
        // the trace's shell (never scheduled) or a real ring
        gpfifo_va: if v.gpfifo { gr::GPFIFO_VA } else { 0 },
        gpfifo_bytes: if v.gpfifo { gr::GPFIFO_ENTRIES * 8 } else { 0x1000 },
        inst: gr::GOLDEN_INST,
        userd: gr::GOLDEN_USERD,
        mthdbuf: c.mthd_golden.bus_addr(),
        vaspace: vas,
    };
    let ch = chan::h_chan(0);
    rm.alloc(rm::H_DEVICE, ch, chan::CLASS_GPFIFO, &chan::alloc_params(&alloc)).map_err(|e| alloc::format!("ALLOC golden channel: {}", e))?;
    let outcome = (|| -> Result<bool, String> {
        // in nouveau's golden VA space the buffers are where `golden_tables` mapped them: the trace's VAs
        let mem = if v.rm_vas { gr::trace_placement(&c.bufs, &c.plan.golden).ok_or_else(|| String::from("a context buffer has no VA in the trace"))? } else { c.plan.golden.clone() };
        let mut e = gr::entries(&c.bufs, true, &mem);
        if let Some(only) = v.only {
            e.retain(|x| only.contains(&x.id));
        }
        if v.low_va {
            for x in e.iter_mut().filter(|x| x.va != 0) {
                x.va = x.va - gr::VA_CTX + LOW_VA;
            }
        }
        match v.edit {
            Edit::None => {}
            Edit::NoInit => {
                for x in e.iter_mut() {
                    (x.init, x.pa, x.size, x.phys_attr) = (false, 0, 0, 0);
                }
            }
            Edit::Nonmapped => {
                for x in e.iter_mut() {
                    (x.nonmapped, x.va) = (true, 0);
                }
            }
        }
        if v.bind {
            rm.control(ch, chan::CTRL_BIND, &chan::bind_params(gr::ENGINE_GR0)).map_err(|e| alloc::format!("BIND: {}", e))?;
        }
        let params = gr::promote_params(rm::H_CLIENT, ch, &e);
        rm.control(rm::H_SUBDEVICE, gr::CTRL_PROMOTE_CTX, &params).map_err(|err| alloc::format!("PROMOTE_CTX ({} entries): {}", e.len(), err))?;
        if v.only.is_some() {
            return Ok(false);
        }
        let t3 = crate::cpu::tsc::read();
        rm.alloc(ch, gr::H_THREED, gr::CLASS_THREED, &[]).map_err(|e| alloc::format!("ALLOC 3D object: {}", e))?;
        let init_ms = crate::cpu::tsc::read().wrapping_sub(t3) * 1000 / crate::cpu::tsc::freq_hz();
        let _ = writeln!(r, "compute: PROMOTE_CTX ({} entries) and the 3D object {:#x} accepted (RM took {} ms)", e.len(), gr::CLASS_THREED, init_ms);
        // RM caches the golden context: the object is not needed any more
        rm.free(gr::H_THREED).map_err(|e| alloc::format!("FREE 3D object: {}", e))?;
        Ok(true)
    })();
    // the channel goes in every case, so the next attempt starts clean
    if let Err(e) = rm.free(ch) {
        let _ = writeln!(r, "compute: FREE golden channel: {}", e);
    }
    outcome
}

/// `/proc/kdebug` line (empty when the level was not asked for).
pub fn render_kdebug() -> String {
    match STATE.load(Ordering::Relaxed) {
        0 => String::new(),
        s => alloc::format!(
            "gpu_compute: state={} rungs={} golden_ms={} chan_ms={} token={:#x} ctx_kib={} launch_host_us={} launch_vram_us={} grid_release={} vram_cpu_view={}",
            if s == 1 { "ok" } else { "failed" },
            RUNGS.load(Ordering::Relaxed),
            GOLDEN_MS.load(Ordering::Relaxed),
            CHAN_MS.load(Ordering::Relaxed),
            TOKEN.load(Ordering::Relaxed),
            CTX_KIB.load(Ordering::Relaxed),
            LAUNCH_HOST_US.load(Ordering::Relaxed),
            LAUNCH_VRAM_US.load(Ordering::Relaxed),
            GRID_RELEASED.load(Ordering::Relaxed),
            CPU_VIEWS[CPU_VIEW.load(Ordering::Relaxed) as usize]
        ),
    }
}
