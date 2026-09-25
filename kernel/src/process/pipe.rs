// kernel/src/process/pipe.rs
//
// pipe(2): anonymous, unidirectional byte-stream IPC.
//
// LOCKING / BLOCKING DESIGN
//
// `PipeReadEnd::read`/`PipeWriteEnd::write` NEVER block internally — a
// generic `FileHandle::read`/`write` call is made from `sys_read`/`sys_write`
// while the process's own `FileDescriptorTable` mutex is held, and this
// kernel's block_current()+jump_to_trapframe() diverges (never returns),
// which would leave that mutex locked forever if it happened mid-call (see
// `sys_close`'s doc comment in syscall.rs for the same hazard). Instead,
// when an operation can't complete immediately, `read`/`write` register a
// `PipeWaiter` (pid + user buffer + count) under `PipeBuffer`'s own mutex
// and return `FileError::WouldBlock`. The caller (`sys_read`/`sys_write`)
// drops the fd-table lock on that normal return, THEN performs the actual
// block — exactly mirroring how `sys_read`'s fd==0 (stdin) branch already
// handles the analogous case with `STDIN_WAITER`/`block_stdin_read`.
//
// Delivery to a blocked peer happens at wake time, computed by whichever
// side is currently running: it translates the blocked process's user
// buffer through *that process's own* `AddressSpace` (valid even though it
// isn't the active CR3 — the same phys-offset-mapping trick
// `syscall.rs::stdin_wakeup` already uses), copies bytes directly, sets
// `rax`, and wakes it. This is required because the blocked process's
// kernel-mode call stack is abandoned, not resumed — only its saved
// user-mode TrapFrame is restored when it runs again, so nothing "returns"
// to finish the transfer itself.
//
// Lock order: `PipeBuffer`'s mutex is always dropped before taking
// `SCHEDULER` (never nested), matching `sys_futex`'s FUTEX_WAITERS ->
// SCHEDULER pattern. That includes asking for the current pid: `read` and
// `write` look it up *before* locking the buffer. They used to do it with
// the buffer locked, on the way to registering as a waiter — while
// `sys_fork` holds `SCHEDULER` and locks every pipe it `dup`s: an ABBA
// deadlock that froze all CPUs (`pipe_multi_test`, a child blocking on a
// pipe while its parent forked the next one).
//
// SEVERAL WAITERS PER END
//
// Blocked readers and blocked writers each wait in a FIFO queue. This used
// to be one `Option` slot per end, and a second process blocking on the
// same end replaced the first, which then never woke (`pipe_multi_test`;
// found by four children using one pipe as a barrier). A waiter cannot go
// stale in the queue: a blocked process leaves `Blocked` only through the
// wakeup that removes it here (`kill` queues a signal and never force-wakes
// a blocked process — see `sys_kill`).

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use crate::sync::Mutex;

use super::file::{FileError, FileHandle, FileResult};
use super::Process;

const PIPE_CAPACITY: usize = 4096;

struct PipeWaiter {
    pid: usize,
    user_buf: u64,
    count: usize,
}

pub struct PipeBuffer {
    data: [u8; PIPE_CAPACITY],
    /// Index of the oldest unread byte.
    head: usize,
    /// Number of valid bytes currently stored, starting at `head`.
    len: usize,
    readers: u32,
    writers: u32,
    read_waiters: VecDeque<PipeWaiter>,
    write_waiters: VecDeque<PipeWaiter>,
    /// Free space promised to a blocked writer whose bytes a reader is
    /// collecting with this lock dropped (it cannot be held across the
    /// scheduler lock). `try_write` leaves it alone, so another writer on
    /// another CPU cannot fill it in between and make those bytes — which
    /// their writer was already told it wrote — not fit.
    reserved: usize,
}

impl PipeBuffer {
    fn new() -> Self {
        Self {
            data: [0; PIPE_CAPACITY],
            head: 0,
            len: 0,
            readers: 1,
            writers: 1,
            read_waiters: VecDeque::new(),
            write_waiters: VecDeque::new(),
            reserved: 0,
        }
    }

    fn try_read(&mut self, buf: &mut [u8]) -> usize {
        let n = core::cmp::min(buf.len(), self.len);
        for i in 0..n {
            buf[i] = self.data[(self.head + i) % PIPE_CAPACITY];
        }
        self.head = (self.head + n) % PIPE_CAPACITY;
        self.len -= n;
        n
    }

