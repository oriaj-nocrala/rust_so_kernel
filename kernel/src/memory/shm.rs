// kernel/src/memory/shm.rs
//
// Shared-memory objects: what a `memfd_create` fd and a
// `MAP_SHARED|MAP_ANONYMOUS` mapping are, underneath. Phase 1 of
// `docs/gui/gui-plan.md`.
//
// An object is a size plus one optional physical frame per page, allocated
// on first touch (a fault in a mapping, or a `write()` through the fd) and
// zeroed. Every mapping maps the object's own frames — that is the whole
// difference from an anonymous VMA, whose frames are private and COW.
//
// **Frame lifetime rides on the COW refcounts (`memory::cow`).** The
// object holds one reference on each of its frames; each PTE that maps one
// holds another. Everything that tears user mappings down
// (`unmap_page_and_free`, `release_user_pages` on process death) already
// does `dec_ref` and frees at zero, so a mapping going away never frees a
// frame the object still owns — without that code knowing shared memory
// exists. The frame is freed by whoever drops the last reference: the
// object's `Drop`, or the last unmap after the object died.
//
// The refcounts are saturating `u8`s. `MAX_MAPPINGS` keeps every frame far
// below 255: a frame's count is at most 1 (the object) + one per mapping.
//
// Lock order: address space (`AddressSpace::vmas`) → `ShmObject::inner` →
// `BUDDY`/`SLAB_ALLOCATOR`. Never touch user memory while holding `inner`:
// a fault on a mapping of this same object would take it again.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use x86_64::structures::paging::PhysFrame;

use crate::allocator::KernelIrq;
use diag::IrqMutex;

/// Mappings (`ShmMapping`s, i.e. VMAs) one object may have at once. Well
/// under the 254 a `u8` refcount can count before saturating; `fork`
/// checks it too, since it clones every VMA.
pub const MAX_MAPPINGS: usize = 200;

/// Largest object size: bounds the per-object page table (`pages`, 8 bytes
/// per page — 512 KiB at this size).
pub const MAX_SIZE: u64 = 256 * 1024 * 1024;

const PAGE: u64 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShmError {
    /// Larger than `MAX_SIZE` (`EFBIG`).
    TooBig,
    /// Shrinking an object that is mapped (`EBUSY`): its frames would have
    /// to be unmapped from every address space holding them.
    Busy,
    /// No memory for a frame or for the page table (`ENOMEM`).
    NoMemory,
}

struct Inner {
    /// One entry per page of `size`, rounded up.
    pages: Vec<Option<PhysFrame>>,
    size: u64,
}

pub struct ShmObject {
    inner: IrqMutex<Inner, KernelIrq>,
    mappings: AtomicUsize,
    /// Frames the object did not allocate and must never free or resize
    /// away: the framebuffer's RAM copy behind `/dev/fb0` (see
    /// [`ShmObject::pinned`]).
    pinned: bool,
}

impl core::fmt::Debug for ShmObject {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "ShmObject {{ size: {}, mappings: {} }}", self.size(), self.mappings())
    }
}

fn pages_for(len: u64) -> usize {
    len.div_ceil(PAGE) as usize
}

unsafe fn frame_ptr(frame: PhysFrame) -> *mut u8 {
    (crate::memory::physical_memory_offset() + frame.start_address().as_u64()).as_mut_ptr::<u8>()
}

/// Drop the object's reference on `frame`, freeing it if nothing maps it.
fn release_frame(frame: PhysFrame) {
    if crate::memory::cow::dec_ref(frame) == 0 {
        unsafe { crate::allocator::phys_free(frame.start_address(), 12) };
    }
}

impl ShmObject {
    pub const fn new() -> Self {
        Self {
            inner: IrqMutex::new(Inner { pages: Vec::new(), size: 0 }),
            mappings: AtomicUsize::new(0),
            pinned: false,
        }
    }

    /// An object over frames that already exist and belong to someone
    /// else — the framebuffer's RAM copy, which `/dev/fb0` lets the
    /// compositor map (`docs/gui/gui-plan.md`, phase 2.1). Mappings work
    /// exactly as for a memfd: each PTE takes a reference, and unmapping or
    /// the mapper's death drops it through the ordinary paths.
    ///
    /// **Must be kept alive forever** (a `static`): the object's own
    /// reference on each frame is what keeps those `dec_ref`s from ever
    /// reaching zero and handing the frames to the Buddy allocator, which
    /// never gave them out as user pages. `Drop` refuses to release them
    /// anyway, as a second guard, and so does `set_size`.
    ///
    /// # Safety
    /// Every frame must stay allocated, and be memory it is harmless for
    /// user space to read and write, for as long as the kernel runs; its
    /// COW refcount must be untracked (zero) until now.
    pub unsafe fn pinned(frames: Vec<PhysFrame>) -> Self {
        for &frame in &frames {
            crate::memory::cow::set_ref(frame, 1);
        }
        let size = frames.len() as u64 * PAGE;
        Self {
            inner: IrqMutex::new(Inner { pages: frames.into_iter().map(Some).collect(), size }),
            mappings: AtomicUsize::new(0),
            pinned: true,
        }
    }

    pub fn size(&self) -> u64 {
        self.inner.with(|i| i.size)
    }

    pub fn mappings(&self) -> usize {
        self.mappings.load(Ordering::Acquire)
    }

    pub(super) fn mapping_added(&self) {
        self.mappings.fetch_add(1, Ordering::AcqRel);
    }

    pub(super) fn mapping_removed(&self) {
        self.mappings.fetch_sub(1, Ordering::AcqRel);
    }

