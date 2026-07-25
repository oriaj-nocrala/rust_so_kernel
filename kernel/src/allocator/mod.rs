// kernel/src/allocator/mod.rs
//
// Thin adapter over the `mm` crate (`mm::buddy::BuddyAllocator` +
// `mm::slab::SlabAllocator`) — the physical/heap allocator logic itself was
// extracted there (see that crate's doc comment and
// `docs/fs/ext2-extraction-plan.md`, the precedent this followed) so it can
// be exercised with `cd mm && cargo test`, no QEMU. Same shape as
// `kernel/src/fs/ext2.rs` after the `ext2` crate extraction: this file owns
// the global state (`BUDDY`, `SLAB_ALLOCATOR`, the `#[global_allocator]`
// registration) and the two seam implementations (`KernelPhysMap`,
// `KernelFrameSource`) that `mm` needed in place of calling straight into
// `crate::memory`/`crate::serial_println_raw!`.
//
// CORRECTED (carried over from the pre-extraction file): Removed
// FRAME_ALLOCATOR and PAGE_TABLE globals.
//
// Previous bug:
//   BootInfoFrameAllocator was initialized over the SAME physical memory
//   regions as the Buddy allocator.  Both could hand out the same frame.
//   In practice this didn't explode because nothing read FRAME_ALLOCATOR
//   after init — but the globals were public and accessible, making it a
//   latent corruption vector.
//
// Current design:
//   - Buddy allocator is the SOLE owner of physical memory after init.
//   - BootInfoFrameAllocator exists as a type (for potential early-boot use)
//     but is NOT stored globally.
//   - Page table operations go through OwnedPageTable (page_table_manager.rs).

use core::alloc::{GlobalAlloc, Layout};
use spin::Mutex;
use x86_64::instructions::interrupts::without_interrupts;
use x86_64::PhysAddr;

// Re-exported so existing call sites can keep saying
// `crate::allocator::BuddyAllocator` — only the removed `buddy_allocator::`
// path segment changed (that submodule now lives in the `mm` crate).
pub use mm::buddy::BuddyAllocator;

/// Production [`mm::PhysMap`]: reads through the bootloader's fixed
/// physical-memory mapping, the same idiom `kernel::hal::KernelPhysMem`
/// uses for `hal::PhysMem`.
pub(crate) struct KernelPhysMap;

impl mm::PhysMap for KernelPhysMap {
    fn virt_for(&self, pa: PhysAddr) -> *mut u8 {
        (crate::memory::physical_memory_offset() + pa.as_u64()).as_mut_ptr::<u8>()
    }
}

/// Production [`mm::FrameSource`]: forwards to this module's own
/// `phys_alloc`/`phys_free` — the pre-extraction slab↔buddy boundary,
/// preserved as a real boundary now that `mm::slab` can't call `mm::buddy`
/// directly (see `mm`'s crate doc comment).
pub(crate) struct KernelFrameSource;

impl mm::FrameSource for KernelFrameSource {
    unsafe fn alloc_order(&self, order: usize) -> Option<PhysAddr> {
        phys_alloc(order)
    }
    unsafe fn free_order(&self, addr: PhysAddr, order: usize) {
        phys_free(addr, order)
    }
}

/// Print a [`mm::buddy::PhantomEvent`] the exact way the pre-extraction
/// `remove_arbitrary_block` did inline — see that type's doc comment for
/// the preserved `NotFound` quirk. Called from every direct
/// `BuddyAllocator::deallocate` call site (not just `phys_free`), so no
/// caller loses output it used to get for free by deallocating inline.
pub(crate) fn log_phantom_event(event: Option<mm::buddy::PhantomEvent>) {
    use mm::buddy::PhantomEvent;
    match event {
        None => {}
        Some(PhantomEvent::EmptyList { addr, order, idx }) => {
            crate::serial_println_raw!(
                "[BUDDY] phantom: block {:#x} order {} in bitmap but free_list[{}] EMPTY — clearing",
                addr.as_u64(), order, idx
            );
        }
        Some(PhantomEvent::LoopLimit { addr, idx }) => {
            crate::serial_println_raw!(
                "[BUDDY] phantom: infinite loop free_list[{}] for {:#x} — clearing",
                idx, addr.as_u64()
            );
        }
        Some(PhantomEvent::NotFound { addr, order }) => {
            crate::serial_println_raw!(
                "[BUDDY] phantom: {:#x} NOT FOUND in free_list[{}] — clearing",
                addr.as_u64(), order
            );
        }
    }
}

// Global instance — the sole owner of physical frames after init (see
// CLAUDE.md's "Key Design Invariants").
pub static BUDDY: Mutex<BuddyAllocator> = Mutex::new(BuddyAllocator::new());

