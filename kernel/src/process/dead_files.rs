//! The fd tables of processes that died without `sys_exit`, waiting to be
//! closed where closing is safe.
//!
//! A process killed by a signal or a fault dies inside the scheduler
//! (`Scheduler::kill_current`, reached from `resolve_signals` and the fault
//! handlers), with `SCHEDULER` held and sometimes inside the timer ISR.
//! Closing its files there cannot work: a socket's `Drop` wakes its peer
//! through `SCHEDULER` (not reentrant), and a pipe end's takes the pipe's
//! buffer lock, which the code the ISR interrupted may be holding.
//!
//! It used to not close them at all — the zombie kept its table until a
//! `waitpid` reaped it, and the reap dropped it **under `SCHEDULER`**:
//! every CPU hung the first time a process holding a socket was
//! `SIGKILL`ed and then reaped (the compositor, 2026-09-25). Until then
//! its peers never saw EOF either, however long the parent took to wait —
//! Linux closes a dying process's files in `do_exit`, before the zombie
//! exists.
//!
//! So `kill_current` moves the table here, and [`drain`] drops it from
//! process context with no lock held — the context `sys_close` runs in:
//! at the entry of every syscall, and in every CPU's idle loop.
//!
//! Address spaces ride the same queue for the same reason: the last drop of one frees every page it maps (`defer_space`).

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use diag::IrqMutex;

use crate::allocator::KernelIrq;
use crate::memory::address_space::AddressSpace;
use crate::process::file::FileDescriptorTable;
use crate::sync::Mutex;

type Table = Arc<Mutex<FileDescriptorTable>>;
type Space = Arc<AddressSpace>;

static DEAD: IrqMutex<Vec<Table>, KernelIrq> = IrqMutex::new(Vec::new());
/// Address spaces whose last owner may be going away: dropping one frees every page of it (seconds for a 15 MB Vulkan program),
/// which no code holding `SCHEDULER` may do — every CPU's tick and syscall entry waits behind that lock.
static DEAD_SPACES: IrqMutex<Vec<Space>, KernelIrq> = IrqMutex::new(Vec::new());
/// Thread-group leaders that died, for `pidfd` readiness (`process::pidfd`); drained with the tables.
static DEAD_PIDS: IrqMutex<Vec<usize>, KernelIrq> = IrqMutex::new(Vec::new());
/// Something is queued: lets [`drain`] skip the lock on every syscall.
static PENDING: AtomicBool = AtomicBool::new(false);
/// Drains that have taken the queue and are still closing what they took.
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// Queues a dead process's table. Callable under `SCHEDULER` and from the
/// timer ISR: it only takes `DEAD` (an `IrqMutex`, after `SCHEDULER` in
/// the lock order) and may allocate, as the scheduler already does there.
pub fn defer(table: Table) {
    DEAD.with(|d| d.push(table));
    PENDING.store(true, Ordering::Release);
}

/// Queues an address space to be dropped by [`drain`], where freeing it holds up nobody. Callable under `SCHEDULER` and from the timer
/// ISR, like [`defer`]. The caller must have switched its CPU off the space's table first (the queue keeps the `Arc`, so nothing is
/// freed early, but `drain` may run on any CPU the moment this returns): see `Scheduler::retire_space`.
pub fn defer_space(space: Space) {
    DEAD_SPACES.with(|d| d.push(space));
    PENDING.store(true, Ordering::Release);
}

/// Drops `space` where the caller stands if that frees nothing (the kernel's own space), else queues it. For code that holds
/// `SCHEDULER` and has a process's space in hand: a plain `drop` there may be the one that frees it.
pub fn release_space(space: Space) {
    if space.is_kernel() {
        drop(space);
    } else {
        defer_space(space);
    }
}

/// Queues the death of thread-group leader `pid`: `drain` marks its pidfds ready and wakes whoever polls them.
pub fn defer_exit(pid: usize) {
    DEAD_PIDS.with(|d| d.push(pid));
    PENDING.store(true, Ordering::Release);
}

/// Closes every queued table. **Only with no lock held** — the tables'
/// handles run their `Drop`s here. With interrupts off, as `sys_exit` and
/// `sys_close` drop theirs: a pipe end's `Drop` takes `SCHEDULER`, which
/// must never be taken with IF=1 (the idle loop, one caller, runs with it
/// on).
pub fn drain() {
    if !PENDING.load(Ordering::Acquire) {
        return;
    }
    // The whole thing with interrupts off, the count included: the idle loop calls this with IF=1, and a tick between the increment
    // and the decrement would switch away from a per-CPU idle process that never runs again, leaving `settle` waiting for ever.
    x86_64::instructions::interrupts::without_interrupts(|| {
        // Announced before the queue is taken, so anyone who then finds `PENDING` clear can tell a drain is still closing files.
        IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
        if PENDING.swap(false, Ordering::SeqCst) {
            let tables = DEAD.with(core::mem::take);
            drop(tables);
            let spaces = DEAD_SPACES.with(core::mem::take);
            drop(spaces);
            for pid in DEAD_PIDS.with(core::mem::take) {
                crate::process::pidfd::mark_exited(pid);
                crate::process::syscall::poll_wakeup_for_pidfd(pid);
            }
        }
        IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
    });
}

/// [`drain`], then wait for any drain another CPU has started to finish. What `wait4` needs: when it hands a child back, that
/// child's files must be closed (Linux closes them in `do_exit`, before the zombie exists), and the CPU that took the queue may
/// still be running their `Drop`s. Without this a parent could `wait` for a dead holder of an exclusive device (`/dev/nvgpu`,
/// `/dev/fb0`) and find it still busy. Call with no lock held.
pub fn settle() {
    drain();
    while IN_FLIGHT.load(Ordering::SeqCst) != 0 {
        // A drainer may be waiting for a TLB shootdown this CPU has not answered.
        crate::memory::tlb::service_pending();
        core::hint::spin_loop();
    }
}