    /// Take buffered bytes for blocked readers, oldest first, each only
    /// what it asked for (the rest stays buffered). Called wherever bytes
    /// enter the ring, so a reader never sleeps while the ring has data;
    /// the caller wakes them with this lock dropped.
    fn take_deliveries(&mut self) -> Vec<(PipeWaiter, Vec<u8>)> {
        let mut deliveries = Vec::new();
        while self.len > 0 {
            let Some(w) = self.read_waiters.pop_front() else { break };
            let mut data = alloc::vec![0u8; w.count.min(self.len)];
            let got = self.try_read(&mut data);
            data.truncate(got);
            deliveries.push((w, data));
        }
        deliveries
    }

    /// Space a writer may use now: what is free, minus what is reserved.
    fn space(&self) -> usize {
        PIPE_CAPACITY - self.len - self.reserved
    }

    fn try_write(&mut self, buf: &[u8]) -> usize {
        let space = self.space();
        let n = core::cmp::min(buf.len(), space);
        let tail = (self.head + self.len) % PIPE_CAPACITY;
        for i in 0..n {
            self.data[(tail + i) % PIPE_CAPACITY] = buf[i];
        }
        self.len += n;
        n
    }
}

/// Copy `src` into a blocked process's user buffer, through that process's
/// own `AddressSpace` — with a user write's semantics (demand-mapped,
/// COW-broken; see `AddressSpace::copy_to_user`). Returns bytes copied.
unsafe fn copy_to_user(proc: &Process, user_addr: u64, src: &[u8]) -> usize {
    proc.address_space.copy_to_user(user_addr, src)
}

/// Copy from a blocked process's user buffer into `dst`, through that
/// process's own `AddressSpace`. Returns bytes copied.
unsafe fn copy_from_user(proc: &Process, user_addr: u64, dst: &mut [u8]) -> usize {
    proc.address_space.copy_from_user(user_addr, dst)
}

/// Run `f` on the waiting process `pid` to compute its syscall return value,
/// then wake it. Usually it is Blocked; with several CPUs it can also still
/// be running, between registering here and blocking — then the result
/// waits in `Process::wake_pending` for its `block_current` (a pipe waiter is
/// registered only on the way to blocking and removed by this very wakeup,
/// so it is never stale). `f` returns the `rax` value.
fn deliver_and_wake(pid: usize, f: impl FnOnce(&super::Process) -> u64) {
    let mut sched = super::scheduler::local_scheduler();
    if !sched.deliver_to_waiter(pid, f) {
        crate::ktrace!(crate::debug::FS, "pipe wake pid={}: NOT WAITING, delivery dropped", pid);
    }
}

/// Hand `data` straight to a blocked reader (or wake it with a 0-byte EOF
/// read if `data` is empty).
fn wake_reader(waiter: PipeWaiter, data: &[u8]) {
    let n = core::cmp::min(data.len(), waiter.count);
    deliver_and_wake(waiter.pid, |proc| unsafe {
        copy_to_user(proc, waiter.user_buf, &data[..n]) as u64
    });
}

/// Pull up to `dst.len()` bytes from a blocked writer's user buffer into
/// `dst` and wake it with that count as its write() return value. Returns
/// bytes actually copied so the caller can push them into the ring buffer.
fn collect_from_writer(waiter: PipeWaiter, dst: &mut [u8]) -> usize {
    let want = core::cmp::min(dst.len(), waiter.count);
    let mut got = 0usize;
    deliver_and_wake(waiter.pid, |proc| {
        got = unsafe { copy_from_user(proc, waiter.user_buf, &mut dst[..want]) };
        got as u64
    });
    got
}

/// Wake a blocked writer with a negative-errno return value (its last
/// reader closed while it slept) — no data transfer.
fn wake_writer_error(waiter: PipeWaiter, errno: i64) {
    deliver_and_wake(waiter.pid, |_| errno as u64);
}

pub struct PipeReadEnd {
    buf: Arc<Mutex<PipeBuffer>>,
}

pub struct PipeWriteEnd {
    buf: Arc<Mutex<PipeBuffer>>,
}

/// Create a connected pipe (read end, write end) with one open reference
/// on each side, matching what `pipe(2)` hands back.
pub fn create() -> (PipeReadEnd, PipeWriteEnd) {
    let buf = Arc::new(Mutex::new(PipeBuffer::new()));
    (PipeReadEnd { buf: buf.clone() }, PipeWriteEnd { buf })
}

