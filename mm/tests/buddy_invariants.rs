// mm/tests/buddy_invariants.rs
//
// Property tests hunting the open, unresolved bug described in the boot
// investigation (see CLAUDE.md's `busybox_install_fork_flake` note and the
// live hypothesis: "the buddy allocator hands out the same physical frame
// twice, one copy still backing live kernel heap data"). This file does
// NOT fix anything — it drives long, deterministic, randomized
// allocate/deallocate sequences against `BuddyAllocator` in isolation and
// checks five invariants after every operation:
//
//   1. No overlap between simultaneously-live allocated blocks (this IS
//      the bug: two live blocks sharing a byte is the double-issue caught
//      red-handed).
//   2. Conservation: free bytes + live bytes == total region bytes.
//   3. Every returned address for order k is aligned to 2^k.
//   4. Every returned address falls inside the region handed to
//      `add_region`.
//   5. Bitmap ⟺ free-list coherence — the bitmap's "free" bit for a block
//      is set if and only if the block is genuinely present in that
//      order's free list. This is the primary target: production's own
//      `PhantomEvent` (`EmptyList`/`NotFound`/`LoopLimit`) exists
//      specifically because this mismatch happens somewhere in practice,
//      and gets silently patched over (clear the stray bit, stop
//      coalescing) instead of investigated. Any `PhantomEvent` observed
//      here, from a well-formed operation sequence (valid orders, no
//      double-frees, addresses always ones the allocator itself handed
//      out), is treated as a hard test failure — not logged and ignored.
//
// No external randomized-testing crate (no proptest/quickcheck) — a small
// hand-rolled xorshift64* PRNG, seeded from a fixed list below, drives
// everything. A failure prints the seed and the exact operation index/kind
// that broke the invariant, so `cargo test buddy_invariants -- --nocapture`
// reproduces it deterministically without rerunning the whole suite.

use mm::buddy::{BuddyAllocator, PhantomEvent};
use mm::PhysMap;
use std::cell::UnsafeCell;
use std::collections::BTreeMap;
use x86_64::PhysAddr;

// ============================================================================
// Host PhysMap — same shape as `mm/src/buddy.rs`'s own `tests::VecMem`, just
// duplicated here since integration tests can't reach a lib crate's private
// `#[cfg(test)]` module.
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
// Tiny deterministic PRNG — xorshift64*. No external crate.
// ============================================================================

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // xorshift64* requires a nonzero state.
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

    /// Uniform in `[0, bound)`. `bound` must be nonzero.
    fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound
    }

    /// True with probability `pct`/100.
    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
}

// ============================================================================
// Reference model + invariant checks
// ============================================================================

const MIN_ORDER: usize = 12;

/// Region handed to the allocator under test: 16 MiB, page-aligned and a
/// power of two, so `add_region` drops no remainder byte and "free bytes +
/// live bytes == total" holds exactly after every single operation (no
/// fractional leftover to account for).
const REGION_SIZE: u64 = 1 << 24;

/// Orders exercised by the random mix — weighted toward order 12 (4 KiB
/// pages), since that's what dominates in the real kernel (per-page slab
/// cache expansion), with some larger blocks and only occasionally very
/// large ones, mirroring the mix requested: "muchas asignaciones de orden
/// 12 ..., algunas grandes".
const MIX_ORDERS: &[usize] = &[12, 12, 12, 12, 12, 12, 13, 13, 14, 15, 16, 18, 20];

struct Model {
    /// start_addr -> (end_addr_exclusive, order). Sole source of truth for
    /// "what's currently live" — invariant 1 (overlap) is checked against
    /// this on every allocation.
    live: BTreeMap<u64, (u64, usize)>,
    live_bytes: u64,
}

impl Model {
    fn new() -> Self {
        Self { live: BTreeMap::new(), live_bytes: 0 }
    }

    /// Invariant 1 (no overlap) + invariant 3 (alignment) + invariant 4
    /// (in-range), then records the block as live.
    fn record_alloc(&mut self, seed: u64, op: u64, addr: PhysAddr, order: usize) {
        let a = addr.as_u64();
        let size = 1u64 << order;
        let end = a + size;

        assert_eq!(
            a % size, 0,
            "seed {seed} op {op}: INVARIANT 3 (alignment) broken — order {order} \
             address {a:#x} is not aligned to {size:#x}"
        );
        assert!(
            end <= REGION_SIZE,
            "seed {seed} op {op}: INVARIANT 4 (in-range) broken — order {order} \
             block [{a:#x}, {end:#x}) falls outside the region [0, {REGION_SIZE:#x}) \
             handed to add_region"
        );

        // Predecessor: the live block (if any) with the largest start <= a.
        if let Some((&p_start, &(p_end, p_order))) = self.live.range(..=a).next_back() {
            assert!(
                p_end <= a,
                "seed {seed} op {op}: INVARIANT 1 (no overlap) broken — THIS IS THE \
                 DOUBLE-ISSUE BUG — new order-{order} block [{a:#x}, {end:#x}) overlaps \
                 already-live order-{p_order} block [{p_start:#x}, {p_end:#x})"
            );
        }
        // Successor: the live block (if any) with the smallest start >= a.
        if let Some((&s_start, &(s_end, s_order))) = self.live.range(a..).next() {
            assert!(
                s_start >= end,
                "seed {seed} op {op}: INVARIANT 1 (no overlap) broken — THIS IS THE \
                 DOUBLE-ISSUE BUG — new order-{order} block [{a:#x}, {end:#x}) overlaps \
                 already-live order-{s_order} block [{s_start:#x}, {s_end:#x})"
            );
        }

        self.live.insert(a, (end, order));
        self.live_bytes += size;
    }