    /// `ftruncate`: grow (new pages read as zeros) or shrink (only while
    /// unmapped; the dropped frames are released, and the bytes past `len`
    /// in the last page are zeroed so a later grow reads zeros there too).
    pub fn set_size(&self, len: u64) -> Result<(), ShmError> {
        if len > MAX_SIZE {
            return Err(ShmError::TooBig);
        }
        if self.pinned {
            return Err(ShmError::Busy);
        }
        self.inner.with(|i| Self::set_size_locked(&self.mappings, i, len))
    }

    fn set_size_locked(mappings: &AtomicUsize, i: &mut Inner, len: u64) -> Result<(), ShmError> {
        let n = pages_for(len);
        if len < i.size {
            // Checked under `inner`: a fault on a mapping added after this
            // check takes `inner` next and sees the new size.
            if mappings.load(Ordering::Acquire) > 0 {
                return Err(ShmError::Busy);
            }
            for frame in i.pages.drain(n..).flatten() {
                release_frame(frame);
            }
            if len % PAGE != 0 {
                if let Some(Some(last)) = i.pages.last() {
                    let off = (len % PAGE) as usize;
                    unsafe { core::ptr::write_bytes(frame_ptr(*last).add(off), 0, PAGE as usize - off) };
                }
            }
        } else if n > i.pages.len() {
            i.pages.try_reserve(n - i.pages.len()).map_err(|_| ShmError::NoMemory)?;
            i.pages.resize(n, None);
        }
        i.size = len;
        Ok(())
    }

    /// The frame for page `idx`, allocated and zeroed if this is its first
    /// touch. Caller holds `inner`.
    fn frame_locked(i: &mut Inner, idx: usize) -> Result<PhysFrame, ShmError> {
        if let Some(f) = i.pages[idx] {
            return Ok(f);
        }
        let frame = unsafe { crate::allocator::phys_alloc(12) }
            .map(PhysFrame::containing_address)
            .ok_or(ShmError::NoMemory)?;
        unsafe { core::ptr::write_bytes(frame_ptr(frame), 0, PAGE as usize) };
        // The object's own reference.
        crate::memory::cow::set_ref(frame, 1);
        i.pages[idx] = Some(frame);
        Ok(frame)
    }

    /// The frame to map for page `idx` of the object, with one reference
    /// already taken for the caller's PTE (taken under `inner`, so a
    /// concurrent shrink cannot free the frame in between). An error if
    /// `idx` is past the object's size (Linux's `SIGBUS`) or out of memory.
    pub fn frame_for_mapping(&self, idx: usize) -> Result<PhysFrame, &'static str> {
        self.inner.with(|i| {
            if idx >= i.pages.len() {
                return Err("shm: page beyond the object's size");
            }
            let frame = Self::frame_locked(i, idx).map_err(|_| "shm: out of memory")?;
            crate::memory::cow::inc_ref(frame);
            Ok(frame)
        })
    }

    /// `read()` through an fd: copy from byte `off` into `buf`, stopping at
    /// the size. Untouched pages read as zeros without being allocated.
    pub fn read_at(&self, off: u64, buf: &mut [u8]) -> usize {
        let mut bounce = [0u8; 512];
        let mut done = 0usize;
        while done < buf.len() {
            let pos = off + done as u64;
            let chunk = (buf.len() - done).min(bounce.len()).min((PAGE - pos % PAGE) as usize);
            let n = self.inner.with(|i| {
                if pos >= i.size {
                    return 0;
                }
                let n = chunk.min((i.size - pos) as usize);
                match i.pages[(pos / PAGE) as usize] {
                    Some(f) => unsafe {
                        core::ptr::copy_nonoverlapping(frame_ptr(f).add((pos % PAGE) as usize), bounce.as_mut_ptr(), n)
                    },
                    None => bounce[..n].fill(0),
                }
                n
            });
            if n == 0 {
                break;
            }
            // Outside `inner`: `buf` may be user memory.
            buf[done..done + n].copy_from_slice(&bounce[..n]);
            done += n;
        }
        done
    }

    /// `write()` through an fd: copy `buf` in at byte `off`, growing the
    /// object if it ends past the size (as a regular file grows).
    pub fn write_at(&self, off: u64, buf: &[u8]) -> Result<usize, ShmError> {
        if self.pinned {
            return Err(ShmError::Busy);
        }
        let end = off.checked_add(buf.len() as u64).ok_or(ShmError::TooBig)?;
        if end > MAX_SIZE {
            return Err(ShmError::TooBig);
        }
        let mut bounce = [0u8; 512];
        let mut done = 0usize;
        while done < buf.len() {
            let pos = off + done as u64;
            let n = (buf.len() - done).min(bounce.len()).min((PAGE - pos % PAGE) as usize);
            // Outside `inner`: `buf` may be user memory.
            bounce[..n].copy_from_slice(&buf[done..done + n]);
            let r = self.inner.with(|i| {
                if pos + n as u64 > i.size {
                    Self::set_size_locked(&self.mappings, i, pos + n as u64)?;
                }
                let f = Self::frame_locked(i, (pos / PAGE) as usize)?;
                unsafe { core::ptr::copy_nonoverlapping(bounce.as_ptr(), frame_ptr(f).add((pos % PAGE) as usize), n) };
                Ok::<(), ShmError>(())
            });
            match r {
                Ok(()) => done += n,
                // A short write, like a regular file that runs out of space.
                Err(_) if done > 0 => break,
                Err(e) => return Err(e),
            }
        }
        Ok(done)
    }
}

impl Drop for ShmObject {
    fn drop(&mut self) {
        if self.pinned {
            // Not ours to free (see `pinned`); leak the references.
            return;
        }
        self.inner.with(|i| {
            for frame in i.pages.drain(..).flatten() {
                release_frame(frame);
            }
        });
    }
}
