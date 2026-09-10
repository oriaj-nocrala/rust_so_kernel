//! `SchedCore<E>` — the priority-indexed run queues and wait queue,
//! generic over whatever concrete type implements [`crate::SchedEntity`].
//!
//! Extracted (step 2 of `docs/sched/sched-extraction-plan.md`) out of the
//! fields and methods `kernel/src/process/scheduler.rs`'s `Scheduler` used to
//! own directly: `run_queues`, `wait_queue`, and `next_pid`, plus the
//! enqueue/dequeue/wake operations that only ever touch those three things
//! and never the context-switch machinery (`TrapFrame`, `fxsave`, CR3, TSS)
//! that stays in the kernel adapter. See that file's crate-level doc comment
//! for the full rationale, and the extraction plan for why this step
//! specifically does NOT move the tick counters (`remaining_ticks`,
//! `global_ticks` — step 4) or the pick-next scans (step 3).

use alloc::{boxed::Box, collections::VecDeque};

use crate::{queue_index, SchedEntity, MIN_EFFECTIVE_PRIORITY, NUM_PRIORITIES};

/// The run queues, wait queue, and pid counter for one scheduler instance.
///
/// `E` is the scheduled entity type (`kernel::process::Process`, in the only
/// real caller) — the core owns `Box<E>` directly rather than a handle into
/// some separate table, so the kernel adapter keeps storing real `Box
/// <Process>` values, unchanged.
///
/// All three fields are private. In particular `run_queues` is reachable
/// from nowhere outside this module (aside from the two TEMPORARY index
/// accessors at the bottom, deleted in step 3) — that's what makes "queue
/// index == queue_index(effective priority) of everything in it" an
/// invariant this type can actually enforce, rather than a convention that
/// used to be copy-pasted correctly (or not) at 7 call sites in the kernel.
pub struct SchedCore<E: SchedEntity> {
    /// Per-priority run queues — ONLY Ready entities, indexed by
    /// `queue_index(effective_priority)`. Moved verbatim out of
    /// `Scheduler::run_queues`.
    run_queues: [VecDeque<Box<E>>; NUM_PRIORITIES],

    /// Blocked/Zombie/Stopped entities. Not scanned by the pick-next scans.
    /// Moved out of `Scheduler::wait_queue`, which used to be a `pub` field
    /// — see `Scheduler::wait_queue()`/`wait_queue_mut()` in the kernel
    /// adapter for the two accessors that replaced that field for the four
    /// external call sites that touched it directly.
    wait_queue: VecDeque<Box<E>>,

    /// Monotonic pid counter. Starts at 1; 0 is reserved for idle. Moved out
    /// of `Scheduler::next_pid`.
    next_pid: usize,
}

impl<E: SchedEntity> SchedCore<E> {
    /// Must stay `const` — `kernel/src/process/scheduler.rs`'s
    /// `static SCHEDULERS: [Mutex<Scheduler>; MAX_CPUS]` is built from
    /// `Mutex::new(Scheduler::new())` repeated in a const array initializer,
    /// which forces `Scheduler::new()` (and therefore this) to be a `const
    /// fn` even though it's generic over `E`. Verified to compile, not just
    /// assumed — see the extraction plan's "hard constraint" section.
    pub const fn new() -> Self {
        Self {
            run_queues: [const { VecDeque::new() }; NUM_PRIORITIES],
            wait_queue: VecDeque::new(),
            next_pid: 1,
        }
    }

    // ========================================================================
    // PID management
    // ========================================================================

    /// Monotonic pid counter. Starts at 1; 0 is reserved for idle.
    ///
    /// Moved verbatim out of `Scheduler::allocate_pid`.
    pub fn allocate_pid(&mut self) -> usize {
        let pid = self.next_pid;
        self.next_pid += 1;
        pid
    }

    // ========================================================================
    // Enqueue operations
    // ========================================================================