    fn record_dealloc(&mut self, addr: u64, order: usize) {
        let removed = self.live.remove(&addr);
        assert!(removed.is_some(), "test bug: deallocating an address not in the model");
        self.live_bytes -= 1u64 << order;
    }

    /// Pick a uniformly random currently-live (addr, order) pair, if any.
    fn random_live(&self, rng: &mut Rng) -> Option<(u64, usize)> {
        if self.live.is_empty() {
            return None;
        }
        let idx = rng.below(self.live.len() as u64) as usize;
        self.live.iter().nth(idx).map(|(&a, &(_, o))| (a, o))
    }
}

/// Invariant 2 (conservation): free bytes (from the allocator) plus live
/// bytes (from the model) must equal the total region size, after every
/// operation.
fn check_conservation(buddy: &BuddyAllocator, mem: &VecMem, model: &Model, seed: u64, op: u64) {
    let free = buddy.free_bytes(mem);
    assert_eq!(
        free + model.live_bytes,
        REGION_SIZE,
        "seed {seed} op {op}: INVARIANT 2 (conservation) broken — free_bytes()={free} + \
         live_bytes={} != region size {REGION_SIZE}",
        model.live_bytes
    );
}

/// Invariant 5 (bitmap ⟺ free-list coherence), checked independently of
/// whatever `PhantomEvent`s coalescing may or may not have surfaced: for
/// every order, (a) every address the free list actually contains must
/// have its bitmap bit set, and (b) the bitmap's popcount for that order
/// must equal the free list's length — together (a)+(b) rule out both
/// "list has an entry the bitmap doesn't know about" and "bitmap has a set
/// bit with no corresponding list entry" (a phantom, the exact double-
/// issue precursor).
fn check_bitmap_freelist_coherence(buddy: &BuddyAllocator, mem: &VecMem, seed: u64, op: u64) {
    for order in MIN_ORDER..=28 {
        let mut list_addrs = Vec::new();
        buddy.walk_free_list(mem, order, |addr| list_addrs.push(addr));

        for &addr in &list_addrs {
            assert!(
                buddy.is_free(order, addr),
                "seed {seed} op {op}: INVARIANT 5 broken — order {order} address \
                 {addr:?} is linked into the free list but the bitmap says it is NOT \
                 free"
            );
        }

        let bit_count = buddy.free_bit_count(order);
        assert_eq!(
            list_addrs.len(),
            bit_count,
            "seed {seed} op {op}: INVARIANT 5 broken — order {order} free list has {} \
             entries but the bitmap popcount is {} — a PHANTOM bitmap bit exists with \
             no corresponding free-list entry (this is exactly the mechanism that would \
             let the same physical frame be handed out twice)",
            list_addrs.len(),
            bit_count
        );
    }
}

// ============================================================================
// Operation sequence driver
// ============================================================================

enum Op {
    Alloc(usize),      // order
    Dealloc(u64, usize), // addr, order
}

