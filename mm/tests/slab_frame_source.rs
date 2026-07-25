// mm/tests/slab_frame_source.rs
//
// Second half of the double-issue hunt (see `tests/buddy_invariants.rs`'s
// doc comment for the full context): exercises `SlabAllocator` sitting on
// top of a REAL `BuddyAllocator` — not the crate's own unit-test
// `BumpFrames` stub, which never frees anything and so could never surface
// this bug — through an instrumented `FrameSource` that records every
// physical frame it hands out and screams the instant the same physical
// range gets delivered twice while still live. This is precisely the
// composition the boot-time crash happens in: `busybox --install`'s
// `fork()` drives kernel heap traffic (small allocations expanding slab
// caches, occasional large ones going straight to the buddy) that ends,
// per the live hypothesis, with the slab handing a caller a pointer that
// backs a physical frame the buddy *also* just handed to someone else.
//
// Same rules as `buddy_invariants.rs`: no external randomized-testing
// crate, a small hand-rolled PRNG with fixed seeds, and any detected
// double-issue is a hard, deterministically reproducible test failure
// (seed + operation index in the panic message).
//
// ## A second, DIFFERENT real bug surfaced along the way
//
// Running this file's original mixed (small+large) property test against
// real workloads reliably (not flakily — every run in this environment,
// same seed, same operation index) hits an *unrelated* panic first, from
// inside `mm/src/slab.rs` itself: `SlabCache::allocate`'s debug-only
// "Use-after-free detected" check (`src/slab.rs:308`). Investigated (not
// fixed — see the hard rules in the task brief) by catching the panic and
// dumping the raw bytes at the flagged pointer before anything else could
// touch them:
//
//   panicked at src/slab.rs:308:21: Use-after-free detected at 0x7f86010dac10
//   32 bytes at that pointer: [10, aa, 0d, 01, 86, 7f, 00, 00, 00, ...]
//
// Those first 8 bytes are NOT a uniform 0xAA poison fill (which is what a
// genuine "this object was already handed out" would look like — the
// pre-extraction `allocate()` fills up to 256 bytes with 0xAA when it
// hands an object out). They're the little-endian encoding of
// `0x7f86010daa10` — a real, valid pointer exactly `size_of::<usize>() *
// object_size` (here: 0x200 = 512 bytes, matching the size class in play)
// below the flagged address. That's the PREVIOUS free object's address —
// i.e. a perfectly legitimate `FreeObject::next` link.
//
// Root cause, read directly out of `src/slab.rs`: `SlabCache::deallocate`
// (line ~328) poisons the object with 0xDD, then immediately overwrites
// the object's own first 8 bytes with `FreeObject { next: old_head }` to
// re-link it into the free list — clobbering the very bytes the 0xDD fill
// just wrote. `SlabCache::allocate`'s UAF check (line ~304) then reads
// exactly `object_size.min(8)` bytes of the object at the free-list
// head — for every size class here (minimum 8 bytes) that's *always*
// precisely those same 8 bytes, i.e. the `next` pointer itself, never the
// real poison. So the check isn't testing "was this object actually
// freed" at all; it's testing "does this valid, in-use linked-list
// pointer happen to contain the byte 0xAA anywhere in its 8-byte
// representation" — true for roughly 1-(255/256)^8 ≈ 3% of *legitimate*
// pointers, independent of the seed, and essentially certain to trigger
// eventually under any workload that churns a slab cache's free list.
//
// **Independently re-confirmed, and directly correlated with the real
// boot-time crash this whole investigation is about:** a second
// instrumentation pass reproduced the identical pattern —
// object `0x7f028e0dac10`, first 8 bytes `[10, aa, 0d, 8e, 02, 7f, 00,
// 00]` = the valid successor address `0x7f028e0daa10` — AND found the same
// shape in a real QEMU boot capture of the production panic: object
// `0x2801e80ac00` in the 512-byte cache, successor at `0x2801e80aa00`.
// Same relative offset, same coincidental `aa` byte, same size class. This
// is very likely a real, previously-unknown contributor to (or possibly
// the entire explanation for) the debug-only `busybox_install_fork_flake`
// panic described in the boot investigation ("un panic de use-after-free
// detectado por el propio slab" — see CLAUDE.md) — it requires no buddy
// misbehavior at all, only enough alloc/free churn on one size class, and
// is gated by the exact same `#[cfg(debug_assertions)]` the flake is
// reported as exclusive to.
//
// This file does NOT patch `slab.rs` (out of scope for this task, and an
// explicit hard rule). Instead, three tests below:
//   - `slab_large_objects_over_real_buddy_never_double_issue_a_frame` runs
//     TODAY, unblocked, green — large (>2048 byte) allocations go straight
//     through `SlabAllocator::allocate_large`/`deallocate_large` to the
//     `FrameSource`, never touching `SlabCache`'s free list or its poison
//     check at all, so it can exercise the actual frame-double-issue
//     invariant this whole file exists to test, for real, right now.
//   - `slab_over_real_buddy_never_double_issues_a_frame` (the full,
//     originally-intended small+large mixed workload) is marked
//     `#[ignore]` — see its own doc comment for exactly why and what
//     unblocks it.
//   - `uaf_false_positive_repro_seed_0xb` (marked `#[ignore]`) captures the
//     minimal, concrete reproduction — seed `0xb`, no wrapping — so
//     `cargo test --test slab_frame_source uaf_false_positive_repro_seed_0xb
//     -- --ignored --nocapture` reproduces the exact panic above on demand.
//
// `safe_allocate` (the `catch_unwind`-based wrapper that classifies this
// exact panic and lets everything else propagate) still exists and still
// backs the ignored mixed test — kept as working infrastructure for the
// day `slab.rs`'s poison check gets fixed and that test can be
// un-ignored, rather than deleted and rewritten later.

