//! Steady-state CPU-share fairness — permanent tests for a measurement that,
//! until now, only ever existed as a throwaway harness
//! (`/tmp/.../scratchpad/fairness/src/main.rs`, never checked in).
//!
//! Every existing property test in this crate (`invariants.rs`) drives
//! `SchedCore` with entities that are **always runnable and never block**,
//! and asks a **transient, binary** question: in which iteration was each
//! pid *first* picked? That has two real gaps, both closed here:
//!
//! 1. **`park`/`wake_matching` are never exercised by any property test.**
//!    Nothing in `invariants.rs` ever blocks. [`interactive_task_gets_only_what_it_asks_for`]
//!    below is the first test in this crate that drives a realistic
//!    scheduling loop — pop, run, age, requeue-or-park, wake-on-timer —
//!    through both of those calls.
//! 2. **"First picked" says nothing about the share of CPU an entity
//!    actually receives.** A policy that picks pid 3 first at iteration 0
//!    and then starves it for the next million ticks still "passes" a
//!    first-pick test. These tests instead run a scheduling loop to steady
//!    state (discarding an initial warm-up) and measure ticks-consumed per
//!    pid — the number that actually characterizes a scheduling policy.
//!
//! ## The finding these tests exist to pin down
//!
//! [`distinct_bases_still_split_evenly_in_steady_state`] is the important
//! one: **in steady state, base priority does not determine CPU share.**
//! `requeue_preempted` decays every non-idle entity's effective priority by
//! one step on every single preemption; `age_processes` only restores one
//! step per `AGING_EPOCH` (50 ticks) — and with several always-ready
//! entities preempting each other every `quantum_for(eff)` (2-12) ticks,
//! decay wins the race by a wide margin. Effective priority collapses to
//! `MIN_EFFECTIVE_PRIORITY` for everyone almost immediately and stays there;
//! `pop_next_ready`'s 11 priority queues become one shared FIFO in practice.
//! Base priorities `[1, 3, 6, 10]` (which, run proportionally, would predict
//! shares of roughly 5%/15%/30%/50%) measure at ~24/24/26/26 instead — see
//! the actual numbers in that test. This is the property a "fix" to aging
//! could silently destroy while every other test in this crate stays green,
//! which is exactly why it gets its own permanent test rather than living
//! only in a deleted scratch file.
//!
//! ## Tick-count choice
//!
//! The reference harness ran 2,000,000 ticks with a 200,000-tick warm-up.
//! Measured directly (not assumed): for every scenario below, the measured
//! percentages are already stable to within a few hundredths of a point at
//! `total_ticks=20_000, warmup=2_000` — a hundredfold reduction — because
//! these scenarios are fully deterministic (no PRNG, no threads) and settle
//! into a short repeating cycle almost immediately. `cargo test` for this
//! whole crate is ~0.4s before this file and stays there after it (see the
//! step report for the measured before/after).
#[cfg(test)]
mod tests {
    use crate::core::SchedCore;
    use crate::{FakeClock, SchedEntity};
    use alloc::boxed::Box;
    use alloc::vec::Vec;

    /// Ticks simulated per scenario, and how much of the front is discarded
    /// as warm-up before ticks start counting toward the measured share. See
    /// the module doc comment's "Tick-count choice" section for how these
    /// were picked.
    const TOTAL_TICKS: u64 = 20_000;
    const WARMUP_TICKS: u64 = 2_000;

    #[derive(Clone, Copy, PartialEq)]
    enum Kind {
        /// Never blocks. Always runnable whenever it isn't the one running.
        CpuBound,
        /// Runs `burst` ticks of CPU, then blocks (via `park`) for `sleep`
        /// ticks before becoming ready again (via `wake_matching`).
        Interactive { burst: u32, sleep: u32 },
    }

    /// Minimal `SchedEntity` for these tests. `ready` is a plain field (not
    /// reused from `core::tests::Ent`) specifically so the simulation loop
    /// below can flip it directly around `park`/`wake_matching`, the same
    /// way the kernel adapter flips `Process::state`.
    struct Ent {
        pid: usize,
        base: u8,
        eff: u8,
        ready: bool,
    }

    impl SchedEntity for Ent {
        fn pid(&self) -> usize {
            self.pid
        }
        fn base_priority(&self) -> u8 {
            self.base
        }
        fn effective_priority(&self) -> u8 {
            self.eff
        }
        fn set_effective_priority(&mut self, p: u8) {
            self.eff = p;
        }
        fn is_idle(&self) -> bool {
            self.pid == 0
        }
        fn is_ready(&self) -> bool {
            self.ready
        }
    }