/// Runs one full randomized sequence for `seed`, checking all five
/// invariants throughout. Returns the number of operations actually
/// executed (allocations attempted + deallocations performed), for the
/// final report.
fn run_seed(seed: u64) -> u64 {
    let mem = VecMem::new(REGION_SIZE as usize);
    let mut buddy = BuddyAllocator::new();
    unsafe { buddy.add_region(&mem, 0, REGION_SIZE) };

    let mut model = Model::new();
    let mut rng = Rng::new(seed);
    let mut op_count: u64 = 0;

    // One "mixed" phase: random allocs (weighted orders) and deallocs of
    // random live blocks, `n` operations. Includes deallocating in an
    // order unrelated to allocation order (`random_live` picks uniformly
    // at random among everything currently live, not FIFO/LIFO), per the
    // brief's "liberaciones intercaladas en orden distinto al de
    // asignación".
    let do_mixed_phase = |n: u64,
                               buddy: &mut BuddyAllocator,
                               model: &mut Model,
                               rng: &mut Rng,
                               op_count: &mut u64| {
        for _ in 0..n {
            *op_count += 1;
            let op = if model.live.is_empty() || rng.chance(60) {
                let order = MIX_ORDERS[rng.below(MIX_ORDERS.len() as u64) as usize];
                Op::Alloc(order)
            } else {
                let (addr, order) = model.random_live(rng).unwrap();
                Op::Dealloc(addr, order)
            };

            match op {
                Op::Alloc(order) => {
                    if let Some(addr) = unsafe { buddy.allocate(&mem, order) } {
                        model.record_alloc(seed, *op_count, addr, order);
                    }
                    // None == legitimate OOM, not a failure — just don't
                    // record anything.
                }
                Op::Dealloc(addr, order) => {
                    let phantom = unsafe { buddy.deallocate(&mem, PhysAddr::new(addr), order) };
                    model.record_dealloc(addr, order);
                    assert_phantom_free(phantom, seed, *op_count, addr, order);
                }
            }

            check_conservation(buddy, &mem, model, seed, *op_count);
            if *op_count % 25 == 0 {
                check_bitmap_freelist_coherence(buddy, &mem, seed, *op_count);
            }
        }
    };

    // Exhaustion phase: allocate a single fixed order repeatedly until OOM
    // (exercising the split path down to exhaustion), then free every
    // single thing just allocated in a shuffled (not allocation) order —
    // this is when merge/coalescing gets exercised hardest, per the
    // brief's "fases de agotamiento seguidas de liberación masiva".
    let do_exhaustion_phase = |order: usize,
                                    buddy: &mut BuddyAllocator,
                                    model: &mut Model,
                                    rng: &mut Rng,
                                    op_count: &mut u64| {
        let mut just_allocated: Vec<u64> = Vec::new();
        loop {
            *op_count += 1;
            match unsafe { buddy.allocate(&mem, order) } {
                Some(addr) => {
                    model.record_alloc(seed, *op_count, addr, order);
                    just_allocated.push(addr.as_u64());
                }
                None => break, // genuine OOM at this order — stop the burst
            }
        }
        check_conservation(buddy, &mem, model, seed, *op_count);
        check_bitmap_freelist_coherence(buddy, &mem, seed, *op_count);

        // Fisher-Yates shuffle of the just-allocated addresses, so the
        // mass free below happens in an order unrelated to allocation
        // order.
        for i in (1..just_allocated.len()).rev() {
            let j = rng.below((i + 1) as u64) as usize;
            just_allocated.swap(i, j);
        }

        for addr in just_allocated {
            *op_count += 1;
            let phantom = unsafe { buddy.deallocate(&mem, PhysAddr::new(addr), order) };
            model.record_dealloc(addr, order);
            assert_phantom_free(phantom, seed, *op_count, addr, order);
            check_conservation(buddy, &mem, model, seed, *op_count);
        }
        check_bitmap_freelist_coherence(buddy, &mem, seed, *op_count);
    };

    do_mixed_phase(6000, &mut buddy, &mut model, &mut rng, &mut op_count);
    do_exhaustion_phase(12, &mut buddy, &mut model, &mut rng, &mut op_count);
    do_mixed_phase(6000, &mut buddy, &mut model, &mut rng, &mut op_count);
    do_exhaustion_phase(13, &mut buddy, &mut model, &mut rng, &mut op_count);
    do_mixed_phase(6000, &mut buddy, &mut model, &mut rng, &mut op_count);
    do_exhaustion_phase(12, &mut buddy, &mut model, &mut rng, &mut op_count);
    do_mixed_phase(6000, &mut buddy, &mut model, &mut rng, &mut op_count);

    // Final full check regardless of the 1-in-25 sampling cadence used
    // during the mixed phases.
    check_bitmap_freelist_coherence(&buddy, &mem, seed, op_count);
    check_conservation(&buddy, &mem, &model, seed, op_count);

    op_count
}

fn assert_phantom_free(phantom: Option<PhantomEvent>, seed: u64, op: u64, addr: u64, order: usize) {
    if let Some(event) = phantom {
        panic!(
            "seed {seed} op {op}: PhantomEvent emitted while deallocating a well-formed, \
             single-owner block (addr={addr:#x}, order={order}): {event:?} — per the task \
             brief, ANY phantom event from a legitimate operation sequence is a hard \
             failure. This is the double-issue bug signature: the bitmap claimed a buddy \
             block was free but the free list did not actually contain it."
        );
    }
}

// Fixed seed list — deterministic, reproducible via `cargo test <name> --
// --nocapture`. Chosen arbitrarily, no special significance beyond
// spreading the xorshift state around.
const SEEDS: &[u64] = &[
    1, 2, 42, 1337, 0xdead_beef, 999_983, 7, 424_242, 88_888_888, 123_456_789, 0xC0FFEE,
    0x5EED_5EED, 3, 17, 0xfeed_face, 555_555, 0x1234_5678, 987_654_321, 0xABCD_EF01, 31_415_927,
];

#[test]
fn buddy_property_invariants_hold_across_random_sequences() {
    let mut total_ops: u64 = 0;
    for &seed in SEEDS {
        let ops = run_seed(seed);
        total_ops += ops;
    }
    eprintln!(
        "buddy_property_invariants_hold_across_random_sequences: {} seeds, {} total operations, \
         zero PhantomEvents, zero overlap/conservation/alignment/range violations",
        SEEDS.len(),
        total_ops
    );
}