impl FileHandle for PipeReadEnd {
    fn read(&mut self, buf: &mut [u8]) -> FileResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        // Before the buffer lock: see the lock-order note at the top.
        let pid = super::scheduler::current_pid().unwrap_or(0);
        let mut pb = self.buf.lock();
        crate::ktrace!(crate::debug::FS, "pipe read: want={} len={} writers={} write_waiters={}",
            buf.len(), pb.len, pb.writers, pb.write_waiters.len());

        if pb.len > 0 {
            let n = pb.try_read(buf);
            // The space this read freed goes to blocked writers, oldest
            // first, each told it wrote whatever `collect_from_writer`
            // pulled — so never more than is free.
            loop {
                let space = pb.space();
                if space == 0 {
                    break;
                }
                let Some(w) = pb.write_waiters.pop_front() else { break };
                pb.reserved += space;
                drop(pb);
                let mut tmp = [0u8; PIPE_CAPACITY];
                let got = collect_from_writer(w, &mut tmp[..space]);
                pb = self.buf.lock();
                pb.reserved -= space;
                let stored = pb.try_write(&tmp[..got]);
                debug_assert_eq!(stored, got);
                // A reader may have found the ring empty and queued while
                // the lock was dropped.
                let deliveries = pb.take_deliveries();
                if !deliveries.is_empty() {
                    drop(pb);
                    for (w, data) in deliveries {
                        wake_reader(w, &data);
                    }
                    pb = self.buf.lock();
                }
            }
            return Ok(n);
        }

        if pb.writers == 0 {
            return Ok(0); // EOF
        }

        pb.read_waiters.push_back(PipeWaiter {
            pid,
            user_buf: buf.as_ptr() as u64,
            count: buf.len(),
        });
        Err(FileError::WouldBlock)
    }

    fn write(&mut self, _buf: &[u8]) -> FileResult<usize> {
        Err(FileError::NotSupported)
    }

    fn name(&self) -> &str { "<pipe:r>" }

    fn dup(&self) -> Option<Box<dyn FileHandle>> {
        self.buf.lock().readers += 1;
        Some(Box::new(PipeReadEnd { buf: self.buf.clone() }))
    }
}

impl FileHandle for PipeWriteEnd {
    fn read(&mut self, _buf: &mut [u8]) -> FileResult<usize> {
        Err(FileError::NotSupported)
    }

    fn write(&mut self, buf: &[u8]) -> FileResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        // Before the buffer lock: see the lock-order note at the top.
        let pid = super::scheduler::current_pid().unwrap_or(0);
        let mut pb = self.buf.lock();

        if pb.readers == 0 {
            return Err(FileError::BrokenPipe);
        }

        let n = pb.try_write(buf);
        crate::ktrace!(crate::debug::FS, "pipe write: want={} wrote={} len={} read_waiters={}",
            buf.len(), n, pb.len, pb.read_waiters.len());
        if n > 0 {
            let deliveries = pb.take_deliveries();
            drop(pb);
            for (w, data) in deliveries {
                wake_reader(w, &data);
            }
            return Ok(n);
        }

        // Buffer full — block until a reader frees space.
        pb.write_waiters.push_back(PipeWaiter {
            pid,
            user_buf: buf.as_ptr() as u64,
            count: buf.len(),
        });
        Err(FileError::WouldBlock)
    }

    fn name(&self) -> &str { "<pipe:w>" }

    fn dup(&self) -> Option<Box<dyn FileHandle>> {
        self.buf.lock().writers += 1;
        Some(Box::new(PipeWriteEnd { buf: self.buf.clone() }))
    }
}

impl Drop for PipeReadEnd {
    fn drop(&mut self) {
        let mut pb = self.buf.lock();
        pb.readers -= 1;
        if pb.readers == 0 {
            let waiters = core::mem::take(&mut pb.write_waiters);
            drop(pb);
            for w in waiters {
                wake_writer_error(w, super::syscall::errno::EPIPE);
            }
        }
    }
}

impl Drop for PipeWriteEnd {
    fn drop(&mut self) {
        let mut pb = self.buf.lock();
        pb.writers -= 1;
        if pb.writers == 0 {
            let waiters = core::mem::take(&mut pb.read_waiters);
            drop(pb);
            for w in waiters {
                wake_reader(w, &[]); // EOF
            }
        }
    }
}
