// kernel/src/memory/cow.rs
//
// Frame refcount table for Copy-on-Write.
//
// Convention:
//   refcount = 0  → frame is a PT intermediate (PDPT/PD/PT), never tracked
//   refcount = 1  → single owner (allocated by map_user_page or demand_paging)
//   refcount ≥ 2  → shared between N processes (COW active)
//
// The table is sized at boot to cover every usable physical frame the
// firmware actually reported (`init_refcount_table`), and allocated from
// the Buddy allocator rather than living in BSS. It used to be a fixed
// `[u8; 512 MiB / 4 KiB]` array, on the assumption — true of every QEMU
// configuration this kernel was developed against, and of nothing else —
// that no physical frame would ever sit above 512 MiB.
//
// That assumption did not merely degrade above the bound: it broke COW
// three separate ways at once, silently, because every accessor failed
// *unsafely* on an untracked index. `inc_ref` was a no-op, so a `fork()`
// never recorded that parent and child now shared the frame; `get_ref`
// returned 0, so the COW fault handler read "sole owner" and restored
// WRITABLE **without copying**, leaving two processes writing the same
// physical frame; and `dec_ref` returned 0, which by the convention above
// means "refcount hit zero, free it", so a child's exit handed the parent's
// still-mapped frames straight back to the Buddy allocator. Measured, not
// reasoned: booting with `-m 768M` (the first size with any RAM above the
// old bound) reliably killed PID 1 with `SEGFAULT (no VMA for address)` the
// moment `busybox --install`'s child exited, while `-m 512M` was clean —
// and that is the same failure that stopped real-hardware bring-up on a
// 5900X, where *all* of RAM sits above 512 MiB.
//
// Out-of-range indices now fail **safe** instead (see the accessors below):
// unreachable once the table covers all of RAM, but if it ever happens the
// cost is a leaked frame, not two processes sharing one.
//
// The counts are `AtomicU8`s (stage 6 of `docs/smp/smp-plan.md`). They
// used to be plain bytes whose read-modify-writes were kept whole by IF=0
// — true on one CPU only; two CPUs dropping their share of one frame at
// once could both read 2, both write 1, and the frame would never be freed
// (or, the other way round, freed while still mapped). The *decisions*
// built on a count are made under the owning address space's lock
// (`AddressSpace::lock`): a count can only rise through `fork()` of an
// address space that maps the frame, which holds that lock, so a faulting
// thread that reads 1 under the same lock really is the last owner.

use core::sync::atomic::{AtomicPtr, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use x86_64::{PhysAddr, structures::paging::PhysFrame};

/// Largest table the Buddy allocator can hand out in one block
/// (`MAX_ORDER` = 28 → 256 MiB), which is one byte per 4 KiB frame across
/// 1 TiB of RAM. Beyond that the tail is untracked and takes the
/// fail-safe path.
const MAX_TABLE_ORDER: usize = 28;

/// Base of the refcount table, or null before `init_refcount_table`.
static FRAME_REFCOUNTS: AtomicPtr<AtomicU8> = AtomicPtr::new(core::ptr::null_mut());

/// Number of frames the table covers. Zero before `init_refcount_table`,
/// which makes every accessor take its fail-safe path.
static TRACKED_FRAMES: AtomicUsize = AtomicUsize::new(0);

/// Size, allocate and zero the frame refcount table.
///
/// `max_phys_addr` is the end of the highest *usable* memory region the
/// bootloader reported — the Buddy allocator never hands out a frame above
/// it, so that is exactly the range that needs tracking.
///
/// # Safety
/// Must be called exactly once, after the Buddy allocator is seeded and
/// before the first `fork()` — in practice from `init::memory::init_core`,
/// which is also what the QEMU integration tests' boot path goes through.
pub unsafe fn init_refcount_table(max_phys_addr: u64) {
    let frames = (max_phys_addr as usize).div_ceil(4096);

    // Smallest Buddy order whose block holds one byte per frame.
    let mut order = 12;
    while order < MAX_TABLE_ORDER && (1usize << order) < frames {
        order += 1;
    }

    let addr = crate::allocator::phys_alloc(order)
        // Failing here is not survivable-but-degraded: every path below
        // would silently take the fail-safe branch, leaking a frame per
        // COW share, so say so loudly instead of limping on.
        .expect("COW refcount table allocation failed");

    let virt = (crate::memory::physical_memory_offset() + addr.as_u64()).as_mut_ptr::<u8>();
    core::ptr::write_bytes(virt, 0, 1usize << order);

    let tracked = frames.min(1usize << order);
    // Pointer first, count second (Release): a reader that sees the count
    // sees the pointer.
    FRAME_REFCOUNTS.store(virt.cast::<AtomicU8>(), Ordering::Release);
    TRACKED_FRAMES.store(tracked, Ordering::Release);

    crate::serial_println!(
        "COW: refcount table {} KiB at phys {:#x}, covers {} frames ({} MiB of RAM)",
        (1usize << order) / 1024,
        addr.as_u64(),
        tracked,
        (tracked * 4096) / (1024 * 1024),
    );
}

/// Number of frames the refcount table covers (0 before init) — read by
/// `/proc/kdebug`'s report so a machine whose RAM outruns the table says
/// so instead of corrupting memory quietly.
pub fn tracked_frames() -> usize {
    TRACKED_FRAMES.load(Ordering::Acquire)
}

/// Slot for `frame`, or `None` if it falls outside the tracked range.
#[inline]
fn slot(frame: PhysFrame) -> Option<&'static AtomicU8> {
    let idx = (frame.start_address().as_u64() / 4096) as usize;
    if idx < TRACKED_FRAMES.load(Ordering::Acquire) {
        // SAFETY: the table is `TRACKED_FRAMES` bytes long, never freed,
        // and published before its length.
        Some(unsafe { &*FRAME_REFCOUNTS.load(Ordering::Relaxed).add(idx) })
    } else {
        None
    }
}

