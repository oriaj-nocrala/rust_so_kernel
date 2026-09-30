// kernel/src/process/eventfd.rs
//
// eventfd2(290) / eventfd(284): a counter behind a file descriptor. `write` adds an 8-byte value, `read` returns the counter
// (or 1 with EFD_SEMAPHORE) and takes it back down, blocking (or `EAGAIN`) at zero. It is what mio's `Waker` (so tokio) uses to
// wake an epoll wait from another thread. Readable while the counter is above zero, writable while it can take another 1.
//
// Blocking follows the pty/socket pattern: a reader that finds the counter at zero registers a wait cell under the state lock,
// rewinds onto `syscall` and blocks; a `write` claims the cells and wakes them, and the read runs again.
//
// Lock order: the state lock is dropped before anything is woken; `EVENTFDS` (the registry poll asks) is only ever a leaf.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::file::{FileError, FileHandle, FileResult};
use crate::sync::Mutex;

pub const EFD_SEMAPHORE: i32 = 1;
pub const EFD_NONBLOCK: i32 = 0x800;
pub const EFD_CLOEXEC: i32 = 0x80000;

/// Largest value the counter may hold (`u64::MAX - 1`, as Linux).
const MAX_COUNT: u64 = u64::MAX - 1;

struct State {
    counter: u64,
    semaphore: bool,
    /// Readers blocked on a zero counter.
    waiters: Vec<(usize, Arc<super::wait::WaitCell>)>,
}

/// Every live eventfd by number, for `poll` (see `process::pipe::PIPES`).
static EVENTFDS: diag::IrqMutex<BTreeMap<u64, Weak<Mutex<State>>>, crate::allocator::KernelIrq> = diag::IrqMutex::new(BTreeMap::new());
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// What `poll` reports for eventfd `id`; `None` once it is gone. A contended lock is answered "ready" (at worst a spurious
/// wakeup), as `pipe::poll_mask` does: this runs from wakeup paths.
pub fn poll_mask(id: u64) -> Option<(bool, bool)> {
    let weak = match EVENTFDS.try_with(|m| m.get(&id).cloned()) {
        Some(found) => found?,
        None => return Some((true, true)),
    };
    let st = weak.upgrade()?;
    let Some(st) = st.try_lock() else { return Some((true, true)) };
    Some((st.counter > 0, st.counter < MAX_COUNT))
}

pub struct EventFd {
    state: Arc<Mutex<State>>,
    nonblock: Arc<AtomicBool>,
    id: u64,
}

/// A new eventfd with counter `initval`; `flags` are `EFD_*` (checked by the caller).
pub fn create(initval: u32, flags: i32) -> EventFd {
    let state = Arc::new(Mutex::new(State { counter: initval as u64, semaphore: flags & EFD_SEMAPHORE != 0, waiters: Vec::new() }));
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    EVENTFDS.with(|m| { m.insert(id, Arc::downgrade(&state)); });
    EventFd { state, nonblock: Arc::new(AtomicBool::new(flags & EFD_NONBLOCK != 0)), id }
}

/// Wake the readers a `write` released, and the pollers watching this eventfd. State lock already dropped.
fn wake(waiters: Vec<(usize, Arc<super::wait::WaitCell>)>, id: u64) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        if !waiters.is_empty() {
            let mut sched = super::scheduler::local_scheduler();
            for (pid, cell) in waiters {
                if cell.claim() {
                    sched.wake_or_defer(pid, super::WakePending::Restart);
                }
            }
        }
        super::syscall::poll_wakeup_for_eventfd(id);
    });
}

impl FileHandle for EventFd {
    fn read(&mut self, buf: &mut [u8]) -> FileResult<usize> {
        if buf.len() < 8 {
            return Err(FileError::InvalidInput);
        }
        // Before the state lock (lock order: `current_pid` takes the scheduler lock).
        let pid = super::scheduler::current_pid().unwrap_or(0);
        let mut st = self.state.lock();
        if st.counter > 0 {
            let value = if st.semaphore { 1 } else { st.counter };
            st.counter -= value;
            drop(st);
            buf[..8].copy_from_slice(&value.to_ne_bytes());
            // The room a read makes: pollers waiting to write.
            wake(Vec::new(), self.id);
            return Ok(8);
        }
        if self.nonblock.load(Ordering::Relaxed) {
            return Err(FileError::Again);
        }
        let tf = super::syscall::current_tf_ptr() as *mut super::TrapFrame;
        if tf.is_null() {
            return Err(FileError::Again);
        }
        let cell = Arc::new(super::wait::WaitCell::new());
        st.waiters.retain(|(p, _)| *p != pid);
        st.waiters.push((pid, cell.clone()));
        drop(st);
        super::scheduler::arm_wait(cell);
        // Block and run the read again once a write wakes us.
        unsafe { (*tf).rip -= 2; }
        Err(FileError::WouldBlock)
    }

    fn write(&mut self, buf: &[u8]) -> FileResult<usize> {
        if buf.len() < 8 {
            return Err(FileError::InvalidInput);
        }
        let value = u64::from_ne_bytes(buf[..8].try_into().unwrap());
        if value == u64::MAX {
            return Err(FileError::InvalidInput);
        }
        let mut st = self.state.lock();
        if st.counter > MAX_COUNT - value {
            // Would overflow: Linux blocks until a read makes room; here it fails like a non-blocking eventfd (a counter at
            // 2^64 - 2 is not something a program reaches).
            return Err(FileError::Again);
        }
        st.counter += value;
        let waiters = if value > 0 { core::mem::take(&mut st.waiters) } else { Vec::new() };
        drop(st);
        wake(waiters, self.id);
        Ok(8)
    }

    fn name(&self) -> &str { "<eventfd>" }

    fn dup(&self) -> Option<Box<dyn FileHandle>> {
        Some(Box::new(EventFd { state: self.state.clone(), nonblock: self.nonblock.clone(), id: self.id }))
    }

    fn nonblocking(&self) -> bool {
        self.nonblock.load(Ordering::Relaxed)
    }

    fn set_nonblocking(&self, on: bool) -> bool {
        self.nonblock.store(on, Ordering::Relaxed);
        true
    }

    fn eventfd_id(&self) -> Option<u64> {
        Some(self.id)
    }
}

impl Drop for EventFd {
    fn drop(&mut self) {
        // The last handle takes the registry entry with it.
        if Arc::strong_count(&self.state) == 1 {
            EVENTFDS.with(|m| { m.remove(&self.id); });
        }
    }
}
