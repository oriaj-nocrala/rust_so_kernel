//! `sched` — host-testable core of the kernel's process scheduler.
//!
//! Extracted out of `kernel/src/process/scheduler.rs` (see
//! `docs/sched/sched-extraction-plan.md` for the full six-step migration this
//! crate is coming out of) for the same reason `hal`, `ext2`, `mm`, `vfs`,
//! and `diag` exist: `kernel` itself cannot run `cargo test` on the host
//! (see `CLAUDE.md`'s "QEMU integration tests" section — `-Z build-std` plus
//! a double build of the `kernel` bin target collides on lang items in
//! `core`, verified, not assumed), so logic that can be made to speak in
//! plain types instead of this kernel's concrete globals gets moved out
//! here, where a plain `cargo test` reaches it.
//!
//! `kernel/src/process/scheduler.rs` today mixes two things of very
//! different nature: **accounting** (11 priority-indexed run queues, a wait
//! queue, decay-on-preemption, periodic anti-starvation aging, and the
//! quantum arithmetic — plain data structures and integer arithmetic) and
//! **context-switch machinery** (`TrapFrame`, `fxsave`/`fxrstor`, `fs_base`
//! via MSR, `AddressSpace::activate()`/CR3, `tss::set_kernel_stack`,
//! `iretq` — pure x86 hardware). Only the first kind can be exercised
//! without booting real QEMU, and it's exactly the kind of bug (a process
//! enqueued in `run_queues[i]` with `effective_priority != i`, a wakeup that
//! forgets to re-enqueue, aging that skips an element while re-queuing
//! mid-iteration, a starved process that never gets boosted) that a QEMU
//! boot catches late or never — it just looks like "the system feels slow",
//! not a crash.
//!
//! ## What lives here
//!
//! - [`entity::SchedEntity`] — the seam that lets the core schedule without
//!   knowing about `Process` — same role `hal::PortIo`/`mm::PhysMap`/
//!   `vfs::ramfs::DirLockObserver` play in their crates.
//! - [`quantum_for`] — the time-slice-length arithmetic, moved verbatim out
//!   of `Scheduler::quantum_for`.
//! - [`queue_index`] — the effective-priority-to-run-queue-index clamp, the
//!   single source of truth for a rule that used to be copy-pasted at 7
//!   call sites in `kernel/src/process/scheduler.rs` with nothing checking
//!   that all 7 copies agreed.
//! - [`core::SchedCore`] — the run queues, the wait queue, and the
//!   monotonic pid counter, generic over `E: SchedEntity` so the queues own
//!   `Box<E>` directly (the kernel keeps storing real `Box<Process>`, not a
//!   handle into some separate table). It provides:
//!   - **Enqueue operations**: `add_reset_to_base` (reset to base priority
//!     and enqueue — used when a process is first created), `requeue_ready`
//!     (re-enqueue with no priority change), `requeue_preempted` (decay by
//!     one, floored at `MIN_EFFECTIVE_PRIORITY`, idle never decays), `park`
//!     (into the wait queue), and `wake_matching` (predicate-driven removal
//!     from the wait queue plus a caller-supplied preparation closure that
//!     runs *before* the re-enqueue index is computed — this generalizes the
//!     kernel's old `wake`/`wake_with_retval`/`wake_stopped`).
//!   - **Pick-next**: `pop_next_ready` (the strict-highest-priority,
//!     FIFO-within-a-queue scan shared by `switch_to_next`, `block_current`,
//!     `kill_and_switch_tf`, and `stop_and_switch_tf`) and
//!     `take_first_startable` (the deliberately different scan `start_first`
//!     uses at boot: skips queue 0, skips the idle entity, and can reach
//!     into the middle of a queue via `remove(i)` — see that method's doc
//!     comment for why unifying the two would start the system on idle).
//!   - **Aging**: `age_processes`, moved verbatim including the two
//!     subtleties its own doc comment documents rather than silently fixes:
//!     `i` is not incremented after a requeue (the next entity has already
//!     shifted into that slot), and the ascending outer loop re-finds an
//!     entity it just promoted at its new, higher index — so one call
//!     restores an entity all the way to its base priority, not one step
//!     toward it, whatever the `+1` in the code and the module comment in
//!     `kernel/src/process/scheduler.rs` suggest.
//!   - **Tick accounting**: `start_slice`/`advance_ticks`/`consume_quantum`,
//!     split out of the kernel's old single `tick()` (rather than merged
//!     into one call here) so the kernel adapter can still interleave its
//!     own `pending_stack_frees`/`pending_vma_frees` retain passes at the
//!     exact point between the tick-counter bump and the aging check where
//!     they ran before.
//!   - **Iteration and lookup**: `iter_queued`/`iter_queued_mut` (run
//!     queues then wait queue), `iter_ready_desc` (run queues only, highest
//!     priority first — backs the boot log), `find_mut`, and
//!     `wait_queue`/`wait_queue_mut` (the two accessors that replaced the
//!     formerly-`pub wait_queue` field for the four external kernel files
//!     that touch it directly).
//!   - `SchedCore::check_invariants` — the checker the property tests
//!     drive: every run-queue entity sits in the queue index its own
//!     effective priority maps to (`Violation::MisplacedEntity`), every
//!     run-queue entity's effective priority stays within its legal band
//!     (`Violation::PriorityOutOfRange` — the wait queue is deliberately
//!     excluded, see that method's doc comment), and no pid appears twice
//!     across both queues (`Violation::DuplicatePid`).
//! - [`invariants::Violation`] — the enum `check_invariants` returns, plus
//!   three property tests (`invariants.rs`) that drive `SchedCore` through
//!   thousands of randomized operations per seed, asserting
//!   `check_invariants()` and a conservation count after every single one,
//!   and the sabotages that prove each test actually catches what it claims
//!   to.
//!
//! ## What stays in the kernel adapter, and why
//!
//! `kernel/src/process/scheduler.rs` keeps: the currently-`running` entity
//! (entangled with `activate()`/`tss`/`fpu`/the per-CPU fast-path pointers),
//! `static SCHEDULERS: [Mutex<Scheduler>; MAX_CPUS]` and
//! `TrackedSchedulerGuard` (the lock and its IF=0 diagnostics), `TrapFrame`,
//! and every bit of `fxsave`/`fs_base`/CR3/TSS context-switch machinery.
//! None of that can be made to speak in plain types without either dragging
//! hardware register access into a `no_std`-without-even-`alloc`-assumptions
//! host-testable crate, or losing the very hardware behavior this kernel
//! depends on — so it doesn't move.
//!
//! Unlike `hal`/`ext2`/`mm`/`vfs`, which all speak in flat, concrete types,
//! this crate is generic over the scheduled entity (`E: SchedEntity`)
//! rather than a fixed data type, because its queues own the entities
//! themselves (`VecDeque<Box<E>>`) — the kernel keeps storing real
//! `Box<Process>`, not a copy or a handle into some separate table.
//!
//! ## Known limitations
//!
//! **Base priority does not determine steady-state CPU share.** This is the
//! most surprising property of this scheduler and the one most likely to
//! mislead someone "improving" it, so it is stated first. Decay-on-preemption
//! (`requeue_preempted`, one step per time slice) outruns aging (one step per
//! `AGING_EPOCH` ticks), so under sustained contention every non-idle entity
//! collapses to `MIN_EFFECTIVE_PRIORITY` and rotates FIFO from there,
//! regardless of what its base priority is. Measured against this crate,
//! steady-state share over a run with the warm-up discarded: four CPU-bound
//! entities at base 5 get 25/25/25/25, and four at bases `[1, 3, 6, 10]` get
//! 24/24/26/26 — very nearly the same. The eleven priority queues are
//! effectively decorative past the transient. See `fairness.rs`, whose tests
//! pin exactly this.
//!
//! A consequence worth stating plainly: raising an entity's base priority
//! buys it a longer time slice (`quantum_for`) and a better position while
//! the system is transient, but not a larger share of a busy CPU. If
//! priorities ever need to mean something in steady state, that calls for a
//! virtual-time model, not more aging heuristics — see
//! `docs/sched/sched-bugs-plan.md`, which records an MLFQ rework that was
//! implemented, measured to be indistinguishable from having no aging at
//! all, and discarded.
//!
//! **Aging is not, today, worth what it costs.** In every measured
//! configuration it is either identical to having no aging (equal bases) or
//! slightly worse for the lowest-base entity (distinct bases). It also walks
//! all eleven queues every `AGING_EPOCH` ticks while the kernel adapter holds
//! its scheduler lock, which is O(entities) under a lock — a real obstacle if
//! this ever runs on more than one CPU.
//!
//! **`.min(base_priority())` in `age_processes` is unreachable code.** The
//! branch only runs when `effective < base` and adds exactly one, so
//! `effective + 1 <= base` always holds. Deleting the clamp leaves the whole
//! suite green; it is not a coverage gap, it is dead code kept only because
//! removing it is a behavior-neutral change nobody has made yet.
//!
//! **What the tests do and do not guard.** `property_no_ready_entity_starves`
//! does guard a real property, but not the one its history suggests: it is
//! insensitive to aging being disabled (disabling aging does not produce
//! starvation, because decay alone still rotates entities fairly), and it is
//! sensitive to the *decay* being removed, which makes the highest-base
//! entity monopolize the CPU forever. If aging silently stopped running
//! altogether, only the unit tests of `advance_ticks` would notice — no
//! property test of scheduler behavior would. That gap is open.
//!
#![no_std]

