// kernel/src/drivers/dev_nvgpu.rs
//
// `/dev/nvgpu`: the GPU for user space, in the terms Mesa's NVK asks for (`nvkmd`). Plan: `docs/gpu/g4-nvkmd-plan.md`; the
// interface is `nvgpu/uapi/nvgpu.h` (Rust mirror `nvgpu::uapi`); every rule about what a caller may ask lives in the host-tested
// `nvgpu::devmodel`. This file is the adapter: the open/close discipline, the copies to and from user memory, the arena of
// system memory and the mapping from `devmodel` errors to errno.
//
// - **Several sessions** (G5 layer 1, up to `hwq::SESSIONS`; one more `open` is `EBUSY`). Each open file description is a session with
//   its own `Device` (BOs, contexts, timelines) and its own 16 GiB slice of the GPU virtual address space (`hwq::session_va`), because
//   there is one set of GPU page tables; VRAM BOs come from one heap all sessions share (`VRAM`). Nothing stops a process from
//   binding another session's addresses through a hand-made page table entry: there is no isolation between processes yet.
//   `dup`/`fork` share the session, which ends with the last reference, so a holder killed by a signal hands its part of the device
//   back: everything it had is unbound, released and forgotten, and the other sessions never notice.
// - **Sharing buffers** (G5 layer 2): `BO_EXPORT` makes a descriptor of a BO (`BoFile`, an ordinary file: `SCM_RIGHTS`, `dup`, fork), `BO_IMPORT`
//   turns one into a BO of the importing session. The system arena and the VRAM heap are global (`STORAGE`), so an arena offset or a VRAM
//   offset means the same memory in every session, and every allocation has a count of holders (its BO, the exported descriptors, the
//   importers' BOs): the storage goes back only when the last lets go, so the exporter can close its handle or die first.
// - **Sharing timelines** (G5 layer 2): `SYNC_EXPORT` / `SYNC_IMPORT`, the same way (`SyncFile`). An exported timeline lives in a global registry
//   (`SYNCS`), not in its session: its value moves as the fences of the work that signals it complete (`gpu::uapi::fence_done`, valid from any
//   session), and whichever session reads it resolves them, so the session that queued the work can be idle, or gone.
// - **One owner of the display.** The first session that `PRESENT`s owns the scanout (another one's `PRESENT` is `EBUSY`) until it closes.
// - **System memory is one shared-memory arena** (`ShmObject`, 1 GiB, pages allocated on first touch). A system BO is a range of
//   it, so `mmap(fd, bo.mmap_offset)` maps that BO like any shared mapping, and `BO_FREE` gives its pages back (`discard`).
// - **Nothing blocks.** `SYNC_WAIT`, and an `EXEC` whose waits are not satisfied, return `EAGAIN`; the caller sleeps and retries
//   (`sys_ioctl` calls in with the fd-table lock held, so parking here would stall the process's other threads; see the plan).
// - **Two devices behind one interface.** With `gpu=uapi` and a GPU that came up (`gpu::uapi::installed`), BOs bind into the GPU's
//   page tables and an `EXEC` runs on its GR channel (G4c, `gpu/uapi.rs`); the fences are the GPU's. Otherwise the device is a
//   software one (`NVG_INFO_SOFTWARE`): memory and bookkeeping are real, execution completes at once without running anything, so the
//   whole user-space stack can be built and tested in QEMU, which has no GPU. `KernelBackend::hw` says which.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use nvgpu::devmodel::{Backend, Backing, Device, Error, Heap, Layout, RangeAlloc, SoftBackend};
use nvgpu::uapi::{self, Push, SyncRef};

use crate::fs::types::{Errno, Stat};
use crate::gpu;
use crate::memory::shm::ShmObject;
use crate::process::file::{FileError, FileHandle, FileResult, IoctlOut};
use crate::process::syscall::{errno, validate_user_buffer};

/// Bytes of system memory all sessions together may hold in BOs: one arena, its pages allocated on first touch (its page table costs
/// 16 bytes per page of this size, once).
const ARENA_BYTES: u64 = 4 << 30;
/// The software device's pretend VRAM.
const SOFT_VRAM_BYTES: u64 = 6 << 30;
/// What the hardware device offers: the user heap of `nvgpu::hwq` (VRAM the kernel does not use).
const HW_VRAM_BYTES: u64 = nvgpu::hwq::USER_VRAM_BYTES;
/// Sessions open now, one bit per slot (`hwq::session_va` says which part of the VA space each slot has).
static SLOTS: AtomicU32 = AtomicU32::new(0);
/// The session (slot + 1) that owns the scanout, 0 if none.
static PRESENTER: AtomicU32 = AtomicU32::new(0);

/// What every session shares: the system-memory arena (one `ShmObject`; a system BO is a range of it, which any session maps through its own
/// device fd) and the VRAM heap, each a range allocator whose allocations have a count of holders (see `release_storage`). Created by the
/// first `open`.
struct Storage {
    arena: Arc<ShmObject>,
    system: RangeAlloc,
    vram: RangeAlloc,
    /// Holders per allocation, keyed by its first offset: the BO that allocated it, each exported descriptor, each imported BO.
    refs: alloc::collections::BTreeMap<(Heap, u64), u32>,
    /// A GPU is behind the sessions: a wedged one may still write into released pages (`gpu::uapi::leaking`).
    hw: bool,
}

