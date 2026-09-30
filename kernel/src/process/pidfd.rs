// kernel/src/process/pidfd.rs
//
// pidfd_open(434) / pidfd_send_signal(424): a file descriptor that names a process (a thread-group leader). It becomes readable
// (POLLIN) when the process has exited — tokio's process driver and many supervisors wait for a child that way.
//
// Readiness lives in a registry keyed by pid, because `poll` asks from wakeup paths that cannot take `SCHEDULER` (lock order:
// the registries come before it). Only pids some pidfd names are tracked (`WATCHED`, with a count of handles): the entry is
// created by `open` (already `exited` if the process is a zombie), marked `exited` when the death reaches
// `dead_files::drain` (every death path goes through `Scheduler::kill_current`, which queues the pid there), and dropped with
// its last handle. Pids are never reused, so a stale entry cannot name another process.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;

use super::file::{FileError, FileHandle, FileResult};

struct Watch {
    refs: usize,
    exited: bool,
}

static WATCHED: diag::IrqMutex<BTreeMap<usize, Watch>, crate::allocator::KernelIrq> = diag::IrqMutex::new(BTreeMap::new());

/// Count one more handle on `pid`, starting its entry if needed (`exited` = it already is).
pub fn watch(pid: usize, already_exited: bool) {
    WATCHED.with(|m| {
        let w = m.entry(pid).or_insert(Watch { refs: 0, exited: already_exited });
        w.refs += 1;
        w.exited |= already_exited;
    });
}

/// `pid` has died: its pidfds become readable. Called from `dead_files::drain` (process context, no lock held).
pub fn mark_exited(pid: usize) {
    WATCHED.with(|m| {
        if let Some(w) = m.get_mut(&pid) {
            w.exited = true;
        }
    });
}

/// Whether the process a pidfd names has exited; `None` if nothing watches `pid`. A contended registry is answered "exited"
/// (a spurious wakeup at worst: the poller looks again), as `pipe::poll_mask` does.
pub fn exited(pid: usize) -> Option<bool> {
    match WATCHED.try_with(|m| m.get(&pid).map(|w| w.exited)) {
        Some(found) => found,
        None => Some(true),
    }
}

pub struct PidFd {
    pid: usize,
    nonblock: alloc::sync::Arc<core::sync::atomic::AtomicBool>,
}

/// A pidfd on `pid`. The caller has checked the process exists; `already_exited` if it is a zombie.
pub fn create(pid: usize, already_exited: bool, nonblock: bool) -> PidFd {
    watch(pid, already_exited);
    PidFd { pid, nonblock: alloc::sync::Arc::new(core::sync::atomic::AtomicBool::new(nonblock)) }
}

impl FileHandle for PidFd {
    fn read(&mut self, _buf: &mut [u8]) -> FileResult<usize> {
        Err(FileError::InvalidInput)
    }

    fn write(&mut self, _buf: &[u8]) -> FileResult<usize> {
        Err(FileError::InvalidInput)
    }

    fn name(&self) -> &str { "<pidfd>" }

    fn dup(&self) -> Option<Box<dyn FileHandle>> {
        watch(self.pid, false);
        Some(Box::new(PidFd { pid: self.pid, nonblock: self.nonblock.clone() }))
    }

    fn nonblocking(&self) -> bool {
        self.nonblock.load(core::sync::atomic::Ordering::Relaxed)
    }

    fn set_nonblocking(&self, on: bool) -> bool {
        self.nonblock.store(on, core::sync::atomic::Ordering::Relaxed);
        true
    }

    fn pidfd_pid(&self) -> Option<usize> {
        Some(self.pid)
    }
}

impl Drop for PidFd {
    fn drop(&mut self) {
        WATCHED.with(|m| {
            if let Some(w) = m.get_mut(&self.pid) {
                w.refs -= 1;
                if w.refs == 0 {
                    m.remove(&self.pid);
                }
            }
        });
    }
}
