// kernel/src/gpu/uapi.rs
//
// G4c of docs/gpu/g4-nvkmd-plan.md: the hardware behind `/dev/nvgpu`, level `gpu=uapi`. The device model (`nvgpu::devmodel`) decides
// what user space may ask; `drivers/dev_nvgpu.rs` calls in here for the effects when a GPU is up. What this file keeps after the
// boot, which `compute::run` used to throw away:
//
//   - the GPU page tables (`PageTables`, pool of `POOL_TABLES` tables at VRAM 64 MiB): `bind` maps user buffers into them, `unbind`
//     removes them, each followed by writing the tables it touched into VRAM and flushing the GPU's TLB (`hwq::tlb_flush_regs`);
//   - the GR channel `compute::run` built (chid 1, runlist 0, the compute object): its GPFIFO ring, USERD and doorbell token. Every
//     context of every session shares this one channel (G4e gives each its own); `hwq::Queue` decides where an `EXEC`'s pushes go and
//     appends the kernel's fence (`gr::fence_push`) that releases a sequence number into a semaphore in host memory;
//   - where to write: the ring, the fence pushes and the tables live in VRAM, and the CPU reaches VRAM at run time through BAR1
//     write-combined (write-only after GSP-RM boots; PRAMIN is the fallback for any span BAR1 does not reach, checked at install).
//
// Nothing here blocks. A full ring is EAGAIN; a GPU that stops answering marks the device dead (EIO on the next EXEC, every fence
// reads as done so waiters drain) and the memory user space had bound is kept out of circulation, because a wedged GPU may still
// write to it.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use nvgpu::chan;
use nvgpu::devmodel::{Backing, Error};
use nvgpu::evo::Pramin;
use nvgpu::gr;
use nvgpu::hwq::{self, Queue, Source};
use nvgpu::mmu::{PageTables, Target};
use nvgpu::uapi::Push;
use nvgpu::Mmio;

use super::compute::Channel;
use super::vaspace::TABLE_VRAM;
use super::Bar0;
use crate::memory::dma::DmaBuf;
use crate::memory::shm::ShmObject;

/// Tables the run-time pool can hold: VRAM `[64 MiB, 96 MiB)`. A bind of N bytes with 4 KiB pages costs N / 2 MiB tables (plus a
/// PD0 per 512 MiB), so this is a few GiB of mapped memory; the 6b test mapping starts at 96 MiB.
pub const POOL_TABLES: usize = 8192;

/// `NVC361_NOTIFY_CHANNEL_PENDING` (see `compute.rs`).
const DOORBELL: u32 = 0xb8_0000 + 0x3_0000 + 0x90;
/// The kernel's fence pushes: 64 slots of 64 bytes in the channel's push page (`gr::PUSH_VA`, VRAM `gr::CHAN_PUSH`), which the boot's
/// rungs are done with.
const FENCE_SLOTS: u32 = 64;
const FENCE_SLOT_BYTES: u32 = 64;
/// The fence semaphore: the host page the rungs used (`gr::HOST_VA + HOST_SEM_OFF`).
const SEM_OFF: u64 = gr::HOST_SEM_OFF;
/// No fence in this long and work in flight: the channel is wedged (a faulted channel is reset by RM and never releases again).
const HANG_MS: u64 = 10_000;
/// The GPU's TLB flush and the quiesce at close each get this long.
const FLUSH_MS: u64 = 2_000;
const QUIESCE_MS: u64 = 3_000;
/// `NV04_PTIMER_TIME_0/1` (`nvkm/subdev/timer/regsnv04.h:6-7`).
const PTIMER_TIME_0: u32 = 0x9400;
const PTIMER_TIME_1: u32 = 0x9410;

// ---- CPU writes to VRAM -----------------------------------------------------------------------------------------------------

/// A write-combined BAR1 mapping of `[vram, vram + len)`.
struct Span {
    vram: u64,
    len: u64,
    base: *mut u8,
}