extern crate alloc;

#[cfg(test)] extern crate std;

pub mod entity;
pub use entity::SchedEntity;

pub mod clock;
pub use clock::{Clock, FakeClock};

pub mod core;
pub use core::SchedCore;

pub mod invariants;

#[cfg(test)]
mod fairness;

/// Number of priority-indexed run queues (effective priorities 0..=10).
pub const NUM_PRIORITIES: usize = 11;

/// Fixed part of every time slice, in timer ticks.
pub const BASE_QUANTUM: u32 = 2;

/// Extra ticks granted per point of effective priority.
pub const PRIORITY_QUANTUM_BONUS: u32 = 1;

/// How many global ticks between aging passes that boost starved processes.
pub const AGING_EPOCH: u32 = 50;

/// Floor effective priority never decays below.
pub const MIN_EFFECTIVE_PRIORITY: u8 = 1;

/// CPUs whose time slices [`SchedCore`] accounts separately (stage 7 of
/// `docs/smp/smp-plan.md`: one core, one run-queue set, one slice per CPU).
/// Must equal the kernel's `cpu::MAX_CPUS`, which asserts it.
pub const MAX_CPUS: usize = 32;

/// Time slice, in timer ticks, granted to an entity scheduled at
/// `effective_priority`.
///
/// Copied verbatim (same arithmetic, same operand order) from the private
/// `Scheduler::quantum_for` this replaces in
/// `kernel/src/process/scheduler.rs` — behavior must stay bit-identical.
pub const fn quantum_for(effective_priority: u8) -> u32 {
    BASE_QUANTUM + (effective_priority as u32) * PRIORITY_QUANTUM_BONUS
}