use mm::buddy::BuddyAllocator;
use mm::slab::SlabAllocator;
use mm::{FrameSource, PhysMap};
use std::alloc::Layout;
use std::cell::{RefCell, UnsafeCell};
use std::collections::BTreeMap;
use std::panic::{self, AssertUnwindSafe};
use x86_64::PhysAddr;

// ============================================================================
// Host PhysMap (same shape as buddy_invariants.rs / buddy.rs's own tests).
// ============================================================================

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

// ============================================================================
// Instrumented FrameSource — wraps a REAL BuddyAllocator (not a bump
// allocator) and tracks every physical range currently outstanding. Any
// `alloc_order` call whose returned range overlaps a still-outstanding one
// is exactly the bug under investigation: the same physical frame(s)
// delivered twice, one copy still backing whatever the first caller put
// there (in production: live kernel heap data).
// ============================================================================

struct TrackingFrameSource<'a> {
    buddy: RefCell<&'a mut BuddyAllocator>,
    mem: &'a VecMem,
    /// start_addr -> (end_addr_exclusive, order) for every frame currently
    /// out on loan to the slab allocator.
    outstanding: RefCell<BTreeMap<u64, (u64, usize)>>,
    seed: u64,
    op: RefCell<u64>,
}

impl<'a> TrackingFrameSource<'a> {
    fn new(buddy: &'a mut BuddyAllocator, mem: &'a VecMem, seed: u64) -> Self {
        Self {
            buddy: RefCell::new(buddy),
            mem,
            outstanding: RefCell::new(BTreeMap::new()),
            seed,
            op: RefCell::new(0),
        }
    }

    fn tick(&self) -> u64 {
        let mut op = self.op.borrow_mut();
        *op += 1;
        *op
    }
}

