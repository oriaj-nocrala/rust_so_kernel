// mm/tests/reentrancy_probes.rs
//
// Family A reentrancy probes (deterministic, single-threaded, no
// `std::thread`) for `mm`'s slab allocator — see
// `vfs/src/mount.rs::direct_children_callback_from_lookup_does_not_self_deadlock`
// for the template this file imitates: a toy collaborator that calls back
// into the SAME shared structure from *inside* the very operation under
// test. For probe 1 below, a HANG (not a panic, not an assertion failure)
// is the expected failure signal if the invariant it documents is ever
// broken — see that test's own doc comment — which is why these tests
// carry NO internal timeout/watchdog: a regression is supposed to make
// `cargo test` hang on this file, and a caller running the suite under
// `timeout` (as this task's own verification step does) sees that hang
// directly. Do not "fix" a future red run here by adding a timeout inside
// the test — that would hide exactly the failure mode this file exists to
// catch.
//
// ## What real bug this models, and what it deliberately does NOT model
//
// The real historical bug (commit ab58dba, the `slab_lock_self_deadlock`
// memory) was the timer ISR firing on this kernel's single vCPU while
// kernel code held `SLAB_ALLOCATOR`/`BUDDY` (both plain `spin::Mutex`,
// non-reentrant) locked, and the ISR's own path
// (`Scheduler::switch_to_next` growing a run-queue `VecDeque`) needing to
// allocate too — reentering the SAME non-reentrant lock on the SAME core
// and spinning forever. Both of those `Mutex`es live in
// `kernel/src/allocator/mod.rs`, entirely outside this crate: `mm`'s own
// crate doc comment ("What did NOT move") is explicit that `mm` owns no
// global state and does no locking of its own — `SlabAllocator`/
// `BuddyAllocator` are plain `&mut self` structs with zero interior
// mutability anywhere in `mm/src/{slab,buddy}.rs`. So these probes CANNOT
// reproduce ab58dba verbatim — there is no lock inside `mm` for an
// ISR-modeled reentrant call to self-deadlock against today. That is a
// real, useful finding on its own (see the accompanying task report), not
// a gap in these tests.
//
// What these probes DO cover, squarely inside `mm`'s own boundary: the
// `FrameSource` seam (`mm::FrameSource`) is the one place `mm`'s own code
// (`SlabCache::expand`, called from `SlabCache::allocate`, called from
// `SlabAllocator::allocate`) calls back out to caller-supplied code from
// *inside* an allocation already in progress on a given `SlabAllocator`
// instance — the same shape the real ISR reentrancy had, just modeled as
// a synchronous nested call (there being no real interrupt to inject in a
// host test) instead of an asynchronous one. `ReentrantFrameSource` below
// reenters the exact same `SlabAllocator` instance via `UnsafeCell`-based
// raw-pointer aliasing (same idea as `slab_frame_source.rs`'s
// `RefCell<&'a mut BuddyAllocator>`, just without an intervening safe
// wrapper). A safe `RefCell`/`Mutex` wrapper around the WHOLE
// `SlabAllocator` was deliberately rejected for this harness: since
// `SlabAllocator::allocate(&mut self, ...)` must hold its `&mut self` for
// its entire body (which is where the nested `frames.alloc_order()` call
// happens), any safe wrapper's guard would necessarily still be alive
// during that nested call — so a reentrant `.lock()`/`.borrow_mut()` on
// the SAME wrapper would deadlock/panic unconditionally, regardless of
// whether `mm`'s own allocator code is reentrancy-safe. That would test
// the test harness, not `mm` — see the task report for the longer version
// of this reasoning.
//
// Today, reentering via the raw-pointer alias works cleanly, because
// nothing inside `SlabCache`/`SlabAllocator` holds any state across the
// `frames.alloc_order()` call that a reentrant path could contend with —
// every method re-reads `&mut self`'s fields fresh after any call that
// might have mutated them (see `SlabCache::allocate`: `self.free_list` is
// read again, freshly, right after `self.expand(...)` returns, not cached
// in a stale local across that call). The sabotage step in the task report
// adds exactly such state to a throwaway copy of `mm/src/slab.rs` — a
// busy-flag "protecting" a cache during expansion, held across the
// `FrameSource` call — and shows probe 1 hangs against it, spinning
// exactly the way a reentered `spin::Mutex` would in the real kernel.
//
// ## BuddyAllocator: no analogous seam
//
// `BuddyAllocator`'s only caller-supplied dependency is `mm::PhysMap`
// (`virt_for`, a pure physical->virtual address translation with no
// plausible reason to call back into the allocator) — it has no
// `FrameSource`-shaped callback of its own; nothing external is ever
// invoked *from inside* one of its methods. So there is no third probe
// here for `BuddyAllocator` — not an oversight, but because the seam the
// task asks probe 3 to target does not exist for it by construction (see
// the task report for the fuller version of this note).

