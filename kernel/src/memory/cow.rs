// kernel/src/memory/cow.rs
//
// Frame refcount table for Copy-on-Write.
//
// Convention:
//   refcount = 0  → frame is a PT intermediate (PDPT/PD/PT), never tracked
//   refcount = 1  → single owner (allocated by map_user_page or demand_paging)
//   refcount ≥ 2  → shared between N processes (COW active)
//
// The array covers 512 MiB / 4 KiB = 131072 frames in 256 KiB of BSS.
// All accesses must be under `cli` (single CPU — no atomics needed).

use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::{PhysAddr, structures::paging::PhysFrame};

const MAX_FRAMES: usize = 512 * 1024 * 1024 / 4096; // 131072

static mut FRAME_REFCOUNTS: [u8; MAX_FRAMES] = [0u8; MAX_FRAMES];

#[inline]
fn frame_idx(frame: PhysFrame) -> usize {
    (frame.start_address().as_u64() / 4096) as usize
}

/// Set the refcount of a data frame to an explicit value.
/// Called after allocating a new data frame (set to 1).
///
/// # Safety
/// Must be called with interrupts disabled (single CPU).
pub unsafe fn set_ref(frame: PhysFrame, count: u8) {
    let idx = frame_idx(frame);
    if idx < MAX_FRAMES {
        FRAME_REFCOUNTS[idx] = count;
    }
}

/// Increment the refcount of a frame (COW share — parent and child now own it).
///
/// # Safety
/// Must be called with interrupts disabled (single CPU).
pub unsafe fn inc_ref(frame: PhysFrame) {
    let idx = frame_idx(frame);
    if idx < MAX_FRAMES {
        FRAME_REFCOUNTS[idx] = FRAME_REFCOUNTS[idx].saturating_add(1);
    }
}

/// Decrement the refcount of a frame.  Returns the NEW refcount value.
/// When the new value is 0, the caller should free the frame to the Buddy allocator.
///
/// # Safety
/// Must be called with interrupts disabled (single CPU).
pub unsafe fn dec_ref(frame: PhysFrame) -> u8 {
    let idx = frame_idx(frame);
    if idx < MAX_FRAMES {
        FRAME_REFCOUNTS[idx] = FRAME_REFCOUNTS[idx].saturating_sub(1);
        FRAME_REFCOUNTS[idx]
    } else {
        0
    }
}

/// Read the refcount of a frame without modifying it.
///
/// # Safety
/// Must be called with interrupts disabled (single CPU).
pub unsafe fn get_ref(frame: PhysFrame) -> u8 {
    let idx = frame_idx(frame);
    if idx < MAX_FRAMES {
        FRAME_REFCOUNTS[idx]
    } else {
        0
    }
}

// ============================================================================
// Zero-page (shared read-only zero frame)
// ============================================================================

/// Physical address of the permanent shared zero frame.
/// Set once at boot by `init_zero_frame`; never changes.
static ZERO_FRAME_PHYS: AtomicU64 = AtomicU64::new(0);

/// Allocate and zero-fill the shared zero frame.  Called once from `init`.
/// The frame is never tracked by the refcount table — it is permanent.
///
/// # Safety
/// Must be called after the Buddy allocator is initialized, before any
/// user processes start.
pub unsafe fn init_zero_frame() {
    let addr = crate::allocator::phys_alloc(12).expect("zero frame alloc");
    let phys_offset = crate::memory::physical_memory_offset();
    let virt = (phys_offset + addr.as_u64()).as_mut_ptr::<u8>();
    core::ptr::write_bytes(virt, 0, 4096);
    ZERO_FRAME_PHYS.store(addr.as_u64(), Ordering::Relaxed);
}

/// Returns the shared zero frame.  Valid after `init_zero_frame`.
pub fn zero_frame() -> PhysFrame {
    PhysFrame::containing_address(PhysAddr::new(
        ZERO_FRAME_PHYS.load(Ordering::Relaxed)
    ))
}

/// Returns `true` if `frame` is the permanent shared zero frame.
pub fn is_zero_frame(frame: PhysFrame) -> bool {
    let addr = ZERO_FRAME_PHYS.load(Ordering::Relaxed);
    addr != 0 && frame.start_address().as_u64() == addr
}
