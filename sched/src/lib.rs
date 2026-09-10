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
//! ## What lives here (as of this step)
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
//!
//! Later steps (see the extraction plan) move the run queues, wait queue,
//! and their operations (`add_process`, `wake_matching`, `pop_next_ready`,
//! `take_first_startable`, `advance_ticks`/`age_processes`/
//! `consume_quantum`) into a generic `SchedCore<E: SchedEntity>` here too.
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
//! this crate is (in its later steps) generic over the scheduled entity
//! (`E: SchedEntity`) rather than a fixed data type, because its queues will
//! own the entities themselves (`VecDeque<Box<E>>`) — the kernel needs to
//! keep storing real `Box<Process>`, not a copy or a handle into some
//! separate table.
#![no_std]

extern crate alloc;

#[cfg(test)] extern crate std;

pub mod entity;
pub use entity::SchedEntity;

pub mod core;
pub use core::SchedCore;

pub mod invariants;

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
