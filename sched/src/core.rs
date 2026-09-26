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

use alloc::{boxed::Box, collections::VecDeque, vec::Vec};

use crate::invariants::Violation;
use crate::{queue_index, quantum_for, Clock, SchedEntity, AGING_EPOCH, MAX_CPUS, MIN_EFFECTIVE_PRIORITY, NUM_PRIORITIES};

/// The run queues, wait queue, and pid counter for one scheduler instance.
///
/// `E` is the scheduled entity type (`kernel::process::Process`, in the only
/// real caller) — the core owns `Box<E>` directly rather than a handle into
/// some separate table, so the kernel adapter keeps storing real `Box
/// <Process>` values, unchanged.
///
/// All three fields are private. `run_queues` is reachable from nowhere
/// outside this module — that's what makes "queue index ==
/// queue_index(effective priority) of everything in it" an invariant this
/// type can actually enforce, rather than a convention that used to be
/// copy-pasted correctly (or not) at 7 call sites in the kernel.
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

    /// Remaining ticks of the slice each CPU is running. Moved out of
    /// `Scheduler::remaining_ticks`; one per CPU since stage 7 of
    /// `docs/smp/smp-plan.md`, when one core started feeding several CPUs.
    remaining_ticks: [u32; MAX_CPUS],

    /// The [`Clock`] reading at which the most recent aging epoch was
    /// declared (or 0, if none ever has been).
    ///
    /// Replaces the old free-running `global_ticks: u32` counter this core
    /// used to own and increment itself. That counter is gone, not renamed
    /// — ticks now live wherever the injected `Clock` says they live (a
    /// kernel-owned `AtomicU64` in production, a `FakeClock` in tests, see
    /// `crate::clock`), and this field only remembers the one thing
    /// `advance_ticks` needs to detect the next crossing: where the last one
    /// happened. `u64`, matching `Clock::now_ticks`'s return type, not the
    /// old field's `u32` — there is no longer a wrapping-every-~4-billion-
    /// ticks concern to inherit from a counter this type doesn't own
    /// anymore.
    last_epoch_tick: u64,
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
            remaining_ticks: [0; MAX_CPUS],
            last_epoch_tick: 0,
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

    // ========================================================================
    // Pick next (preemption / boot)
    // ========================================================================

    /// Highest-priority Ready entity, or `None` if every run queue is
    /// empty.
    ///
    /// Scans queue indices high to low (`(0..NUM_PRIORITIES).rev()`) and
    /// pops from the FRONT of the first non-empty queue (FIFO within a
    /// queue). Never looks at `wait_queue` — only Ready entities are ever
    /// queued in `run_queues` in the first place.
    ///
    /// Moved verbatim out of the four kernel call sites that shared this
    /// exact scan: `kill_and_switch_tf`, `stop_and_switch_tf`,
    /// `block_current`, and `switch_to_next`. See
    /// [`Self::take_first_startable`] for the deliberately different scan
    /// `start_first` uses instead, and why unifying the two would be wrong.
    pub fn pop_next_ready(&mut self) -> Option<Box<E>> {
        for priority in (0..NUM_PRIORITIES).rev() {
            if let Some(entity) = self.run_queues[priority].pop_front() {
                return Some(entity);
            }
        }
        None
    }

    /// [`Self::pop_next_ready`], skipping entities `eligible` rejects: the
    /// same strict-priority, FIFO-within-a-queue order among the eligible
    /// ones, and every rejected entity stays exactly where it was.
    ///
    /// Stage 7 of `docs/smp/smp-plan.md`: with several CPUs picking from one
    /// core, an entity can be Ready while another CPU is still executing on
    /// its kernel stack (it has just switched away and not yet left it) —
    /// the kernel adapter rejects those until that CPU is off the stack.
    pub fn pop_next_ready_where(&mut self, mut eligible: impl FnMut(&E) -> bool) -> Option<Box<E>> {
        for priority in (0..NUM_PRIORITIES).rev() {
            let queue = &mut self.run_queues[priority];
            if let Some(i) = queue.iter().position(|e| eligible(e)) {
                return queue.remove(i);
            }
        }
        None
    }

    /// Is any entity Ready (queued in a run queue)?
    pub fn has_ready(&self) -> bool {
        self.run_queues.iter().any(|q| !q.is_empty())
    }

    /// The first entity `start_first` (boot) should run.
    ///
    /// Deliberately NOT the same scan as [`Self::pop_next_ready`], and must
    /// stay that way:
    ///
    /// - Starts at priority **1**, not 0 — so it can never pick something
    ///   sitting in queue 0.
    /// - Requires `is_ready()` — a filter `pop_next_ready` doesn't need,
    ///   since by construction every run queue holds only Ready entities;
    ///   kept here as an explicit, defensive check for the one caller that
    ///   runs before the rest of the scheduling machinery has ever moved
    ///   anything.
    /// - Skips the idle entity (`is_idle()`) — the idle entity must never
    ///   be the first thing started, or the system boots straight into
    ///   idle and never runs anything else.
    /// - Uses `remove(i)`, not `pop_front()` — so it can take a startable
    ///   entity out of the *middle* of a queue whose front entity is not
    ///   startable (not ready, or idle), instead of being stuck on it.
    ///
    /// Unifying this with `pop_next_ready` would make the first process
    /// started at boot the idle one.
    pub fn take_first_startable(&mut self) -> Option<Box<E>> {
        for priority in (1..NUM_PRIORITIES).rev() {
            let queue = &mut self.run_queues[priority];
            for i in 0..queue.len() {
                if queue[i].is_ready() && !queue[i].is_idle() {
                    return queue.remove(i);
                }
            }
        }
        None
    }

    /// Every run-queue entity, highest queue index first, insertion order
    /// within each queue. Run queues only — never the wait queue.
    ///
    /// Backs `start_first`'s "Available processes:" boot log, which scans
    /// `(0..NUM_PRIORITIES).rev()` — starting at 0, unlike
    /// [`Self::take_first_startable`]'s scan just above, because this is
    /// only logging, not picking a candidate to start.
    /// The most recently allocated pid (0 before the first) —
    /// `/proc/loadavg`'s last field.
    pub fn last_pid(&self) -> usize {
        self.next_pid - 1
    }

    pub fn iter_ready_desc(&self) -> impl Iterator<Item = &E> + '_ {
        self.run_queues.iter().rev().flat_map(|q| q.iter()).map(|b| b.as_ref())
    }

    // ========================================================================
    // Priority aging
    // ========================================================================

    /// Boost every Ready entity's effective priority toward its base
    /// priority, one step per call — exactly what the `+1` right here and
    /// the module comment in `kernel/src/process/scheduler.rs` ("Every
    /// AGING_EPOCH ticks: boost waiting processes' eff_pri toward base")
    /// both say it does.
    ///
    /// Moved verbatim out of `Scheduler::age_processes`, translating field
    /// access to this core's own `run_queues` and the trait accessors
    /// (`pid.0 == 0` → `is_idle()`, `effective_priority`/`priority` →
    /// `effective_priority()`/`base_priority()`) — except for the outer
    /// loop's direction, fixed here (see subtlety (b)).
    ///
    /// Two subtleties:
    ///
    /// (a) `i` is NOT incremented after a requeue
    /// (`self.run_queues[pri].remove(i)`) — `remove(i)` shifts every later
    /// element down by one, so the next entity to examine has already
    /// shifted into position `i`. Incrementing `i` here would skip it. This
    /// stays correct under the descending outer loop too: nothing about it
    /// depends on which direction the outer loop runs.
    ///
    /// (b) **The important one.** The outer loop runs DESCENDING
    /// (`(0..NUM_PRIORITIES).rev()`), not ascending. Aging only ever moves
    /// an entity UPWARD (into `queue_index(effective + 1)`, which is always
    /// `>=` its current queue), so a descending outer loop has already
    /// finished visiting every queue index an entity could be promoted
    /// into by the time it processes that entity — the promoted entity
    /// lands in an already-visited queue and is never found again this
    /// call. An ascending loop (the bug this replaced — see git history /
    /// the doc comment this one overwrote) does the opposite: it moves a
    /// promoted entity into a queue the loop has NOT visited yet, so the
    /// same entity gets re-found and re-aged repeatedly within one call,
    /// turning "+1 per call" into "restore all the way to base in one
    /// call". Verified directly, not just reasoned about: an entity with
    /// base 8 sitting at effective priority 1 in queue 1 now ends a single
    /// `age_processes()` call at effective priority 2, in queue 2 (see
    /// `age_processes_boosts_exactly_one_step_per_call` below), where the
    /// old ascending loop drove it all the way to 8.
    pub fn age_processes(&mut self) {
        for pri in (0..NUM_PRIORITIES).rev() {
            let mut i = 0;
            while i < self.run_queues[pri].len() {
                let entity = &self.run_queues[pri][i];

                if entity.is_idle() {
                    i += 1;
                    continue;
                }

                if entity.effective_priority() < entity.base_priority() {
                    let mut entity = self.run_queues[pri].remove(i).unwrap();
                    let new_eff = (entity.effective_priority() + 1).min(entity.base_priority());
                    entity.set_effective_priority(new_eff);
                    let new_pri = queue_index(entity.effective_priority());
                    self.run_queues[new_pri].push_back(entity);
                    // Don't increment i — next element shifted into position i
                } else {
                    i += 1;
                }
            }
        }
    }

    // ========================================================================
    // Tick accounting (step 4)
    // ========================================================================

    /// Grant a fresh time slice sized for `effective_priority`. Replaces the
    /// five `self.remaining_ticks = sched::quantum_for(...)` assignments in
    /// the kernel adapter.
    pub fn start_slice(&mut self, effective_priority: u8) {
        self.start_slice_on(0, effective_priority);
    }

    /// [`Self::start_slice`] for CPU `cpu`'s slice. Each CPU's slice is
    /// independent: starting one never touches another's.
    pub fn start_slice_on(&mut self, cpu: usize, effective_priority: u8) {
        self.remaining_ticks[cpu] = quantum_for(effective_priority);
    }

    /// Read `clock` and report whether an aging epoch has been crossed
    /// since the last time this returned `true` (or since this `SchedCore`
    /// was created, if it never has).
    ///
    /// Split out of the kernel's old `Scheduler::tick` as its own method,
    /// rather than folded into one `tick()` here, because the kernel's tick
    /// handler interleaves two `retain` passes over `pending_stack_frees` /
    /// `pending_vma_frees` between the tick-counter read and the
    /// aging-epoch check — both of those call into real physical-memory
    /// freeing that cannot leave the kernel (they're not expressible in
    /// terms of `SchedEntity`). Splitting `tick` into `advance_ticks`, the
    /// kernel's own retain passes, then `age_processes`/`consume_quantum`
    /// preserves that exact ordering instead of forcing it all into one
    /// opaque call.
    ///
    /// **Why this is a crossing check (`now - last >= AGING_EPOCH`), not the
    /// modulo check (`ticks % AGING_EPOCH == 0`) this replaced.** The old
    /// check was only ever correct because its own counter guaranteed the
    /// precondition it silently relied on: `global_ticks` was incremented by
    /// exactly 1 on every single call, so it visited every integer in
    /// sequence and a modulo test was equivalent to "did we just cross a
    /// multiple". Reading an injected [`Clock`] instead breaks that
    /// precondition on purpose — a real clock backing a coalesced or
    /// missed-tick path, and deliberately a `FakeClock` under test (see
    /// `crate::clock`), can both report a jump of more than one tick between
    /// two calls. A modulo check over such a jump can step over the exact
    /// multiple and silently skip an entire aging epoch (e.g. `last = 40`,
    /// `now = 61`: no value in `41..=61` is a multiple of `AGING_EPOCH`
    /// (50), so `now % AGING_EPOCH == 0` never fires even though a whole
    /// epoch boundary — 50 — was crossed). The crossing check instead asks
    /// the only question that stays correct regardless of step size: "is the
    /// distance since the last declared epoch at least one epoch's worth",
    /// which is true exactly when a boundary was passed, jumped over or
    /// landed on exactly. `wrapping_sub`, to stay well-defined across a
    /// `u64` wraparound the same way the old field's `wrapping_add` did
    /// (`now` can never lie behind `last_epoch_tick` in practice — `Clock`'s
    /// own contract forbids going backwards — but wrapping arithmetic keeps
    /// this total rather than leaning on that guarantee to avoid a panic).
    ///
    /// See `advance_ticks_matches_old_modulo_formula_under_one_tick_per_call`
    /// (below) for the proof that this and the old formula agree exactly
    /// when ticks really do arrive one at a time, which is what production
    /// does today (see the kernel adapter's `KernelClock`).
    pub fn advance_ticks(&mut self, clock: &impl Clock) -> bool {
        let now = clock.now_ticks();
        if now.wrapping_sub(self.last_epoch_tick) >= AGING_EPOCH as u64 {
            self.last_epoch_tick = now;
            true
        } else {
            false
        }
    }

    /// Consume one tick of the running entity's slice. Returns whether the
    /// slice is now exhausted (i.e. a context switch is due).
    ///
    /// The `> 0` guard is what stops a `u32` underflow when a tick arrives
    /// with no slice outstanding (e.g. before `start_slice` has ever been
    /// called) — keep it.
    pub fn consume_quantum(&mut self) -> bool {
        self.consume_quantum_on(0)
    }

    /// [`Self::consume_quantum`] for CPU `cpu`'s slice.
    pub fn consume_quantum_on(&mut self, cpu: usize) -> bool {
        let left = &mut self.remaining_ticks[cpu];
        if *left > 0 {
            *left -= 1;
        }
        *left == 0
    }

    // ========================================================================
    // Invariant checking (step 5)
    // ========================================================================

    /// Check every structural invariant this type is responsible for,
    /// returning the first violation found.
    ///
    /// Lives here rather than in `invariants.rs` (where [`Violation`] itself
    /// lives) because it needs direct access to the private `run_queues`
    /// field. The alternative — a `pub(crate)` accessor onto `run_queues` so
    /// `invariants.rs` could implement this itself — was rejected: it would
    /// add API surface whose only purpose is letting one file reach across a
    /// module boundary this crate deliberately drew (see the doc comment on
    /// the `run_queues` field itself: it "is reachable from nowhere outside
    /// this module"). Keeping the method beside the field it reads keeps
    /// that sentence true in the strongest sense — not just "outside the
    /// crate", but "outside this one module" — for zero extra surface.
    ///
    /// Checks, strictly in this order (each check runs to completion over
    /// every relevant entity before the next one starts, so if multiple
    /// violations exist simultaneously, the earlier-numbered check's is
    /// always the one returned):
    ///
    /// 1. **`MisplacedEntity`** — every entity in `run_queues[i]` must have
    ///    `queue_index(effective_priority()) == i`. This is the invariant
    ///    the whole extraction exists to buy: the clamp used to be
    ///    copy-pasted at 7 call sites in the kernel with nothing checking
    ///    they agreed.
    ///
    /// 2. **`PriorityOutOfRange`** — **run queues only**. For every run-queue
    ///    entity, `floor <= effective_priority() <= base_priority()`, where
    ///    `floor = MIN_EFFECTIVE_PRIORITY.min(base_priority())`. The floor is
    ///    written that way, not simply `MIN_EFFECTIVE_PRIORITY`, because the
    ///    idle entity has base priority 0 (`kernel/src/init/processes.rs`
    ///    gives idle priority 0, and `Process::set_priority` sets base and
    ///    effective to the same clamped value) — strictly below
    ///    `MIN_EFFECTIVE_PRIORITY` (1). A plain floor of 1 would flag idle on
    ///    every real boot.
    ///
    ///    **The wait queue is deliberately excluded from this check.** The
    ///    core only takes custody of parked entities — `park`/`wake_matching`
    ///    never touch a parked entity's priority — so there is no band a
    ///    parked entity's priority is obliged to sit in. This is a scoping
    ///    decision, not an oversight: applying the run-queue band to the wait
    ///    queue would mean enforcing an invariant this type never actually
    ///    maintains there.
    ///
    /// 3. **`DuplicatePid`** — no pid appears twice across the run queues
    ///    plus the wait queue. Most of today's operations genuinely cannot
    ///    trip this: each one moves an **owned** `Box<E>` from one container
    ///    to another, never `Clone`s it, so producing a duplicate pid out of
    ///    a single already-unique population is not something
    ///    `requeue_ready`/`requeue_preempted`/`wake_matching`/`park`/
    ///    `age_processes` can do by themselves. But pid uniqueness in the
    ///    first place is [`Self::allocate_pid`]'s job, not something this
    ///    check re-derives from nothing — and sabotaging *that* one
    ///    operation (removing its `next_pid += 1`, so two calls hand out the
    ///    same pid) reliably trips this check: verified directly, not
    ///    assumed (Sabotage F, this step's report) — `check_invariants`
    ///    reports `DuplicatePid` the moment a second same-pid entity is
    ///    created and both are simultaneously queued.
    ///
    /// **What this cannot see:** an entity the adapter is holding outside
    /// the core — the kernel's `running: Option<Box<Process>>` slot
    /// deliberately stays in `kernel/src/process/scheduler.rs` (see the
    /// extraction plan's decision 2). "Every entity is in exactly one
    /// container" — counting `running` as a container — is therefore NOT a
    /// property this method can verify on its own: while an entity sits in
    /// `running`, this core does not know it exists at all. Conservation
    /// across that boundary can only be checked by a harness that models the
    /// running slot itself, which is exactly what this crate's property test
    /// `property_random_operations_preserve_invariants_and_conservation`
    /// (in `invariants.rs`) does, by keeping its own `Option<Box<Ent>>`
    /// stand-in and asserting `iter_queued().count() + running.is_some() as
    /// usize` equals the number of entities ever created.
    pub fn check_invariants(&self) -> Result<(), Violation> {
        // (1) MisplacedEntity — over ALL run-queue entities before (2) ever
        // starts, so a misplaced entity is always reported ahead of an
        // out-of-range one when both exist.
        for (found_in, queue) in self.run_queues.iter().enumerate() {
            for entity in queue.iter() {
                let effective = entity.effective_priority();
                let expected = queue_index(effective);
                if found_in != expected {
                    return Err(Violation::MisplacedEntity {
                        pid: entity.pid(),
                        found_in,
                        expected,
                        effective,
                    });
                }
            }
        }

        // (2) PriorityOutOfRange — run queues only, see doc comment above.
        for queue in self.run_queues.iter() {
            for entity in queue.iter() {
                let effective = entity.effective_priority();
                let base = entity.base_priority();
                let floor = MIN_EFFECTIVE_PRIORITY.min(base);
                if effective < floor || effective > base {
                    return Err(Violation::PriorityOutOfRange {
                        pid: entity.pid(),
                        effective,
                        base,
                        floor,
                    });
                }
            }
        }

        // (3) DuplicatePid — across run queues plus the wait queue.
        self.check_unique_pids(core::iter::empty())
    }

    /// [`Self::check_invariants`], plus the entities the adapter holds
    /// outside the core — one `running` slot per CPU (stage 7 of
    /// `docs/smp/smp-plan.md`). Adds two checks the core alone cannot make:
    ///
    /// - **`DuplicatePid`** now spans the running slots too: an entity
    ///   running on one CPU must not also be queued, or running on a second
    ///   CPU. That is the SMP scheduler's first safety property — two CPUs
    ///   resuming one entity's saved state would run it twice at once.
    /// - **`RunningNotRunning`**: a running entity must not report
    ///   `is_ready()` — the adapter marks it Running as it takes it, and a
    ///   Ready entity in a running slot is one some path forgot to hand
    ///   back.
    ///
    /// Idle entities are skipped by both: the kernel gives every CPU its own
    /// idle entity, all with pid 0, as Linux does.
    pub fn check_invariants_with_running<'a>(
        &self,
        running: impl Iterator<Item = &'a E> + Clone,
    ) -> Result<(), Violation>
    where
        E: 'a,
    {
        self.check_invariants()?;
        for entity in running.clone() {
            if !entity.is_idle() && entity.is_ready() {
                return Err(Violation::RunningNotRunning { pid: entity.pid() });
            }
        }
        self.check_unique_pids(running.filter(|e| !e.is_idle()))
    }

    fn check_unique_pids<'a>(&self, extra: impl Iterator<Item = &'a E>) -> Result<(), Violation>
    where
        E: 'a,
    {
        let mut seen: Vec<usize> = Vec::new();
        let queued = self.iter_queued().map(|e| e.pid());
        for pid in queued.chain(extra.map(|e| e.pid())) {
            if seen.contains(&pid) {
                return Err(Violation::DuplicatePid { pid });
            }
            seen.push(pid);
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::FakeClock;

    /// Minimal test entity — same role as `lib.rs`'s `TestEntity`, kept
    /// separate (and named differently) since this module tests the core's
    /// queue behavior, not just the trait's plain accessors.
    ///
    /// `pub(crate)` (along with [`ent`]/[`parked`] below): step 5's property
    /// tests in `invariants.rs` reuse this exact type rather than defining an
    /// equivalent of their own, so both test suites exercise `SchedCore`
    /// through the identical `SchedEntity` impl. Its fields stay private —
    /// nothing outside this module needs them; `invariants.rs` only ever
    /// touches `Ent` through the `SchedEntity` trait and the two
    /// constructors.
    pub(crate) struct Ent {
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

    /// Assert that exactly one entity is queued anywhere, that it physically
    /// sits in `run_queues[idx]`, and that its effective priority is `eff`.
    ///
    /// Reads the private `run_queues` field directly — legal because
    /// `mod tests` is a child module of `core`, so it sees the field that
    /// nothing outside this file can. This is deliberately stronger than
    /// trusting the index a method *returned* plus a scan via
    /// `iter_queued()`: a method that returned the right index while pushing
    /// the entity into a different queue would sail through that weaker
    /// check, because the entity's own `effective_priority` field reads the
    /// same wherever it landed.
    fn assert_only_entity_at(core: &SchedCore<Ent>, idx: usize, eff: u8) {
        let total: usize =
            core.run_queues.iter().map(|q| q.len()).sum::<usize>() + core.wait_queue.len();
        assert_eq!(total, 1, "expected exactly one queued entity, found {total}");
        assert_eq!(core.run_queues[idx].len(), 1, "entity is not in run_queues[{idx}]");
        assert_eq!(core.run_queues[idx][0].effective_priority(), eff);
    }

    pub(crate) fn ent(pid: usize, base: u8, eff: u8) -> Box<Ent> {
        Box::new(Ent { pid, base, eff, ready: true })
    }

    pub(crate) fn parked(pid: usize, base: u8, eff: u8) -> Box<Ent> {
        Box::new(Ent { pid, base, eff, ready: false })
    }

    impl Ent {
        /// What the kernel adapter does to `Process::state` as an entity
        /// moves between a run queue (Ready) and a CPU or the wait queue.
        pub(crate) fn set_ready(&mut self, ready: bool) {
            self.ready = ready;
        }
    }

    /// Stage 7: each CPU's slice is its own — exhausting one never ends
    /// another's, and starting one never refills another's. Sabotage (one
    /// shared counter again) fails the first assertion.
    #[test]
    fn slices_are_per_cpu() {
        let mut core = SchedCore::<Ent>::new();
        core.start_slice_on(0, 0); // quantum_for(0) == BASE_QUANTUM == 2
        core.start_slice_on(1, 10);
        assert!(!core.consume_quantum_on(0));
        assert!(core.consume_quantum_on(0), "cpu 0's 2-tick slice ends on its 2nd tick");
        assert!(!core.consume_quantum_on(1), "cpu 1's slice is untouched by cpu 0's ticks");
        core.start_slice_on(0, 0);
        for _ in 0..(crate::quantum_for(10) - 2) {
            assert!(!core.consume_quantum_on(1));
        }
        assert!(core.consume_quantum_on(1));
        assert!(!core.consume_quantum_on(0), "refilling cpu 0 left it a full slice");
    }

    /// `pop_next_ready_where` keeps strict priority and FIFO order among the
    /// eligible entities, and leaves a rejected one exactly where it was.
    #[test]
    fn pop_next_ready_where_skips_without_reordering() {
        let mut core = SchedCore::<Ent>::new();
        core.add_reset_to_base(ent(1, 5, 5));
        core.add_reset_to_base(ent(2, 5, 5));
        core.add_reset_to_base(ent(3, 5, 5));
        core.add_reset_to_base(ent(4, 3, 3));
        assert_eq!(core.pop_next_ready_where(|e| e.pid() != 1).unwrap().pid(), 2);
        assert_eq!(core.pop_next_ready_where(|e| e.pid() != 1).unwrap().pid(), 3);
        // Only the rejected entity is left at priority 5: fall through to 3.
        assert_eq!(core.pop_next_ready_where(|e| e.pid() != 1).unwrap().pid(), 4);
        assert!(core.pop_next_ready_where(|e| e.pid() != 1).is_none());
        assert!(core.has_ready());
        assert_eq!(core.pop_next_ready().unwrap().pid(), 1, "the skipped entity is still queued");
        assert!(!core.has_ready());
    }

    /// `check_invariants_with_running` sees the running slots: an entity
    /// running on one CPU and also queued, or running on two CPUs, is a
    /// `DuplicatePid`; a Ready entity in a running slot is
    /// `RunningNotRunning`; any number of idle (pid 0) entities is fine.
    #[test]
    fn invariants_with_running_catch_double_running() {
        let mut core = SchedCore::<Ent>::new();
        core.add_reset_to_base(ent(1, 5, 5));
        let idle_a = parked(0, 0, 0);
        let idle_b = parked(0, 0, 0);
        let mut run2 = ent(2, 5, 5);
        run2.set_ready(false);
        let ok = [&*idle_a, &*idle_b, &*run2];
        assert_eq!(core.check_invariants_with_running(ok.iter().copied()), Ok(()));

        let twice = [&*run2, &*run2];
        assert_eq!(
            core.check_invariants_with_running(twice.iter().copied()),
            Err(Violation::DuplicatePid { pid: 2 })
        );

        let mut also_queued = ent(1, 5, 5);
        also_queued.set_ready(false);
        let q = [&*also_queued];
        assert_eq!(
            core.check_invariants_with_running(q.iter().copied()),
            Err(Violation::DuplicatePid { pid: 1 })
        );

        let still_ready = ent(9, 5, 5);
        let r = [&*still_ready];
        assert_eq!(
            core.check_invariants_with_running(r.iter().copied()),
            Err(Violation::RunningNotRunning { pid: 9 })
        );
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
        assert_only_entity_at(&core, 4, 4);
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
        assert_only_entity_at(&core, NUM_PRIORITIES - 1, 200);
    }

    /// 3. `requeue_preempted` lowers effective priority by exactly 1 and
    /// enqueues at the new index. If the decay amount or target queue
    /// changed, time slices would drift under contention.
    #[test]
    fn requeue_preempted_decays_by_one() {
        let mut core = SchedCore::<Ent>::new();
        let idx = core.requeue_preempted(ent(1, 5, 5));
        assert_eq!(idx, 4);
        assert_only_entity_at(&core, 4, 4);
    }

    /// 4. `requeue_preempted` on an entity already at `MIN_EFFECTIVE_PRIORITY`
    /// leaves it unchanged. Sabotage B (`>` → `>=`) targets exactly this.
    #[test]
    fn requeue_preempted_does_not_decay_below_floor() {
        let mut core = SchedCore::<Ent>::new();
        let idx = core.requeue_preempted(ent(1, 5, MIN_EFFECTIVE_PRIORITY));
        assert_eq!(idx, MIN_EFFECTIVE_PRIORITY as usize);
        assert_only_entity_at(&core, MIN_EFFECTIVE_PRIORITY as usize, MIN_EFFECTIVE_PRIORITY);
    }

    /// 5. `requeue_preempted` on an idle entity (pid == 0) never decays it,
    /// whatever its priority. Sabotage A (dropping the `is_idle` guard)
    /// targets exactly this.
    #[test]
    fn requeue_preempted_never_decays_idle() {
        let mut core = SchedCore::<Ent>::new();
        let idx = core.requeue_preempted(ent(0, 5, 5));
        assert_eq!(idx, 5);
        assert_only_entity_at(&core, 5, 5);
    }

    /// 6. `requeue_ready` changes no priority. If it started decaying or
    /// resetting priority, a process resumed as already-Ready would
    /// silently change priority for no reason.
    #[test]
    fn requeue_ready_changes_no_priority() {
        let mut core = SchedCore::<Ent>::new();
        let idx = core.requeue_ready(ent(1, 5, 3));
        assert_eq!(idx, 3);
        assert_only_entity_at(&core, 3, 3);
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
    ///
    /// A marker entity is planted at queue 7 — strictly between the wrong
    /// index (base 9) and the right one (effective 5) — because `run_queue`/
    /// `run_queue_mut` no longer exist to inspect a specific queue directly
    /// (deleted in step 3): `iter_ready_desc`'s descending order is what
    /// exposes which queue the woken entity actually landed in. If it were
    /// enqueued by base priority (9), it would come out ahead of the marker;
    /// enqueued correctly by effective priority (5), it comes out behind.
    #[test]
    fn wake_matching_moves_from_wait_to_run_queue() {
        let mut core = SchedCore::<Ent>::new();
        core.park(parked(1, 9, 5));
        let found = core.wake_matching(|e| e.pid() == 1, |_| {});
        assert!(found);
        assert_eq!(core.wait_queue().len(), 0);
        // Asserted directly against the private field: `mod tests` is a child
        // module of `core`, so it can see `run_queues` even though nothing
        // outside this file can. That is stronger than inferring the index
        // from the relative order of a planted marker entity — it pins the
        // exact queue, which is the thing base-vs-effective gets wrong.
        assert_eq!(core.run_queues[5].len(), 1);
        assert_eq!(core.run_queues[5][0].pid(), 1);
        assert_eq!(core.run_queues[9].len(), 0, "enqueued by base priority, not effective");
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
        assert!(core.pop_next_ready().is_none(), "no run queue should have gained an entity");
    }

    /// 9. `wake_matching`'s `prepare` runs before the enqueue: `prepare`
    /// changes the effective priority here, and the entity must land in the
    /// queue matching the NEW priority, not the old one. Sabotage D
    /// (computing the index before running `prepare`) targets exactly this
    /// — and only this: tests 7/8 still pass under that sabotage, which is
    /// the point of having this test separately.
    ///
    /// Same marker technique as test 7 above: queue 4 sits strictly between
    /// the pre-`prepare` effective priority (2) and the post-`prepare` one
    /// (7), so `iter_ready_desc`'s order reveals which one the enqueue index
    /// was actually computed from.
    #[test]
    fn wake_matching_prepare_runs_before_enqueue() {
        let mut core = SchedCore::<Ent>::new();
        core.park(parked(1, 5, 2));
        let found = core.wake_matching(|e| e.pid() == 1, |e| e.set_effective_priority(7));
        assert!(found);
        assert_eq!(core.run_queues[7].len(), 1, "prepare's priority change must be reflected in the enqueue index");
        assert_eq!(core.run_queues[2].len(), 0);
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

    // ========================================================================
    // pop_next_ready (step 3)
    // ========================================================================

    /// 13. `pop_next_ready` returns the entity from the highest-index
    /// non-empty run queue. Sabotage A (ascending scan instead of `.rev()`)
    /// targets exactly this.
    #[test]
    fn pop_next_ready_returns_from_highest_priority_queue() {
        let mut core = SchedCore::<Ent>::new();
        core.add_reset_to_base(ent(1, 3, 0));
        core.add_reset_to_base(ent(2, 7, 0));
        let popped = core.pop_next_ready().unwrap();
        assert_eq!(popped.pid(), 2, "must prefer the higher-priority queue");
    }

    /// 14. `pop_next_ready` is FIFO within one queue: of two entities pushed
    /// into the same queue, the first-pushed comes out first. Sabotage B
    /// (`pop_back` instead of `pop_front`) targets exactly this.
    #[test]
    fn pop_next_ready_is_fifo_within_one_queue() {
        let mut core = SchedCore::<Ent>::new();
        core.add_reset_to_base(ent(1, 5, 0));
        core.add_reset_to_base(ent(2, 5, 0));
        let first = core.pop_next_ready().unwrap();
        assert_eq!(first.pid(), 1, "first-pushed must come out first (FIFO)");
        let second = core.pop_next_ready().unwrap();
        assert_eq!(second.pid(), 2);
    }

    /// 15. `pop_next_ready` returns `None` when every run queue is empty.
    #[test]
    fn pop_next_ready_none_when_all_queues_empty() {
        let mut core = SchedCore::<Ent>::new();
        assert!(core.pop_next_ready().is_none());
    }

    /// 16. `pop_next_ready` never looks at the wait queue: with the wait
    /// queue non-empty and every run queue empty, it returns `None` and
    /// leaves the wait queue untouched. If this method started scanning
    /// `wait_queue` too, a Blocked/Zombie/Stopped entity could get handed
    /// back out as if it were Ready.
    #[test]
    fn pop_next_ready_ignores_wait_queue() {
        let mut core = SchedCore::<Ent>::new();
        core.park(parked(1, 5, 5));
        assert!(core.pop_next_ready().is_none());
        assert_eq!(core.wait_queue().len(), 1, "wait queue must be left untouched");
    }

    // ========================================================================
    // take_first_startable (step 3)
    // ========================================================================

    /// 17. `take_first_startable` never returns an entity sitting in queue
    /// 0, even when it is the only entity anywhere. Sabotage C (starting the
    /// scan at priority 0 instead of 1) targets exactly this — the entity
    /// must still be found afterward (via `pop_next_ready`, which DOES scan
    /// queue 0), proving it was left in place rather than lost.
    #[test]
    fn take_first_startable_never_returns_queue_zero() {
        let mut core = SchedCore::<Ent>::new();
        core.add_reset_to_base(ent(1, 0, 0)); // lands in run_queue index 0
        assert!(core.take_first_startable().is_none());
        let popped = core.pop_next_ready().unwrap();
        assert_eq!(popped.pid(), 1, "entity must still be queued, untouched, in queue 0");
    }

    /// 18. `take_first_startable` skips the idle entity (pid 0) even when it
    /// sits in a higher-priority queue than a real, non-idle candidate.
    /// Sabotage D (dropping the `!is_idle()` filter) targets exactly this.
    #[test]
    fn take_first_startable_skips_idle() {
        let mut core = SchedCore::<Ent>::new();
        core.add_reset_to_base(ent(0, 5, 0)); // idle, in the higher queue (5)
        core.add_reset_to_base(ent(2, 3, 0)); // real process, in queue 3
        let started = core.take_first_startable().unwrap();
        assert_eq!(started.pid(), 2, "must skip the idle entity even though it outranks the real one");
    }

    /// 19. `take_first_startable` skips a non-ready entity and returns a
    /// ready one instead, even from a lower-priority queue.
    #[test]
    fn take_first_startable_skips_non_ready() {
        let mut core = SchedCore::<Ent>::new();
        core.add_reset_to_base(parked(3, 5, 0)); // not ready, in the higher queue (5)
        core.add_reset_to_base(ent(4, 3, 0));    // ready, in queue 3
        let started = core.take_first_startable().unwrap();
        assert_eq!(started.pid(), 4, "must skip the non-ready entity even though it outranks the ready one");
    }

    /// 20. `take_first_startable` takes from the *middle* of a queue: a
    /// non-ready entity sits at the front, a ready one right behind it — the
    /// ready one must come back, and the non-ready one must still be queued
    /// afterward. This is what `remove(i)` buys over `pop_front()`. Sabotage
    /// E (using a pop_front-equivalent instead of `remove(i)`) targets
    /// exactly this: it would wrongly return/discard the front (non-ready)
    /// entity instead of reaching past it.
    #[test]
    fn take_first_startable_takes_from_middle_of_queue() {
        let mut core = SchedCore::<Ent>::new();
        core.add_reset_to_base(parked(5, 4, 0)); // not ready, pushed first -> front
        core.add_reset_to_base(ent(6, 4, 0));    // ready, pushed second -> behind it
        let started = core.take_first_startable().unwrap();
        assert_eq!(started.pid(), 6, "must reach past the non-ready front entity via remove(i)");
        // The front entity (pid 5) must still be queued afterward.
        let remaining = core.pop_next_ready().unwrap();
        assert_eq!(remaining.pid(), 5, "the skipped entity must still be queued, not lost");
    }

    /// 21. `take_first_startable` prefers the higher-priority queue when
    /// both have a startable entity.
    #[test]
    fn take_first_startable_prefers_higher_priority_queue() {
        let mut core = SchedCore::<Ent>::new();
        core.add_reset_to_base(ent(7, 3, 0));
        core.add_reset_to_base(ent(8, 6, 0));
        let started = core.take_first_startable().unwrap();
        assert_eq!(started.pid(), 8);
    }

    // ========================================================================
    // age_processes (step 3)
    // ========================================================================

    /// 22. An entity already at its base priority is left completely alone
    /// by `age_processes`.
    #[test]
    fn age_processes_leaves_entity_at_base_alone() {
        let mut core = SchedCore::<Ent>::new();
        core.add_reset_to_base(ent(1, 5, 5));
        core.age_processes();
        let queued: alloc::vec::Vec<_> = core.iter_queued().collect();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].effective_priority(), 5);
    }

    /// 23. The idle entity is never aged, whatever its effective priority.
    /// Sabotage H (dropping the `is_idle()` skip) targets exactly this.
    #[test]
    fn age_processes_never_ages_idle() {
        let mut core = SchedCore::<Ent>::new();
        // requeue_ready (unlike add_reset_to_base) does not reset effective
        // priority to base, so this actually constructs the eff-below-base
        // situation age_processes would otherwise act on.
        core.requeue_ready(ent(0, 9, 2));
        core.age_processes();
        let queued: alloc::vec::Vec<_> = core.iter_queued().collect();
        assert_eq!(queued[0].effective_priority(), 2, "idle entity must never be aged");
    }

    /// 24. The one-step-per-call behavior, pinned explicitly: an entity
    /// with base 8 sitting at effective priority 1 in queue 1 ends a SINGLE
    /// `age_processes()` call at effective priority 2, in queue 2 — not at
    /// its base.
    ///
    /// This test used to be named
    /// `age_processes_pins_multi_boost_within_single_call` and asserted the
    /// entity landed at effective priority 8 (its base) after one call.
    /// That pinned a real bug: the outer loop ran ASCENDING
    /// (`0..NUM_PRIORITIES`), so aging (which only ever moves an entity
    /// UPWARD to a higher queue index) kept re-finding and re-promoting the
    /// same entity within one call, all the way to base, instead of the
    /// single `+1` the code's own arithmetic and the module comment in
    /// `kernel/src/process/scheduler.rs` ("boost waiting processes' eff_pri
    /// toward base") both describe. Fixed by making the outer loop
    /// DESCENDING (`(0..NUM_PRIORITIES).rev()`) — see `age_processes`'s doc
    /// comment for why that direction change is what stops the re-find.
    #[test]
    fn age_processes_boosts_exactly_one_step_per_call() {
        let mut core = SchedCore::<Ent>::new();
        core.requeue_ready(ent(1, 8, 1)); // lands in queue 1
        core.age_processes();
        // Queue 2, not queue 8: one call, exactly one promotion.
        assert_only_entity_at(&core, 2, 2);
    }

    /// 25. Every entity in a queue gets processed exactly once by a single
    /// `age_processes()` pass — none skipped, none lost — even though
    /// entities are being removed from and re-inserted into the very queue
    /// being iterated. This is the test that guards the deliberate "don't
    /// increment i after a requeue" behavior (subtlety (a) in
    /// `age_processes`'s doc comment). Sabotage F (incrementing `i` after a
    /// requeue anyway) targets exactly this, and is the most important
    /// sabotage of this step.
    ///
    /// All four entities start in the same queue (index 2). Given today's
    /// "+1 per call" behavior (test 24 above), each of the three agable
    /// entities ends this single pass exactly one step above where it
    /// started; the one already at base is left untouched. Four in, four
    /// out.
    ///
    /// This test's expected values changed when `age_processes` was fixed
    /// to age by one step per call instead of restoring all the way to
    /// base within a single call (see `age_processes`'s doc comment and
    /// `age_processes_boosts_exactly_one_step_per_call` above) — it used to
    /// assert each agable entity landed on its own base priority
    /// (`[(1, 4), (2, 6), (3, 9), (10, 2)]`); what it actually guards
    /// (every entity visited exactly once, none skipped or duplicated) is
    /// unchanged by that fix, only the per-entity post-aging value is.
    #[test]
    fn age_processes_processes_every_entity_in_a_queue_exactly_once() {
        let mut core = SchedCore::<Ent>::new();
        core.requeue_ready(ent(10, 2, 2)); // already at base — must stay untouched
        core.requeue_ready(ent(1, 4, 2));
        core.requeue_ready(ent(2, 6, 2));
        core.requeue_ready(ent(3, 9, 2));
        core.age_processes();

        let mut result: alloc::vec::Vec<(usize, u8)> =
            core.iter_queued().map(|e| (e.pid(), e.effective_priority())).collect();
        result.sort();
        assert_eq!(result, alloc::vec![(1, 3), (2, 3), (3, 3), (10, 2)]);
    }

    /// 26. Aging stops exactly at base priority: an entity with base 3 at
    /// effective 2 ends at exactly 3, in queue 3.
    ///
    /// This does NOT guard the `.min(base_priority())` call in
    /// `age_processes`, and it is worth being precise about why, because the
    /// name it originally carried ("caps_at_base_does_not_overshoot")
    /// claimed that it did. **Nothing guards that `.min()`, and nothing can:
    /// it is unreachable defensive code.** The branch it sits in only runs
    /// when `effective_priority() < base_priority()` has just been checked
    /// true, and the value is incremented by exactly 1, so `eff + 1 <= base`
    /// always holds and the clamp never has anything to clamp. Measured, not
    /// argued: deleting the `.min()` entirely leaves the whole suite green
    /// (33/33, exit 0). What this test really pins is the *terminal* step of
    /// aging — that an entity one below its base lands on it and stops —
    /// which is genuine behavior worth keeping asserted.
    #[test]
    fn age_processes_stops_exactly_at_base() {
        let mut core = SchedCore::<Ent>::new();
        core.requeue_ready(ent(1, 3, 2));
        core.age_processes();
        assert_only_entity_at(&core, 3, 3);
    }

    /// New: the outer loop's traversal direction matters. Three entities
    /// start in three distinct, non-adjacent queues, all below their base
    /// by more than one step. After a SINGLE `age_processes()` call, each
    /// must have moved by exactly one step, landing in the queue matching
    /// its new effective priority — no more.
    ///
    /// This is exactly the test an ascending outer loop (the old bug) would
    /// fail: promoting the lowest entity moves it into a queue the
    /// ascending scan has not visited yet, so the scan re-finds and
    /// re-promotes it as it walks upward, cascading it (and anything it
    /// meets along the way) toward base well past a single `+1`. A
    /// descending loop never revisits a queue it already passed, so no
    /// entity here can be found twice in one call. Sabotage S1 (reverting
    /// the outer loop to ascending) targets exactly this.
    #[test]
    fn age_processes_outer_loop_order_determines_single_step_boost() {
        let mut core = SchedCore::<Ent>::new();
        core.requeue_ready(ent(1, 9, 2)); // queue 2, far below base 9
        core.requeue_ready(ent(2, 9, 5)); // queue 5, far below base 9
        core.requeue_ready(ent(3, 9, 7)); // queue 7, far below base 9
        core.age_processes();

        let mut result: alloc::vec::Vec<(usize, u8)> =
            core.iter_queued().map(|e| (e.pid(), e.effective_priority())).collect();
        result.sort();
        assert_eq!(
            result,
            alloc::vec![(1, 3), (2, 6), (3, 8)],
            "each entity must move by exactly one step, not cascade toward base"
        );
        core.check_invariants().expect("every entity must also sit in the queue matching its new priority");
    }

    /// New: repeated `age_processes()` calls converge an entity to its base
    /// one step at a time, and further calls after it arrives are no-ops —
    /// the gradual counterpart of the old (wrong) "one call restores fully"
    /// behavior pinned by `age_processes_boosts_exactly_one_step_per_call`
    /// above.
    #[test]
    fn age_processes_converges_to_base_over_repeated_calls_then_stops() {
        let mut core = SchedCore::<Ent>::new();
        core.requeue_ready(ent(1, 6, 1)); // 5 steps needed to reach base 6

        for step in 1..=5 {
            core.age_processes();
            let eff = 1 + step;
            assert_only_entity_at(&core, eff as usize, eff);
        }

        // Already at base -- further calls must not move it past base.
        core.age_processes();
        assert_only_entity_at(&core, 6, 6);
        core.age_processes();
        assert_only_entity_at(&core, 6, 6);
    }

    // ========================================================================
    // iter_ready_desc (step 3)
    // ========================================================================

    /// 27. `iter_ready_desc` visits the highest queue index first, and
    /// insertion order within a queue, and does NOT include a parked
    /// (wait-queue) entity.
    #[test]
    fn iter_ready_desc_visits_highest_queue_first_in_insertion_order() {
        let mut core = SchedCore::<Ent>::new();
        core.add_reset_to_base(ent(1, 3, 0));
        core.add_reset_to_base(ent(2, 3, 0)); // same queue as pid 1, inserted after
        core.add_reset_to_base(ent(3, 7, 0));
        core.park(parked(4, 1, 1));

        let pids: alloc::vec::Vec<usize> = core.iter_ready_desc().map(|e| e.pid()).collect();
        assert_eq!(pids, alloc::vec![3, 1, 2], "highest queue first, insertion order within a queue, no wait-queue entries");
    }

    // ========================================================================
    // Tick accounting: start_slice / advance_ticks / consume_quantum (step 4)
    // ========================================================================

    /// 28. `start_slice` grants exactly `quantum_for(eff)` ticks: calling
    /// `consume_quantum` that many times returns `false` every time except
    /// the last, where it returns `true`. Checked at effective priority 0
    /// (quantum 2) and effective priority 10 (quantum 12). Sabotage E
    /// (`start_slice` always granting `BASE_QUANTUM`) targets exactly this at
    /// priority 10, where `BASE_QUANTUM` (2) and `quantum_for(10)` (12)
    /// diverge.
    #[test]
    fn start_slice_grants_exactly_quantum_for_ticks() {
        for eff in [0u8, 10u8] {
            let mut core = SchedCore::<Ent>::new();
            core.start_slice(eff);
            let quantum = quantum_for(eff);
            for i in 0..quantum {
                let exhausted = core.consume_quantum();
                if i + 1 == quantum {
                    assert!(exhausted, "eff={eff}: last of {quantum} ticks must report exhausted");
                } else {
                    assert!(!exhausted, "eff={eff}: tick {} of {quantum} must not yet report exhausted", i + 1);
                }
            }
        }
    }

    /// 29. `consume_quantum` on a fresh core (no `start_slice` ever called,
    /// so `remaining_ticks` starts at 0) returns `true` every time and never
    /// panics from `u32` underflow. This is the test that guards the `> 0`
    /// guard in `consume_quantum` — sabotage C (dropping that guard) either
    /// fails this as a wrong assertion or panics outright on underflow,
    /// depending on build profile (debug vs release overflow checks).
    #[test]
    fn consume_quantum_on_fresh_core_never_underflows() {
        let mut core = SchedCore::<Ent>::new();
        for i in 0..5 {
            assert!(core.consume_quantum(), "call {i}: an empty slice must always report exhausted");
        }
    }

    /// 30. `advance_ticks` returns `false` for the first `AGING_EPOCH - 1`
    /// calls and `true` on exactly the `AGING_EPOCH`-th, then `false` again
    /// right after, when the clock is advanced by exactly one tick between
    /// calls (the kernel's real cadence — see `KernelClock` in the kernel
    /// adapter). Pins "crossing, not modulo, relative to the last declared
    /// epoch": sabotage S10 (`>` instead of `>=`) would push the first
    /// `true` to call `AGING_EPOCH + 1` instead of `AGING_EPOCH`.
    #[test]
    fn advance_ticks_first_true_is_at_aging_epoch() {
        let mut core = SchedCore::<Ent>::new();
        let clock = FakeClock::new();
        let mut first_true = None;
        for i in 1..=AGING_EPOCH {
            clock.advance(1);
            if core.advance_ticks(&clock) {
                first_true = Some(i);
                break;
            }
        }
        assert_eq!(first_true, Some(AGING_EPOCH), "the first `true` must land exactly on the AGING_EPOCH-th call");
        clock.advance(1);
        assert!(!core.advance_ticks(&clock), "the very next call after an epoch must be false again");
    }

    /// 31. `advance_ticks` returns `true` on exactly the multiples of
    /// `AGING_EPOCH`, across 3 full epochs, under the same one-tick-per-call
    /// cadence as test 30. Sabotage S11 (not updating `last_epoch_tick` when
    /// it fires) would make every call after the first epoch return `true`
    /// forever instead of only on later multiples, producing a vector
    /// nothing like `[AGING_EPOCH, 2*AGING_EPOCH, 3*AGING_EPOCH]`.
    #[test]
    fn advance_ticks_true_on_every_multiple_of_aging_epoch() {
        let mut core = SchedCore::<Ent>::new();
        let clock = FakeClock::new();
        let mut true_indices = alloc::vec::Vec::new();
        for i in 1..=(3 * AGING_EPOCH) {
            clock.advance(1);
            if core.advance_ticks(&clock) {
                true_indices.push(i);
            }
        }
        assert_eq!(true_indices, alloc::vec![AGING_EPOCH, 2 * AGING_EPOCH, 3 * AGING_EPOCH]);
    }

    /// New: equivalence with the old `global_ticks % AGING_EPOCH == 0`
    /// formula, under the one-tick-per-call cadence production actually
    /// uses. This is the test that justifies the crossing-check rewrite as a
    /// pure refactor rather than a behavior change: it drives a `FakeClock`
    /// through 1, 2, 3, ..., 500 — exactly mirroring `global_ticks` after
    /// each of the old code's `wrapping_add(1)` calls — and asserts
    /// `advance_ticks` returns `true` at exactly the same call indices the
    /// old modulo formula would have. See `advance_ticks`'s doc comment for
    /// why the two formulas can diverge once ticks stop arriving one at a
    /// time (they never do here) — this test is the evidence for the "when
    /// they do arrive one at a time" half of that claim.
    #[test]
    fn advance_ticks_matches_old_modulo_formula_under_one_tick_per_call() {
        let mut core = SchedCore::<Ent>::new();
        let clock = FakeClock::new();
        for n in 1u64..=500 {
            clock.advance(1);
            let new_behavior = core.advance_ticks(&clock);
            let old_behavior = n % AGING_EPOCH as u64 == 0;
            assert_eq!(
                new_behavior, old_behavior,
                "tick {n}: new crossing-check result must match the old `global_ticks % AGING_EPOCH == 0` formula"
            );
        }
    }

    /// 32. `advance_ticks` and `consume_quantum` are independent counters:
    /// interleaving calls to one does not perturb the other. Built by
    /// driving `consume_quantum` down from a 3-tick slice with an
    /// `advance_ticks` call before each one (none of which reach an aging
    /// epoch), and separately verifying that a burst of `consume_quantum`
    /// calls exhausting the slice doesn't move `advance_ticks` off its own
    /// expected schedule.
    #[test]
    fn advance_ticks_and_consume_quantum_are_independent() {
        let mut core = SchedCore::<Ent>::new();
        let clock = FakeClock::new();
        core.start_slice(1); // quantum_for(1) == 3

        // Interleave: advance_ticks (never hits an epoch here, well below
        // AGING_EPOCH) then consume_quantum, three times — the slice must
        // still take exactly 3 consume_quantum calls to exhaust, unaffected
        // by the interleaved advance_ticks calls.
        clock.advance(1);
        assert!(!core.advance_ticks(&clock));
        assert!(!core.consume_quantum());
        clock.advance(1);
        assert!(!core.advance_ticks(&clock));
        assert!(!core.consume_quantum());
        clock.advance(1);
        assert!(!core.advance_ticks(&clock));
        assert!(core.consume_quantum(), "third consume_quantum must exhaust the quantum_for(1) == 3 slice");

        // Now drive advance_ticks the rest of the way to its own epoch,
        // calling consume_quantum (on an already-exhausted, unre-armed
        // slice) in between each — the aging epoch must still land exactly
        // on the AGING_EPOCH-th advance_ticks call, unaffected by the
        // interleaved consume_quantum calls.
        let mut first_true = None;
        for i in 4..=AGING_EPOCH {
            clock.advance(1);
            let aging_due = core.advance_ticks(&clock);
            assert!(core.consume_quantum(), "an exhausted, never-rearmed slice must keep reporting exhausted");
            if aging_due {
                first_true = Some(i);
                break;
            }
        }
        assert_eq!(first_true, Some(AGING_EPOCH), "consume_quantum calls must not shift when the aging epoch lands");
    }

    /// 33. `start_slice` re-arms a partially-consumed slice back to the FULL
    /// quantum, not just topping up the remainder: consume 1 of 3 ticks,
    /// call `start_slice` again, and the slice must once again take the
    /// full `quantum_for(eff)` calls to exhaust — not just the 2 remaining
    /// before the re-arm.
    #[test]
    fn start_slice_rearms_a_partially_consumed_slice_to_full_quantum() {
        let mut core = SchedCore::<Ent>::new();
        let eff = 1u8;
        let quantum = quantum_for(eff);
        core.start_slice(eff);
        assert!(!core.consume_quantum(), "one tick consumed out of quantum_for(1) == 3 must not yet exhaust");

        core.start_slice(eff); // re-arm to the FULL quantum, discarding the partial consumption
        for i in 0..quantum {
            let exhausted = core.consume_quantum();
            if i + 1 == quantum {
                assert!(exhausted, "re-armed slice's last tick must report exhausted");
            } else {
                assert!(!exhausted, "re-armed slice must not exhaust early, at tick {}", i + 1);
            }
        }
    }
}