/// Where the CPU writes VRAM: through a [`Span`] when one covers the address, else through PRAMIN (BAR0's sliding window). One `Io`
/// per operation; [`finish`](Io::finish) orders the stores and puts the PRAMIN window back.
struct Io<'a> {
    regs: &'a Bar0,
    spans: &'a [Span],
    pram: Option<Pramin<'a>>,
    last_pram: u64,
}

impl<'a> Io<'a> {
    fn new(regs: &'a Bar0, spans: &'a [Span]) -> Self {
        Io { regs, spans, pram: None, last_pram: 0 }
    }

    fn span(&self, at: u64, len: u64) -> Option<*mut u8> {
        self.spans.iter().find(|s| at >= s.vram && at + len <= s.vram + s.len).map(|s| {
            // SAFETY: inside the mapping made in `install`.
            unsafe { s.base.add((at - s.vram) as usize) }
        })
    }

    fn pramin(&mut self) -> &mut Pramin<'a> {
        let regs = self.regs;
        self.pram.get_or_insert_with(|| Pramin::new(regs))
    }

    fn wr32(&mut self, at: u64, v: u32) {
        match self.span(at, 4) {
            // SAFETY: `span` checked the range against a live device mapping; the stores are ordered by `finish`.
            Some(p) => unsafe { core::ptr::write_volatile(p as *mut u32, v) },
            None => {
                self.last_pram = at;
                self.pramin().wr32(at, v);
            }
        }
    }

    fn wr64(&mut self, at: u64, v: u64) {
        match self.span(at, 8) {
            // SAFETY: as above.
            Some(p) => unsafe { core::ptr::write_volatile(p as *mut u64, v) },
            None => {
                self.wr32(at, v as u32);
                self.wr32(at + 4, (v >> 32) as u32);
            }
        }
    }

    /// Everything written so far is on its way to VRAM, in order, before the caller's next store (the doorbell, the flush).
    fn finish(mut self) {
        super::copy::sfence();
        if let Some(mut p) = self.pram.take() {
            // a read through the window comes back only after the posted writes before it
            let _ = p.rd32(self.last_pram);
            p.restore();
        }
    }
}

// ---- the state --------------------------------------------------------------------------------------------------------------

struct Hw {
    regs: Bar0,
    pt: PageTables,
    spans: Vec<Span>,
    queue: Queue,
    token: u32,
    /// The host page holding the fence semaphore (at `SEM_OFF`).
    host: DmaBuf,
    /// The TSC when the oldest work in flight started, or the last time a fence was seen while work was in flight.
    progress: u64,
    dead: bool,
    /// The GPU may still touch bound memory (it hung, or did not go idle at close): never give it back.
    leak: bool,
}

// SAFETY: the mappings and the register window are device memory used under `HW`'s lock.
unsafe impl Send for Hw {}

static HW: crate::sync::Mutex<Option<Hw>> = crate::sync::Mutex::new(None);

// 0 = not run, 1 = installed, 2 = failed
static STATE: AtomicU32 = AtomicU32::new(0);
static BINDS: AtomicU64 = AtomicU64::new(0);
static UNBINDS: AtomicU64 = AtomicU64::new(0);
static PAGES_BOUND: AtomicU64 = AtomicU64::new(0);
static EXECS: AtomicU64 = AtomicU64::new(0);
static AGAIN: AtomicU64 = AtomicU64::new(0);
static FENCES: AtomicU64 = AtomicU64::new(0);
static TLB_FLUSHES: AtomicU64 = AtomicU64::new(0);
static TLB_US_MAX: AtomicU64 = AtomicU64::new(0);
static TABLES_WRITTEN: AtomicU64 = AtomicU64::new(0);
static DEAD: AtomicU32 = AtomicU32::new(0);
static SPANS_BAR1: AtomicU32 = AtomicU32::new(0);
static SPANS_PRAMIN: AtomicU32 = AtomicU32::new(0);

fn ticks_to_us(t: u64) -> u64 {
    t * 1_000_000 / crate::cpu::tsc::freq_hz().max(1)
}