static STORAGE: crate::sync::Mutex<Option<Storage>> = crate::sync::Mutex::new(None);

impl Storage {
    fn heap(&mut self, h: Heap) -> &mut RangeAlloc {
        match h {
            Heap::System => &mut self.system,
            Heap::Vram => &mut self.vram,
        }
    }
}

/// A timeline shared between sessions (`SYNC_EXPORT`): what `Device` keeps per timeline, moved here so any session can read and advance it.
struct SharedSync {
    value: u64,
    /// The highest value queued work will signal.
    pending: u64,
    /// Signals queued on it and not yet applied: (channel, fence sequence number on that channel, value).
    pend: Vec<(gpu::uapi::ChanId, u64, u64)>,
    /// Holders: the session timelines standing for it, and the descriptors made of it.
    refs: u32,
}

static SYNCS: crate::sync::Mutex<alloc::collections::BTreeMap<u64, SharedSync>> = crate::sync::Mutex::new(alloc::collections::BTreeMap::new());
static NEXT_SYNC: AtomicU64 = AtomicU64::new(1);

impl SharedSync {
    /// Apply the queued signals whose fences have completed (a dead channel's all are: its work is gone).
    fn resolve(&mut self) {
        let value = &mut self.value;
        self.pend.retain(|&(chan, seq, v)| {
            if gpu::uapi::fence_done(chan, seq) {
                *value = (*value).max(v);
                false
            } else {
                true
            }
        });
    }
}

/// Apply every completed signal of every shared timeline: a channel is about to go, and no entry may outlive it (its slot is reused).
fn resolve_all_syncs() {
    for t in SYNCS.lock().values_mut() {
        t.resolve();
    }
}

fn release_sync(id: u64) {
    let mut g = SYNCS.lock();
    if let Some(t) = g.get_mut(&id) {
        t.refs -= 1;
        if t.refs == 0 {
            g.remove(&id);
        }
    }
}

/// `/proc/kdebug` line: what sharing keeps alive. A leak of storage or of a shared timeline shows here as a count that does not return to 0
/// once every session and descriptor is closed (the tests read it).
pub fn render_kdebug() -> alloc::string::String {
    let allocs = STORAGE.lock().as_ref().map_or(0, |st| st.refs.len());
    alloc::format!(
        "gpu_share: sessions={} storage_allocs={} syncs={} presenter={}",
        SLOTS.load(Ordering::Relaxed).count_ones(),
        allocs,
        SYNCS.lock().len(),
        PRESENTER.load(Ordering::Relaxed)
    )
}

/// Bytes of the shared VRAM heap in use, by every session.
fn vram_used_all() -> u64 {
    STORAGE.lock().as_ref().map_or(0, |st| st.vram.used())
}

fn heap_of(b: Backing) -> (Heap, u64) {
    match b {
        Backing::System { arena_off } => (Heap::System, arena_off),
        Backing::Vram { vram_off } => (Heap::Vram, vram_off),
    }
}

/// One more holder of the storage that starts at `off`.
fn hold_storage(heap: Heap, off: u64) {
    if let Some(n) = STORAGE.lock().as_mut().and_then(|st| st.refs.get_mut(&(heap, off))) {
        *n += 1;
    }
}

/// One holder lets go of `[off, off + size)`. The last one gives the storage back: the pages of a system range are discarded (unless a
/// wedged GPU may still be writing to them) **before** the range can be handed out again, so a new owner never loses its data to a late
/// discard.
fn release_storage(heap: Heap, off: u64, size: u64) {
    let mut g = STORAGE.lock();
    let Some(st) = g.as_mut() else { return };
    let Some(n) = st.refs.get_mut(&(heap, off)) else {
        crate::serial_println!("[nvgpu] release of storage nobody holds ({:?} {:#x}): ignored", heap, off);
        return;
    };
    *n -= 1;
    if *n > 0 {
        return;
    }
    st.refs.remove(&(heap, off));
    if heap == Heap::System && !(st.hw && gpu::uapi::leaking()) {
        st.arena.discard(off, size);
    }
    st.heap(heap).free(off, size);
}

/// The arena, whose pages a released system BO gives back, plus either `SoftBackend`'s bookkeeping or the GPU (`gpu::uapi`).
struct KernelBackend {
    soft: SoftBackend,
    arena: Arc<ShmObject>,
    /// A GPU is behind this session: page tables, the channels and the fences are its.
    hw: bool,
    /// Which channel each context runs on (hardware only).
    kinds: alloc::collections::BTreeMap<u32, gpu::uapi::ChanId>,
}

impl Backend for KernelBackend {
    fn bo_create(&mut self, backing: Backing, size: u64) -> Result<(), Error> {
        self.soft.bo_create(backing, size)
    }

    fn bo_release(&mut self, backing: Backing, size: u64) {
        // the pages go back with the last holder of the storage (`heap_free`), not with this BO
        self.soft.bo_release(backing, size);
    }

