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

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use diag::IrqMutex;

use crate::allocator::KernelIrq;
use crate::process::file::FileDescriptorTable;
use crate::sync::Mutex;

type Table = Arc<Mutex<FileDescriptorTable>>;

static DEAD: IrqMutex<Vec<Table>, KernelIrq> = IrqMutex::new(Vec::new());
/// Something is queued: lets [`drain`] skip the lock on every syscall.
static PENDING: AtomicBool = AtomicBool::new(false);

/// Queues a dead process's table. Callable under `SCHEDULER` and from the
/// timer ISR: it only takes `DEAD` (an `IrqMutex`, after `SCHEDULER` in
/// the lock order) and may allocate, as the scheduler already does there.
pub fn defer(table: Table) {
    DEAD.with(|d| d.push(table));
    PENDING.store(true, Ordering::Release);
}

/// Closes every queued table. **Only with no lock held** — the tables'
/// handles run their `Drop`s here. With interrupts off, as `sys_exit` and
/// `sys_close` drop theirs: a pipe end's `Drop` takes `SCHEDULER`, which
/// must never be taken with IF=1 (the idle loop, one caller, runs with it
/// on).
pub fn drain() {
    if !PENDING.swap(false, Ordering::AcqRel) {
        return;
    }
    x86_64::instructions::interrupts::without_interrupts(|| {
        let tables = DEAD.with(core::mem::take);
        drop(tables);
    });
}
