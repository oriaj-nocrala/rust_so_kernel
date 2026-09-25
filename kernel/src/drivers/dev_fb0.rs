// kernel/src/drivers/dev_fb0.rs
//
// `/dev/fb0`: the screen, for a compositor. Phase 2.1 of
// `docs/gui/gui-plan.md`.
//
// `/dev/fb` is the text console — every process's stdout — and stays that.
// `/dev/fb0` is the other way to the screen, with three properties:
//
// - **Exclusive.** One open file description at a time; a second `open` is
//   `EBUSY`. `dup`/`fork` share it.
// - **Open is graphics mode** (Linux's `KD_GRAPHICS`, see
//   `framebuffer_console::enter_graphics_mode`): the console stops drawing
//   while it is held. The mode ends in `Drop` of the last reference, the
//   way `EVIOCGRAB` does, so a holder killed by `SIGKILL` hands the screen
//   back without anyone having to remember to.
// - **`mmap(MAP_SHARED)` maps the framebuffer's RAM copy** (the shadow of
//   `docs/fb/wc-shadow-plan.md`), not VRAM: the holder draws into RAM and
//   asks for the copy to VRAM with `FBIO_FLUSH`, so `Framebuffer::flush`
//   stays the only code that touches VRAM. The shadow is wrapped once in a
//   pinned `ShmObject` (see `ShmObject::pinned`), which makes the mapping
//   an ordinary shared-memory mapping: same fault path, same `fork`, same
//   teardown as a memfd's.
//
// Without a shadow (its allocation failed at boot and the console runs in
// direct mode) there is nothing to map: `open` is `ENODEV`.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, Ordering};
use x86_64::structures::paging::PhysFrame;
use x86_64::{PhysAddr, VirtAddr};

use crate::fs::types::{Errno, Stat};
use crate::framebuffer::FRAMEBUFFER;
use crate::memory::shm::ShmObject;
use crate::process::file::{FileError, FileHandle, FileResult};

/// `ioctl(FBIO_GET_INFO, struct fb0_info *)`.
pub const FBIO_GET_INFO: u64 = 0x4642_0010;
/// `ioctl(FBIO_FLUSH, struct fb0_flush *)`.
pub const FBIO_FLUSH: u64 = 0x4642_0011;

/// Rectangles one `FBIO_FLUSH` carries at most.
const MAX_FLUSH_RECTS: usize = 16;

