// kernel/src/process/wait.rs
//
// What a blocked process is waiting on, as far as a signal is concerned:
// whether the wait can be interrupted, how, and what the interrupted call
// becomes. The race between a waker and a signal is decided by a one-shot
// `sched::WaitCell` — see `sched/src/wait.rs` for the protocol and why it
// has to be lock-free here.
//
// Every `block_current` names its wait (`Wait`). A wait that is not
// interruptible (`Interruptible::No`) behaves as every wait did before:
// signals are queued and delivered once something else wakes it.

use alloc::sync::Arc;
pub use sched::{RestartPolicy, WaitCell};

/// How a signal interrupts this wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interruptible {
    /// It doesn't: the signal waits for the natural wakeup.
    No,
    /// By cancelling the cell armed for it (`Scheduler::begin_wait`/
    /// `arm_wait`); the registrations holding that cell go stale.
    Cell(Cleanup),
    /// `waitpid`: the only registration is `Process::waiting_for`, which the
    /// scheduler lock covers, so clearing it is the whole cancellation.
    WaitPid,
}

/// What an interruption tidies up beyond the cell (optional — a stale
/// registration is harmless — but a queued timer would otherwise sit in
/// the hrtimer queue until it expired).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cleanup {
    None,
    Timer(u32),
}

/// The wait a blocking syscall is about to enter, handed to
/// `Scheduler::block_current`.
#[derive(Debug, Clone, Copy)]
pub struct Wait {
    pub how: Interruptible,
    /// The syscall number, for re-executing the call.
    pub nr: u64,
    /// `rip` just past the call's `syscall` instruction — the return
    /// address, whether or not the frame has since been rewound (sockets
    /// rewind it before blocking).
    pub ret_rip: u64,
    pub policy: RestartPolicy,
}

impl Wait {
    pub const fn uninterruptible() -> Self {
        Self { how: Interruptible::No, nr: 0, ret_rip: 0, policy: RestartPolicy::NoHandlerOnly }
    }

    /// A cell-backed wait of syscall `nr`, returning to `ret_rip`.
    pub const fn cell(nr: u64, ret_rip: u64, policy: RestartPolicy, cleanup: Cleanup) -> Self {
        Self { how: Interruptible::Cell(cleanup), nr, ret_rip, policy }
    }
}

/// The wait a Blocked process is in (`Process::wait`).
#[derive(Debug)]
pub struct ActiveWait {
    pub wait: Wait,
    pub cell: Option<Arc<WaitCell>>,
}

/// An interruption not yet turned into `EINTR` or a restart: that depends
/// on what the signal does, known only when it is delivered
/// (`signal::deliver_pending`).
#[derive(Debug, Clone, Copy)]
pub struct Interrupted {
    pub nr: u64,
    pub ret_rip: u64,
    pub policy: RestartPolicy,
}