/// Which run queue an entity with this effective priority belongs in.
///
/// This is the single source of truth for "which `run_queues[i]` does an
/// entity with this effective priority go in" — before this crate existed,
/// the exact same clamp (`(effective_priority as usize).min(NUM_PRIORITIES
/// - 1)`) was copy-pasted at 7 call sites in
/// `kernel/src/process/scheduler.rs`, with nothing checking that all 7
/// copies agreed with each other. Routing every one of those call sites
/// through this single function is what makes the invariant "queue index ==
/// queue_index(effective priority of every entity in it)" a checkable
/// property instead of a convention that has to stay correct by hand at 7
/// separate places.
///
/// Written with an `if` rather than `usize::min` because `usize::min` is not
/// (yet) a `const fn`, and this function needs to be usable in a `const`
/// context (ultimately from inside `Scheduler::new()`, which itself must
/// stay a `const fn` — see the extraction plan's "hard constraint" section).
pub const fn queue_index(effective_priority: u8) -> usize {
    let idx = effective_priority as usize;
    if idx > NUM_PRIORITIES - 1 {
        NUM_PRIORITIES - 1
    } else {
        idx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins down the exact quantum values at a handful of priorities. If
    /// `quantum_for`'s arithmetic ever changes (e.g. the multiply/add
    /// swapped, or the wrong constant substituted), this fails immediately
    /// with a concrete, easy-to-diff wrong number instead of only showing up
    /// as "the system feels slow" after a QEMU boot.
    #[test]
    fn quantum_for_known_values() {
        assert_eq!(quantum_for(0), 2);
        assert_eq!(quantum_for(1), 3);
        assert_eq!(quantum_for(5), 7);
        assert_eq!(quantum_for(10), 12);
    }

    /// Exhaustive check against the defining formula, for every possible
    /// `u8` effective priority (not just in-range 0..=10 — the function
    /// itself places no restriction on its input). If a future edit changes
    /// the arithmetic for only some inputs (an off-by-one range guard, a
    /// special case), this still catches it because it evaluates the
    /// formula independently at all 256 values rather than trusting a few
    /// samples.
    #[test]
    fn quantum_for_matches_formula_exhaustively() {
        for eff in 0u8..=255 {
            assert_eq!(
                quantum_for(eff),
                BASE_QUANTUM + eff as u32 * PRIORITY_QUANTUM_BONUS,
                "mismatch at effective_priority={eff}"
            );
        }
    }

    /// In the normal in-range case, `queue_index` must be the identity —
    /// if this broke, every process would end up in the wrong run queue
    /// even without ever exercising the clamp.
    #[test]
    fn queue_index_identity_in_range() {
        for pri in 0u8..=10 {
            assert_eq!(queue_index(pri), pri as usize);
        }
    }

    /// Values above the highest real priority must clamp down to the last
    /// queue, not panic, wrap, or silently index out of bounds when later
    /// code uses this as a `run_queues` index.
    #[test]
    fn queue_index_clamps_out_of_range_values() {
        for pri in [11u8, 12, 200, 255] {
            assert_eq!(queue_index(pri), NUM_PRIORITIES - 1, "pri={pri}");
        }
    }

    /// Exhaustive safety property: no `u8` input can ever produce an index
    /// that would be out of bounds for a `[_; NUM_PRIORITIES]` array. This
    /// is the property an out-of-bounds `run_queues` access would violate.
    #[test]
    fn queue_index_never_out_of_bounds_exhaustively() {
        for pri in 0u8..=255 {
            assert!(queue_index(pri) < NUM_PRIORITIES, "pri={pri}");
        }
    }

    /// A minimal stand-in for `kernel::process::Process`, used only to
    /// prove the trait itself is usable and that its accessors round-trip
    /// correctly — this is what would break if `SchedEntity`'s methods
    /// stopped being plain field accessors (e.g. if `set_effective_priority`
    /// silently clamped or `is_idle` used the wrong field).
    struct TestEntity {
        pid: usize,
        base: u8,
        eff: u8,
        ready: bool,
    }

    impl SchedEntity for TestEntity {
        fn pid(&self) -> usize { self.pid }
        fn base_priority(&self) -> u8 { self.base }
        fn effective_priority(&self) -> u8 { self.eff }
        fn set_effective_priority(&mut self, pri: u8) { self.eff = pri; }
        fn is_idle(&self) -> bool { self.pid == 0 }
        fn is_ready(&self) -> bool { self.ready }
    }

    #[test]
    fn sched_entity_accessors_round_trip() {
        let mut idle = TestEntity { pid: 0, base: 0, eff: 0, ready: true };
        let mut normal = TestEntity { pid: 7, base: 5, eff: 3, ready: false };

        assert_eq!(normal.pid(), 7);
        assert_eq!(normal.base_priority(), 5);
        assert_eq!(normal.effective_priority(), 3);
        assert!(!normal.is_ready());

        normal.set_effective_priority(9);
        assert_eq!(normal.effective_priority(), 9);

        assert!(idle.is_idle());
        assert!(!normal.is_idle());

        idle.set_effective_priority(1);
        assert_eq!(idle.effective_priority(), 1);
        assert!(idle.is_ready());
    }
}