/// Map `[vram, vram + len)` of BAR1 write-combined and check that stores through it land at that VRAM address, reading them back
/// through PRAMIN at `probe` (a word the caller knows is unused). `None` if it cannot be mapped or the check fails: the span is then
/// served by PRAMIN.
fn map_span(r: &mut String, regs: &Bar0, bar1: u64, vram: u64, len: u64, probe: u64, what: &str) -> Option<Span> {
    // SAFETY: BAR1 is the VRAM aperture; the range is VRAM the kernel keeps for this purpose (tables, the channel) and writes from
    // here on only through this mapping (or PRAMIN, which is what a span that fails the check falls back to).
    let Some(v) = (unsafe { crate::memory::mmio::map(x86_64::PhysAddr::new(bar1 + vram), len as usize) }) else {
        let _ = writeln!(r, "uapi: {}: cannot map BAR1 at VRAM {:#x} (+{:#x}); PRAMIN will serve it", what, vram, len);
        return None;
    };
    let wc = crate::memory::memtype::set_pat_index_range(v.as_u64(), len, hal::memtype::PAT_WC_INDEX).is_ok();
    let span = Span { vram, len, base: v.as_u64() as *mut u8 };
    let mut ok = true;
    for k in 0..4u32 {
        let want = 0xc0de_a000 | (k << 4) | (probe as u32 >> 12 & 0xf);
        let at = probe + 4 * k as u64;
        // SAFETY: inside the mapping.
        unsafe { core::ptr::write_volatile(span.base.add((at - vram) as usize) as *mut u32, want) };
        super::copy::sfence();
        let mut p = Pramin::new(regs);
        let got = p.rd32(at);
        p.restore();
        if got != want {
            let _ = writeln!(r, "uapi: {}: BAR1 store at VRAM {:#x} reads back {:#x} through PRAMIN, wrote {:#x}", what, at, got, want);
            ok = false;
        }
    }
    let _ = writeln!(r, "uapi: {}: BAR1 span VRAM {:#x}..{:#x} {} ({})", what, vram, vram + len, if ok { "stores land" } else { "DOES NOT REACH VRAM" }, if wc { "write-combined" } else { "NOT write-combined" });
    ok.then_some(span)
}

/// Keep the boot's GPU state for `/dev/nvgpu`. Called by `vaspace::setup` after `compute::run` succeeded: `pt` is the tree the boot
/// built and RM was handed, `ch` the GR channel's position.
pub(super) fn install(r: &mut String, regs: &Bar0, mut pt: PageTables, ch: Channel) {
    let Some(pci) = super::gsp::pci_info().filter(|p| p.bar1 != 0) else {
        STATE.store(2, Ordering::Relaxed);
        let _ = writeln!(r, "uapi: STOP: no BAR1");
        return;
    };
    // the tables the boot wrote were verified through PRAMIN then; only what a bind changes is written from now on
    let boot_tables = pt.take_dirty().len();

    let mut spans = Vec::new();
    // the pool's last table (unused: a bind allocates from the front) and the channel's DST page (its last rung is done)
    let pool_bytes = POOL_TABLES as u64 * 0x1000;
    if let Some(s) = map_span(r, regs, pci.bar1, TABLE_VRAM, pool_bytes, TABLE_VRAM + pool_bytes - 0x1000, "tables") {
        spans.push(s);
    }
    if let Some(s) = map_span(r, regs, pci.bar1, gr::VRAM_CHAN, 0x1_0000, gr::CHAN_DST, "channel") {
        spans.push(s);
    }
    SPANS_BAR1.store(spans.len() as u32, Ordering::Relaxed);
    SPANS_PRAMIN.store(2 - spans.len() as u32, Ordering::Relaxed);

    // The fence semaphore starts at 0 and the ring where the boot's pushes left it (all of them were waited for).
    ch.host.copy_in(SEM_OFF as usize, &0u32.to_le_bytes());
    let queue = Queue::new(gr::GPFIFO_ENTRIES, ch.slot, FENCE_SLOTS, gr::PUSH_VA, FENCE_SLOT_BYTES, gr::FENCE_PUSH_BYTES);
    let _ = writeln!(
        r,
        "uapi: installed: {} tables ({} written at boot, pool {}), {} BAR1 span(s) + {} through PRAMIN, GR channel token {:#x}, ring at entry {} of {}",
        pt.len(),
        boot_tables,
        POOL_TABLES,
        spans.len(),
        2 - spans.len(),
        ch.token,
        ch.slot,
        gr::GPFIFO_ENTRIES
    );
    *HW.lock() = Some(Hw {
        regs: Bar0 { base: regs.base, len: regs.len },
        pt,
        spans,
        queue,
        token: ch.token,
        host: ch.host,
        progress: crate::cpu::tsc::read(),
        dead: false,
        leak: false,
    });
    STATE.store(1, Ordering::Relaxed);
}