impl<'a> FrameSource for TrackingFrameSource<'a> {
    unsafe fn alloc_order(&self, order: usize) -> Option<PhysAddr> {
        let addr = self.buddy.borrow_mut().allocate(self.mem, order)?;
        let op = self.tick();
        let a = addr.as_u64();
        let end = a + (1u64 << order);

        let mut outstanding = self.outstanding.borrow_mut();

        // Same overlap check as buddy_invariants.rs's Model::record_alloc —
        // check both neighbors in the outstanding map. This is the
        // authoritative double-issue detector for this file: it fires the
        // instant the buddy hands out an overlapping frame, strictly
        // before the slab could do anything with it (see this module's
        // doc comment on why that ordering matters for `safe_allocate`).
        if let Some((&p_start, &(p_end, p_order))) = outstanding.range(..=a).next_back() {
            assert!(
                p_end <= a,
                "seed {} op {op}: DOUBLE-ISSUE via FrameSource — order-{order} frame \
                 [{a:#x}, {end:#x}) overlaps still-outstanding order-{p_order} frame \
                 [{p_start:#x}, {p_end:#x}) that the buddy allocator already handed to \
                 a different caller and never got back",
                self.seed
            );
        }
        if let Some((&s_start, &(s_end, s_order))) = outstanding.range(a..).next() {
            assert!(
                s_start >= end,
                "seed {} op {op}: DOUBLE-ISSUE via FrameSource — order-{order} frame \
                 [{a:#x}, {end:#x}) overlaps still-outstanding order-{s_order} frame \
                 [{s_start:#x}, {s_end:#x}) that the buddy allocator already handed to \
                 a different caller and never got back",
                self.seed
            );
        }

        outstanding.insert(a, (end, order));
        Some(addr)
    }

    unsafe fn free_order(&self, addr: PhysAddr, order: usize) {
        let op = self.tick();
        let a = addr.as_u64();
        {
            let mut outstanding = self.outstanding.borrow_mut();
            let removed = outstanding.remove(&a);
            assert_eq!(
                removed,
                Some((a + (1u64 << order), order)),
                "seed {} op {op}: freed frame {a:#x} order {order} does not match what \
                 was recorded as outstanding ({removed:?}) — frame-tracking bug in the \
                 test itself, or the caller freed something it never received",
                self.seed
            );
        }
        self.buddy.borrow_mut().deallocate(self.mem, addr, order);
        // Any PhantomEvent from this deallocate would already be a bug per
        // buddy_invariants.rs's invariant 5 — not re-checked here since
        // that file covers the buddy in isolation exhaustively; this file's
        // job is the slab<->buddy composition (overlap of delivered
        // frames), which the check above already covers.
    }
}

// ============================================================================
// Known-issue-aware allocate wrapper — see this module's doc comment.
// ============================================================================

/// Outcome of a guarded `SlabAllocator::allocate` call.
enum AllocOutcome {
    Ok(mm::slab::AllocResult),
    /// The call hit the known `src/slab.rs:308` false-positive UAF panic
    /// (see module doc comment) instead of returning normally.
    KnownUafFalsePositive,
}

/// Calls `slab.allocate(...)`, but classifies the specific, already-
/// diagnosed false-positive panic from `SlabCache::allocate`'s debug-mode
/// poison check instead of letting it abort the whole test run. Any OTHER
/// panic (in particular anything from `TrackingFrameSource`'s own overlap
/// asserts, which is what this whole file exists to catch) is
/// `resume_unwind`'d unchanged and still fails the test.
fn safe_allocate(
    slab: &mut SlabAllocator,
    mem: &VecMem,
    frames: &TrackingFrameSource,
    layout: Layout,
) -> AllocOutcome {
    let result = panic::catch_unwind(AssertUnwindSafe(|| unsafe { slab.allocate(mem, frames, layout) }));
    match result {
        Ok(r) => AllocOutcome::Ok(r),
        Err(payload) => {
            let msg = panic_payload_message(&payload);
            if msg.contains("Use-after-free detected") {
                AllocOutcome::KnownUafFalsePositive
            } else {
                panic::resume_unwind(payload);
            }
        }
    }
}

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    String::new()
}

/// Unwrapped allocate — calls straight through with no `catch_unwind`. If
/// `SlabCache::allocate`'s debug-mode UAF check panics, that panic
/// propagates normally and fails whatever test called this. Used only by
/// `uaf_false_positive_repro_seed_0xb`, which specifically wants the raw
/// panic to surface rather than be classified/swallowed by
/// `safe_allocate`.
fn raw_allocate(
    slab: &mut SlabAllocator,
    mem: &VecMem,
    frames: &TrackingFrameSource,
    layout: Layout,
) -> AllocOutcome {
    AllocOutcome::Ok(unsafe { slab.allocate(mem, frames, layout) })
}

// ============================================================================
// Tiny deterministic PRNG — same xorshift64* as buddy_invariants.rs,
// duplicated rather than shared (each `tests/*.rs` file compiles as its own
// independent crate, so there's no natural place to share it without
// touching `src/`).
// ============================================================================

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(if seed == 0 { 0xdead_beef_cafe_babe } else { seed })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound
    }

    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
}