use mm::slab::{AllocEvent, SlabAllocator};
use mm::{FrameSource, PhysMap};
use std::alloc::Layout;
use std::cell::{Cell, RefCell, UnsafeCell};
use std::collections::HashSet;
use x86_64::PhysAddr;

// ── Host scaffolding (same shape as slab.rs's own tests / slab_frame_source.rs) ──

struct VecMem {
    buf: UnsafeCell<Vec<u8>>,
}

impl VecMem {
    fn new(size: usize) -> Self {
        Self { buf: UnsafeCell::new(vec![0u8; size]) }
    }
}

unsafe impl Sync for VecMem {}

impl PhysMap for VecMem {
    fn virt_for(&self, pa: PhysAddr) -> *mut u8 {
        unsafe {
            let buf = &mut *self.buf.get();
            assert!((pa.as_u64() as usize) < buf.len(), "test PhysMap OOB: {:#x}", pa.as_u64());
            buf.as_mut_ptr().add(pa.as_u64() as usize)
        }
    }
}

/// Plain bump allocator standing in for the real buddy allocator — these
/// probes are about the `FrameSource` reentrancy SEAM itself, not about
/// buddy correctness (already covered by `buddy_invariants.rs`/
/// `slab_frame_source.rs`), so the simplest possible "hand out the next
/// never-reused range" backing is enough. Frees are no-ops (matches
/// `mm/src/slab.rs`'s own `tests::BumpFrames`).
struct BumpFrames {
    next: Cell<u64>,
    limit: u64,
}

impl BumpFrames {
    fn new(limit: u64) -> Self {
        Self { next: Cell::new(4096), limit } // start past addr 0
    }
}

impl FrameSource for BumpFrames {
    unsafe fn alloc_order(&self, order: usize) -> Option<PhysAddr> {
        let size = 1u64 << order;
        let aligned = (self.next.get() + size - 1) & !(size - 1);
        if aligned + size > self.limit {
            return None;
        }
        self.next.set(aligned + size);
        Some(PhysAddr::new(aligned))
    }
    unsafe fn free_order(&self, _addr: PhysAddr, _order: usize) {}
}

const ARENA_SIZE: u64 = 16 * 1024 * 1024; // 16 MiB — plenty for these probes' workloads

/// Everything the `ReentrantFrameSource` needs a raw handle back into: the
/// SAME `SlabAllocator` instance the outer, top-level call is already
/// mid-operation on. `UnsafeCell` (not `RefCell`/`Mutex`) is deliberate —
/// see this file's module doc comment for why a safe interior-mutability
/// wrapper would make these probes fail unconditionally, regardless of
/// whether `mm`'s own allocator code is reentrancy-safe.
struct Harness {
    mem: VecMem,
    backing: BumpFrames,
    slab: UnsafeCell<SlabAllocator>,
}

impl Harness {
    fn new() -> Self {
        Self {
            mem: VecMem::new(ARENA_SIZE as usize),
            backing: BumpFrames::new(ARENA_SIZE),
            slab: UnsafeCell::new(SlabAllocator::new()),
        }
    }

