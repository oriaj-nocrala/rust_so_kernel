//! Structural invariants for [`crate::SchedCore`], and the property tests
//! that stress them against randomized operation sequences.
//!
//! This is step 5 of `docs/sched/sched-extraction-plan.md` — the deliverable
//! the whole extraction exists for. Steps 1-4 only moved code; this step
//! adds the thing that couldn't exist before the move: a checker that can
//! see every entity in `SchedCore` at once, plus tests that hammer it with
//! long randomized operation sequences and verify it never lies.
//!
//! [`Violation`] lives here. `SchedCore::check_invariants`, the method that
//! produces it, lives in `core.rs` instead — see that method's doc comment
//! for why (short version: it needs the private `run_queues` field, and
//! keeping the method beside the field avoids adding any `pub(crate)`
//! accessor whose only job would be letting this file reach across the
//! module boundary `core.rs` deliberately drew).
//!
//! This crate has, and must keep, zero dependencies — no `rand`, no
//! `proptest`, no `quickcheck` (see the crate-level doc comment and the
//! extraction plan). The property tests below drive themselves with a
//! six-line xorshift64 PRNG seeded with fixed constants, so every run is
//! exactly as deterministic as any other test in this crate.

use alloc::vec::Vec;

/// A structural invariant of [`crate::SchedCore`] that does not hold.
///
/// Returned by `SchedCore::check_invariants`. See that method's doc comment
/// (in `core.rs`) for the exact check order and the documented scoping
/// decisions (the wait queue is excluded from the priority-range check; the
/// `running` slot the kernel adapter holds outside the core is invisible to
/// this checker entirely).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Violation {
    /// An entity sits in `run_queues[found_in]` but its effective priority
    /// says it belongs in `queue_index(effective) == expected`.
    MisplacedEntity { pid: usize, found_in: usize, expected: usize, effective: u8 },
    /// A run-queue entity's effective priority is outside its legal band.
    PriorityOutOfRange { pid: usize, effective: u8, base: u8, floor: u8 },
    /// The same pid appears more than once across the run queues and the
    /// wait queue.
    DuplicatePid { pid: usize },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::tests::{ent, parked, Ent};
    use crate::core::SchedCore;
    use crate::{SchedEntity, MIN_EFFECTIVE_PRIORITY, NUM_PRIORITIES};
    use alloc::boxed::Box;

    // ========================================================================
    // A tiny, dependency-free, deterministic PRNG
    // ========================================================================

    /// xorshift64 — six lines, no dependency, fully deterministic from a
    /// fixed seed. This crate must never gain `rand`/`proptest`/
    /// `quickcheck` as a dependency (see the crate-level doc comment), so
    /// property tests roll their own generator instead of a "real" one.
    struct Xorshift64 {
        state: u64,
    }

    impl Xorshift64 {
        fn new(seed: u64) -> Self {
            // xorshift64 is undefined at an all-zero state; substitute a
            // fixed nonzero value rather than ever silently producing the
            // same (degenerate) sequence for two different requested seeds.
            Self { state: if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed } }
        }

        fn next_u64(&mut self) -> u64 {
            let mut x = self.state;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.state = x;
            x
        }

        /// A value in `0..bound` (`bound` must be > 0). Modulo bias is
        /// irrelevant here: this only selects which test operation to run
        /// next, not anything statistical.
        fn below(&mut self, bound: u64) -> u64 {
            self.next_u64() % bound
        }
    }

    // ========================================================================
    // B1: randomized operation sequence + invariants + conservation
    // ========================================================================

    /// The property tests' shared operation set, used by B1. Kept as a
    /// `u64` match rather than an enum purely so `Xorshift64::below` can
    /// select one directly.
    const NUM_OPS: u64 = 11;

    /// B1. Drive `SchedCore<Ent>` through >= 2000 randomly chosen operations
    /// per seed, asserting after **every single one** that (a)
    /// `check_invariants()` is `Ok`, and (b) conservation holds: the number
    /// of entities the core can see (`iter_queued().count()`) plus whether
    /// the harness's own `running` stand-in is occupied equals the number of
    /// entities ever created.
    ///
    /// The `running` slot is the harness's stand-in for the kernel's
    /// `running: Option<Box<Process>>`, which `check_invariants` cannot see
    /// (see that method's doc comment) — modeling it here, and folding it
    /// into the conservation count, is the only way this test suite covers
    /// conservation across that boundary at all.
    #[test]
    fn property_random_operations_preserve_invariants_and_conservation() {
        const OPS_PER_SEED: usize = 2000;
        let seeds: [u64; 5] = [1, 2, 3, 12345, 0xDEAD_BEEF];

        for seed in seeds {
            let mut rng = Xorshift64::new(seed);
            let mut core = SchedCore::<Ent>::new();
            let mut running: Option<Box<Ent>> = None;
            let mut created_pids: Vec<usize> = Vec::new();

            for op_idx in 0..OPS_PER_SEED {
                match rng.below(NUM_OPS) {
                    // Create a new entity.
                    0 => {
                        let pid = core.allocate_pid();
                        let base = rng.below(NUM_PRIORITIES as u64) as u8; // 0..=10
                        core.add_reset_to_base(ent(pid, base, base));
                        created_pids.push(pid);
                    }
                    // pop_next_ready into `running`, only if empty.
                    1 => {
                        if running.is_none() {
                            running = core.pop_next_ready();
                        }
                    }
                    // requeue_preempted, only if occupied.
                    2 => {
                        if let Some(e) = running.take() {
                            core.requeue_preempted(e);
                        }
                    }
                    // requeue_ready, only if occupied.
                    3 => {
                        if let Some(e) = running.take() {
                            core.requeue_ready(e);
                        }
                    }
                    // park, only if occupied.
                    4 => {
                        if let Some(e) = running.take() {
                            core.park(e);
                        }
                    }
                    // wake_matching on a random live pid, priority untouched.
                    5 => {
                        if !created_pids.is_empty() {
                            let idx = rng.below(created_pids.len() as u64) as usize;
                            let pid = created_pids[idx];
                            core.wake_matching(|e| e.pid() == pid, |_e| {});
                        }
                    }
                    // wake_matching on a random live pid, priority set to a
                    // random value within that entity's own legal band.
                    6 => {
                        if !created_pids.is_empty() {
                            let idx = rng.below(created_pids.len() as u64) as usize;
                            let pid = created_pids[idx];
                            let roll = rng.below(u32::MAX as u64) as u32;
                            core.wake_matching(
                                |e| e.pid() == pid,
                                |e| {
                                    let base = e.base_priority();
                                    let floor = MIN_EFFECTIVE_PRIORITY.min(base);
                                    let span = (base - floor) as u32 + 1;
                                    let new_eff = floor + (roll % span) as u8;
                                    e.set_effective_priority(new_eff);
                                },
                            );
                        }
                    }
                    7 => core.age_processes(),
                    8 => {
                        core.advance_ticks();
                    }
                    9 => {
                        core.consume_quantum();
                    }
                    10 => {
                        let pri = rng.below(NUM_PRIORITIES as u64) as u8;
                        core.start_slice(pri);
                    }
                    _ => unreachable!("NUM_OPS bound must match the match arms above"),
                }

                if let Err(v) = core.check_invariants() {
                    panic!("seed={seed} op={op_idx}: check_invariants() failed: {v:?}");
                }

                let total = core.iter_queued().count() + running.is_some() as usize;
                assert_eq!(
                    total,
                    created_pids.len(),
                    "seed={seed} op={op_idx}: conservation violated \
                     (queued={}, running={}, created={})",
                    core.iter_queued().count(),
                    running.is_some(),
                    created_pids.len()
                );
            }
        }
    }

    // ========================================================================
    // B2: no Ready entity starves
    // ========================================================================

    /// B2. A realistic schedule loop — pop the highest-priority Ready
    /// entity, run its full time slice (ticking `advance_ticks` and calling
    /// `age_processes()` on every epoch, exactly like the kernel's `tick`
    /// does), then `requeue_preempted()` it — and assert that within a
    /// bounded number of iterations, every non-idle entity has been picked
    /// at least once.
    ///
    /// Entities: one idle (pid 0, base 0 — matches `init/processes.rs`
    /// giving idle priority 0) plus three real entities spanning low to high
    /// base priority (1, 5, 10).
    ///
    /// **Bound justification, and an important caveat about what this bound
    /// does NOT detect (found while choosing it, not swept under the rug):**
    ///
    /// A normal run of exactly this config picks pid(base=10) at iteration
    /// 0, pid(base=5) at iteration 5, and pid(base=1) at iteration 13 —
    /// measured directly, not estimated. `ITERATIONS = 200` is roughly 15x
    /// that observed worst case, generous headroom for a fully deterministic
    /// (no-PRNG) test with no reason to vary from one run to the next.
    ///
    /// The caveat this step's report covers in full: **this specific test
    /// does NOT fail under sabotage E** (`age_processes` made a no-op).
    /// Measured, not assumed: with aging entirely disabled, this exact
    /// config produces the *identical* first-pick iterations (0, 5, 13) —
    /// because with only 3 widely-spaced entities, plain decay-on-preempt
    /// converges everyone toward `MIN_EFFECTIVE_PRIORITY` and they rotate
    /// fairly via FIFO regardless of whether aging ever restores anyone.
    /// A brute-force search over all 165 distinct 2- and 3-entity base
    /// combinations in `1..=10` (see this step's report) found **no**
    /// configuration where aging-on passes this property within a bound
    /// that aging-off fails — and at 4+ continuously-ready entities, aging
    /// was observed to make things *worse* for the lowest-base entity, not
    /// better (a config exists where aging ON never picks the lowest-base
    /// pid at all, even after 2,000,000 iterations, while the same config
    /// with aging OFF picks it within 16). This is reported as a genuine
    /// finding about today's scheduler under sustained full-CPU contention,
    /// not a defect in this test.
    #[test]
    fn property_no_ready_entity_starves() {
        const ITERATIONS: usize = 200;

        let mut core = SchedCore::<Ent>::new();
        core.add_reset_to_base(ent(0, 0, 0)); // idle: pid 0, base 0

        let bases: [u8; 3] = [1, 5, 10];
        let mut real_pids: Vec<usize> = Vec::new();
        for &base in bases.iter() {
            let pid = core.allocate_pid();
            core.add_reset_to_base(ent(pid, base, base));
            real_pids.push(pid);
        }

        let mut picked: Vec<usize> = Vec::new();
        let mut iterations_used = 0usize;

        for i in 0..ITERATIONS {
            iterations_used = i + 1;
            if real_pids.iter().all(|p| picked.contains(p)) {
                break;
            }

            let entity = match core.pop_next_ready() {
                Some(e) => e,
                None => panic!("iteration {i}: every entity is requeued each round, run_queues must never all be empty"),
            };
            let pid = entity.pid();
            if pid != 0 && !picked.contains(&pid) {
                picked.push(pid);
            }

            let eff = entity.effective_priority();
            core.start_slice(eff);
            loop {
                let epoch_due = core.advance_ticks();
                let exhausted = core.consume_quantum();
                if epoch_due {
                    core.age_processes();
                }
                if exhausted {
                    break;
                }
            }
            core.requeue_preempted(entity);
        }

        for &pid in &real_pids {
            assert!(
                picked.contains(&pid),
                "pid {pid} starved: not picked within {ITERATIONS} iterations (used {iterations_used})"
            );
        }
    }

    // ========================================================================
    // B3: age_processes restores every decayed entity to its base
    // ========================================================================

    /// B3. Per `age_processes`'s doc comment in `core.rs`: a single call
    /// does not nudge an entity one step toward its base — it restores it
    /// **all the way** to base, because the outer loop's ascending scan
    /// re-finds a just-boosted entity at its new, higher queue index within
    /// the same call. This test pins that exact behavior at scale, across
    /// several entities decayed all the way to the floor first.
    ///
    /// This pins TODAY's behavior, not endorsed scheduling semantics — same
    /// caveat `age_processes`'s own doc comment and its
    /// `age_processes_pins_multi_boost_within_single_call` test in `core.rs`
    /// carry.
    #[test]
    fn property_age_processes_restores_every_decayed_entity_to_base() {
        let mut core = SchedCore::<Ent>::new();
        core.add_reset_to_base(ent(0, 0, 0)); // idle

        let bases: [u8; 5] = [2, 4, 6, 8, 10];
        let mut pids: Vec<(usize, u8)> = Vec::new();
        for &base in bases.iter() {
            let pid = core.allocate_pid();
            core.add_reset_to_base(ent(pid, base, base));
            pids.push((pid, base));
        }

        // Decay everything to the floor: each full sweep (drain every run
        // queue via pop_next_ready, then requeue_preempted every popped
        // entity) decays every entity present by exactly one step. The
        // highest base used above is 10, needing 9 decays to reach the
        // floor (MIN_EFFECTIVE_PRIORITY == 1) -- 20 sweeps is deliberately
        // generous headroom over that, not a tight bound (this test isn't
        // measuring a starvation property, just needs "definitely decayed
        // all the way" before the real assertion).
        for _ in 0..20 {
            let mut popped = Vec::new();
            while let Some(e) = core.pop_next_ready() {
                popped.push(e);
            }
            for e in popped {
                core.requeue_preempted(e);
            }
        }

        for entity in core.iter_queued() {
            if !entity.is_idle() {
                assert_eq!(
                    entity.effective_priority(),
                    MIN_EFFECTIVE_PRIORITY,
                    "pid {} did not decay to the floor before aging -- test setup is broken, not the thing under test",
                    entity.pid()
                );
            }
        }

        // The actual property: ONE age_processes() call, not several.
        core.age_processes();

        for &(pid, base) in &pids {
            let found = core.iter_queued().find(|e| e.pid() == pid).expect("pid must still be queued");
            assert_eq!(
                found.effective_priority(),
                base,
                "pid {pid} not restored all the way to its base {base} by a single age_processes() pass"
            );
        }

        // Also confirm every restored entity landed in the run queue
        // matching its new (== base) effective priority, not just that the
        // stored priority value itself is right.
        core.check_invariants().expect("restored entities must also sit in the queue matching their new priority");
    }

    // Sanity: `ent`/`parked` from `core::tests` really are usable from here,
    // and a plain SchedEntity round-trip works through the shared type.
    #[test]
    fn shared_test_entity_is_usable_from_this_module() {
        let e = ent(1, 5, 5);
        assert_eq!(e.pid(), 1);
        let p = parked(2, 3, 3);
        assert_eq!(p.pid(), 2);
    }
}
