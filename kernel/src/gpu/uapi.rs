// kernel/src/gpu/uapi.rs
//
// G4c of docs/gpu/g4-nvkmd-plan.md: the hardware behind `/dev/nvgpu`, level `gpu=uapi`. The device model (`nvgpu::devmodel`) decides
// what user space may ask; `drivers/dev_nvgpu.rs` calls in here for the effects when a GPU is up. What this file keeps after the
// boot, which `compute::run` used to throw away:
//
//   - the GPU page tables (`PageTables`, pool of `POOL_TABLES` tables at VRAM 64 MiB): `bind` maps user buffers into them, `unbind`
//     removes them, each followed by writing the tables it touched into VRAM and flushing the GPU's TLB (`hwq::tlb_flush_regs`);
//   - the GR channel `compute::run` built (chid 1, runlist 0, the compute object) and the copy channel `copy::run` left (chid 2, CE2):
//     their GPFIFO rings, USERD and doorbell tokens. A compute context runs on the first, a copy-only context (NVK's upload queue) on
//     the second; every context of a kind shares its channel (one channel per context is G4e). `hwq::Queue` decides where an `EXEC`'s
//     pushes go and appends the kernel's fence (`gr::fence_push`, `chan::release_push`) that releases a sequence number into a
//     semaphore in host memory; a copy context also gets `chan::bind_push` first, because NVK never binds the copy class itself;
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
use nvgpu::gr::{self, CtxBuf, Mem};
use nvgpu::mmu::Flags;
use nvgpu::rm;
use nvgpu::hwq::{self, Queue, Source};
use nvgpu::mmu::{MapError, PageTables, Target};
use nvgpu::uapi::Push;
use nvgpu::Mmio;

use super::compute::Channel;
use super::copy::Handover;
use super::vaspace::TABLE_VRAM;
use super::Bar0;
use crate::memory::dma::DmaBuf;
use crate::memory::shm::ShmObject;

/// Tables the run-time pool can hold: VRAM `[64 MiB, 96 MiB)`. A bind of N bytes with 4 KiB pages costs N / 2 MiB tables (plus a
/// PD0 per 512 MiB), so this is a few GiB of mapped memory; the 6b test mapping starts at 96 MiB.
pub const POOL_TABLES: usize = 8192;

/// `NVC361_NOTIFY_CHANNEL_PENDING` (see `compute.rs`).
const DOORBELL: u32 = 0xb8_0000 + 0x3_0000 + 0x90;
/// The kernel's fence pushes: 64 slots of 64 bytes in each channel's push page (`gr::PUSH_VA` / VRAM `gr::CHAN_PUSH`; `chan::PUSH_VA` /
/// `chan::PUSH_VRAM`), which the boot's rungs are done with.
const FENCE_SLOTS: u32 = 64;
const FENCE_SLOT_BYTES: u32 = 64;
/// The GR channel's fence semaphore: the host page the rungs used (`gr::HOST_VA + HOST_SEM_OFF`).
const SEM_OFF: u64 = gr::HOST_SEM_OFF;
/// No fence in this long and work in flight: the channel is wedged (a faulted channel is reset by RM and never releases again).
const HANG_MS: u64 = 10_000;
/// The GPU's TLB flush and the quiesce at close each get this long.
const FLUSH_MS: u64 = 2_000;
const QUIESCE_MS: u64 = 3_000;
/// Work in flight and no fence for this long: look at RM's status queue for an RC (a fault), before the hang limit.
const RC_CHECK_MS: u64 = 20;
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

/// What a channel does: its fence push and whether its submissions get a class-binding prelude.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChanKind {
    /// A GR channel: compute and 3D.
    Gr,
    /// The copy channel.
    Ce,
}

/// A context's channel: an index into `Hw::chans`. 0 is the GR channel the boot made, 1 the copy channel, 2.. GR channels made at run time
/// (one per context once the boot's is taken: a fault kills only the channel it happened on).
pub type ChanId = usize;
const GR_BOOT: ChanId = 0;
const CE: ChanId = 1;
const RT_FIRST: ChanId = 2;
/// GR channels that can exist at run time besides the boot's.
const MAX_RT: usize = 8;

/// Where run-time channel `j` lives: VRAM `RT_VRAM + j * RT_SLOT` (its instance block, USERD, GPFIFO ring and fence-push page in the
/// first 64 KiB, its own context buffers after them), and VA `RT_VA + j * RT_VA_SLOT`. VRAM: between the user heap's end and the boot's
/// GR context buffers (`gr::VRAM_CTX`); VA: above the boot's channel area, below the user range.
const RT_VRAM: u64 = nvgpu::hwq::USER_VRAM_END;
const RT_SLOT: u64 = 0x20_0000;
const RT_VA: u64 = 0x3_9000_0000;
const RT_VA_SLOT: u64 = 0x1000_0000;
/// The hardware channel ids of run-time channels (the boot's GR is 1, its copy channel 2).
const RT_CHID0: u32 = 3;
const DMA_MASK: u64 = (1 << 40) - 1;

/// What a run-time channel owns besides its ring: freed with it.
struct Rt {
    slot: usize,
    /// RM object handles: the channel, then its compute, 3D and copy objects.
    handles: [u32; 4],
    mthd: DmaBuf,
    host: DmaBuf,
    /// Ranges mapped in our tables for it: (va, len).
    mapped: Vec<(u64, u64)>,
}

/// A BAR1 mapping of a slot's VRAM, copied out of [`Hw::spans`] (the mappings live as long as the GPU state) so the clearing can run without the lock.
#[derive(Clone, Copy)]
struct SpanRef {
    vram: u64,
    len: u64,
    base: *mut u8,
}

// SAFETY: a device mapping that is never unmapped; the slot it covers is reserved for one creator while it is written.
unsafe impl Send for SpanRef {}