    fn bind(&mut self, va: u64, size: u64, backing: Backing, bo_off: u64, pte_kind: u32) -> Result<(), Error> {
        if self.hw {
            gpu::uapi::bind(&self.arena, va, size, backing, bo_off, pte_kind)?;
        }
        self.soft.bind(va, size, backing, bo_off, pte_kind)
    }

    fn unbind(&mut self, va: u64, size: u64) {
        if self.hw {
            gpu::uapi::unbind(va, size);
        }
        self.soft.unbind(va, size);
    }

    fn ctx_create(&mut self, ctx: u32, engines: u32) -> Result<(), Error> {
        if self.hw {
            let id = gpu::uapi::ctx_create(engines)?;
            self.kinds.insert(ctx, id);
        }
        self.soft.ctx_create(ctx, engines)
    }

    fn ctx_destroy(&mut self, ctx: u32) {
        if let Some(id) = self.kinds.remove(&ctx) {
            // what the channel finished is applied to the shared timelines before the channel can go (and its slot be reused)
            resolve_all_syncs();
            gpu::uapi::ctx_destroy(id);
        }
        self.soft.ctx_destroy(ctx);
    }

    fn submit(&mut self, ctx: u32, pushes: &[Push]) -> Result<u64, Error> {
        if self.hw {
            // the model's fence numbers are the GPU's; the soft log is not kept
            let id = *self.kinds.get(&ctx).ok_or(Error::NoEnt)?;
            return gpu::uapi::submit(id, pushes);
        }
        self.soft.submit(ctx, pushes)
    }

    fn fence_done(&mut self, ctx: u32, seq: u64) -> bool {
        if self.hw {
            return self.kinds.get(&ctx).is_none_or(|&id| gpu::uapi::fence_done(id, seq));
        }
        self.soft.fence_done(ctx, seq)
    }

    fn quiesce(&mut self) -> bool {
        // only this session's channels: the others are still in use
        let ids: Vec<gpu::uapi::ChanId> = self.kinds.values().copied().collect();
        !self.hw || gpu::uapi::quiesce(&ids)
    }

    fn shared_heaps(&self) -> bool {
        true
    }

    fn heap_alloc(&mut self, heap: Heap, size: u64) -> Option<u64> {
        let mut g = STORAGE.lock();
        let st = g.as_mut()?;
        let off = st.heap(heap).alloc(size, 0x1000)?;
        st.refs.insert((heap, off), 1);
        Some(off)
    }

    fn heap_hold(&mut self, heap: Heap, off: u64) {
        hold_storage(heap, off);
    }

    fn heap_free(&mut self, heap: Heap, off: u64, size: u64) {
        release_storage(heap, off, size);
    }

    fn shared_sync_create(&mut self, value: u64, pending: u64) -> Option<u64> {
        let id = NEXT_SYNC.fetch_add(1, Ordering::Relaxed);
        SYNCS.lock().insert(id, SharedSync { value, pending, pend: Vec::new(), refs: 1 });
        Some(id)
    }

    fn shared_sync_hold(&mut self, id: u64) -> bool {
        match SYNCS.lock().get_mut(&id) {
            Some(t) => {
                t.refs += 1;
                true
            }
            None => false,
        }
    }

    fn shared_sync_release(&mut self, id: u64) {
        release_sync(id);
    }

    fn shared_sync_value(&mut self, id: u64) -> (u64, u64) {
        match SYNCS.lock().get_mut(&id) {
            Some(t) => {
                t.resolve();
                (t.value, t.pending)
            }
            None => (0, 0),
        }
    }

    fn shared_sync_signal(&mut self, id: u64, value: u64) {
        if let Some(t) = SYNCS.lock().get_mut(&id) {
            t.value = t.value.max(value);
            t.pending = t.pending.max(value);
        }
    }

    fn shared_sync_queue(&mut self, id: u64, ctx: u32, seq: u64, value: u64) {
        let chan = if self.hw { self.kinds.get(&ctx).copied() } else { None };
        if let Some(t) = SYNCS.lock().get_mut(&id) {
            t.pending = t.pending.max(value);
            match chan {
                Some(c) => t.pend.push((c, seq, value)),
                // the software device completes at once
                None => t.value = t.value.max(value),
            }
        }
    }
}

// ---- BOs as descriptors ---------------------------------------------------------------------------------------------------------

/// An exported timeline's claim on it: one holder, released when the last descriptor made from it is closed.
struct SyncShare {
    id: u64,
}

impl Drop for SyncShare {
    fn drop(&mut self) {
        release_sync(self.id);
    }
}

/// The descriptor `SYNC_EXPORT` returns (`dup`, `SCM_RIGHTS` and fork share the `SyncShare`).
struct SyncFile {
    share: Arc<SyncShare>,
}

impl FileHandle for SyncFile {
    fn read(&mut self, _buf: &mut [u8]) -> FileResult<usize> {
        Err(FileError::NotSupported)
    }

    fn write(&mut self, _buf: &[u8]) -> FileResult<usize> {
        Err(FileError::NotSupported)
    }

    fn stat(&self) -> Option<Stat> {
        Some(Stat::chardev(0))
    }

    fn name(&self) -> &str {
        "nvgpu-sync"
    }