// ============================================================================
// Random size classes for allocate() calls — weighted heavily toward small
// objects (which expand slab caches, page-granular FrameSource traffic)
// with an occasional large object (which goes straight to the FrameSource
// at a multi-page order), per the brief's "Mezcla asignaciones pequeñas
// ... con grandes".
// ============================================================================

const SMALL_SIZES: &[usize] = &[8, 16, 16, 32, 32, 64, 64, 128, 256, 512, 1024, 2048];
const LARGE_SIZES: &[usize] = &[4096, 8192, 16384, 4096, 4096, 32768];

fn random_layout(rng: &mut Rng) -> Layout {
    let size = if rng.chance(15) {
        LARGE_SIZES[rng.below(LARGE_SIZES.len() as u64) as usize]
    } else {
        SMALL_SIZES[rng.below(SMALL_SIZES.len() as u64) as usize]
    };
    Layout::from_size_align(size, size.min(4096)).unwrap()
}

/// Region backing the buddy allocator under the slab: 32 MiB, comfortably
/// bigger than the 16 MiB used in `buddy_invariants.rs` since large-object
/// requests here can reach 32 KiB and there are many more small ones
/// churning page-sized frames underneath via cache expansion.
const REGION_SIZE: u64 = 32 * 1024 * 1024;

const SEEDS: &[u64] = &[11, 22, 333, 4444, 0xC0DE, 55_555, 0x9E37_79B9, 271_828];

struct SeedOutcome {
    ops: u64,
    /// Set if this seed's run was cut short by the known slab.rs
    /// false-positive UAF panic (see module doc comment) rather than
    /// running to completion. Reported, not hidden — see the final test's
    /// summary line.
    hit_known_uaf_false_positive: bool,
}