/// What the RM phase of a run-time channel's creation needs, copied out of `Hw` by `prepare_rt`.
struct RtPlan {
    slot: usize,
    chid: u32,
    inst: u64,
    userd: u64,
    gpfifo: u64,
    push: u64,
    gpfifo_va: u64,
    push_va: u64,
    sem_va: u64,
    mem: Vec<Mem>,
    bufs: Vec<CtxBuf>,
    threed: bool,
    grcopy: bool,
    mthd: DmaBuf,
    host: DmaBuf,
    /// Ranges mapped in our tables for it: (va, len, page shift).
    mapped: Vec<(u64, u64, u8)>,
    /// VRAM ranges still to clear (through the BAR1 span; empty when `prepare_rt` already did it through PRAMIN).
    zero: Vec<(u64, u64)>,
    span: Option<SpanRef>,
}

// SAFETY: the DMA buffers are kernel memory owned by the plan; `SpanRef` is covered above.
unsafe impl Send for RtPlan {}

/// The RM phase of creating a run-time GR channel, **without the `HW` lock**: clear the slot's VRAM (BAR1, write-combined), then the RM channel
/// with BIND, SCHEDULE, PROMOTE_CTX and the compute, 3D and copy objects. Returns the doorbell token. Only this creator touches the slot
/// (it is reserved), and GSP-RM serialises itself (`gsp::with_rm`).
fn run_rt(plan: &RtPlan) -> Result<u32, String> {
    if let Some(sp) = plan.span {
        for &(at, len) in &plan.zero {
            debug_assert!(at >= sp.vram && at + len <= sp.vram + sp.len);
            let mut off = 0;
            while off < len {
                // SAFETY: inside the mapping (`prepare_rt` checked the slot is covered), 8-byte aligned (ring block and buffers are page aligned).
                unsafe { core::ptr::write_volatile(sp.base.add((at - sp.vram + off) as usize) as *mut u64, 0) };
                off += 8;
            }
        }
        super::copy::sfence();
    }
    let (slot, chid) = (plan.slot, plan.chid);
    let ch = chan::h_chan(chid);
    let hobj = |k: u32| 0x7a00_0000 | (slot as u32) << 4 | k;
    let (threed, grcopy, bufs, mem) = (plan.threed, plan.grcopy, &plan.bufs, &plan.mem);
    let made = super::gsp::with_rm(|rm| -> Result<u32, String> {
        let alloc = chan::ChanAlloc {
            chid,
            privileged: false,
            engine_type: gr::ENGINE_GR0,
            gpfifo_va: plan.gpfifo_va,
            gpfifo_bytes: gr::GPFIFO_ENTRIES * 8,
            inst: plan.inst,
            userd: plan.userd,
            mthdbuf: plan.mthd.bus_addr(),
            vaspace: rm::H_VASPACE,
        };
        rm.alloc(rm::H_DEVICE, ch, chan::CLASS_GPFIFO, &chan::alloc_params(&alloc)).map_err(|e| alloc::format!("ALLOC channel: {}", e))?;
        let rest = (|| -> Result<(), String> {
            rm.control(ch, chan::CTRL_BIND, &chan::bind_params(gr::ENGINE_GR0)).map_err(|e| alloc::format!("BIND: {}", e))?;
            rm.control(ch, chan::CTRL_GPFIFO_SCHEDULE, &chan::schedule_params()).map_err(|e| alloc::format!("GPFIFO_SCHEDULE: {}", e))?;
            let e = gr::entries(bufs, false, mem);
            rm.control(rm::H_SUBDEVICE, gr::CTRL_PROMOTE_CTX, &gr::promote_params(rm::H_CLIENT, ch, &e)).map_err(|e| alloc::format!("PROMOTE_CTX: {}", e))?;
            rm.alloc(ch, hobj(1), gr::CLASS_COMPUTE, &[]).map_err(|e| alloc::format!("compute object: {}", e))?;
            if threed {
                rm.alloc(ch, hobj(2), gr::CLASS_THREED, &[]).map_err(|e| alloc::format!("3D object: {}", e))?;
            }
            if grcopy {
                rm.alloc(ch, hobj(3), chan::CLASS_COPY, &chan::copy_params(chan::ENGINE_COPY0)).map_err(|e| alloc::format!("copy object: {}", e))?;
            }
            Ok(())
        })();
        match rest {
            Ok(()) => Ok(chan::doorbell_token(0, chid)),
            Err(e) => {
                let _ = rm.free(ch);
                Err(e)
            }
        }
    });
    made.unwrap_or_else(|| Err(String::from("GSP-RM is not running")))
}

/// One channel's ring, doorbell and fence.
struct Chan {
    kind: ChanKind,
    /// The hardware channel id (RM's `RC_TRIGGERED` names it).
    chid: u32,
    token: u32,
    queue: Queue,
    /// The fence semaphore's word (host memory) and the GPU VA it is mapped at.
    sem: *const u32,
    sem_va: u64,
    /// VRAM addresses the CPU writes: `GP_PUT`'s USERD, the GPFIFO ring, the fence-push slots.
    userd: u64,
    gpfifo: u64,
    slots: u64,
    /// The TSC when the oldest work in flight started, or the last time a fence was seen while work was in flight.
    progress: u64,
    /// RM reset it after a fault (or it hung): nothing more runs on it, every fence reads as done and a submission is EIO.
    dead: bool,
    /// Contexts using it.
    users: u32,
    rt: Option<Rt>,
}

impl Chan {
    /// The kernel's fence push for `payload`, and the prelude the channel's queue puts before every submission (empty for GR).
    fn pushes(&self, payload: u32) -> (Vec<u32>, Vec<u32>) {
        match self.kind {
            ChanKind::Gr => (gr::fence_push(self.sem_va, payload), Vec::new()),
            ChanKind::Ce => (chan::release_push(self.sem_va, payload), chan::bind_push()),
        }
    }
}