    fn dup(&self) -> Option<Box<dyn FileHandle>> {
        Some(Box::new(SyncFile { share: self.share.clone() }))
    }

    fn device_ref(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        Some(self.share.clone() as Arc<dyn Any + Send + Sync>)
    }
}

/// An exported BO's claim on its storage: one holder, released when the last descriptor made from it is closed.
struct BoShare {
    backing: Backing,
    size: u64,
}

impl Drop for BoShare {
    fn drop(&mut self) {
        let (heap, off) = heap_of(self.backing);
        release_storage(heap, off, self.size);
    }
}

/// The descriptor `BO_EXPORT` returns. `dup` (and so `SCM_RIGHTS` and fork) shares the `BoShare`.
struct BoFile {
    share: Arc<BoShare>,
}

impl FileHandle for BoFile {
    fn read(&mut self, _buf: &mut [u8]) -> FileResult<usize> {
        Err(FileError::NotSupported)
    }

    fn write(&mut self, _buf: &[u8]) -> FileResult<usize> {
        Err(FileError::NotSupported)
    }

    fn stat(&self) -> Option<Stat> {
        Some(Stat::chardev(0))
    }

    fn name(&self) -> &str {
        "nvgpu-bo"
    }

    fn dup(&self) -> Option<Box<dyn FileHandle>> {
        Some(Box::new(BoFile { share: self.share.clone() }))
    }

    fn device_ref(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        Some(self.share.clone() as Arc<dyn Any + Send + Sync>)
    }
}

struct Session {
    /// Which slice of the VA space and which bit of `SLOTS` this session has.
    slot: usize,
    dev: crate::sync::Mutex<Device<KernelBackend>>,
    arena: Arc<ShmObject>,
    hw: bool,
    /// A buffer of this session is (or was) on the screen: closing the device flips the console's picture back.
    presented: AtomicBool,
    /// VRAM storage (heap offset, size) the display may be reading because of this session's `PRESENT`s, each held (`hold_storage`): the
    /// latest buffer shown (or about to be) and the one it replaced, which stays on screen until the latest one's flip takes effect. Freeing
    /// a BO handle, or the BO's last descriptor, cannot hand that VRAM to someone else while the display scans it out.
    shown: crate::sync::Mutex<[Option<(u64, u64)>; 2]>,
}

/// What the display scans out, if a driver flips buffers (`gpu=scanout`).
fn scanout_info() -> Option<uapi::ScanoutInfo> {
    let g = crate::framebuffer::FRAMEBUFFER.lock();
    let fb = g.as_ref().filter(|fb| fb.page_flipping() && fb.bytes_per_pixel() == 4)?;
    let (w, h) = fb.dimensions();
    let pitch = (fb.stride() * 4) as u32;
    Some(uapi::ScanoutInfo { width: w as u32, height: h as u32, pitch_b: pitch, format: uapi::SCANOUT_XRGB8888, size_b: pitch as u64 * h as u64, flags: 0 })
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.presented.load(Ordering::SeqCst) {
            // the console's picture back (the flip of an earlier present may still be pending: a vblank or two)
            let t0 = crate::cpu::tsc::read();
            loop {
                let r = crate::framebuffer::FRAMEBUFFER.lock().as_mut().map(|fb| fb.restore_front());
                match r {
                    Some(Err(crate::framebuffer::PresentError::Busy)) if crate::cpu::tsc::read().wrapping_sub(t0) < crate::cpu::tsc::freq_hz() / 5 => {
                        crate::memory::tlb::service_pending();
                        core::hint::spin_loop();
                    }
                    _ => break,
                }
            }
        }
        // the display is on the console's picture again: what it was scanning out can go (before `teardown`, which frees the BOs' own holds)
        let held = core::mem::take(&mut *self.shown.lock());
        for (off, size) in held.into_iter().flatten() {
            release_storage(Heap::Vram, off, size);
        }
        // a GPU that did not go idle may still be writing into the BOs' pages: `release_storage` keeps them (`gpu::uapi::leaking`)
        let _quiet = self.dev.lock().teardown();
        if self.presented.load(Ordering::SeqCst) {
            let _ = PRESENTER.compare_exchange(self.slot as u32 + 1, 0, Ordering::SeqCst, Ordering::SeqCst);
        }
        // the slot is free (its VA range is empty: teardown unbound and freed everything) only now
        SLOTS.fetch_and(!(1 << self.slot), Ordering::SeqCst);
    }
}

pub struct NvgpuHandle {
    session: Arc<Session>,
}

/// Take a free session slot, lowest first.
fn take_slot() -> Option<usize> {
    loop {
        let cur = SLOTS.load(Ordering::SeqCst);
        let slot = (0..nvgpu::hwq::SESSIONS).find(|&k| cur & (1 << k) == 0)?;
        if SLOTS.compare_exchange(cur, cur | 1 << slot, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            return Some(slot);
        }
    }
}