    /// # Safety
    /// Caller must not hold another live `&mut` derived from this same
    /// `UnsafeCell` across overlapping, *concurrent* (real multi-threaded)
    /// use. Sequential reentrant use from a single logical thread of
    /// control (the whole point of these probes — see module doc comment)
    /// is exactly what this harness exists to exercise.
    unsafe fn slab_mut(&self) -> &mut SlabAllocator {
        &mut *self.slab.get()
    }

    fn cache_stats_for(&self, size: usize) -> (usize, usize, usize) {
        let stats = unsafe { self.slab_mut() }.cache_stats();
        *stats.iter().find(|(sz, _, _)| *sz == size).expect("size class must exist")
    }
}

unsafe impl Sync for Harness {}

/// The reentrancy injector. Models the timer ISR: `alloc_order` is `mm`'s
/// one and only seam where caller code runs *during* an allocation already
/// in progress on a `SlabAllocator` instance (`SlabCache::expand` calls it
/// from inside `SlabAllocator::allocate`'s own call chain) — exactly where
/// a real interrupt would land if it weren't masked.
///
/// `arm(layout)` schedules exactly one reentrant nested allocation of
/// `layout`, consumed (via `.take()`) the next time `alloc_order` actually
/// runs — so re-arming before every top-level call the test driver makes
/// is harmless even when that particular call doesn't end up needing to
/// expand a cache at all. Consuming via `.take()` also bounds recursion
/// depth to exactly 1: the nested call reenters with the SAME
/// `ReentrantFrameSource`, but by then `reenter_layout` has already been
/// cleared, so if the nested allocation itself needs to expand, it just
/// forwards straight to the real backing source — an ISR firing once
/// mid-allocation, not an unbounded storm (unbounded reentry would
/// stack-overflow instead of hang/fail cleanly, a different failure mode
/// these probes aren't targeting).
struct ReentrantFrameSource<'a> {
    harness: &'a Harness,
    reenter_layout: Cell<Option<Layout>>,
    /// Every pointer a reentrant nested allocation returned, in order —
    /// collected so the test driver can fold them into its own live-set
    /// bookkeeping (see probe 2) instead of only checking the outer,
    /// top-level allocations for uniqueness.
    reenter_ptrs: RefCell<Vec<usize>>,
}

impl<'a> ReentrantFrameSource<'a> {
    fn new(harness: &'a Harness) -> Self {
        Self {
            harness,
            reenter_layout: Cell::new(None),
            reenter_ptrs: RefCell::new(Vec::new()),
        }
    }

    fn arm(&self, layout: Layout) {
        self.reenter_layout.set(Some(layout));
    }

    fn reenter_count(&self) -> usize {
        self.reenter_ptrs.borrow().len()
    }
}

impl<'a> FrameSource for ReentrantFrameSource<'a> {
    unsafe fn alloc_order(&self, order: usize) -> Option<PhysAddr> {
        if let Some(layout) = self.reenter_layout.take() {
            // The ISR fires here, mid-allocation, and needs its own small
            // heap object from the SAME SlabAllocator instance the outer
            // call is still inside — the real ab58dba shape, modeled as a
            // synchronous nested call (see module doc comment). If
            // `SlabAllocator`/`SlabCache` ever held internal state across
            // this very call (the thing the sabotage step in the task
            // report adds), this line is where it would hang.
            let slab = self.harness.slab_mut();
            let result = slab.allocate(&self.harness.mem, self, layout);
            assert!(
                !result.ptr.is_null(),
                "reentrant ISR-modeled allocation must itself succeed for this probe to mean anything"
            );
            self.reenter_ptrs.borrow_mut().push(result.ptr as usize);
        }
        self.harness.backing.alloc_order(order)
    }

    unsafe fn free_order(&self, addr: PhysAddr, order: usize) {
        self.harness.backing.free_order(addr, order);
    }
}