    /// Reset `entity`'s effective priority to its base priority, then
    /// enqueue at `queue_index(effective_priority)`. Returns the run-queue
    /// index used, so the caller can log it (see `Scheduler::add_process`,
    /// the call site this replaces).
    ///
    /// Note: this clamps only the *index* the entity lands in, not the
    /// *stored* effective priority — a base priority above the highest real
    /// queue leaves `effective_priority` at that unclamped value while still
    /// landing in the last queue. That is today's behavior, copied
    /// deliberately, not "fixed": see test
    /// `add_reset_to_base_pins_index_clamp_without_clamping_value`.
    pub fn add_reset_to_base(&mut self, mut entity: Box<E>) -> usize {
        entity.set_effective_priority(entity.base_priority());
        let idx = queue_index(entity.effective_priority());
        self.run_queues[idx].push_back(entity);
        idx
    }

    /// Enqueue at `queue_index(effective_priority)`, changing no priority.
    ///
    /// Moved out of the `ProcessState::Ready` arm of `switch_to_next`'s save
    /// half (an entity that was already `Ready` — not `Running`, not
    /// `Blocked`/`Zombie`/`Stopped` — going back into its run queue
    /// unchanged).
    pub fn requeue_ready(&mut self, entity: Box<E>) -> usize {
        let idx = queue_index(entity.effective_priority());
        self.run_queues[idx].push_back(entity);
        idx
    }

    /// Decay effective priority by 1 — unless the entity is idle, or is
    /// already at `MIN_EFFECTIVE_PRIORITY` — then enqueue at the (possibly
    /// new) `queue_index(effective_priority)`.
    ///
    /// Moved out of the `ProcessState::Running` arm of `switch_to_next`'s
    /// save half (`if proc.pid.0 != 0 && proc.effective_priority >
    /// MIN_EFFECTIVE_PRIORITY { proc.effective_priority -= 1; }`), same
    /// operator (`>`, not `>=`) and short-circuit order.
    pub fn requeue_preempted(&mut self, mut entity: Box<E>) -> usize {
        if !entity.is_idle() && entity.effective_priority() > MIN_EFFECTIVE_PRIORITY {
            entity.set_effective_priority(entity.effective_priority() - 1);
        }
        let idx = queue_index(entity.effective_priority());
        self.run_queues[idx].push_back(entity);
        idx
    }

    /// Push onto the wait queue (blocked / zombie / stopped).
    ///
    /// Moved out of `kill_current`'s, `stop_and_switch_tf`'s,
    /// `block_current`'s, and `switch_to_next`'s `self.wait_queue.push_back
    /// (proc)` call sites.
    pub fn park(&mut self, entity: Box<E>) {
        self.wait_queue.push_back(entity);
    }

    /// Remove the first wait-queue entry satisfying `pred`, run `prepare` on
    /// it, then enqueue it at `queue_index(effective_priority)`. Returns
    /// whether one was found.
    ///
    /// `prepare` runs *before* the enqueue, deliberately — the enqueue index
    /// depends on the effective priority `prepare` may itself change (see
    /// test `wake_matching_prepare_runs_before_enqueue`). Generalizes
    /// `wake`/`wake_with_retval`/`wake_stopped`'s shared shape: the kernel
    /// supplies the predicate (state == Blocked / == Stopped) and the
    /// preparation (state = Ready, rax = ..., stopped_by_signal = None); this
    /// core contributes only the clamp and the correct enqueue.
    pub fn wake_matching(
        &mut self,
        mut pred: impl FnMut(&E) -> bool,
        prepare: impl FnOnce(&mut E),
    ) -> bool {
        if let Some(pos) = self.wait_queue.iter().position(|e| pred(e)) {
            if let Some(mut entity) = self.wait_queue.remove(pos) {
                prepare(&mut entity);
                let idx = queue_index(entity.effective_priority());
                self.run_queues[idx].push_back(entity);
            }
            true
        } else {
            false
        }
    }

    // ========================================================================
    // Lookup
    // ========================================================================