pub fn open() -> Result<Box<dyn FileHandle>, Errno> {
    let slot = take_slot().ok_or(Errno::EBUSY)?;
    let give_back = |e: Errno| {
        SLOTS.fetch_and(!(1 << slot), Ordering::SeqCst);
        Err(e)
    };
    // a GPU that came up and then died is not replaced by a software device behind the caller's back
    let hw = gpu::uapi::installed();
    if hw && gpu::uapi::dead() {
        return give_back(Errno::EIO);
    }
    // the arena and the VRAM heap exist from the first session on (every session is hardware or software alike)
    let arena = {
        let mut g = STORAGE.lock();
        if g.is_none() {
            let arena = Arc::new(ShmObject::with_limit(ARENA_BYTES));
            if arena.set_size(ARENA_BYTES).is_err() {
                drop(g);
                return give_back(Errno::ENOMEM);
            }
            *g = Some(Storage {
                arena,
                system: RangeAlloc::new(0, ARENA_BYTES),
                vram: RangeAlloc::new(0, if hw { HW_VRAM_BYTES } else { SOFT_VRAM_BYTES }),
                refs: Default::default(),
                hw,
            });
        }
        g.as_ref().map(|st| st.arena.clone()).expect("storage was just created")
    };
    let (va_start, va_end) = nvgpu::hwq::session_va(slot);
    // the model's own heaps are unused (the backend's are shared): their sizes are 0
    let layout = Layout { arena_bytes: 0, vram_bytes: 0, va_start, va_end };
    let backend = KernelBackend { soft: SoftBackend::default(), arena: arena.clone(), hw, kinds: Default::default() };
    let dev = Device::new(backend, layout);
    Ok(Box::new(NvgpuHandle { session: Arc::new(Session { slot, dev: crate::sync::Mutex::new(dev), arena, hw, presented: AtomicBool::new(false), shown: crate::sync::Mutex::new([None; 2]) }) }))
}

// ---- user memory ----------------------------------------------------------------------------------------------------------

/// Read a `T` from user memory. Only the address range is checked, as for every other ioctl argument here; a fault on an unmapped
/// page is the page-fault handler's to resolve or report.
fn read_user<T: Copy>(ptr: u64) -> Result<T, i64> {
    validate_user_buffer(ptr, core::mem::size_of::<T>())?;
    // SAFETY: range checked to lie in user space above; `read_unaligned` because nothing aligns a user pointer.
    Ok(unsafe { core::ptr::read_unaligned(ptr as *const T) })
}

fn write_user<T: Copy>(ptr: u64, v: T) -> Result<(), i64> {
    validate_user_buffer(ptr, core::mem::size_of::<T>())?;
    // SAFETY: as above.
    unsafe { core::ptr::write_unaligned(ptr as *mut T, v) };
    Ok(())
}

/// Read `count` `T`s (at most `max`) from user memory into a `Vec`.
fn read_user_array<T: Copy>(ptr: u64, count: u32, max: usize) -> Result<Vec<T>, i64> {
    let count = count as usize;
    if count > max {
        return Err(errno::EINVAL);
    }
    let mut v = Vec::new();
    v.try_reserve_exact(count).map_err(|_| errno::ENOMEM)?;
    if count > 0 {
        validate_user_buffer(ptr, count * core::mem::size_of::<T>())?;
    }
    for i in 0..count {
        v.push(read_user::<T>(ptr + (i * core::mem::size_of::<T>()) as u64)?);
    }
    Ok(v)
}

fn errno_of(e: Error) -> i64 {
    match e {
        Error::Inval => errno::EINVAL,
        Error::NoEnt => errno::ENOENT,
        Error::NoMem => errno::ENOMEM,
        Error::NoSpc => errno::ENOSPC,
        Error::Busy => errno::EBUSY,
        Error::Exist => errno::EEXIST,
        Error::Fault => errno::EFAULT,
        Error::Again => errno::EAGAIN,
        Error::Io => errno::EIO,
    }
}

fn info(va: (u64, u64), vram_used: u64, hw: bool) -> uapi::Info {
    let mut name = [0u8; 64];
    let n: &[u8] = if hw { b"NVIDIA GeForce RTX 3050 (constanos)" } else { b"constanos software GPU (GA106 model)" };
    name[..n.len()].copy_from_slice(n);
    // the real floorsweeping when the GPU told (RM's GPC/TPC masks), else the model's 3 GPCs and 10 TPCs
    let topo = if hw { gpu::uapi::topology().unwrap_or((3, 10)) } else { (3, 10) };
    let mut chip = [0u8; 16];
    chip[..5].copy_from_slice(b"GA106");
    uapi::Info {
        abi_version: uapi::ABI_VERSION,
        flags: if hw { 0 } else { uapi::INFO_SOFTWARE },
        device_id: 0x2504,
        chipset: 0x196,
        sm: 86,
        gpc_count: topo.0 as u8,
        tpc_count: topo.1 as u16,
        mp_per_tpc: 2,
        max_warps_per_mp: 48,
        max_blocks_per_mp: 16,
        _pad0: 0,
        cls_copy: 0xc7b5,
        cls_eng2d: 0x902d,
        cls_eng3d: 0xc797,
        // What nouveau's winsys reports on Turing and later: KEPLER_INLINE_TO_MEMORY_B. A class at or below FERMI_MEMORY_TO_MEMORY_FORMAT_A
        // (0x9039, and 0 is below it) makes NVK push Fermi M2MF methods at a subchannel the queue does not have.
        cls_m2mf: 0xa140,
        cls_compute: 0xc7c0,
        cls_gpfifo: 0xc56f,
        cls_vdec: 0,
        max_smem_per_wg_kb: 99,
        vram_size_b: if hw { HW_VRAM_BYTES } else { SOFT_VRAM_BYTES },
        vram_used_b: vram_used,
        bar_size_b: 0,
        va_start: va.0,
        va_end: va.1,
        device_name: name,
        chipset_name: chip,
    }
}