// ── Probe 1 ──────────────────────────────────────────────────────────────

/// FAMILY A (reentrancy probe): deterministic, single-threaded, no
/// `std::thread`. Models the timer ISR firing mid-allocation and itself
/// needing a small heap object from the SAME global allocator instance —
/// the shape of commit ab58dba's real bug (see this file's module doc
/// comment for the precise scope: `mm` itself owns no lock, so this cannot
/// reproduce ab58dba verbatim; it guards against `mm` ever regaining
/// internal, non-reentrant-safe state across the `FrameSource` seam).
///
/// Models the kernel's SINGLE vCPU, not SMP contention — there is exactly
/// one logical thread of control here, and the "reentrancy" is a
/// synchronous nested call standing in for an asynchronous interrupt, not
/// concurrent access from a second core.
///
/// **Expected failure mode if this ever regresses: a HANG, not a panic** —
/// see this file's module doc comment for why no watchdog/timeout is added
/// here; that hang IS the signal, the same way it is in the
/// `vfs::mount` template this file imitates. Verified against a real
/// regression in the accompanying task report's sabotage step (a busy-flag
/// added to `SlabCache`, held across `frames.alloc_order()`, makes this
/// exact test spin forever — `timeout 60 cargo test ... ; echo EXIT=$?`
/// reports 124).
#[test]
fn frame_source_reentering_the_allocator_does_not_self_deadlock() {
    let harness = Harness::new();
    let frames = ReentrantFrameSource::new(&harness);
    let outer_layout = Layout::from_size_align(8, 8).unwrap();
    let reentrant_layout = Layout::from_size_align(8, 8).unwrap();

    // Fresh SlabAllocator: the very first allocation always needs to
    // expand its cache (free_list starts empty), which guarantees
    // `alloc_order` — and so the armed reentrant call — actually fires.
    frames.arm(reentrant_layout);

    let result = unsafe { harness.slab_mut().allocate(&harness.mem, &frames, outer_layout) };

    assert!(!result.ptr.is_null(), "outer allocation must succeed");
    assert_eq!(frames.reenter_count(), 1, "the reentrant callback must have fired exactly once");
    assert_ne!(
        result.ptr as usize,
        frames.reenter_ptrs.borrow()[0],
        "the outer and reentrant allocations must not alias the same object"
    );
    // If control ever reaches this line, the reentrant call above did NOT
    // hang — that IS the pass condition for this probe.
}

// ── Probe 2 ──────────────────────────────────────────────────────────────