struct Hw {
    /// The latest binds and unbinds of GPU virtual addresses (oldest first, at most [`VA_LOG_MAX`]): what explains a fault RM reports
    /// (`hwq::describe_va`).
    va_log: Vec<hwq::VaEvent>,
    regs: Bar0,
    bar1: u64,
    pt: PageTables,
    spans: Vec<Span>,
    /// [`GR_BOOT`], [`CE`], then `MAX_RT` slots for run-time GR channels.
    chans: Vec<Option<Chan>>,
    /// The boot's context buffers and where the GR channel has them (the global ones are shared by every channel): what a run-time
    /// channel is built from.
    gr_bufs: Vec<CtxBuf>,
    gr_chan0: Vec<Mem>,
    /// The GR channel has the 3D object (contexts may ask for the 3D engine).
    threed: bool,
    /// The GR channel has a copy object (COPY0): whether copy pushes on it can work is what the `grcopy` test measures.
    #[allow(dead_code)]
    grcopy: bool,
    topo: Option<nvgpu::gr::Topology>,
    /// The host page holding the boot GR channel's fence semaphore (at `SEM_OFF`); the copy channel's lives in `copy`'s.
    #[allow(dead_code)]
    host: DmaBuf,
    /// The device as a whole is wedged (the TLB flush never completed): nothing more is touched.
    dead: bool,
    /// Run-time channel slots whose creation is under way (bit per slot): chosen by `prepare_rt`, not yet a channel.
    rt_reserved: u32,
    /// RC_TRIGGERED channel ids RM reported that no channel has claimed yet (bit per chid).
    rc_pending: u64,
    /// The GPU may still touch bound memory (a channel hung without RM resetting it, or did not go idle at close): never give it back.
    leak: bool,
}

// SAFETY: the mappings, the semaphore pointers and the register window are device/DMA memory used under `HW`'s lock.
unsafe impl Send for Hw {}

static HW: crate::sync::Mutex<Option<Hw>> = crate::sync::Mutex::new(None);

// 0 = not run, 1 = installed, 2 = failed
static STATE: AtomicU32 = AtomicU32::new(0);
static BINDS: AtomicU64 = AtomicU64::new(0);
static UNBINDS: AtomicU64 = AtomicU64::new(0);
static PAGES_BOUND: AtomicU64 = AtomicU64::new(0);
static EXECS: AtomicU64 = AtomicU64::new(0);
static CE_EXECS: AtomicU64 = AtomicU64::new(0);
static AGAIN: AtomicU64 = AtomicU64::new(0);
static FENCES: AtomicU64 = AtomicU64::new(0);
static TLB_FLUSHES: AtomicU64 = AtomicU64::new(0);
static TLB_US_MAX: AtomicU64 = AtomicU64::new(0);
static TABLES_WRITTEN: AtomicU64 = AtomicU64::new(0);
static DEAD: AtomicU32 = AtomicU32::new(0);
static CHANS_DEAD: AtomicU64 = AtomicU64::new(0);

/// Binds and unbinds `Hw::va_log` keeps.
const VA_LOG_MAX: usize = 256;
static CHANS_MADE: AtomicU64 = AtomicU64::new(0);
/// The last run-time channel creation that failed, for /proc/kdebug.
static LAST_ERR: spin::Mutex<String> = spin::Mutex::new(String::new());
static CHANS_FREED: AtomicU64 = AtomicU64::new(0);
/// How long the run-time channel calls and the lock itself hold up everyone else (microseconds, worst case): a session creating or destroying a
/// channel talks to GSP-RM with `HW` held, and every other session's submit and fence query waits for it.
static CREATE_US_MAX: AtomicU64 = AtomicU64::new(0);
/// The part of a creation that does hold the lock (`prepare_rt`).
static PREPARE_US_MAX: AtomicU64 = AtomicU64::new(0);
static DESTROY_US_MAX: AtomicU64 = AtomicU64::new(0);
static LOCK_WAIT_US_MAX: AtomicU64 = AtomicU64::new(0);
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
pub(super) fn install(r: &mut String, regs: &Bar0, mut pt: PageTables, ch: Channel, ce: Option<Handover>) {
    let Some(pci) = super::gsp::pci_info().filter(|p| p.bar1 != 0) else {
        STATE.store(2, Ordering::Relaxed);
        let _ = writeln!(r, "uapi: STOP: no BAR1");
        return;
    };
    // the tables the boot wrote were verified through PRAMIN then; only what a bind changes is written from now on
    let boot_tables = pt.take_dirty().len();

    let mut spans = Vec::new();
    let mut wanted = 2;
    // the pool's last table (unused: a bind allocates from the front), the GR channel's DST page (its last rung is done), the copy
    // channel's VRAM fence page (only the ladder used it)
    let pool_bytes = POOL_TABLES as u64 * 0x1000;
    if let Some(s) = map_span(r, regs, pci.bar1, TABLE_VRAM, pool_bytes, TABLE_VRAM + pool_bytes - 0x1000, "tables") {
        spans.push(s);
    }
    if let Some(s) = map_span(r, regs, pci.bar1, gr::VRAM_CHAN, 0x1_0000, gr::CHAN_DST, "channel") {
        spans.push(s);
    }
    if ce.is_some() {
        wanted += 1;
        if let Some(s) = map_span(r, regs, pci.bar1, chan::VRAM_CHAN, 0x1_0000, chan::FENCE_VRAM, "copy channel") {
            spans.push(s);
        }
    }
    SPANS_BAR1.store(spans.len() as u32, Ordering::Relaxed);
    SPANS_PRAMIN.store((wanted - spans.len()) as u32, Ordering::Relaxed);

    // Each fence semaphore starts at 0 and each ring where the boot's pushes left it (all of them were waited for).
    ch.host.copy_in(SEM_OFF as usize, &0u32.to_le_bytes());
    let gr_chan = Chan {
        kind: ChanKind::Gr,
        chid: 1,
        token: ch.token,
        queue: Queue::new(gr::GPFIFO_ENTRIES, ch.slot, FENCE_SLOTS, gr::PUSH_VA, FENCE_SLOT_BYTES, gr::FENCE_PUSH_BYTES, 0),
        // SAFETY: the host page is a live DMA allocation of ours, at least `SEM_OFF + 4` bytes long (kept in `Hw::host`).
        sem: unsafe { ch.host.virt().add(SEM_OFF as usize) } as *const u32,
        sem_va: gr::HOST_VA + SEM_OFF,
        userd: gr::CHAN_USERD,
        gpfifo: gr::CHAN_GPFIFO,
        slots: gr::CHAN_PUSH,
        progress: crate::cpu::tsc::read(),
        dead: false,
        users: 0,
        rt: None,
    };
    let ce_chan = ce.map(|h| {
        // SAFETY: the copy channel's host fence page (`chan::HFENCE_VA`), a leaked 4 KiB DMA allocation; the channel is ours now.
        unsafe { core::ptr::write_volatile(h.hfence as *mut u32, 0) };
        Chan {
            kind: ChanKind::Ce,
            chid: 2,
            token: h.token,
            queue: Queue::new(chan::GPFIFO_ENTRIES, h.slot, FENCE_SLOTS, chan::PUSH_VA, FENCE_SLOT_BYTES, chan::RELEASE_PUSH_BYTES, chan::BIND_PUSH_BYTES),
            sem: h.hfence as *const u32,
            sem_va: chan::HFENCE_VA,
            userd: chan::USERD_VRAM,
            gpfifo: chan::GPFIFO_VRAM,
            slots: chan::PUSH_VRAM,
            progress: crate::cpu::tsc::read(),
            dead: false,
            users: 0,
            rt: None,
        }
    });
    let _ = writeln!(
        r,
        "uapi: installed: {} tables ({} written at boot, pool {}), {} BAR1 span(s) + {} through PRAMIN, GR channel token {:#x} at ring entry {} of {}, copy channel {}",
        pt.len(),
        boot_tables,
        POOL_TABLES,
        spans.len(),
        wanted - spans.len(),
        ch.token,
        ch.slot,
        gr::GPFIFO_ENTRIES,
        match &ce_chan {
            Some(c) => alloc::format!("token {:#x}", c.token),
            None => String::from("not available (contexts of the copy engine alone are refused)"),
        }
    );
    let mut chans: Vec<Option<Chan>> = Vec::new();
    chans.push(Some(gr_chan));
    chans.push(ce_chan);
    chans.extend((0..MAX_RT).map(|_| None));
    *HW.lock() = Some(Hw {
        regs: Bar0 { base: regs.base, len: regs.len },
        bar1: pci.bar1,
        pt,
        spans,
        chans,
        gr_bufs: ch.bufs,
        gr_chan0: ch.chan0,
        threed: ch.threed,
        grcopy: ch.copy,
        topo: ch.topo,
        host: ch.host,
        va_log: Vec::new(),
        dead: false,
        rt_reserved: 0,
        rc_pending: 0,
        leak: false,
    });
    STATE.store(1, Ordering::Relaxed);
}