// ---- the ioctls -----------------------------------------------------------------------------------------------------------

impl NvgpuHandle {
    /// A `PRESENT` that did not happen gives the display back if this session had only just claimed it.
    fn release_display_claim(&self) {
        if !self.session.presented.load(Ordering::SeqCst) {
            let _ = PRESENTER.compare_exchange(self.session.slot as u32 + 1, 0, Ordering::SeqCst, Ordering::SeqCst);
        }
    }

    /// `BO_EXPORT`: a descriptor for the BO, which holds its storage.
    fn bo_export(&self, arg: u64) -> Result<Box<dyn FileHandle>, i64> {
        let r: uapi::BoExport = read_user(arg)?;
        if r.flags != 0 {
            return Err(errno::EINVAL);
        }
        let dev = self.session.dev.lock();
        let (backing, size) = dev.bo_backing(r.handle).map_err(errno_of)?;
        let (heap, off) = heap_of(backing);
        hold_storage(heap, off);
        Ok(Box::new(BoFile { share: Arc::new(BoShare { backing, size }) }))
    }

    /// `SYNC_EXPORT`: a descriptor for the timeline (made shareable on the way), which holds it.
    fn sync_export(&self, arg: u64) -> Result<Box<dyn FileHandle>, i64> {
        let r: uapi::SyncExport = read_user(arg)?;
        if r.flags != 0 {
            return Err(errno::EINVAL);
        }
        let mut dev = self.session.dev.lock();
        let id = dev.sync_share(r.handle).map_err(errno_of)?;
        Ok(Box::new(SyncFile { share: Arc::new(SyncShare { id }) }))
    }

    /// `SYNC_IMPORT`: the timeline behind the descriptor `sys_ioctl` looked up (`peer`), as a timeline of this session.
    fn sync_import(&self, arg: u64, peer: Option<Arc<dyn Any + Send + Sync>>) -> Result<i64, i64> {
        let mut r: uapi::SyncImport = read_user(arg)?;
        if r.flags != 0 {
            return Err(errno::EINVAL);
        }
        if r.fd < 0 {
            return Err(errno::EBADF);
        }
        // a BO descriptor, or a file, has no `SyncShare` behind it
        let share = peer.ok_or(errno::EINVAL)?.downcast::<SyncShare>().map_err(|_| errno::EINVAL)?;
        let mut dev = self.session.dev.lock();
        r.handle = dev.sync_import(share.id).map_err(errno_of)?;
        if let Err(e) = write_user(arg, r) {
            let _ = dev.sync_destroy(r.handle);
            return Err(e);
        }
        Ok(0)
    }

    /// `BO_IMPORT`: the BO behind the descriptor `sys_ioctl` looked up (`peer`), as a BO of this session.
    fn bo_import(&self, arg: u64, peer: Option<Arc<dyn Any + Send + Sync>>) -> Result<i64, i64> {
        let mut r: uapi::BoImport = read_user(arg)?;
        if r.flags != 0 {
            return Err(errno::EINVAL);
        }
        if r.fd < 0 {
            return Err(errno::EBADF);
        }
        // a descriptor that is not one of ours (a file, a socket) has no `BoShare` behind it
        let share = peer.ok_or(errno::EINVAL)?.downcast::<BoShare>().map_err(|_| errno::EINVAL)?;
        let mut dev = self.session.dev.lock();
        let (handle, mmap, size) = dev.bo_import(share.backing, share.size).map_err(errno_of)?;
        r.handle = handle;
        r.mmap_offset = mmap;
        r.size_out = size;
        if let Err(e) = write_user(arg, r) {
            let _ = dev.bo_free(handle);
            return Err(e);
        }
        Ok(0)
    }

