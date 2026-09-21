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
// All accesses must be under `cli` (single CPU — no atomics needed).

use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::{PhysAddr, structures::paging::PhysFrame};

/// Largest table the Buddy allocator can hand out in one block
/// (`MAX_ORDER` = 28 → 256 MiB), which is one byte per 4 KiB frame across
/// 1 TiB of RAM. Beyond that the tail is untracked and takes the
/// fail-safe path.
const MAX_TABLE_ORDER: usize = 28;

/// Base of the refcount table, or null before `init_refcount_table`.
static mut FRAME_REFCOUNTS: *mut u8 = core::ptr::null_mut();

/// Number of frames the table covers. Zero before `init_refcount_table`,
/// which makes every accessor take its fail-safe path.
static mut TRACKED_FRAMES: usize = 0;

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

    FRAME_REFCOUNTS = virt;
    TRACKED_FRAMES = frames.min(1usize << order);

    crate::serial_println!(
        "COW: refcount table {} KiB at phys {:#x}, covers {} frames ({} MiB of RAM)",
        (1usize << order) / 1024,
        addr.as_u64(),
        TRACKED_FRAMES,
        (TRACKED_FRAMES * 4096) / (1024 * 1024),
    );
}

/// Number of frames the refcount table covers (0 before init) — read by
/// `/proc/kdebug`'s report so a machine whose RAM outruns the table says
/// so instead of corrupting memory quietly.
pub fn tracked_frames() -> usize {
    unsafe { TRACKED_FRAMES }
}

#[inline]
fn frame_idx(frame: PhysFrame) -> usize {
    (frame.start_address().as_u64() / 4096) as usize
}

/// Slot for `frame`, or `None` if it falls outside the tracked range.
#[inline]
unsafe fn slot(frame: PhysFrame) -> Option<*mut u8> {
    let idx = frame_idx(frame);
    if idx < TRACKED_FRAMES {
        Some(FRAME_REFCOUNTS.add(idx))
    } else {
        None
    }
}

/// Check whether the calling accessor's invariant ("must be called with
/// interrupts disabled") actually holds, recording anything that doesn't
/// into the per-accessor counter passed in (see `debug::COW_IF_VIOLATIONS_*`
/// for `inc_ref`/`dec_ref`/`get_ref`, and `debug::COW_IF_ENABLED_SET_REF`
/// for `set_ref`, which is instrumented the same way despite not being a
/// real violation — see its own doc comment) instead of just
/// asserting/panicking — this is a bug hunt, not a case where crashing
/// harder helps, and a live counter survives to be read from
/// `/proc/kdebug`/the panic snapshot even on a run that goes on to
/// hang/double-fault before a fix could ever print anything. Split by
/// accessor (rather than one shared counter) because `set_ref` (a plain
/// write — always into a just-allocated, exclusively-owned frame index,
/// so not itself a lost-update hazard) and `inc_ref`/`dec_ref` (a real
/// non-atomic read-modify-write, the actual lost-update hazard if a
/// timer tick lands mid-sequence and something else touches the same
/// frame index before it resumes) have very different risk profiles —
/// a shared "last caller" would hide whichever violation happened
/// second. One relaxed load + branch — same cost model as `ktrace!`.
#[inline]
#[track_caller]
fn check_if_disabled(diag: &crate::debug::IfViolationDiag) {
    if x86_64::instructions::interrupts::are_enabled() {
        diag.record(core::panic::Location::caller());
    }
}

/// Set the refcount of a data frame to an explicit value.
/// Called after allocating a new data frame (set to 1).
///
/// # Safety
/// Unlike `inc_ref`/`dec_ref` below, this one does NOT actually require
/// interrupts disabled, and callers running with IF=1 are not a bug.
/// `FRAME_REFCOUNTS[idx] = count` is a single-byte store — indivisible on
/// this architecture regardless of interrupt state — and every call site
/// writes into a frame index that was *just* allocated and is exclusively
/// owned by the caller at that point (no other code path can be racing to
/// touch the same index), so there is no read-modify-write sequence for a
/// timer tick to land in the middle of and no lost update to lose. This
/// was originally documented the same as the other three accessors, on
/// the theory that "all `cow.rs` accessors" shared one invariant; measured
/// instead of assumed: `sys_exec`/`memory/elf_loader.rs` legitimately call
/// this with interrupts enabled ~675 times per boot in the normal ELF-load
/// path (`elf_loader.rs`'s PT_LOAD segment loop), and every one of those is
/// correct, not a race. Note the counter's `last_caller` reports
/// `page_table_manager.rs`'s `map_user_page`, not `elf_loader` — that is
/// the immediate call site, one frame below where the IF=1 actually
/// originates; don't read the mismatch as the counter pointing somewhere
/// unexpected. The accompanying
/// `debug::COW_IF_ENABLED_SET_REF` counter is informational — evidence of
/// how this accessor is actually used — not a fault detector; see its own
/// doc comment. `inc_ref`/`dec_ref` (real non-atomic read-modify-write) and
/// `get_ref` (a plain read, tracked for completeness) still have a genuine
/// "interrupts disabled" contract — see their doc comments below.
#[track_caller]
pub unsafe fn set_ref(frame: PhysFrame, count: u8) {
    check_if_disabled(&crate::debug::COW_IF_ENABLED_SET_REF);
    // Untracked: nothing to record. Harmless on its own — `get_ref` below
    // reports such a frame as shared regardless, which is the conservative
    // answer this accessor would otherwise be overriding to 1.
    if let Some(p) = slot(frame) {
        *p = count;
    }
}

/// Increment the refcount of a frame (COW share — parent and child now own it).
///
/// # Safety
/// Must be called with interrupts disabled (single CPU).
#[track_caller]
pub unsafe fn inc_ref(frame: PhysFrame) {
    check_if_disabled(&crate::debug::COW_IF_VIOLATIONS_INC_REF);
    // Untracked: no count to raise, and none is needed — `get_ref` already
    // answers "shared" and `dec_ref` never reaches zero for such a frame.
    if let Some(p) = slot(frame) {
        *p = (*p).saturating_add(1);
    }
}

/// Decrement the refcount of a frame.  Returns the NEW refcount value.
/// When the new value is 0, the caller should free the frame to the Buddy allocator.
///
/// # Safety
/// Must be called with interrupts disabled (single CPU).
#[track_caller]
pub unsafe fn dec_ref(frame: PhysFrame) -> u8 {
    check_if_disabled(&crate::debug::COW_IF_VIOLATIONS_DEC_REF);
    match slot(frame) {
        Some(p) => {
            *p = (*p).saturating_sub(1);
            *p
        }
        // Untracked: never report zero. Callers free the frame to the
        // Buddy allocator on a zero return, and an untracked frame may
        // still be mapped by another process — leaking it is recoverable,
        // handing a live frame back to the allocator is not.
        None => 1,
    }
}

/// Read the refcount of a frame without modifying it.
///
/// # Safety
/// Must be called with interrupts disabled (single CPU).
#[track_caller]
pub unsafe fn get_ref(frame: PhysFrame) -> u8 {
    check_if_disabled(&crate::debug::COW_IF_VIOLATIONS_GET_REF);
    match slot(frame) {
        Some(p) => *p,
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

/// Returns `true` if `frame` is the permanent shared zero frame.
pub fn is_zero_frame(frame: PhysFrame) -> bool {
    let addr = ZERO_FRAME_PHYS.load(Ordering::Relaxed);
    addr != 0 && frame.start_address().as_u64() == addr
}