pub(super) fn install_failed(r: &mut String, why: core::fmt::Arguments) {
    STATE.store(2, Ordering::Relaxed);
    let _ = writeln!(r, "uapi: STOP: {}", why);
}

/// What floorsweeping left of the GR engine (GPCs, TPCs), if RM said.
pub fn topology() -> Option<(u32, u32)> {
    HW.lock().as_ref().and_then(|h| h.topo).map(|t| (t.gpcs, t.tpcs))
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
    fn log_va(&mut self, bind: bool, va: u64, size: u64) {
        if self.va_log.len() == VA_LOG_MAX {
            self.va_log.remove(0);
        }
        self.va_log.push(hwq::VaEvent { bind, va, size, at_ms: crate::time::ktime_get() / 1_000_000 });
    }

    /// The whole device is wedged.
    fn mark_dead(&mut self, why: &str) {
        if !self.dead {
            self.dead = true;
            self.leak = true;
            DEAD.store(1, Ordering::Relaxed);
            crate::serial_println!("[nvgpu] DEAD: {}", why);
            crate::kalert!("nvgpu: the GPU stopped answering ({})", why);
        }
    }

    /// One channel is gone (RM reset it after a fault, or it hung): the others, and new contexts, go on.
    fn kill_chan(&mut self, id: ChanId, why: &str, hung: bool) {
        let Some(c) = self.chans[id].as_mut() else { return };
        if c.dead {
            return;
        }
        c.dead = true;
        CHANS_DEAD.fetch_add(1, Ordering::Relaxed);
        if hung {
            // nothing told us the GPU stopped touching its memory
            self.leak = true;
        }
        let chid = c.chid;
        // RM said it was a fault: where was the address
        let fault = super::gsp::last_rc_fault().filter(|_| !hung).map(|(except, addr, kind)| {
            alloc::format!("; exception {} (31 = MMU fault), mmu fault type {} (0 = no PDE, 1 = PTE not valid), address {:#x}: {}", except, kind, addr, hwq::describe_va(&self.va_log, addr))
        });
        crate::serial_println!("[nvgpu] channel {} (chid {}) is dead: {}{}", id, chid, why, fault.as_deref().unwrap_or(""));
        crate::kalert!("nvgpu: a GPU channel was lost ({})", why);
    }

    /// Read a channel's fence semaphore, retire what it completes, and notice a dead channel: RM's `RC_TRIGGERED` names the channel it
    /// reset (looked at when work has been quiet for a moment: the status queue has no interrupt), and a channel quiet for `HANG_MS`
    /// with nothing from RM is declared hung.
    fn poll(&mut self, id: ChanId) {
        let Some(c) = self.chans[id].as_mut() else { return };
        // SAFETY: the semaphore word is host memory of ours that outlives the channel.
        let sem = unsafe { core::ptr::read_volatile(c.sem) };
        let before = c.queue.done_seq();
        if c.queue.observe(sem) {
            FENCES.fetch_add(c.queue.done_seq() - before, Ordering::Relaxed);
            super::pacing::done(id, c.queue.done_seq());
            c.progress = crate::cpu::tsc::read();
        }
        if c.dead || c.queue.in_flight() == 0 {
            return;
        }
        let quiet = crate::cpu::tsc::read().wrapping_sub(c.progress);
        let (chid, in_flight, last) = (c.chid, c.queue.in_flight(), c.queue.last_seq());
        if quiet > super::copy::ms_ticks(RC_CHECK_MS) {
            super::gsp::poll_events_try();
            self.rc_pending |= super::gsp::take_rc_mask();
            let known: u64 = self.chans.iter().flatten().filter(|c| !c.dead).map(|c| 1u64 << (c.chid & 63)).fold(0, |a, b| a | b);
            if self.rc_pending & (1u64 << (chid & 63)) != 0 {
                self.rc_pending &= !(1u64 << (chid & 63));
                let why = alloc::format!("RM reset it (RC_TRIGGERED) with {} submission(s) in flight, semaphore {:#x}, last submitted {}", in_flight, sem, last);
                self.kill_chan(id, &why, false);
                return;
            }
            // an RC naming a channel we do not know: it is this one that has been quiet
            if self.rc_pending & !known != 0 {
                self.rc_pending &= known;
                let why = alloc::format!("RM reported an RC for an unknown channel while this one was quiet with {} submission(s) in flight", in_flight);
                self.kill_chan(id, &why, false);
                return;
            }
        }
        if quiet > super::copy::ms_ticks(HANG_MS) {
            let why = alloc::format!("no fence for {} ms with {} submission(s) in flight, semaphore {:#x}, last submitted {}", HANG_MS, in_flight, sem, last);
            self.kill_chan(id, &why, true);
        }
    }

    /// Map `[vram, vram + len)` of BAR1 (once) so the ring writes of a run-time channel go through write-combined stores.
    fn ensure_span(&mut self, vram: u64, len: u64, probe: u64) {
        if self.spans.iter().any(|s| s.vram <= vram && vram + len <= s.vram + s.len) {
            return;
        }
        let mut log = String::new();
        if let Some(s) = map_span(&mut log, &self.regs, self.bar1, vram, len, probe, "run-time channel") {
            self.spans.push(s);
        }
        crate::serial_print!("{}", log);
    }

    /// First of the three phases that make a GR channel at run time (like `compute::run` did at boot), with `HW` held: pick a slot and reserve
    /// it, lay out its VRAM and VA, allocate its host pages, put the ring and the context buffers into our tables (and flush the GPU's
    /// TLB), and make sure BAR1 reaches the slot. What takes long (clearing the slot's VRAM, and the RM calls that make the channel)
    /// is left for [`run_rt`], which runs **without** the lock: with it held every other session's `submit` and fence query waited for the
    /// whole creation (80-93 ms measured on the Ryzen: a 180 ms hitch in another client's frame pacing, Ryzen #163).
    fn prepare_rt(&mut self) -> Result<RtPlan, String> {
        let slot = (0..MAX_RT)
            .find(|&j| self.chans[RT_FIRST + j].is_none() && self.rt_reserved & (1 << j) == 0)
            .ok_or_else(|| String::from("all run-time channel slots are in use"))?;
        let j = slot as u64;
        let chid = RT_CHID0 + slot as u32;
        let vram = RT_VRAM + j * RT_SLOT;
        let va0 = RT_VA + j * RT_VA_SLOT;
        let (inst, userd, gpfifo, push) = (vram, vram + 0x1000, vram + 0x2000, vram + 0x4000);
        let (gpfifo_va, push_va, sem_va) = (va0, va0 + 0x1_0000, va0 + 0x2_0000);
        let (mem, pa_end, _) = gr::chan_layout(&self.gr_bufs, &self.gr_chan0, vram + 0x1_0000, va0 + 0x10_0000);
        if pa_end > vram + RT_SLOT {
            return Err(alloc::format!("the context buffers end at {:#x}, past the slot", pa_end));
        }
        let mthd = DmaBuf::alloc(chan::MTHDBUF_SIZE as usize, DMA_MASK).map_err(|e| alloc::format!("method buffer: {:?}", e))?;
        let host = match DmaBuf::alloc(0x1000, DMA_MASK) {
            Ok(h) => h,
            Err(e) => {
                mthd.free();
                return Err(alloc::format!("fence page: {:?}", e));
            }
        };
        // a clean start: the ring block, and the context buffers RM initialises (nouveau allocates them zeroed)
        let mut zero: Vec<(u64, u64)> = Vec::new();
        zero.push((vram, 0x6000));
        for (b, m) in self.gr_bufs.iter().zip(&mem) {
            if !b.global && b.init {
                zero.push((m.pa, gr::mapped_len(b)));
            }
        }
        // the ring and the buffers into our tables
        let mut mapped: Vec<(u64, u64, u8)> = Vec::new();
        let f = Flags::default();
        let maps = (|| -> Result<(), MapError> {
            self.pt.map_range(gpfifo_va, gpfifo, gr::GPFIFO_ENTRIES as u64 * 8, Target::Vram, f)?;
            mapped.push((gpfifo_va, gr::GPFIFO_ENTRIES as u64 * 8, 12));
            self.pt.map_range(push_va, push, 0x1000, Target::Vram, f)?;
            mapped.push((push_va, 0x1000, 12));
            self.pt.map(sem_va, host.bus_addr(), Target::Host, f)?;
            mapped.push((sem_va, 0x1000, 12));
            for m in gr::chan_mappings(&self.gr_bufs, &mem) {
                let pf = Flags { privileged: true, read_only: m.ro, kind: 0 };
                match m.page {
                    21 => self.pt.map_huge_range(m.va, m.pa, m.len, Target::Vram, pf)?,
                    16 => self.pt.map_big_range(m.va, m.pa, m.len, Target::Vram, pf)?,
                    _ => self.pt.map_range(m.va, m.pa, m.len, Target::Vram, pf)?,
                }
                mapped.push((m.va, m.len, m.page));
            }
            Ok(())
        })();
        let undo = |hw: &mut Hw, mapped: &[(u64, u64, u8)]| {
            for &(va, len, page) in mapped {
                hwq::unbind_pages(&mut hw.pt, va, len, page);
            }
            hw.publish_tables();
        };
        if let Err(e) = maps {
            undo(self, &mapped);
            mthd.free();
            host.free();
            return Err(alloc::format!("mapping the channel: {:?}", e));
        }
        if !self.publish_tables() {
            undo(self, &mapped);
            mthd.free();
            host.free();
            return Err(String::from("the TLB flush after mapping the channel did not complete"));
        }
        // BAR1 over the whole slot, so the clearing below is a burst of write-combined stores (PRAMIN, a 4-byte window of BAR0 that every
        // user shares, is the fallback and needs the lock)
        self.ensure_span(vram, RT_SLOT, vram + 0x8000);
        let span = self.spans.iter().find(|s| s.vram <= vram && vram + RT_SLOT <= s.vram + s.len).map(|s| SpanRef { vram: s.vram, len: s.len, base: s.base });
        if span.is_none() {
            // the old way, with the lock held: through PRAMIN
            let mut p = Pramin::new(&self.regs);
            for &(at, len) in &zero {
                for off in (0..len).step_by(4) {
                    p.wr32(at + off, 0);
                }
            }
            p.restore();
            zero.clear();
        }
        self.rt_reserved |= 1 << slot;
        Ok(RtPlan { slot, chid, inst, userd, gpfifo, push, gpfifo_va, push_va, sem_va, mem, bufs: self.gr_bufs.clone(), threed: self.threed, grcopy: self.grcopy, mthd, host, mapped, zero, span })
    }

    /// Give a reservation back with everything `prepare_rt` did undone (the RM phase failed, or never ran).
    fn abandon_rt(&mut self, plan: RtPlan) {
        for &(va, len, page) in &plan.mapped {
            hwq::unbind_pages(&mut self.pt, va, len, page);
        }
        self.publish_tables();
        if !self.leak {
            plan.mthd.free();
            plan.host.free();
        }
        self.rt_reserved &= !(1 << plan.slot);
    }

    /// Last phase, with `HW` held: the RM phase's answer becomes a channel in its slot with one user.
    fn finish_rt(&mut self, plan: RtPlan, made: Result<u32, String>) -> Result<ChanId, String> {
        let token = match made {
            Ok(t) => t,
            Err(e) => {
                self.abandon_rt(plan);
                return Err(e);
            }
        };
        let RtPlan { slot, chid, userd, gpfifo, push, push_va, sem_va, mthd, host, mapped, .. } = plan;
        let (ch, hobj) = (chan::h_chan(chid), |k: u32| 0x7a00_0000 | (slot as u32) << 4 | k);
        let id = RT_FIRST + slot;
        self.chans[id] = Some(Chan {
            kind: ChanKind::Gr,
            chid,
            token,
            queue: Queue::new(gr::GPFIFO_ENTRIES, 0, FENCE_SLOTS, push_va, FENCE_SLOT_BYTES, gr::FENCE_PUSH_BYTES, 0),
            // SAFETY: the page is a live DMA allocation of ours, kept in `Rt::host` until the channel goes.
            sem: host.virt() as *const u32,
            sem_va,
            userd,
            gpfifo,
            slots: push,
            progress: crate::cpu::tsc::read(),
            dead: false,
            users: 1,
            rt: Some(Rt { slot, handles: [ch, hobj(1), hobj(2), hobj(3)], mthd, host, mapped: mapped.iter().map(|&(va, len, page)| (va, len | (page as u64) << 56)).collect() }),
        });
        self.rt_reserved &= !(1 << slot);
        CHANS_MADE.fetch_add(1, Ordering::Relaxed);
        crate::serial_println!("[nvgpu] GR channel made at run time: slot {}, chid {}, token {:#x}", slot, chid, token);
        Ok(id)
    }

    /// Give a run-time channel back, phase 2 of 3 (see [`ctx_destroy`]): take it out of its slot, which stays reserved until
    /// [`finish_destroy_rt`](Self::finish_destroy_rt), so nobody makes a channel over RM objects that are still there.
    fn take_rt(&mut self, id: ChanId) -> Option<Rt> {
        let mut c = self.chans[id].take()?;
        let rt = c.rt.take()?;
        self.rt_reserved |= 1 << rt.slot;
        Some(rt)
    }

    /// Phase 3, with `HW` held: unmap what was mapped for the channel, free its host pages (unless the GPU may still touch them), give the slot back.
    fn finish_destroy_rt(&mut self, rt: Rt) {
        for &(va, packed) in &rt.mapped {
            hwq::unbind_pages(&mut self.pt, va, packed & ((1 << 56) - 1), (packed >> 56) as u8);
        }
        self.publish_tables();
        if !self.leak {
            rt.mthd.free();
            rt.host.free();
        }
        self.rt_reserved &= !(1 << rt.slot);
        CHANS_FREED.fetch_add(1, Ordering::Relaxed);
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

/// Holds of the GPU lock longer than this are recorded (`gpu_uapi_slow:` in /proc/kdebug).
const SLOW_HOLD_US: u64 = 2_000;
/// How many of them are kept (the latest).
const SLOW_KEEP: usize = 16;

/// One long hold: what held the lock, when it let go (`ktime_get`, the clock `CLOCK_MONOTONIC` reads), for how long, and how long it had waited.
#[derive(Clone, Copy)]
struct SlowHold {
    op: &'static str,
    released_ns: u64,
    hold_us: u64,
    wait_us: u64,
}

static SLOW: crate::sync::Mutex<alloc::collections::VecDeque<SlowHold>> = crate::sync::Mutex::new(alloc::collections::VecDeque::new());

/// The GPU state, locked. Records the wait for the lock, and on release the hold if it was long: with one lock for every session, the
/// longest hold is the longest another client's frame can be stalled, and this says which operation it was.
struct HwGuard {
    g: spin::mutex::MutexGuard<'static, Option<Hw>>,
    op: &'static str,
    acquired: u64,
    wait_us: u64,
}

impl core::ops::Deref for HwGuard {
    type Target = Option<Hw>;
    fn deref(&self) -> &Option<Hw> {
        &self.g
    }
}

impl core::ops::DerefMut for HwGuard {
    fn deref_mut(&mut self) -> &mut Option<Hw> {
        &mut self.g
    }
}

impl Drop for HwGuard {
    fn drop(&mut self) {
        let hold_us = ticks_to_us(crate::cpu::tsc::read().wrapping_sub(self.acquired));
        if hold_us >= SLOW_HOLD_US {
            let mut q = SLOW.lock();
            if q.len() == SLOW_KEEP {
                q.pop_front();
            }
            q.push_back(SlowHold { op: self.op, released_ns: crate::time::ktime_get(), hold_us, wait_us: self.wait_us });
        }
    }
}

/// Take the GPU state for `op`, recording how long the lock made the caller wait.
fn lock_hw(op: &'static str) -> HwGuard {
    let t0 = crate::cpu::tsc::read();
    let g = HW.lock();
    let acquired = crate::cpu::tsc::read();
    let wait_us = ticks_to_us(acquired.wrapping_sub(t0));
    LOCK_WAIT_US_MAX.fetch_max(wait_us, Ordering::Relaxed);
    HwGuard { g, op, acquired, wait_us }
}

fn with<T>(op: &'static str, f: impl FnOnce(&mut Hw) -> Result<T, Error>) -> Result<T, Error> {
    let mut g = lock_hw(op);
    let hw = g.as_mut().ok_or(Error::Io)?;
    if hw.dead {
        return Err(Error::Io);
    }
    f(hw)
}

/// Map `size` bytes of `backing` (from `bo_off`) at `va`. System pages are pinned (one reference each, taken from the arena) while
/// they are mapped.
pub fn bind(arena: &ShmObject, va: u64, size: u64, backing: Backing, bo_off: u64, kind: u32) -> Result<(), Error> {
    with("bind", |hw| {
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
        hw.log_va(true, va, size);
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
    let mut g = lock_hw("unbind");
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
    hw.log_va(false, va, size);
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

/// The channel a context with `engines` runs on. NVK's queue families ask for 3D + compute (+ copy, Vulkan's transfer bit; even a
/// compute-only one gets the 3D engine, for MME indirect dispatch): they run on a GR channel, which has compute, 3D and copy objects (if
/// RM allowed them). The first such context takes the boot's GR channel; every other one gets a channel of its own, made here (so a fault
/// on one context's channel leaves the others alone, and a new context after a fault gets a fresh channel). A copy-only context (NVK's
/// upload queue) shares the copy channel. 2D and M2MF are not offered.
pub fn ctx_create(engines: u32) -> Result<ChanId, Error> {
    use nvgpu::uapi::{ENGINE_3D, ENGINE_COMPUTE, ENGINE_COPY};
    /// What the decision (made with the lock) came to.
    enum Choice {
        /// A channel that exists: its user count is already taken.
        Have(ChanId),
        /// A new run-time channel, to be made by `run_rt` without the lock.
        Make(RtPlan),
    }
    let choice = with("ctx_create", |hw| {
        let id = match engines {
            e if e & ENGINE_COMPUTE != 0 && e & !(ENGINE_COMPUTE | ENGINE_COPY | ENGINE_3D) == 0 => {
                if e & ENGINE_3D != 0 && !hw.threed {
                    return Err(Error::Inval);
                }
                match hw.chans[GR_BOOT].as_ref() {
                    Some(c) if c.users == 0 && !c.dead => GR_BOOT,
                    _ => {
                        let t0 = crate::cpu::tsc::read();
                        let prepared = hw.prepare_rt();
                        PREPARE_US_MAX.fetch_max(ticks_to_us(crate::cpu::tsc::read().wrapping_sub(t0)), Ordering::Relaxed);
                        return match prepared {
                            Ok(plan) => Ok(Choice::Make(plan)),
                            Err(e) => {
                                crate::serial_println!("[nvgpu] a new GR channel: {}", e);
                                *LAST_ERR.lock() = e;
                                Err(Error::NoSpc)
                            }
                        };
                    }
                }
            }
            ENGINE_COPY => CE,
            _ => return Err(Error::Inval),
        };
        let c = hw.chans[id].as_mut().ok_or(Error::Inval)?;
        c.users += 1;
        Ok(Choice::Have(id))
    })?;
    match choice {
        Choice::Have(id) => Ok(id),
        Choice::Make(plan) => {
            // GSP-RM makes the channel while the other sessions go on submitting
            let t0 = crate::cpu::tsc::read();
            let made = run_rt(&plan);
            CREATE_US_MAX.fetch_max(ticks_to_us(crate::cpu::tsc::read().wrapping_sub(t0)), Ordering::Relaxed);
            let mut g = lock_hw("finish_rt");
            let Some(hw) = g.as_mut() else { return Err(Error::Io) };
            hw.finish_rt(plan, made).map_err(|e| {
                crate::serial_println!("[nvgpu] a new GR channel: {}", e);
                *LAST_ERR.lock() = e;
                Error::NoSpc
            })
        }
    }
}

/// A context is gone: its channel is given back when it was a run-time one nobody else uses. In three steps so that the slow part does not hold
/// the GPU lock (freeing the RM objects took up to 25 ms with it held, stalling every other session's submits, Ryzen #165): count the user
/// out and wait for the channel's work to finish, taking the lock only for each look; take the channel out of its slot; free its RM objects
/// **without** the lock; then unmap and free under it.
pub fn ctx_destroy(id: ChanId) {
    {
        let mut g = lock_hw("ctx_destroy");
        let Some(hw) = g.as_mut() else { return };
        let Some(c) = hw.chans.get_mut(id).and_then(|c| c.as_mut()) else { return };
        c.users = c.users.saturating_sub(1);
        if !(id >= RT_FIRST && c.users == 0) {
            return;
        }
    }
    let t0 = crate::cpu::tsc::read();
    loop {
        {
            let mut g = lock_hw("ctx_destroy_wait");
            let Some(hw) = g.as_mut() else { return };
            hw.poll(id);
            let Some(c) = hw.chans[id].as_ref() else { return };
            if c.dead || c.queue.in_flight() == 0 {
                break;
            }
            if crate::cpu::tsc::read().wrapping_sub(t0) > super::copy::ms_ticks(QUIESCE_MS) {
                hw.leak = true;
                crate::serial_println!("[nvgpu] channel {} did not go idle at close: kept", id);
                return;
            }
        }
        crate::memory::tlb::service_pending();
        core::hint::spin_loop();
    }
    let rt = {
        let mut g = lock_hw("ctx_destroy_take");
        let Some(hw) = g.as_mut() else { return };
        match hw.take_rt(id) {
            Some(rt) => rt,
            None => return,
        }
    };
    let t1 = crate::cpu::tsc::read();
    let [ch, h1, h2, h3] = rt.handles;
    // a channel RM reset after a fault is freed like any other
    let _ = super::gsp::with_rm(|rm| {
        for h in [h3, h2, h1] {
            let _ = rm.free(h);
        }
        let _ = rm.free(ch);
    });
    DESTROY_US_MAX.fetch_max(ticks_to_us(crate::cpu::tsc::read().wrapping_sub(t1)), Ordering::Relaxed);
    let mut g = lock_hw("ctx_destroy_finish");
    if let Some(hw) = g.as_mut() {
        hw.finish_destroy_rt(rt);
    }
}

/// Queue `pushes` (already validated) on channel `id` with the fence after them; returns the sequence number the fence completes as.
pub fn submit(id: ChanId, pushes: &[Push]) -> Result<u64, Error> {
    with("submit", |hw| {
        hw.poll(id);
        let c = hw.chans.get_mut(id).and_then(|c| c.as_mut()).ok_or(Error::Io)?;
        if c.dead {
            return Err(Error::Io);
        }
        let plan = match c.queue.plan(pushes) {
            Ok(p) => p,
            Err(Error::Again) => {
                AGAIN.fetch_add(1, Ordering::Relaxed);
                return Err(Error::Again);
            }
            Err(e) => return Err(e),
        };
        if c.queue.in_flight() == 1 {
            c.progress = crate::cpu::tsc::read();
        }
        let (fence, prelude) = c.pushes(plan.payload);
        let (slots, gpfifo, userd, token, kind) = (c.slots, c.gpfifo, c.userd, c.token, c.kind);
        let mut io = Io::new(&hw.regs, &hw.spans);
        let slot_at = slots + plan.fence_slot as u64 * FENCE_SLOT_BYTES as u64;
        for (i, w) in fence.iter().enumerate() {
            io.wr32(slot_at + 4 * i as u64, *w);
        }
        for (i, w) in prelude.iter().enumerate() {
            io.wr32(slot_at + 4 * fence.len() as u64 + 4 * i as u64, *w);
        }
        for (idx, e) in &plan.entries {
            io.wr64(gpfifo + 8 * *idx as u64, *e);
        }
        io.wr32(userd + chan::USERD_GP_PUT, plan.gp_put);
        io.finish();
        core::sync::atomic::fence(Ordering::SeqCst);
        hw.regs.wr32(DOORBELL, token);
        super::pacing::submit(id, plan.seq);
        EXECS.fetch_add(1, Ordering::Relaxed);
        if kind == ChanKind::Ce {
            CE_EXECS.fetch_add(1, Ordering::Relaxed);
        }
        Ok(plan.seq)
    })
}

/// Whether the fence of `seq` on channel `id` has completed. A dead channel (or device) reports everything done, so waiters drain and the
/// next `EXEC` on it says EIO.
pub fn fence_done(id: ChanId, seq: u64) -> bool {
    let mut g = lock_hw("fence_done");
    let Some(hw) = g.as_mut() else { return true };
    hw.poll(id);
    hw.dead || hw.chans.get(id).and_then(|c| c.as_ref()).is_none_or(|c| c.dead || c.queue.is_done(seq))
}

/// Wait for the work in flight on the channels `ids` (those of a session that is closing) to finish. Other sessions go on running:
/// their channels are not waited for. `false`: the work did not finish, and what user space had bound stays leaked.
pub fn quiesce(ids: &[ChanId]) -> bool {
    let t0 = crate::cpu::tsc::read();
    loop {
        {
            let mut g = lock_hw("quiesce");
            let Some(hw) = g.as_mut() else { return true };
            for &id in ids {
                hw.poll(id);
            }
            if ids.iter().all(|&id| hw.chans.get(id).and_then(|c| c.as_ref()).is_none_or(|c| c.dead || c.queue.in_flight() == 0)) {
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

/// `/proc/kdebug` line (empty when the level was not asked for), then `gpu_uapi_slow:` with the latest long holds of the GPU lock.
pub fn render_kdebug() -> String {
    let mut out = render_state_kdebug();
    if !out.is_empty() {
        let q = SLOW.lock();
        let mut line = String::from("\ngpu_uapi_slow:");
        for h in q.iter() {
            let _ = write!(line, " [{} at {}.{:03} held {}.{:01} ms, had waited {} us]", h.op, h.released_ns / 1_000_000_000, (h.released_ns / 1_000_000) % 1000, h.hold_us / 1000, (h.hold_us % 1000) / 100, h.wait_us);
        }
        if q.is_empty() {
            line.push_str(" none");
        }
        out.push_str(&line);
    }
    out
}

fn render_state_kdebug() -> String {
    match STATE.load(Ordering::Relaxed) {
        0 => String::new(),
        s => alloc::format!(
            "gpu_uapi: state={} spans_bar1={} spans_pramin={} binds={} unbinds={} pages_bound={} tables_written={} tlb_flushes={} tlb_us_max={} execs={} ce_execs={} again={} fences={} dead={} chans_made={} chans_freed={} chans_dead={} create_us_max={} prepare_us_max={} destroy_us_max={} lock_wait_us_max={} last_chan_err=\"{}\"",
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
            CE_EXECS.load(Ordering::Relaxed),
            AGAIN.load(Ordering::Relaxed),
            FENCES.load(Ordering::Relaxed),
            DEAD.load(Ordering::Relaxed),
            CHANS_MADE.load(Ordering::Relaxed),
            CHANS_FREED.load(Ordering::Relaxed),
            CHANS_DEAD.load(Ordering::Relaxed),
            CREATE_US_MAX.load(Ordering::Relaxed),
            PREPARE_US_MAX.load(Ordering::Relaxed),
            DESTROY_US_MAX.load(Ordering::Relaxed),
            LOCK_WAIT_US_MAX.load(Ordering::Relaxed),
            LAST_ERR.lock().replace('"', "'")
        ),
    }
}