    fn ioctl_inner(&self, request: u32, arg: u64) -> Result<i64, i64> {
        let mut dev = self.session.dev.lock();
        match request {
            uapi::IOC_INFO => {
                write_user(arg, info(nvgpu::hwq::session_va(self.session.slot), vram_used_all(), self.session.hw))?;
            }
            uapi::IOC_BO_CREATE => {
                let mut r: uapi::BoCreate = read_user(arg)?;
                let (handle, mmap, size) = dev.bo_create(r.size, r.flags).map_err(errno_of)?;
                r.handle = handle;
                r.mmap_offset = mmap;
                r.size_out = size;
                if let Err(e) = write_user(arg, r) {
                    let _ = dev.bo_free(handle);
                    return Err(e);
                }
            }
            uapi::IOC_BO_FREE => {
                let r: uapi::BoFree = read_user(arg)?;
                dev.bo_free(r.handle).map_err(errno_of)?;
            }
            uapi::IOC_VA_ALLOC => {
                let mut r: uapi::VaAlloc = read_user(arg)?;
                if r.flags != 0 {
                    return Err(errno::EINVAL);
                }
                r.va = dev.va_alloc(r.size, r.align).map_err(errno_of)?;
                if let Err(e) = write_user(arg, r) {
                    let _ = dev.va_free(r.va, r.size);
                    return Err(e);
                }
            }
            uapi::IOC_VA_FREE => {
                let r: uapi::VaFree = read_user(arg)?;
                dev.va_free(r.va, r.size).map_err(errno_of)?;
            }
            uapi::IOC_VA_BIND => {
                let r: uapi::VaBind = read_user(arg)?;
                dev.va_bind(&r).map_err(errno_of)?;
            }
            uapi::IOC_VA_UNBIND => {
                let r: uapi::VaUnbind = read_user(arg)?;
                dev.va_unbind(r.va, r.size).map_err(errno_of)?;
            }
            uapi::IOC_CTX_CREATE => {
                let mut r: uapi::CtxCreate = read_user(arg)?;
                r.ctx = dev.ctx_create(r.engines).map_err(errno_of)?;
                if let Err(e) = write_user(arg, r) {
                    let _ = dev.ctx_destroy(r.ctx);
                    return Err(e);
                }
            }
            uapi::IOC_CTX_DESTROY => {
                let r: uapi::CtxDestroy = read_user(arg)?;
                dev.ctx_destroy(r.ctx).map_err(errno_of)?;
            }
            uapi::IOC_EXEC => {
                let r: uapi::Exec = read_user(arg)?;
                let pushes = read_user_array::<Push>(r.pushes, r.push_count, nvgpu::devmodel::MAX_PUSHES)?;
                let waits = read_user_array::<SyncRef>(r.waits, r.wait_count, nvgpu::devmodel::MAX_SYNC_REFS)?;
                let signals = read_user_array::<SyncRef>(r.signals, r.sig_count, nvgpu::devmodel::MAX_SYNC_REFS)?;
                // All waits satisfied, or nothing happens and the caller retries.
                if dev.waits_ready(&waits, false, false).map_err(errno_of)?.is_none() {
                    return Err(errno::EAGAIN);
                }
                dev.exec(r.ctx, &pushes, &waits, &signals).map_err(errno_of)?;
            }
            uapi::IOC_SYNC_CREATE => {
                let mut r: uapi::SyncCreate = read_user(arg)?;
                r.handle = dev.sync_create(r.initial).map_err(errno_of)?;
                if let Err(e) = write_user(arg, r) {
                    let _ = dev.sync_destroy(r.handle);
                    return Err(e);
                }
            }
            uapi::IOC_SYNC_DESTROY => {
                let r: uapi::SyncDestroy = read_user(arg)?;
                dev.sync_destroy(r.handle).map_err(errno_of)?;
            }
            uapi::IOC_SYNC_SIGNAL => {
                let r: uapi::SyncSignal = read_user(arg)?;
                dev.sync_signal(r.handle, r.value).map_err(errno_of)?;
            }
            uapi::IOC_SYNC_WAIT => {
                let mut r: uapi::SyncWait = read_user(arg)?;
                if r.flags & !(uapi::WAIT_ANY | uapi::WAIT_PENDING) != 0 || r.count == 0 {
                    return Err(errno::EINVAL);
                }
                let refs = read_user_array::<SyncRef>(r.refs, r.count, nvgpu::devmodel::MAX_SYNC_REFS)?;
                let any = r.flags & uapi::WAIT_ANY != 0;
                let pending = r.flags & uapi::WAIT_PENDING != 0;
                match dev.waits_ready(&refs, any, pending).map_err(errno_of)? {
                    Some(i) => {
                        r.first_ready = i as u32;
                        write_user(arg, r)?;
                    }
                    None => return Err(errno::EAGAIN),
                }
            }
            uapi::IOC_SYNC_QUERY => {
                let mut r: uapi::SyncQuery = read_user(arg)?;
                (r.value, r.pending) = dev.sync_query(r.handle).map_err(errno_of)?;
                write_user(arg, r)?;
            }
            uapi::IOC_SCANOUT_INFO => {
                if !self.session.hw {
                    return Err(errno::ENODEV);
                }
                write_user(arg, scanout_info().ok_or(errno::ENODEV)?)?;
            }
            uapi::IOC_PRESENT => {
                let r: uapi::Present = read_user(arg)?;
                if !self.session.hw {
                    return Err(errno::ENODEV);
                }
                if r.flags != 0 {
                    return Err(errno::EINVAL);
                }
                let si = scanout_info().ok_or(errno::ENODEV)?;
                let (vram_off, size) = dev.vram_bo(r.handle).ok_or(errno::EINVAL)?;
                let me = self.session.slot as u32 + 1;
                match PRESENTER.compare_exchange(0, me, Ordering::SeqCst, Ordering::SeqCst) {
                    Ok(_) => {}
                    Err(owner) if owner == me => {}
                    Err(_) => return Err(errno::EBUSY),
                }
                if r.offset % 256 != 0 || r.offset.checked_add(si.size_b).is_none_or(|e| e > size) {
                    return Err(errno::EINVAL);
                }
                let pa = nvgpu::hwq::user_vram_pa(vram_off) + r.offset;
                let res = crate::framebuffer::FRAMEBUFFER.lock().as_mut().map(|fb| fb.present_external(pa));
                match res {
                    Some(Ok(())) => {
                        self.session.presented.store(true, Ordering::SeqCst);
                        // The display reads this buffer from the next vblank on, and the one before it until then (this present was accepted, so
                        // the flip before it had taken effect: the one before *that* is off the screen). Hold both; let the older go.
                        hold_storage(Heap::Vram, vram_off);
                        let old = {
                            let mut shown = self.session.shown.lock();
                            let old = shown[1];
                            shown[1] = shown[0];
                            shown[0] = Some((vram_off, size));
                            old
                        };
                        if let Some((off, sz)) = old {
                            release_storage(Heap::Vram, off, sz);
                        }
                    }
                    Some(Err(crate::framebuffer::PresentError::Busy)) => {
                        self.release_display_claim();
                        return Err(errno::EBUSY);
                    }
                    Some(Err(crate::framebuffer::PresentError::Failed(_))) => {
                        self.release_display_claim();
                        return Err(errno::EIO);
                    }
                    None => {
                        self.release_display_claim();
                        return Err(errno::ENODEV);
                    }
                }
            }
            uapi::IOC_FLIP_STATE => {
                if !self.session.hw {
                    return Err(errno::ENODEV);
                }
                let mut r: uapi::FlipState = read_user(arg)?;
                let settled = crate::framebuffer::FRAMEBUFFER.lock().as_mut().map(|fb| fb.flip_settled()).ok_or(errno::ENODEV)?;
                r.pending = !settled as u32;
                r.vblank_seq = crate::gpu::vblank::seq();
                write_user(arg, r)?;
            }
            uapi::IOC_TIMESTAMP => {
                if self.session.hw {
                    let ns = gpu::uapi::timestamp_ns().ok_or(errno::EIO)?;
                    write_user(arg, uapi::Timestamp { ns })?;
                    return Ok(0);
                }
                let hz = crate::cpu::tsc::freq_hz().max(1);
                let ticks = crate::cpu::tsc::read() as u128;
                write_user(arg, uapi::Timestamp { ns: (ticks * 1_000_000_000 / hz as u128) as u64 })?;
            }
            _ => return Err(errno::ENOTTY),
        }
        Ok(0)
    }
}