/// Set the refcount of a data frame to an explicit value.
/// Called after allocating a new data frame (set to 1), before the frame
/// is mapped anywhere — nobody else can be touching its count.
pub fn set_ref(frame: PhysFrame, count: u8) {
    // Untracked: nothing to record. Harmless on its own — `get_ref` below
    // reports such a frame as shared regardless, which is the conservative
    // answer this accessor would otherwise be overriding to 1.
    if let Some(c) = slot(frame) {
        c.store(count, Ordering::Release);
    }
}

/// Increment the refcount of a frame (COW share — parent and child now own it).
pub fn inc_ref(frame: PhysFrame) {
    // Untracked: no count to raise, and none is needed — `get_ref` already
    // answers "shared" and `dec_ref` never reaches zero for such a frame.
    if let Some(c) = slot(frame) {
        let _ = c.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| Some(n.saturating_add(1)));
    }
}

/// Decrement the refcount of a frame.  Returns the NEW refcount value.
/// When the new value is 0, the caller should free the frame to the Buddy
/// allocator — and exactly one caller sees 0, since the decrement and the
/// read of its result are one atomic operation.
pub fn dec_ref(frame: PhysFrame) -> u8 {
    match slot(frame) {
        Some(c) => {
            let old = c
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| Some(n.saturating_sub(1)))
                .unwrap_or(0);
            old.saturating_sub(1)
        }
        // Untracked: never report zero. Callers free the frame to the
        // Buddy allocator on a zero return, and an untracked frame may
        // still be mapped by another process — leaking it is recoverable,
        // handing a live frame back to the allocator is not.
        None => 1,
    }
}

/// Read the refcount of a frame without modifying it.
pub fn get_ref(frame: PhysFrame) -> u8 {
    match slot(frame) {
        Some(c) => c.load(Ordering::Acquire),
        // Untracked: report "shared" so the COW fault handler copies
        // instead of handing the faulting process write access to a frame
        // someone else may still own. Costs a copy that may be needless;
        // the alternative is two processes writing one frame.
        None => 2,
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

/// The zero frame's physical address, or `None` before `init_zero_frame`.
pub fn zero_frame_phys() -> Option<u64> {
    match ZERO_FRAME_PHYS.load(Ordering::Relaxed) {
        0 => None,
        a => Some(a),
    }
}

/// Returns `true` if `frame` is the permanent shared zero frame.
pub fn is_zero_frame(frame: PhysFrame) -> bool {
    let addr = ZERO_FRAME_PHYS.load(Ordering::Relaxed);
    addr != 0 && frame.start_address().as_u64() == addr
}
