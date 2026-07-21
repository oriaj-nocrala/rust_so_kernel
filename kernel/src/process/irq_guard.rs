// kernel/src/process/irq_guard.rs
//
// RAII replacements for the hand-paired `asm!("cli")` / `asm!("sti")` calls
// that used to be scattered through syscall.rs. That manual pattern caused a
// real, reproducible full-kernel hang (see `sys_close`'s history / the
// `deadlock_scheduler_filehandle_drop` session memory): a scheduler guard
// bound to a name in the same block as an explicit `sti()` call doesn't
// actually drop (release the lock) until the block's closing brace, *after*
// `sti()` had already run — reopening interrupts while SCHEDULER was still
// held. `scheduler::TrackedSchedulerGuard` turned that into a loud assertion
// instead of a silent hang, but an assertion only *detects* the mistake —
// these two types make it impossible to write in the first place, since
// `sti`/unlock now happen exactly at Rust's own scope-exit point, on every
// return path (including early `return`/`?`) automatically.
//
// Scope: syscall.rs's self-contained, non-nested cli/sti pairs only. The
// boot/ISR/panic-path cli/sti sites in trapframe.rs, process/mod.rs, and
// panic.rs, and scheduler.rs's own `local_scheduler()`/`TrackedSchedulerGuard`
// are deliberately left alone — see `TrackedSchedulerGuard`'s doc comment for
// why automatic cli/sti was rejected at that layer (risk of corrupting IRQ
// nesting state across ISR/`jump_to_user` boundaries).
//
// Do not nest these guards on the same core. `SchedGuard` nesting would
// self-deadlock immediately via the non-reentrant `spin::Mutex` underneath
// `local_scheduler()` — loud and easy to diagnose. `InterruptGuard` nesting
// would have the inner guard's `sti` fire while the outer guard is still
// logically active — same non-nesting discipline the manual cli/sti code
// already required, just narrower in scope now (one guard's lifetime instead
// of matching two free-standing asm calls by hand).

use super::scheduler::TrackedSchedulerGuard;

/// `cli` on construction, `sti` on `Drop`. No lock.
pub struct InterruptGuard(());

impl InterruptGuard {
    pub fn new() -> Self {
        unsafe { core::arch::asm!("cli"); }
        Self(())
    }
}

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        unsafe { core::arch::asm!("sti"); }
    }
}

/// `cli` + the current core's SCHEDULER lock, unlock + `sti` on `Drop`.
///
/// Field order matters: `sched` is declared before `_irq` so it drops
/// first — releasing the lock while interrupts are still off, satisfying
/// `TrackedSchedulerGuard::drop`'s own assertion for free — and only then
/// does `_irq` drop and re-enable interrupts.
pub struct SchedGuard {
    sched: TrackedSchedulerGuard,
    _irq: InterruptGuard,
}

impl SchedGuard {
    #[track_caller]
    pub fn lock() -> Self {
        let _irq = InterruptGuard::new();
        let sched = super::scheduler::local_scheduler();
        Self { sched, _irq }
    }
}

impl core::ops::Deref for SchedGuard {
    type Target = super::scheduler::Scheduler;
    fn deref(&self) -> &Self::Target { &self.sched }
}

impl core::ops::DerefMut for SchedGuard {
    fn deref_mut(&mut self) -> &mut Self::Target { &mut self.sched }
}