    /// Run a realistic scheduling loop — pop the highest-priority ready
    /// entity, run it tick by tick (aging on every crossed epoch, exactly
    /// like the kernel's `tick` does), then either `park` it (its burst just
    /// ended) or `requeue_preempted` it (quantum exhausted) — for
    /// `total_ticks`, and return each non-idle pid's ticks-of-CPU consumed
    /// strictly after `warmup` ticks have elapsed.
    ///
    /// One `SchedCore`/`FakeClock` pair per call; `spec` is `(base_priority,
    /// Kind)` per entity, in the order pids are handed out (pid 1 is
    /// `spec[0]`, etc — pid 0 is the idle entity, added separately and never
    /// included in the returned ticks).
    fn cpu_ticks(spec: &[(u8, Kind)], total_ticks: u64, warmup: u64) -> Vec<(usize, u64)> {
        let mut core = SchedCore::<Ent>::new();
        let clock = FakeClock::new();
        core.add_reset_to_base(Box::new(Ent { pid: 0, base: 0, eff: 0, ready: true }));

        let mut kinds: Vec<(usize, Kind)> = Vec::new();
        for &(base, kind) in spec {
            let pid = core.allocate_pid();
            core.add_reset_to_base(Box::new(Ent { pid, base, eff: base, ready: true }));
            kinds.push((pid, kind));
        }

        let mut ticks: Vec<(usize, u64)> = kinds.iter().map(|&(p, _)| (p, 0)).collect();
        // pid -> tick at which it should wake up (moved out of the wait
        // queue via wake_matching).
        let mut sleeping: Vec<(usize, u64)> = Vec::new();
        // Remaining burst ticks per interactive pid; unused (stays 0) for
        // CpuBound entities.
        let mut burst_left: Vec<(usize, u32)> = kinds
            .iter()
            .map(|&(p, k)| {
                (
                    p,
                    match k {
                        Kind::Interactive { burst, .. } => burst,
                        Kind::CpuBound => 0,
                    },
                )
            })
            .collect();

        let mut now: u64 = 0;
        while now < total_ticks {
            // Wake anything whose sleep expired before picking next.
            let due: Vec<usize> = sleeping.iter().filter(|&&(_, t)| t <= now).map(|&(p, _)| p).collect();
            for pid in due {
                sleeping.retain(|&(p, _)| p != pid);
                core.wake_matching(|e| e.pid() == pid, |e| e.ready = true);
                if let Some(b) = burst_left.iter_mut().find(|(p, _)| *p == pid) {
                    b.1 = match kinds.iter().find(|(p, _)| *p == pid).unwrap().1 {
                        Kind::Interactive { burst, .. } => burst,
                        Kind::CpuBound => 0,
                    };
                }
            }

            let mut entity = match core.pop_next_ready() {
                Some(e) => e,
                // Every entity is asleep or blocked right now; just let time
                // pass until the next wake.
                None => {
                    now += 1;
                    continue;
                }
            };
            let pid = entity.pid();
            let kind = kinds.iter().find(|(p, _)| *p == pid).unwrap().1;
            core.start_slice(entity.effective_priority());

            let mut blocked = false;
            loop {
                clock.advance(1);
                let epoch = core.advance_ticks(&clock);
                let exhausted = core.consume_quantum();
                now += 1;
                if now >= warmup && pid != 0 {
                    if let Some(t) = ticks.iter_mut().find(|(p, _)| *p == pid) {
                        t.1 += 1;
                    }
                }
                if let Kind::Interactive { sleep, .. } = kind {
                    if let Some(b) = burst_left.iter_mut().find(|(p, _)| *p == pid) {
                        if b.1 > 0 {
                            b.1 -= 1;
                        }
                        if b.1 == 0 {
                            sleeping.push((pid, now + sleep as u64));
                            blocked = true;
                        }
                    }
                }
                if epoch {
                    core.age_processes();
                }
                if exhausted || blocked {
                    break;
                }
            }

            if blocked {
                entity.ready = false;
                core.park(entity);
            } else {
                core.requeue_preempted(entity);
            }
        }
        ticks
    }

    /// Ticks per pid -> percent of the total, same pid order as input.
    fn shares_pct(ticks: &[(usize, u64)]) -> Vec<(usize, f64)> {
        let sum: u64 = ticks.iter().map(|&(_, t)| t).sum();
        ticks.iter().map(|&(p, t)| (p, 100.0 * t as f64 / sum.max(1) as f64)).collect()
    }