impl FileHandle for NvgpuHandle {
    fn read(&mut self, _buf: &mut [u8]) -> FileResult<usize> {
        Err(FileError::NotSupported)
    }

    fn write(&mut self, _buf: &[u8]) -> FileResult<usize> {
        Err(FileError::NotSupported)
    }

    fn stat(&self) -> Option<Stat> {
        Some(Stat::chardev(0))
    }

    fn name(&self) -> &str {
        "nvgpu"
    }

    fn dup(&self) -> Option<Box<dyn FileHandle>> {
        Some(Box::new(NvgpuHandle { session: self.session.clone() }))
    }

    fn ioctl(&mut self, request: u64, arg: u64) -> Option<i64> {
        // The command is 32 bits, as in Linux (`unsigned int cmd`): musl declares `ioctl(int, int, ...)`, so a request with the top
        // bit set arrives sign-extended (0xffffffffc0a04e01). Anything that is not ours belongs to the generic ioctls.
        let request = request as u32;
        if (request >> 8) & 0xff != u32::from(b'N') {
            return None;
        }
        Some(match self.ioctl_inner(request, arg) {
            Ok(v) => v,
            Err(e) => e,
        })
    }

    fn ioctl_fd_arg(&self, request: u64, arg: u64) -> Option<usize> {
        match request as u32 {
            uapi::IOC_BO_IMPORT => usize::try_from(read_user::<uapi::BoImport>(arg).ok()?.fd).ok(),
            uapi::IOC_SYNC_IMPORT => usize::try_from(read_user::<uapi::SyncImport>(arg).ok()?.fd).ok(),
            _ => None,
        }
    }

    fn ioctl_ex(&mut self, request: u64, arg: u64, peer: Option<Arc<dyn Any + Send + Sync>>) -> Option<IoctlOut> {
        match request as u32 {
            uapi::IOC_BO_EXPORT => Some(match self.bo_export(arg) {
                Ok(file) => IoctlOut::NewFile(file),
                Err(e) => IoctlOut::Value(e),
            }),
            uapi::IOC_BO_IMPORT => Some(IoctlOut::Value(self.bo_import(arg, peer).unwrap_or_else(|e| e))),
            uapi::IOC_SYNC_EXPORT => Some(match self.sync_export(arg) {
                Ok(file) => IoctlOut::NewFile(file),
                Err(e) => IoctlOut::Value(e),
            }),
            uapi::IOC_SYNC_IMPORT => Some(IoctlOut::Value(self.sync_import(arg, peer).unwrap_or_else(|e| e))),
            _ => self.ioctl(request, arg).map(IoctlOut::Value),
        }
    }

    fn shm_object(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        Some(self.session.arena.clone() as Arc<dyn Any + Send + Sync>)
    }
}