// ── Interrupt safety ────────────────────────────────────────────────────────
//
// `BUDDY` and `SLAB_ALLOCATOR` are both plain `spin::Mutex`es with no `cli`
// discipline of their own — every acquisition site below wraps its critical
// section in `without_interrupts` instead. This is the same invariant
// CLAUDE.md already documents for `SCHEDULER` ("always cli before acquiring
// it, sti after releasing — the timer ISR acquires the lock too"), applied
// here for the first time: found missing via a real, reproducible ~1-in-10
// debug-build boot hang (`busybox --install` at boot, see the
// `debug_hang_selfdeadlock` session). Mechanism, confirmed live via
// gdbstub: ordinary (interruptible) kernel code — e.g. `sys_mkdir` →
// `vfs::resolve_inner`, which allocates a `Vec<&str>` to split the path —
// can hold one of these locks when the timer fires; `timer_preempt_handler`
// → `Scheduler::switch_to_next` can itself need to allocate (growing a run
// queue's `VecDeque`), reentering the SAME non-reentrant `spin::Mutex` on
// the SAME CPU. Since this kernel is single-core, that reentrant `.lock()`
// call can only ever be from the code it just interrupted — it spins
// forever, 100% CPU, no progress, matching the reported symptom exactly.
//
// `without_interrupts` (not a bare `cli`/`sti` pair) because it saves and
// restores the *previous* interrupt-enable state rather than unconditionally
// forcing interrupts back on — safe to call from a context that already has
// them disabled (the timer ISR itself, or any other `without_interrupts`/
// `cli`-protected caller), which a hand-rolled `cli; ...; sti` would not be.
// Accepted cost: interrupt latency around every kernel heap/physical-frame
// allocation — the correct trade for closing a self-deadlock that a purely
// local fix (e.g. pre-reserving scheduler run-queue capacity) would not:
// `resolve_inner`'s plain path-split `Vec<&str>` is enough to trigger this on
// its own, so *any* allocating kernel code is a potential trigger, not just
// the scheduler's own.

/// Allocate 2^order bytes of physical memory from the global buddy allocator.
pub unsafe fn phys_alloc(order: usize) -> Option<PhysAddr> {
    let result = without_interrupts(|| BUDDY.lock().allocate(&KernelPhysMap, order));
    if result.is_none() {
        crate::serial_println_raw!("Buddy: OOM for order {}", order);
    }
    result
}

/// Return 2^order bytes of physical memory to the buddy allocator.
pub unsafe fn phys_free(addr: PhysAddr, order: usize) {
    let event = without_interrupts(|| BUDDY.lock().deallocate(&KernelPhysMap, addr, order));
    log_phantom_event(event);
}

/// (total_bytes, free_bytes) read from a single `BUDDY` lock acquisition —
/// used wherever both figures need to come from the same instant
/// (`/proc/meminfo`, `statvfs`). Same values `BUDDY.lock().total_bytes()` /
/// `.free_bytes(&KernelPhysMap)` always returned.
pub fn mem_stats() -> (u64, u64) {
    without_interrupts(|| {
        let buddy = BUDDY.lock();
        (buddy.total_bytes(), buddy.free_bytes(&KernelPhysMap))
    })
}

/// Free physical memory, in bytes. See `mem_stats` if you also need the total.
pub fn free_bytes() -> u64 {
    without_interrupts(|| BUDDY.lock().free_bytes(&KernelPhysMap))
}

/// Debug: print Buddy allocator statistics (was `BuddyAllocator::
/// debug_print_stats` pre-extraction; `mm::buddy::BuddyAllocator` can't do
/// its own logging anymore — see that crate's doc comment — so this
/// reproduces the exact same lines from `order_stats`/`total_bytes`/
/// `bitmap_bytes`). `order_stats`/`total_bytes`/`bitmap_bytes` are all
/// plain reads returning owned/`Copy` data, so the lock (and the
/// interrupts-disabled window) only needs to span the reads themselves —
/// the actual `serial_println_raw!` calls happen afterward, with
/// interrupts back on.
pub fn debug_print_buddy_stats() {
    let (total, bitmap_bytes, stats) = without_interrupts(|| {
        let buddy = BUDDY.lock();
        (buddy.total_bytes(), buddy.bitmap_bytes(), buddy.order_stats(&KernelPhysMap))
    });

    crate::serial_println_raw!("Buddy Allocator Stats:");
    crate::serial_println_raw!("  Total memory: {}MB", total / (1024 * 1024));
    crate::serial_println_raw!("  Bitmap size: {} bytes", bitmap_bytes);

    for stat in stats {
        if stat.block_count > 0 {
            let block_size = 1u64 << stat.order;
            if block_size >= 1024 * 1024 {
                crate::serial_println_raw!(
                    "  Order {}: {} blocks of {}MB",
                    stat.order, stat.block_count, block_size / (1024 * 1024)
                );
            } else {
                crate::serial_println_raw!(
                    "  Order {}: {} blocks of {}KB",
                    stat.order, stat.block_count, block_size / 1024
                );
            }
        }
    }
}

// ============================================================================
// Slab / GlobalAlloc
// ============================================================================

static SLAB_ALLOCATOR: Mutex<mm::slab::SlabAllocator> = Mutex::new(mm::slab::SlabAllocator::new());