// The `mixed_phase!` macro below assigns `hit_known_issue = true` at each of
// its three call sites; at the first call site the function returns
// immediately afterward without reading the variable again (it returns the
// macro's own `true` directly), which trips `unused_assignments` there even
// though the later call sites do read it. Harmless — silenced at the
// function level rather than restructuring three call sites around one
// linter false-positive of our own.
#[allow(unused_assignments)]
fn run_seed(seed: u64, allocate: fn(&mut SlabAllocator, &VecMem, &TrackingFrameSource, Layout) -> AllocOutcome) -> SeedOutcome {
    let mem = VecMem::new(REGION_SIZE as usize);
    let mut buddy = BuddyAllocator::new();
    unsafe { buddy.add_region(&mem, 0, REGION_SIZE) };

    let frames = TrackingFrameSource::new(&mut buddy, &mem, seed);
    let mut slab = SlabAllocator::new();
    let mut rng = Rng::new(seed);

    // (ptr, layout) for everything currently allocated through the slab —
    // needed so deallocate() gets called with the exact layout that was
    // used for the matching allocate() (slab_index requires that
    // symmetry).
    let mut live: Vec<(*mut u8, Layout)> = Vec::new();
    let mut ops: u64 = 0;
    let mut hit_known_issue = false;

    // Mixed phase: mostly allocate, occasionally free a random live
    // object — same "not FIFO/LIFO" shape as buddy_invariants.rs's
    // random_live. Returns `true` if it should stop early (hit the known
    // issue).
    macro_rules! mixed_phase {
        ($n:expr) => {{
            let mut stop = false;
            for _ in 0..$n {
                ops += 1;
                if live.is_empty() || rng.chance(65) {
                    let layout = random_layout(&mut rng);
                    match allocate(&mut slab, &mem, &frames, layout) {
                        AllocOutcome::Ok(result) => {
                            if !result.ptr.is_null() {
                                live.push((result.ptr, layout));
                            }
                            // A null ptr means OOM inside this run's fixed
                            // region — legitimate, not a bug.
                        }
                        AllocOutcome::KnownUafFalsePositive => {
                            hit_known_issue = true;
                            stop = true;
                            break;
                        }
                    }
                } else {
                    let idx = rng.below(live.len() as u64) as usize;
                    let (ptr, layout) = live.swap_remove(idx);
                    unsafe { slab.deallocate(&mem, &frames, ptr, layout) };
                }
            }
            stop
        }};
    }

    // Phase 1: general mixed traffic.
    if mixed_phase!(4000u64) {
        return SeedOutcome { ops, hit_known_uaf_false_positive: true };
    }

    // Phase 2: exhaustion burst — allocate small objects until the region
    // is OOM (driving many cache expansions, i.e. many FrameSource
    // alloc_order(12) calls), then free everything just accumulated across
    // the whole run in shuffled order (mass free -> heavy buddy
    // coalescing underneath, same shape as buddy_invariants.rs).
    loop {
        ops += 1;
        let layout = Layout::from_size_align(64, 64).unwrap();
        match allocate(&mut slab, &mem, &frames, layout) {
            AllocOutcome::Ok(result) => {
                if result.ptr.is_null() {
                    break;
                }
                live.push((result.ptr, layout));
            }
            AllocOutcome::KnownUafFalsePositive => {
                hit_known_issue = true;
                break;
            }
        }
    }

    if !hit_known_issue {
        for i in (1..live.len()).rev() {
            let j = rng.below((i + 1) as u64) as usize;
            live.swap(i, j);
        }
        while let Some((ptr, layout)) = live.pop() {
            ops += 1;
            unsafe { slab.deallocate(&mem, &frames, ptr, layout) };
        }

        // Phase 3: more mixed traffic on the now-mostly-free region, to
        // churn through reuse of the frames just freed (exactly where a
        // stale pointer/overlap bug would resurface).
        hit_known_issue = mixed_phase!(4000u64);
    }

    // Drain whatever's left live so the outstanding-frames check below is
    // meaningful (skipped entirely if the known issue already cut the run
    // short, since slab/buddy internal state past a caught panic is not
    // trusted for further calls).
    if !hit_known_issue {
        for (ptr, layout) in live.drain(..) {
            ops += 1;
            unsafe { slab.deallocate(&mem, &frames, ptr, layout) };
        }
        assert!(
            frames.outstanding.borrow().is_empty(),
            "seed {seed}: {} frame(s) still outstanding after draining every live slab \
             object — either a leak in this test's own bookkeeping or the slab allocator \
             freed a frame without going through FrameSource::free_order",
            frames.outstanding.borrow().len()
        );
    }

    SeedOutcome { ops, hit_known_uaf_false_positive: hit_known_issue }
}

/// Full small+large mixed workload — this is the originally-intended,
/// most thorough version of the frame-double-issue property test (per the
/// task brief's "Mezcla asignaciones pequeñas ... con grandes"), driving
/// `SlabAllocator` through its cache-expansion path (small objects) AND
/// its direct-`FrameSource` path (large objects) in the same run.
///
/// **Blocked, not broken:** every seed here reliably hits the KNOWN,
/// separately-diagnosed `mm/src/slab.rs:308` false-positive UAF panic
/// (see this file's module doc comment) within a few thousand operations
/// of small-object cache churn — a real, pre-existing bug in `slab.rs`
/// itself, unrelated to what THIS test measures. `safe_allocate` already
/// classifies and routes around that exact panic (letting anything else,
/// in particular a genuine `TrackingFrameSource` double-issue assert,
/// still fail the test loudly) — but because the false positive triggers
/// so early and so reliably, every seed's run is currently cut short
/// long before it reaches the deep exhaustion/mass-coalescing phases this
/// test is meant to exercise. Ignored rather than left "passing" on a
/// severely truncated run that would misrepresent how much of the
/// invariant space actually got covered.
///
/// **What unblocks this:** fixing `mm/src/slab.rs`'s poison check (make
/// `SlabCache::allocate` inspect real poison bytes instead of the
/// `FreeObject::next` link that currently clobbers them — see the module
/// doc comment) is explicitly out of scope for this task. Once that's
/// fixed elsewhere, remove the `#[ignore]` here — no other change to this
/// test should be needed, since `safe_allocate`'s classification simply
/// stops finding anything to classify.
///
/// Run explicitly today with:
///   cargo test --test slab_frame_source slab_over_real_buddy_never_double_issues_a_frame -- --ignored --nocapture
#[test]
#[ignore = "blocked by the REAL, separately-documented mm/src/slab.rs:308 UAF \
            false-positive (see this file's module doc comment and this test's own \
            doc comment) — every seed hits it within a few thousand small-object ops, \
            long before the deep exhaustion/coalescing phases this test wants to cover. \
            NOT a bug in this test, NOT the buddy double-issue bug it's designed to \
            catch. See `slab_large_objects_over_real_buddy_never_double_issue_a_frame` \
            for the subset of this coverage (large objects, direct FrameSource path) \
            that runs clean today. Un-ignore once slab.rs's poison check is fixed."]