/// FAMILY A (reentrancy probe): deterministic, single-threaded, no
/// `std::thread`. Drives the 8-byte cache through several real expansions
/// (`AllocEvent::Expand`) by sheer volume, injecting one bounded reentrant
/// nested allocation (same ISR-modeling shape as probe 1) into EVERY one
/// of them, then checks the allocator's own bookkeeping stayed consistent:
///
///   - every live pointer handed out — outer AND reentrant — is pairwise
///     distinct (no aliasing between what the "interrupted" code holds and
///     what the "ISR" holds);
///   - `cache_stats()`'s `used` count for the 8-byte class matches exactly
///     how many distinct live objects this test actually holds;
///   - every reentrant nested call actually happened exactly once per
///     expansion (`reenter_count() == expansions`) — i.e. the bounded,
///     one-per-expansion shape this harness is built around held for the
///     WHOLE run, not just the first expansion probe 1 exercises.
///
/// Models the single vCPU kernel, not SMP contention (see probe 1's doc
/// comment). Unlike probe 1, the expected failure mode here is a plain
/// **assertion failure, not a hang**: a corrupted free-list link (for
/// example, an `expand()` that captured a stale snapshot of
/// `self.free_list` *before* calling `frames.alloc_order()` and then
/// chained its own newly-built objects onto that stale snapshot instead of
/// onto whatever the reentrant call had already linked in) silently
/// ORPHANS objects rather than looping forever — `cache_stats` would then
/// report more `total` objects than are actually reachable, which this
/// probe would catch as either a `used`/live-set mismatch or a duplicate
/// pointer, depending on exactly how the corruption manifests. Both a hang
/// and a plain failure are valid Family-A outcomes per this task's brief;
/// this probe's designed failure mode is the latter.
#[test]
fn expansion_path_reentering_the_frame_source_stays_consistent() {
    let harness = Harness::new();
    let frames = ReentrantFrameSource::new(&harness);
    let layout = Layout::from_size_align(8, 8).unwrap();

    let mut seen = HashSet::new();
    let mut expansions: usize = 0;

    // Enough iterations to force several real expansions of the 8-byte
    // cache (each page holds hundreds of 8-byte slots, fewer under
    // `--features slab-debug`'s doubled slot size — either way, comfortably
    // more than one expansion over 2000 allocations). `arm()` before every
    // call is harmless on the calls that don't end up expanding — the
    // scheduled reentrant allocation just waits for the next one that
    // does.
    for _ in 0..2000 {
        frames.arm(layout);
        let result = unsafe { harness.slab_mut().allocate(&harness.mem, &frames, layout) };
        assert!(!result.ptr.is_null(), "OOM allocating 8-byte objects in a 16 MiB arena is unexpected");
        assert!(
            seen.insert(result.ptr as usize),
            "outer allocation returned a pointer that is already live"
        );
        if matches!(result.event, AllocEvent::Expand(t) if t.ok) {
            expansions += 1;
        }
    }

    assert!(
        expansions >= 2,
        "test setup: expected multiple expansions to actually exercise this probe, got {expansions}"
    );
    assert_eq!(
        frames.reenter_count(),
        expansions,
        "every expansion should have carried exactly one reentrant nested allocation"
    );

    // Fold the reentrant allocations into the same live-set uniqueness
    // check — this is where an aliasing bug between the "interrupted" and
    // "ISR" allocations would show up.
    for &p in frames.reenter_ptrs.borrow().iter() {
        assert!(
            seen.insert(p),
            "a reentrant nested allocation returned a pointer some outer allocation also holds live"
        );
    }

    let (_, total, used) = harness.cache_stats_for(8);
    assert_eq!(
        used, seen.len(),
        "cache_stats used must match the total number of distinct live objects (outer + reentrant)"
    );
    assert!(total >= used, "total objects must never be less than used objects");

    // The discriminating check for the stale-snapshot corruption class:
    // `used`/`total` bookkeeping alone does NOT catch it (`used_objects`
    // increments correctly on every successful pop regardless of whether
    // the free-list chain itself is intact, and `total_objects` counts
    // every object ever carved out of a page whether or not it stayed
    // reachable — both hold trivially even when most of a page's objects
    // were silently orphaned). What a stale `self.free_list` snapshot
    // actually breaks is REACHABILITY: some objects `cache_stats` counts
    // as free are not actually retrievable without a further expansion.
    // Drain up to `claimed_free` more allocations (capped, so a badly
    // corrupted run doesn't blow up this test's own runtime/arena) and
    // assert NONE of them needed a fresh expansion — that is exactly what
    // "claimed free" must mean.
    let claimed_free = (total - used).min(4000);
    for i in 0..claimed_free {
        let result = unsafe { harness.slab_mut().allocate(&harness.mem, &frames, layout) };
        assert!(
            !result.ptr.is_null(),
            "drain #{i}/{claimed_free}: cache_stats claimed at least this many free objects, got OOM"
        );
        assert!(
            matches!(result.event, AllocEvent::None),
            "drain #{i}/{claimed_free}: cache_stats claimed this object was already free, but \
             allocate() needed a fresh expansion instead (event={:?}) — some previously \"free\" \
             object is unreachable, exactly the shape a stale free-list snapshot across a \
             reentrant FrameSource call would cause",
            result.event
        );
        assert!(seen.insert(result.ptr as usize), "drain #{i}: pointer already live elsewhere");
    }
}