pub(super) fn install_failed(r: &mut String, why: core::fmt::Arguments) {
    STATE.store(2, Ordering::Relaxed);
    let _ = writeln!(r, "uapi: STOP: {}", why);
}

/// A GPU is up behind `/dev/nvgpu` (installed, whether or not it has since died).
pub fn installed() -> bool {
    HW.lock().is_some()
}

/// The GPU is installed but wedged: `open` refuses rather than pretend to be a software device.
pub fn dead() -> bool {
    HW.lock().as_ref().is_some_and(|h| h.dead)
}

impl Hw {
    fn mark_dead(&mut self, why: &str) {
        if !self.dead {
            self.dead = true;
            self.leak = true;
            DEAD.store(1, Ordering::Relaxed);
            crate::serial_println!("[nvgpu] DEAD: {}", why);
            crate::kalert!("nvgpu: the GPU stopped answering ({})", why);
        }
    }

    /// Read the fence semaphore, retire what it completes, and declare the channel dead when work has been in flight too long.
    fn poll(&mut self) {
        // SAFETY: the host page is a live DMA allocation of ours, at least `SEM_OFF + 4` bytes long.
        let sem = unsafe { core::ptr::read_volatile(self.host.virt().add(SEM_OFF as usize) as *const u32) };
        let before = self.queue.done_seq();
        if self.queue.observe(sem) {
            FENCES.fetch_add(self.queue.done_seq() - before, Ordering::Relaxed);
            self.progress = crate::cpu::tsc::read();
        }
        if !self.dead && self.queue.in_flight() > 0 && crate::cpu::tsc::read().wrapping_sub(self.progress) > super::copy::ms_ticks(HANG_MS) {
            let why = alloc::format!("no fence for {} ms with {} submission(s) in flight, semaphore {:#x}, last submitted {}", HANG_MS, self.queue.in_flight(), sem, self.queue.last_seq());
            self.mark_dead(&why);
        }
    }

    /// Write the tables a bind or unbind touched into VRAM, then flush the GPU's TLB.
    fn publish_tables(&mut self) -> bool {
        let dirty = self.pt.take_dirty();
        if self.dead {
            // a wedged GPU is not poked again (its TLB flush would wait out the whole timeout, once per range a teardown unbinds)
            return false;
        }
        let mut io = Io::new(&self.regs, &self.spans);
        for (pa, img) in &dirty {
            for (i, w) in img.chunks_exact(8).enumerate() {
                io.wr64(pa + 8 * i as u64, u64::from_le_bytes(w.try_into().unwrap()));
            }
        }
        io.finish();
        TABLES_WRITTEN.fetch_add(dirty.len() as u64, Ordering::Relaxed);
        let (writes, poll) = hwq::tlb_flush_regs(self.pt.root());
        let t0 = crate::cpu::tsc::read();
        for (reg, val) in writes {
            self.regs.wr32(reg, val);
        }
        let limit = super::copy::ms_ticks(FLUSH_MS);
        let ok = loop {
            if self.regs.rd32(poll) & (1 << 31) == 0 {
                break true;
            }
            if crate::cpu::tsc::read().wrapping_sub(t0) > limit {
                break false;
            }
        };
        let us = ticks_to_us(crate::cpu::tsc::read().wrapping_sub(t0));
        TLB_FLUSHES.fetch_add(1, Ordering::Relaxed);
        TLB_US_MAX.fetch_max(us, Ordering::Relaxed);
        if !ok {
            self.mark_dead("the TLB invalidate never completed");
        }
        ok
    }
}