    /// First entity satisfying `pred`, searching every run queue (low index
    /// to high) and then the wait queue. Deliberately does NOT see whatever
    /// the adapter is holding outside the core (the running entity) — see
    /// `Scheduler::find_process_mut`, the call site this replaces, which
    /// already documents that `running` is handled separately by its own
    /// callers.
    pub fn find_mut(&mut self, mut pred: impl FnMut(&E) -> bool) -> Option<&mut E> {
        for queue in self.run_queues.iter_mut() {
            if let Some(entity) = queue.iter_mut().find(|e| pred(e)) {
                return Some(entity.as_mut());
            }
        }
        self.wait_queue.iter_mut().find(|e| pred(e)).map(|e| e.as_mut())
    }

    // ========================================================================
    // Iteration
    // ========================================================================

    /// Every queued entity: run queues (index 0 upward) then wait queue.
    /// Backs `Scheduler::iter_all` (chained with `running`),
    /// `Scheduler::queue_signal_to_group`'s two loops, and
    /// `Scheduler::find_process_mut`'s run-queue scan (via [`Self::find_mut`]).
    pub fn iter_queued(&self) -> impl Iterator<Item = &E> + '_ {
        self.run_queues
            .iter()
            .flat_map(|q| q.iter())
            .map(|b| b.as_ref())
            .chain(self.wait_queue.iter().map(|b| b.as_ref()))
    }

    /// Mutable counterpart of [`Self::iter_queued`], same order.
    pub fn iter_queued_mut(&mut self) -> impl Iterator<Item = &mut E> + '_ {
        self.run_queues
            .iter_mut()
            .flat_map(|q| q.iter_mut())
            .map(|b| b.as_mut())
            .chain(self.wait_queue.iter_mut().map(|b| b.as_mut()))
    }

    /// The blocked/zombie/stopped queue, read-only. Replaces the four
    /// external kernel files' direct reads of the formerly-`pub
    /// wait_queue` field (`process/pipe.rs`, `process/syscall/{fs,ipc,
    /// process_ctl}.rs`) via `Scheduler::wait_queue()`.
    pub fn wait_queue(&self) -> &VecDeque<Box<E>> {
        &self.wait_queue
    }

    /// Mutable counterpart of [`Self::wait_queue`], replacing the same four
    /// files' mutating accesses via `Scheduler::wait_queue_mut()`.
    pub fn wait_queue_mut(&mut self) -> &mut VecDeque<Box<E>> {
        &mut self.wait_queue
    }

    // ── TEMPORARY, deleted in step 3 ──────────────────────────────────────
    //
    // Step 3 replaces every caller of these two with `pop_next_ready`/
    // `take_first_startable` and deletes them — nothing new should start
    // using them. They exist only so the five pick-next scans in
    // `kernel/src/process/scheduler.rs` (`kill_and_switch_tf`,
    // `stop_and_switch_tf`, `block_current`, `switch_to_next`, `start_first`,
    // plus `start_first`'s logging loop) keep compiling in this step without
    // being restructured.

    /// TEMPORARY (see block comment above) — read-only access to one run
    /// queue by index. Deleted in step 3.
    pub fn run_queue(&self, index: usize) -> &VecDeque<Box<E>> {
        &self.run_queues[index]
    }

    /// TEMPORARY (see block comment above) — mutable access to one run
    /// queue by index. Deleted in step 3.
    pub fn run_queue_mut(&mut self, index: usize) -> &mut VecDeque<Box<E>> {
        &mut self.run_queues[index]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal test entity — same role as `lib.rs`'s `TestEntity`, kept
    /// separate (and named differently) since this module tests the core's
    /// queue behavior, not just the trait's plain accessors.
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
        fn set_effective_priority(&mut self, pri: u8) {
            self.eff = pri;
        }
        fn is_idle(&self) -> bool {
            self.pid == 0
        }
        fn is_ready(&self) -> bool {
            self.ready
        }
    }

    fn ent(pid: usize, base: u8, eff: u8) -> Box<Ent> {
        Box::new(Ent { pid, base, eff, ready: true })
    }

    fn parked(pid: usize, base: u8, eff: u8) -> Box<Ent> {
        Box::new(Ent { pid, base, eff, ready: false })
    }

    /// 1. `add_reset_to_base` resets effective priority to base and lands
    /// the entity in `queue_index(base)`. If this broke, a process re-added
    /// after aging/decay would keep a stale effective priority instead of
    /// starting fresh.
    #[test]
    fn add_reset_to_base_resets_and_enqueues() {
        let mut core = SchedCore::<Ent>::new();
        let idx = core.add_reset_to_base(ent(1, 4, 9));
        assert_eq!(idx, 4);
        assert_eq!(core.run_queue(4).len(), 1);
        assert_eq!(core.run_queue(4)[0].effective_priority(), 4);
    }

    /// 2. `add_reset_to_base` with base priority 200: the entity lands in
    /// the last run queue but its stored `effective_priority` stays 200.
    /// This is a deliberate record of today's "clamp the index, not the
    /// value" behavior, not an endorsement — see this method's doc comment.
    #[test]
    fn add_reset_to_base_pins_index_clamp_without_clamping_value() {
        let mut core = SchedCore::<Ent>::new();
        let idx = core.add_reset_to_base(ent(1, 200, 0));
        assert_eq!(idx, NUM_PRIORITIES - 1);
        assert_eq!(core.run_queue(NUM_PRIORITIES - 1)[0].effective_priority(), 200);
    }

    /// 3. `requeue_preempted` lowers effective priority by exactly 1 and
    /// enqueues at the new index. If the decay amount or target queue
    /// changed, time slices would drift under contention.
    #[test]
    fn requeue_preempted_decays_by_one() {
        let mut core = SchedCore::<Ent>::new();
        let idx = core.requeue_preempted(ent(1, 5, 5));
        assert_eq!(idx, 4);
        assert_eq!(core.run_queue(4)[0].effective_priority(), 4);
    }

    /// 4. `requeue_preempted` on an entity already at `MIN_EFFECTIVE_PRIORITY`
    /// leaves it unchanged. Sabotage B (`>` → `>=`) targets exactly this.
    #[test]
    fn requeue_preempted_does_not_decay_below_floor() {
        let mut core = SchedCore::<Ent>::new();
        let idx = core.requeue_preempted(ent(1, 5, MIN_EFFECTIVE_PRIORITY));
        assert_eq!(idx, MIN_EFFECTIVE_PRIORITY as usize);
        assert_eq!(
            core.run_queue(MIN_EFFECTIVE_PRIORITY as usize)[0].effective_priority(),
            MIN_EFFECTIVE_PRIORITY
        );
    }

    /// 5. `requeue_preempted` on an idle entity (pid == 0) never decays it,
    /// whatever its priority. Sabotage A (dropping the `is_idle` guard)
    /// targets exactly this.
    #[test]
    fn requeue_preempted_never_decays_idle() {
        let mut core = SchedCore::<Ent>::new();
        let idx = core.requeue_preempted(ent(0, 5, 5));
        assert_eq!(idx, 5);
        assert_eq!(core.run_queue(5)[0].effective_priority(), 5);
    }

    /// 6. `requeue_ready` changes no priority. If it started decaying or
    /// resetting priority, a process resumed as already-Ready would
    /// silently change priority for no reason.
    #[test]
    fn requeue_ready_changes_no_priority() {
        let mut core = SchedCore::<Ent>::new();
        let idx = core.requeue_ready(ent(1, 5, 3));
        assert_eq!(idx, 3);
        assert_eq!(core.run_queue(3)[0].effective_priority(), 3);
    }

    /// 7. `wake_matching` moves a matching entity out of the wait queue and
    /// into the run queue matching its **effective** priority, returns true,
    /// and leaves the wait queue one shorter.
    ///
    /// The entity deliberately has `base != eff` (base 9, effective 5). An
    /// earlier version of this test used base == eff == 5, and a sabotage run
    /// proved that version did not actually guard what its name claims:
    /// swapping `effective_priority()` for `base_priority()` in the enqueue
    /// left it green, because with the two equal the substitution is
    /// invisible. Keep them distinct — a wake must restore an entity to the
    /// priority it had decayed to, not to its base.
    #[test]
    fn wake_matching_moves_from_wait_to_run_queue() {
        let mut core = SchedCore::<Ent>::new();
        core.park(parked(1, 9, 5));
        let found = core.wake_matching(|e| e.pid() == 1, |_| {});
        assert!(found);
        assert_eq!(core.wait_queue().len(), 0);
        assert_eq!(core.run_queue(5).len(), 1);
        assert_eq!(core.run_queue(5)[0].pid(), 1);
        assert_eq!(core.run_queue(9).len(), 0, "enqueued by base priority, not effective");
    }

    /// 8. `wake_matching` with a predicate matching nothing returns false
    /// and mutates nothing (wait queue and all run queues unchanged).
    #[test]
    fn wake_matching_no_match_mutates_nothing() {
        let mut core = SchedCore::<Ent>::new();
        core.park(parked(1, 5, 5));
        let found = core.wake_matching(|e| e.pid() == 99, |_| {});
        assert!(!found);
        assert_eq!(core.wait_queue().len(), 1);
        for i in 0..NUM_PRIORITIES {
            assert_eq!(core.run_queue(i).len(), 0);
        }
    }

    /// 9. `wake_matching`'s `prepare` runs before the enqueue: `prepare`
    /// changes the effective priority here, and the entity must land in the
    /// queue matching the NEW priority, not the old one. Sabotage D
    /// (computing the index before running `prepare`) targets exactly this
    /// — and only this: tests 7/8 still pass under that sabotage, which is
    /// the point of having this test separately.
    #[test]
    fn wake_matching_prepare_runs_before_enqueue() {
        let mut core = SchedCore::<Ent>::new();
        core.park(parked(1, 5, 2));
        let found = core.wake_matching(|e| e.pid() == 1, |e| e.set_effective_priority(7));
        assert!(found);
        assert_eq!(core.run_queue(7).len(), 1);
        assert_eq!(core.run_queue(2).len(), 0);
    }

    /// 10. `iter_queued` visits every queued entity exactly once, run
    /// queues before wait queue (index order within the run queues too).
    #[test]
    fn iter_queued_visits_run_queues_then_wait_queue_in_order() {
        let mut core = SchedCore::<Ent>::new();
        core.add_reset_to_base(ent(1, 2, 0)); // lands in run_queue(2)
        core.add_reset_to_base(ent(2, 5, 0)); // lands in run_queue(5)
        core.park(parked(3, 1, 1));

        let pids: alloc::vec::Vec<usize> = core.iter_queued().map(|e| e.pid()).collect();
        assert_eq!(pids, alloc::vec![1, 2, 3]);
    }

    /// 11. `allocate_pid` returns 1, 2, 3 on successive calls.
    #[test]
    fn allocate_pid_increments() {
        let mut core = SchedCore::<Ent>::new();
        assert_eq!(core.allocate_pid(), 1);
        assert_eq!(core.allocate_pid(), 2);
        assert_eq!(core.allocate_pid(), 3);
    }

    /// 12. `find_mut` finds an entity sitting in a run queue, finds one
    /// sitting in the wait queue, and returns `None` for a pid in neither.
    #[test]
    fn find_mut_searches_run_queues_then_wait_queue() {
        let mut core = SchedCore::<Ent>::new();
        core.add_reset_to_base(ent(1, 3, 0));
        core.park(parked(2, 1, 1));

        assert_eq!(core.find_mut(|e| e.pid() == 1).map(|e| e.pid()), Some(1));
        assert_eq!(core.find_mut(|e| e.pid() == 2).map(|e| e.pid()), Some(2));
        assert!(core.find_mut(|e| e.pid() == 99).is_none());
    }
}
