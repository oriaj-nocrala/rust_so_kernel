// kernel/src/drivers/dev_nvgpu.rs
//
// `/dev/nvgpu`: the GPU for user space, in the terms Mesa's NVK asks for (`nvkmd`). Plan: `docs/gpu/g4-nvkmd-plan.md`; the
// interface is `nvgpu/uapi/nvgpu.h` (Rust mirror `nvgpu::uapi`); every rule about what a caller may ask lives in the host-tested
// `nvgpu::devmodel`. This file is the adapter: the open/close discipline, the copies to and from user memory, the arena of
// system memory and the mapping from `devmodel` errors to errno.
//
// - **Exclusive.** One open file description at a time (a second `open` is `EBUSY`); `dup`/`fork` share it. The session ends
//   with the last reference, so a holder killed by a signal hands the device back: everything is unbound, released and forgotten.
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
use core::sync::atomic::{AtomicBool, Ordering};

use nvgpu::devmodel::{Backend, Backing, Device, Error, Layout, SoftBackend};
use nvgpu::uapi::{self, Push, SyncRef};

use crate::fs::types::{Errno, Stat};
use crate::gpu;
use crate::memory::shm::ShmObject;
use crate::process::file::{FileError, FileHandle, FileResult};
use crate::process::syscall::{errno, validate_user_buffer};

/// Bytes of system memory one session may hold in BOs.
const ARENA_BYTES: u64 = 1 << 30;
/// The software device's pretend VRAM.
const SOFT_VRAM_BYTES: u64 = 6 << 30;
/// What the hardware device offers: the user heap of `nvgpu::hwq` (VRAM the kernel does not use).
const HW_VRAM_BYTES: u64 = nvgpu::hwq::USER_VRAM_BYTES;
/// GPU virtual addresses user space may allocate: [64 GiB, 256 GiB). Below 2^40 because some methods take 40-bit addresses
/// (`SET_VERTEX_STREAM_SUBSTITUTE_A` keeps the upper part in 8 bits; nouveau's own heap ends at 2^38), and above the fixed
/// mappings the boot-time GPU code makes (4 GiB, 8 GiB..).
const VA_START: u64 = 1 << 36;
const VA_END: u64 = 1 << 38;

static HELD: AtomicBool = AtomicBool::new(false);

/// The arena, whose pages a released system BO gives back, plus either `SoftBackend`'s bookkeeping or the GPU (`gpu::uapi`).
struct KernelBackend {
    soft: SoftBackend,
    arena: Arc<ShmObject>,
    /// A GPU is behind this session: page tables, the channel and the fences are its.
    hw: bool,
}

impl Backend for KernelBackend {
    fn bo_create(&mut self, backing: Backing, size: u64) -> Result<(), Error> {
        self.soft.bo_create(backing, size)
    }

    fn bo_release(&mut self, backing: Backing, size: u64) {
        if let Backing::System { arena_off } = backing {
            // a wedged GPU may still write to these pages: they stay allocated
            if !(self.hw && gpu::uapi::leaking()) {
                self.arena.discard(arena_off, size);
            }
        }
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
            gpu::uapi::ctx_create(engines)?;
        }
        self.soft.ctx_create(ctx, engines)
    }

    fn ctx_destroy(&mut self, ctx: u32) {
        self.soft.ctx_destroy(ctx);
    }

    fn submit(&mut self, ctx: u32, pushes: &[Push]) -> Result<u64, Error> {
        if self.hw {
            // the model's fence numbers are the GPU's; the soft log is not kept
            return gpu::uapi::submit(pushes);
        }
        self.soft.submit(ctx, pushes)
    }

    fn fence_done(&mut self, ctx: u32, seq: u64) -> bool {
        if self.hw {
            return gpu::uapi::fence_done(seq);
        }
        self.soft.fence_done(ctx, seq)
    }

    fn quiesce(&mut self) -> bool {
        !self.hw || gpu::uapi::quiesce()
    }
}

struct Session {
    dev: crate::sync::Mutex<Device<KernelBackend>>,
    arena: Arc<ShmObject>,
    hw: bool,
}

impl Drop for Session {
    fn drop(&mut self) {
        let quiet = self.dev.lock().teardown();
        if !quiet {
            // the GPU did not go idle: it may still be writing into the arena's pages, so they are never freed
            core::mem::forget(self.arena.clone());
        }
        HELD.store(false, Ordering::SeqCst);
    }
}

pub struct NvgpuHandle {
    session: Arc<Session>,
}

pub fn open() -> Result<Box<dyn FileHandle>, Errno> {
    if HELD.swap(true, Ordering::SeqCst) {
        return Err(Errno::EBUSY);
    }
    // a GPU that came up and then died is not replaced by a software device behind the caller's back
    let hw = gpu::uapi::installed();
    if hw && gpu::uapi::dead() {
        HELD.store(false, Ordering::SeqCst);
        return Err(Errno::EIO);
    }
    let arena = Arc::new(ShmObject::with_limit(ARENA_BYTES));
    if arena.set_size(ARENA_BYTES).is_err() {
        HELD.store(false, Ordering::SeqCst);
        return Err(Errno::ENOMEM);
    }
    let layout = Layout { arena_bytes: ARENA_BYTES, vram_bytes: if hw { HW_VRAM_BYTES } else { SOFT_VRAM_BYTES }, va_start: VA_START, va_end: VA_END };
    let backend = KernelBackend { soft: SoftBackend::default(), arena: arena.clone(), hw };
    let dev = Device::new(backend, layout);
    Ok(Box::new(NvgpuHandle { session: Arc::new(Session { dev: crate::sync::Mutex::new(dev), arena, hw }) }))
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

fn info(vram_used: u64, hw: bool) -> uapi::Info {
    let mut name = [0u8; 64];
    let n: &[u8] = if hw { b"NVIDIA GeForce RTX 3050 (constanos)" } else { b"constanos software GPU (GA106 model)" };
    name[..n.len()].copy_from_slice(n);
    let mut chip = [0u8; 16];
    chip[..5].copy_from_slice(b"GA106");
    uapi::Info {
        abi_version: uapi::ABI_VERSION,
        flags: if hw { 0 } else { uapi::INFO_SOFTWARE },
        device_id: 0x2504,
        chipset: 0x196,
        sm: 86,
        gpc_count: 3,
        tpc_count: 10,
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
        va_start: VA_START,
        va_end: VA_END,
        device_name: name,
        chipset_name: chip,
    }
}

// ---- the ioctls -----------------------------------------------------------------------------------------------------------

impl NvgpuHandle {
    fn ioctl_inner(&self, request: u32, arg: u64) -> Result<i64, i64> {
        let mut dev = self.session.dev.lock();
        match request {
            uapi::IOC_INFO => {
                write_user(arg, info(dev.vram_used(), self.session.hw))?;
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

    fn shm_object(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        Some(self.session.arena.clone() as Arc<dyn Any + Send + Sync>)
    }
}