// ---- what `dev_nvgpu` calls -------------------------------------------------------------------------------------------------

fn with<T>(f: impl FnOnce(&mut Hw) -> Result<T, Error>) -> Result<T, Error> {
    let mut g = HW.lock();
    let hw = g.as_mut().ok_or(Error::Io)?;
    if hw.dead {
        return Err(Error::Io);
    }
    f(hw)
}

/// Map `size` bytes of `backing` (from `bo_off`) at `va`. System pages are pinned (one reference each, taken from the arena) while
/// they are mapped.
pub fn bind(arena: &ShmObject, va: u64, size: u64, backing: Backing, bo_off: u64, kind: u32) -> Result<(), Error> {
    with(|hw| {
        let kind = u8::try_from(kind).map_err(|_| Error::Inval)?;
        let mut pinned: Vec<u64> = Vec::new();
        let r = match backing {
            Backing::Vram { vram_off } => hwq::bind_range(&mut hw.pt, va, size, bo_off, Source::Vram { vram_off }, kind),
            Backing::System { arena_off } => {
                let mut frame = |off: u64| {
                    let idx = ((arena_off + off) / 0x1000) as usize;
                    let f = arena.frame_for_mapping(idx).ok()?;
                    let pa = f.start_address().as_u64();
                    pinned.push(pa);
                    Some(pa)
                };
                hwq::bind_range(&mut hw.pt, va, size, bo_off, Source::System(&mut frame), kind)
            }
        };
        if let Err(e) = r {
            for pa in pinned {
                crate::memory::shm::unpin_frame(pa);
            }
            // whatever the failed attempt did to the tables is rolled back; the tree may still have new (empty) tables to write
            hw.publish_tables();
            return Err(e);
        }
        BINDS.fetch_add(1, Ordering::Relaxed);
        PAGES_BOUND.fetch_add(size / 0x1000, Ordering::Relaxed);
        if !hw.publish_tables() {
            return Err(Error::Io);
        }
        Ok(())
    })
}

/// Unmap `[va, va + size)` (gaps are fine) and give the pins back. After the TLB flush the GPU cannot reach the pages any more, unless
/// it is wedged, in which case they are never released.
pub fn unbind(va: u64, size: u64) {
    let mut g = HW.lock();
    let Some(hw) = g.as_mut() else { return };
    let mut frames: Vec<u64> = Vec::new();
    let mut off = 0;
    while off < size {
        if let Some((pa, Target::Host)) = hwq::resolve(&hw.pt, va + off) {
            frames.push(pa);
        }
        off += 0x1000;
    }
    hwq::unbind_range(&mut hw.pt, va, size);
    UNBINDS.fetch_add(1, Ordering::Relaxed);
    let flushed = hw.publish_tables();
    if flushed && !hw.leak {
        for pa in frames {
            crate::memory::shm::unpin_frame(pa);
        }
    }
}

/// Whether memory given back by the model must be kept (the GPU is wedged).
pub fn leaking() -> bool {
    HW.lock().as_ref().is_some_and(|h| h.leak)
}

/// The engines a context may ask for. The channel has the compute object only; the others are allocated per channel in G4e.
pub fn ctx_create(engines: u32) -> Result<(), Error> {
    with(|_| if engines == nvgpu::uapi::ENGINE_COMPUTE { Ok(()) } else { Err(Error::Inval) })
}