    /// Assert `pid`'s measured share sits within `expected_pct +-
    /// tol_pct`. Never compares floats with `==` — steady-state CPU share
    /// is inherently a ratio, not a value this simulation can be expected to
    /// hit exactly.
    fn assert_share_within(shares: &[(usize, f64)], pid: usize, expected_pct: f64, tol_pct: f64) {
        let pct = shares.iter().find(|&&(p, _)| p == pid).unwrap().1;
        assert!(
            (pct - expected_pct).abs() <= tol_pct,
            "pid {pid}: expected {expected_pct}% +- {tol_pct}, got {pct:.3}% (all shares: {shares:?})"
        );
    }

    /// 1. Four CPU-bound entities, all base priority 5 (what
    /// `init/processes.rs` actually creates today: idle + user + shell, all
    /// at the same base). Each should receive ~25% of the CPU.
    ///
    /// Tolerance: +-1 point. Measured value at these tick counts is within
    /// 0.01 of 25% for every pid (fully deterministic, no PRNG involved) —
    /// 1 point is generous headroom, not a tight bound, matching this
    /// crate's existing style of "generous headroom, not the tightest bound
    /// that happens to pass" (see e.g. `property_no_ready_entity_starves`'s
    /// `ITERATIONS` justification in `invariants.rs`).
    #[test]
    fn equal_bases_split_cpu_evenly() {
        let spec = [(5u8, Kind::CpuBound); 4];
        let shares = shares_pct(&cpu_ticks(&spec, TOTAL_TICKS, WARMUP_TICKS));
        for pid in 1..=4 {
            assert_share_within(&shares, pid, 25.0, 1.0);
        }
    }

    /// 2. THE central property (see the module doc comment): four CPU-bound
    /// entities at distinct base priorities `[1, 3, 6, 10]` still split the
    /// CPU approximately evenly in steady state — base priority does NOT
    /// govern steady-state share, because decay-per-preemption outpaces
    /// one-step-per-epoch aging and collapses everyone to
    /// `MIN_EFFECTIVE_PRIORITY`.
    ///
    /// Tolerance: +-6 points around 25% (band 19-31%). Measured values are
    /// ~24/24/26/26 — comfortably inside that band — while what base
    /// priority *proportional* scheduling would predict (base / sum-of-bases
    /// * 100) is 5% / 15% / 30% / 50%, wildly outside it. The band is wide
    /// enough to tolerate a legitimate future tuning change to the decay/
    /// aging constants while still failing hard the moment someone
    /// reintroduces real base-priority dominance (e.g. sabotage S7 below).
    #[test]
    fn distinct_bases_still_split_evenly_in_steady_state() {
        let spec = [(1u8, Kind::CpuBound), (3, Kind::CpuBound), (6, Kind::CpuBound), (10, Kind::CpuBound)];
        let shares = shares_pct(&cpu_ticks(&spec, TOTAL_TICKS, WARMUP_TICKS));
        for pid in 1..=4 {
            assert_share_within(&shares, pid, 25.0, 6.0);
        }
    }

    /// 3. Three CPU-bound hogs plus one interactive entity (burst 2 ticks,
    /// sleep 40 ticks — a shell-like duty cycle), all base priority 5. The
    /// interactive entity should receive only what it asks for
    /// (~2/(2+40) = 4.76% of the CPU), not more (it would be starved-looking
    /// if `wake_matching` failed to actually re-enqueue it) and not
    /// dramatically less (it would be under-served if `park` dropped it, or
    /// if it had to fight the hogs for priority every time it woke).
    ///
    /// This is the first test in this crate to exercise `park` +
    /// `wake_matching` inside a real scheduling loop, not just as isolated
    /// unit calls.
    ///
    /// Tolerances: interactive entity +-1.5 points around 4.76%; the three
    /// hogs +-3.5 points around the remaining ~31.75% each (measured:
    /// ~31.78/31.78/31.78/4.67 — matches the reference harness's
    /// 31.8/31.8/31.8/4.7 to one decimal place).
    #[test]
    fn interactive_task_gets_only_what_it_asks_for() {
        let spec = [
            (5u8, Kind::CpuBound),
            (5, Kind::CpuBound),
            (5, Kind::CpuBound),
            (5, Kind::Interactive { burst: 2, sleep: 40 }),
        ];
        let shares = shares_pct(&cpu_ticks(&spec, TOTAL_TICKS, WARMUP_TICKS));
        assert_share_within(&shares, 4, 100.0 * 2.0 / 42.0, 1.5);
        for pid in 1..=3 {
            assert_share_within(&shares, pid, 100.0 * 40.0 / 3.0 / 42.0, 3.5);
        }
    }
}
