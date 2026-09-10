//! The seam between the scheduler core and whatever type actually represents
//! a schedulable unit of execution in the host application.
//!
//! `SchedEntity` plays the same role for `sched` that `hal::PortIo`/
//! `hal::PhysMem` play for `hal`, that `mm::PhysMap` plays for `mm`, and that
//! `vfs::ramfs::DirLockObserver` plays for `vfs`: it is the trait a foreign,
//! kernel-only concrete type (`kernel::process::Process`) implements so the
//! host-testable core can operate on it without ever naming it, and without
//! this crate depending on `alloc`-heavy kernel globals, `TrapFrame`s,
//! `AddressSpace`s, or any other piece of the context-switch machinery that
//! has to stay in the kernel adapter (see the crate-level doc comment).
pub trait SchedEntity {
    /// This entity's process/thread id.
    ///
    /// The core never interprets this value beyond equality/ordering — it
    /// exists so the core's introspection and lookup helpers (e.g. "find the
    /// entity with this pid") can work without knowing anything else about
    /// the concrete type.
    fn pid(&self) -> usize;

    /// The ceiling priority aging restores this entity toward.
    ///
    /// Unlike `effective_priority`, this value never changes once the entity
    /// is created — it's the "home" priority, and every aging pass nudges
    /// `effective_priority` back up toward it, one step at a time, without
    /// ever overshooting it.
    fn base_priority(&self) -> u8;

    /// The priority this entity is *currently* scheduled at.
    ///
    /// This is what decays by one step on every preemption and what indexes
    /// the run queue the entity sits in (via [`crate::queue_index`]) — the
    /// value that actually drives "which queue is this entity in right now",
    /// as opposed to `base_priority`, which only matters to aging.
    fn effective_priority(&self) -> u8;

    /// Overwrite this entity's current effective priority.
    ///
    /// Called by the core exactly when it decays priority on preemption or
    /// restores it during aging — never as a free-standing setter the
    /// adapter calls on its own, since the core is the only thing that knows
    /// when the invariant "queue index matches effective priority" needs to
    /// be re-established afterward.
    fn set_effective_priority(&mut self, pri: u8);

    /// Whether this is the one, always-present idle entity (conventionally
    /// pid 0 in the kernel adapter).
    ///
    /// Exists so the core never hardcodes "pid == 0" internally: the idle
    /// entity is never aged (`age_processes` skips it — an idle task has no
    /// "home" priority worth restoring) and is never selected as the first
    /// entity started at boot (`start_first`/`take_first_startable` must
    /// pick a real process, or the system boots straight into idle and
    /// never runs anything else).
    fn is_idle(&self) -> bool;

    /// Whether this entity is currently in the `Ready` state.
    ///
    /// Only needed by the start-first scan (`take_first_startable`), which
    /// filters candidates on it — the ordinary pick-next path
    /// (`pop_next_ready`) has no analogous need, because by construction the
    /// run queues only ever contain Ready entities in the first place.
    fn is_ready(&self) -> bool;
}