fn slab_over_real_buddy_never_double_issues_a_frame() {
    let mut total_ops: u64 = 0;
    let mut known_issue_seeds: Vec<u64> = Vec::new();
    for &seed in SEEDS {
        let outcome = run_seed(seed, safe_allocate);
        total_ops += outcome.ops;
        if outcome.hit_known_uaf_false_positive {
            known_issue_seeds.push(seed);
        }
    }
    eprintln!(
        "slab_over_real_buddy_never_double_issues_a_frame: {} seeds, {} total slab \
         allocate/deallocate operations, zero frame double-issues detected via \
         TrackingFrameSource (that is this test's actual pass/fail criterion). \
         {} seed(s) were cut short by the KNOWN, separately-documented slab.rs UAF \
         false-positive (see this file's module doc comment): {:?}",
        SEEDS.len(),
        total_ops,
        known_issue_seeds.len(),
        known_issue_seeds
    );
}

// ============================================================================
// Large-object-only variant — runs TODAY, unblocked by the slab.rs poison
// check, because large (> MAX_SLAB_SIZE) requests never touch a
// `SlabCache`'s free list at all: `SlabAllocator::allocate`/`deallocate`
// dispatch them straight to `allocate_large`/`deallocate_large`, which go
// directly to the `FrameSource` (see `src/slab.rs`'s `size > MAX_SLAB_SIZE`
// branch). This still genuinely exercises the composition under test —
// real multi-page `FrameSource::alloc_order`/`free_order` traffic against
// a real `BuddyAllocator`, checked by the same `TrackingFrameSource`
// overlap assert as the full mixed test — just without small-object cache
// churn in the mix.
// ============================================================================

fn run_seed_large_only(seed: u64) -> u64 {
    let mem = VecMem::new(REGION_SIZE as usize);
    let mut buddy = BuddyAllocator::new();
    unsafe { buddy.add_region(&mem, 0, REGION_SIZE) };

    let frames = TrackingFrameSource::new(&mut buddy, &mem, seed);
    let mut slab = SlabAllocator::new();
    let mut rng = Rng::new(seed);
    let mut live: Vec<(*mut u8, Layout)> = Vec::new();
    let mut ops: u64 = 0;

    let mixed_phase = |n: u64,
                            slab: &mut SlabAllocator,
                            live: &mut Vec<(*mut u8, Layout)>,
                            rng: &mut Rng,
                            ops: &mut u64| {
        for _ in 0..n {
            *ops += 1;
            if live.is_empty() || rng.chance(60) {
                let size = LARGE_SIZES[rng.below(LARGE_SIZES.len() as u64) as usize];
                let layout = Layout::from_size_align(size, size.min(4096)).unwrap();
                let result = unsafe { slab.allocate(&mem, &frames, layout) };
                if !result.ptr.is_null() {
                    live.push((result.ptr, layout));
                }
                // Null == legitimate OOM in this run's fixed region.
            } else {
                let idx = rng.below(live.len() as u64) as usize;
                let (ptr, layout) = live.swap_remove(idx);
                unsafe { slab.deallocate(&mem, &frames, ptr, layout) };
            }
        }
    };

    // Phase 1: general mixed large-object traffic.
    mixed_phase(1500, &mut slab, &mut live, &mut rng, &mut ops);

    // Phase 2: exhaustion burst on one fixed large size, then mass free in
    // shuffled order — same "fases de agotamiento seguidas de liberación
    // masiva" shape as buddy_invariants.rs, exercising the buddy's
    // coalescing hard underneath direct FrameSource traffic.
    let layout = Layout::from_size_align(8192, 4096).unwrap();
    loop {
        ops += 1;
        let result = unsafe { slab.allocate(&mem, &frames, layout) };
        if result.ptr.is_null() {
            break;
        }
        live.push((result.ptr, layout));
    }
    for i in (1..live.len()).rev() {
        let j = rng.below((i + 1) as u64) as usize;
        live.swap(i, j);
    }
    while let Some((ptr, layout)) = live.pop() {
        ops += 1;
        unsafe { slab.deallocate(&mem, &frames, ptr, layout) };
    }

    // Phase 3: more mixed traffic reusing the just-freed frames.
    mixed_phase(1500, &mut slab, &mut live, &mut rng, &mut ops);

    for (ptr, layout) in live.drain(..) {
        ops += 1;
        unsafe { slab.deallocate(&mem, &frames, ptr, layout) };
    }
    assert!(
        frames.outstanding.borrow().is_empty(),
        "seed {seed}: {} frame(s) still outstanding after draining every live large \
         allocation",
        frames.outstanding.borrow().len()
    );

    ops
}