pub struct SlabGlobalAlloc;

// Always-on canary for a reentrant acquisition of this lock — see
// `kernel::debug::SLAB_LOCK_CONTENDED`. Allocation-free on purpose
// (`serial_println_raw!` + a plain atomic) so it cannot recurse into the
// allocator it is watching.
//
// **Must be called with interrupts already disabled**, i.e. from inside the
// `without_interrupts` block that guards the real acquisition — never before
// it. This originally ran *outside*, on the reasoning that a momentary
// `try_lock` probe is self-contained. It is not: when `try_lock` *succeeds*
// it really does hold the lock until the returned guard drops, and doing that
// with interrupts enabled recreates precisely the window the `without_interrupts`
// discipline exists to close. A timer landing in it sends the ISR's own
// allocation into the reentrant `.lock()` that spins forever — so the canary
// caused the very deadlock it was added to detect, at a measured ~2 boots in
// 24. From in here it can no longer open that window, and it still catches the
// case that remains possible: a *direct* reentrant allocation from within the
// allocator's own critical section, which no `cli` can prevent.
#[inline]
fn probe_contention() {
    if SLAB_ALLOCATOR.try_lock().is_none() {
        crate::debug::inc_slab_lock_contended();
        crate::serial_println_raw!(
            "[ALLOC] SLAB_ALLOCATOR already held on this CPU — reentrant lock, about to spin (self-deadlock signature)"
        );
    }
}

unsafe impl GlobalAlloc for SlabGlobalAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let result = without_interrupts(|| {
            probe_contention();
            SLAB_ALLOCATOR.lock().allocate(&KernelPhysMap, &KernelFrameSource, layout)
        });
        log_alloc_event(&result.event);
        result.ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let event = without_interrupts(|| {
            probe_contention();
            SLAB_ALLOCATOR.lock().deallocate(&KernelPhysMap, &KernelFrameSource, ptr, layout)
        });
        log_dealloc_event(&event);
    }
}

/// Reproduces the pre-extraction `SlabAllocator::allocate_large`/
/// `SlabCache::expand` `serial_println_raw!` calls from the
/// [`mm::slab::AllocEvent`] `mm::slab::SlabAllocator::allocate` now reports
/// instead of printing directly (see `mm`'s crate doc comment).
fn log_alloc_event(event: &mm::slab::AllocEvent) {
    use mm::slab::AllocEvent;
    match event {
        AllocEvent::None => {}
        AllocEvent::Expand(t) => {
            if t.ok {
                crate::serial_println_raw!(
                    "Slab: Expanded {}B cache (+{} objects, total {})",
                    t.object_size, t.added_objects, t.total_objects
                );
            } else {
                crate::serial_println_raw!("Slab: Failed to expand {}B cache (OOM)", t.object_size);
            }
        }
        AllocEvent::Large(t) => {
            crate::serial_println_raw!(">>> allocate_large: size={} order={}", t.size, t.order);
            if t.ok {
                crate::serial_println_raw!(">>> allocate_large: OK at {:#x}", t.result_addr);
            } else {
                crate::serial_println_raw!(">>> allocate_large: FAILED");
            }
        }
    }
}

/// Reproduces the pre-extraction `SlabAllocator::deallocate`/
/// `deallocate_large` `serial_println_raw!` calls from the
/// [`mm::slab::DeallocEvent`] `mm::slab::SlabAllocator::deallocate` now
/// reports instead of printing directly.
fn log_dealloc_event(event: &mm::slab::DeallocEvent) {
    use mm::slab::DeallocEvent;
    if let DeallocEvent::Large(t) = event {
        crate::serial_println_raw!(">>> Slab: Large dealloc");
        crate::serial_println_raw!(
            "[SLAB] deallocate_large: virt={:#x} phys={:#x} size={} order={}",
            t.virt, t.phys, t.size, t.order
        );
        if t.hot_range {
            crate::serial_println_raw!("[SLAB]   ^^^ THIS IS IN THE HOT RANGE!");
        }
    }
}

#[global_allocator]
static GLOBAL_ALLOCATOR: SlabGlobalAlloc = SlabGlobalAlloc;

/// Debug: print slab allocator statistics (was `SlabAllocator::stats`
/// pre-extraction — see `debug_print_buddy_stats`'s doc comment for why
/// this now lives here instead of on the `mm` type).
pub fn slab_stats() {
    // `cache_stats()` returns an owned, fixed-size array (see mm::slab), so
    // — same as `debug_print_buddy_stats` above — the interrupts-disabled
    // window only needs to cover the lock + the call itself, not the prints.
    let stats = without_interrupts(|| SLAB_ALLOCATOR.lock().cache_stats());
    crate::serial_println_raw!("Slab Allocator Stats:");
    for (size_class, total, used) in stats {
        if total > 0 {
            crate::serial_println_raw!(
                "  {}B: {}/{} objects ({}% used)",
                size_class, used, total, (used * 100) / total.max(1)
            );
        }
    }
}