#[repr(C)]
#[derive(Clone, Copy)]
struct Fb0Info {
    width: u32,
    height: u32,
    /// Pixels per scanline as stored; not necessarily `width`.
    stride: u32,
    bytes_per_pixel: u32,
    /// Byte offset of pixel (0,0) from the start of an `mmap` at offset 0.
    offset: u64,
    /// Bytes to `mmap` to cover every pixel.
    map_len: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Fb0Rect {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

#[repr(C)]
struct Fb0Flush {
    count: u32,
    _pad: u32,
    rects: [Fb0Rect; MAX_FLUSH_RECTS],
}

/// Someone holds `/dev/fb0`.
static HELD: AtomicBool = AtomicBool::new(false);

/// The shadow as a pinned `ShmObject`, and the byte offset of pixel (0,0)
/// in it. Built on first open; `None` in direct mode, or if the shadow
/// turns out not to be where its frames can be named (checked, see
/// `build_shadow_object`).
static SHADOW: spin::Once<Option<(Arc<ShmObject>, usize)>> = spin::Once::new();

/// Name every page of the shadow by its frame. The shadow is one large
/// allocation, which the slab takes straight from the Buddy allocator and
/// hands out through the physical-memory window — so its frames are
/// `virt - window` and contiguous. That is checked page by page against
/// the live page table rather than assumed: a frame named wrongly here
/// would map someone else's memory into user space.
fn build_shadow_object() -> Option<(Arc<ShmObject>, usize)> {
    let (first, pages, offset) = FRAMEBUFFER.lock().as_ref()?.shadow_pages()?;
    let window = crate::memory::physical_memory_offset().as_u64();
    let mut frames = Vec::new();
    frames.try_reserve_exact(pages).ok()?;
    for i in 0..pages as u64 {
        let virt = first + i * 4096;
        let phys = crate::memory::memtype::leaf_for(VirtAddr::new(virt)).map(|l| l.phys);
        if virt < window || phys != Some(virt - window) {
            crate::serial_println!("[fb0] shadow page {:#x} is not window-mapped ({:?}); /dev/fb0 disabled", virt, phys);
            return None;
        }
        frames.push(PhysFrame::containing_address(PhysAddr::new(virt - window)));
    }
    // SAFETY: the frames are the shadow's, a boot-time allocation that is
    // never freed (`framebuffer::attach_shadow`), holding nothing but
    // pixels and the zeroed slack around them. Nothing tracked their COW
    // refcounts: they were never user pages.
    let obj = unsafe { ShmObject::pinned(frames) };
    Some((Arc::new(obj), offset))
}

fn shadow() -> Option<&'static (Arc<ShmObject>, usize)> {
    SHADOW.call_once(build_shadow_object).as_ref()
}

/// One open of `/dev/fb0`, shared by its `dup`s. Graphics mode lasts as
/// long as this does.
struct Session;

impl Drop for Session {
    fn drop(&mut self) {
        crate::drivers::framebuffer_console::leave_graphics_mode();
        HELD.store(false, Ordering::SeqCst);
    }
}

pub struct Fb0Handle {
    session: Arc<Session>,
}

pub fn open() -> Result<Box<dyn FileHandle>, Errno> {
    if shadow().is_none() {
        return Err(Errno::ENODEV);
    }
    if HELD.swap(true, Ordering::SeqCst) {
        return Err(Errno::EBUSY);
    }
    crate::drivers::framebuffer_console::enter_graphics_mode();
    Ok(Box::new(Fb0Handle { session: Arc::new(Session) }))
}

/// Read a `T` from user memory at `ptr`. Only the address range is
/// checked, as for every other ioctl argument here; a fault on an unmapped
/// page is the page-fault handler's to resolve or report.
fn read_user<T: Copy>(ptr: u64) -> Result<T, i64> {
    crate::process::syscall::validate_user_buffer(ptr, core::mem::size_of::<T>())?;
    // SAFETY: range checked to lie in user space above; `read_unaligned`
    // because nothing aligns a user pointer.
    Ok(unsafe { core::ptr::read_unaligned(ptr as *const T) })
}

fn get_info(arg: u64) -> i64 {
    use crate::process::syscall::errno;
    let Some((_, offset)) = shadow() else { return errno::ENODEV };
    let info = {
        let guard = FRAMEBUFFER.lock();
        let Some(fb) = guard.as_ref() else { return errno::ENODEV };
        let (width, height) = fb.dimensions();
        Fb0Info {
            width: width as u32,
            height: height as u32,
            stride: fb.stride() as u32,
            bytes_per_pixel: fb.bytes_per_pixel() as u32,
            offset: *offset as u64,
            map_len: (*offset + fb.byte_len()) as u64,
        }
    };
    if let Err(e) = crate::process::syscall::validate_user_buffer(arg, core::mem::size_of::<Fb0Info>()) {
        return e;
    }
    // SAFETY: range checked above; no lock is held across the user write.
    unsafe { core::ptr::write_unaligned(arg as *mut Fb0Info, info) };
    0
}

fn flush(arg: u64) -> i64 {
    use crate::process::syscall::errno;
    // Everything is read out of user memory before `FRAMEBUFFER` is taken:
    // a fault while holding it would stall every console writer.
    let count = match read_user::<u32>(arg) {
        Ok(n) => n as usize,
        Err(e) => return e,
    };
    if count > MAX_FLUSH_RECTS {
        return errno::EINVAL;
    }
    let rects = match read_user::<[Fb0Rect; MAX_FLUSH_RECTS]>(arg + 8) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let _ = core::mem::size_of::<Fb0Flush>(); // the layout the two reads follow
    let mut guard = FRAMEBUFFER.lock();
    let Some(fb) = guard.as_mut() else { return errno::ENODEV };
    for r in &rects[..count] {
        fb.flush_rect(r.x as usize, r.y as usize, r.w as usize, r.h as usize);
    }
    0
}

impl FileHandle for Fb0Handle {
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
        "fb0"
    }

    fn dup(&self) -> Option<Box<dyn FileHandle>> {
        Some(Box::new(Fb0Handle { session: self.session.clone() }))
    }

    fn ioctl(&mut self, request: u64, arg: u64) -> Option<i64> {
        match request {
            FBIO_GET_INFO => Some(get_info(arg)),
            FBIO_FLUSH => Some(flush(arg)),
            _ => None,
        }
    }

    fn shm_object(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        shadow().map(|(obj, _)| obj.clone() as Arc<dyn Any + Send + Sync>)
    }
}