#[test]
fn slab_large_objects_over_real_buddy_never_double_issue_a_frame() {
    let mut total_ops: u64 = 0;
    for &seed in SEEDS {
        total_ops += run_seed_large_only(seed);
    }
    eprintln!(
        "slab_large_objects_over_real_buddy_never_double_issue_a_frame: {} seeds, {} \
         total large-object allocate/deallocate operations, zero frame double-issues \
         detected via TrackingFrameSource",
        SEEDS.len(),
        total_ops
    );
}

/// Minimal, concrete, deterministic reproduction of the false-positive
/// slab.rs bug documented at the top of this file — seed `0xb` (11),
/// `raw_allocate` (no `catch_unwind`, no classification), so the real
/// `src/slab.rs:308` panic is left to surface and fail this test exactly
/// as first observed:
///
///   panicked at src/slab.rs:308:21: Use-after-free detected at 0x7f86...
///   (op 2386 in this exact sequence, layout size=512 — the object
///   size class in play when it happens; the precise op count was stable
///   across repeated process runs in this environment, but it is a
///   byte-pattern coincidence in a raw heap pointer's value, not something
///   this test's own PRNG seed actually controls, so it may land on a
///   different op count — or need more seeds/iterations — on a platform
///   where the host allocator hands out different addresses)
///
/// This is a REAL pre-existing bug in `mm/src/slab.rs`, not in this test
/// or in the buddy allocator — see the module doc comment for the full
/// root-cause analysis (the free-list `next` pointer clobbers the poison
/// bytes the UAF check reads). Deliberately left failing-when-run and
/// `#[ignore]`d rather than fixed, per the task's hard rules: running this
/// test is EXPECTED to end in `test result: FAILED` with the panic above,
/// not a clean pass.
///
/// Reproduce with:
///   cargo test --test slab_frame_source uaf_false_positive_repro_seed_0xb -- --ignored --nocapture
#[test]
#[ignore = "reproduces a REAL pre-existing false-positive bug in mm/src/slab.rs's \
            debug-mode UAF check (src/slab.rs:308) — NOT the buddy double-issue this \
            file's main test hunts, and NOT a bug in this test. See this file's module \
            doc comment for the full root-cause analysis. Left ignored rather than \
            fixed per the task's hard rules. EXPECTED TO PANIC/FAIL when run explicitly: \
            `cargo test --test slab_frame_source uaf_false_positive_repro_seed_0xb -- \
            --ignored --nocapture`."]
fn uaf_false_positive_repro_seed_0xb() {
    let outcome = run_seed(0xb, raw_allocate);
    // If we get here without panicking, the known false positive did not
    // reproduce in this run/environment — fail loudly with the op count so
    // that's visible rather than silently reporting a pass for a repro
    // test whose entire point is to reproduce a specific crash.
    panic!(
        "expected the real mm/src/slab.rs:308 'Use-after-free detected' panic to \
         interrupt this run; instead it completed {} ops cleanly (hit_known_uaf_false_positive={}). \
         The byte-pattern coincidence this repro depends on did not occur in this \
         environment — see module doc comment.",
        outcome.ops, outcome.hit_known_uaf_false_positive
    );
}