/// Queue `pushes` (already validated) and the fence after them; returns the sequence number the fence completes as.
pub fn submit(pushes: &[Push]) -> Result<u64, Error> {
    with(|hw| {
        hw.poll();
        if hw.dead {
            return Err(Error::Io);
        }
        let plan = match hw.queue.plan(pushes) {
            Ok(p) => p,
            Err(Error::Again) => {
                AGAIN.fetch_add(1, Ordering::Relaxed);
                return Err(Error::Again);
            }
            Err(e) => return Err(e),
        };
        if hw.queue.in_flight() == 1 {
            hw.progress = crate::cpu::tsc::read();
        }
        let sem_va = gr::HOST_VA + SEM_OFF;
        let fence = gr::fence_push(sem_va, plan.payload);
        let mut io = Io::new(&hw.regs, &hw.spans);
        let slot_at = gr::CHAN_PUSH + plan.fence_slot as u64 * FENCE_SLOT_BYTES as u64;
        for (i, w) in fence.iter().enumerate() {
            io.wr32(slot_at + 4 * i as u64, *w);
        }
        for (idx, e) in &plan.entries {
            io.wr64(gr::CHAN_GPFIFO + 8 * *idx as u64, *e);
        }
        io.wr32(gr::CHAN_USERD + chan::USERD_GP_PUT, plan.gp_put);
        io.finish();
        core::sync::atomic::fence(Ordering::SeqCst);
        hw.regs.wr32(DOORBELL, hw.token);
        EXECS.fetch_add(1, Ordering::Relaxed);
        Ok(plan.seq)
    })
}

/// Whether the fence of `seq` has completed. A dead device reports everything done, so waiters drain and the next `EXEC` says EIO.
pub fn fence_done(seq: u64) -> bool {
    let mut g = HW.lock();
    let Some(hw) = g.as_mut() else { return true };
    hw.poll();
    hw.dead || hw.queue.is_done(seq)
}

/// Wait for the work in flight to finish (the device is closing). `false`: it did not, and what user space had bound stays leaked.
pub fn quiesce() -> bool {
    let t0 = crate::cpu::tsc::read();
    loop {
        {
            let mut g = HW.lock();
            let Some(hw) = g.as_mut() else { return true };
            hw.poll();
            if hw.queue.in_flight() == 0 {
                return !hw.dead;
            }
            if crate::cpu::tsc::read().wrapping_sub(t0) > super::copy::ms_ticks(QUIESCE_MS) {
                hw.leak = true;
                crate::serial_println!("[nvgpu] the GPU did not go idle in {} ms at close; bound memory is kept", QUIESCE_MS);
                return false;
            }
        }
        // IF may be 0 here (syscall entry): other CPUs still need their TLB shootdowns answered
        crate::memory::tlb::service_pending();
        core::hint::spin_loop();
    }
}

/// The GPU's global timer in nanoseconds (`nv04_timer_read`).
pub fn timestamp_ns() -> Option<u64> {
    let g = HW.lock();
    let hw = g.as_ref()?;
    loop {
        let hi = hw.regs.rd32(PTIMER_TIME_1);
        let lo = hw.regs.rd32(PTIMER_TIME_0);
        if hi == hw.regs.rd32(PTIMER_TIME_1) {
            return Some((hi as u64) << 32 | lo as u64);
        }
    }
}

/// `/proc/kdebug` line (empty when the level was not asked for).
pub fn render_kdebug() -> String {
    match STATE.load(Ordering::Relaxed) {
        0 => String::new(),
        s => alloc::format!(
            "gpu_uapi: state={} spans_bar1={} spans_pramin={} binds={} unbinds={} pages_bound={} tables_written={} tlb_flushes={} tlb_us_max={} execs={} again={} fences={} dead={}",
            if s == 1 { "ok" } else { "failed" },
            SPANS_BAR1.load(Ordering::Relaxed),
            SPANS_PRAMIN.load(Ordering::Relaxed),
            BINDS.load(Ordering::Relaxed),
            UNBINDS.load(Ordering::Relaxed),
            PAGES_BOUND.load(Ordering::Relaxed),
            TABLES_WRITTEN.load(Ordering::Relaxed),
            TLB_FLUSHES.load(Ordering::Relaxed),
            TLB_US_MAX.load(Ordering::Relaxed),
            EXECS.load(Ordering::Relaxed),
            AGAIN.load(Ordering::Relaxed),
            FENCES.load(Ordering::Relaxed),
            DEAD.load(Ordering::Relaxed)
        ),
    }
}
