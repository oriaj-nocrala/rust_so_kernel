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
// SCHEDULER pattern.

use alloc::boxed::Box;
use alloc::sync::Arc;
use spin::Mutex;
use x86_64::{VirtAddr, structures::paging::{Page, Size4KiB}};

use super::file::{FileError, FileHandle, FileResult};
use super::{Process, ProcessState};

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
    read_waiter: Option<PipeWaiter>,
    write_waiter: Option<PipeWaiter>,
}

impl PipeBuffer {
    fn new() -> Self {
        Self {
            data: [0; PIPE_CAPACITY],
            head: 0,
            len: 0,
            readers: 1,
            writers: 1,
            read_waiter: None,
            write_waiter: None,
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

    fn try_write(&mut self, buf: &[u8]) -> usize {
        let space = PIPE_CAPACITY - self.len;
        let n = core::cmp::min(buf.len(), space);
        let tail = (self.head + self.len) % PIPE_CAPACITY;
        for i in 0..n {
            self.data[(tail + i) % PIPE_CAPACITY] = buf[i];
        }
        self.len += n;
        n
    }
}

/// Copy `src` into a blocked process's user buffer (translated via that
/// process's own `AddressSpace`). Returns bytes actually copied.
unsafe fn copy_to_user(proc: &Process, user_addr: u64, src: &[u8]) -> usize {
    // The target buffer may not be demand-paged yet (e.g. a freshly
    // allocated, never-written stack slot) — map it now rather than
    // silently truncating the copy at the first unmapped page below.
    super::ensure_user_pages_mapped(proc, user_addr, src.len() as u64);

    let phys_offset = crate::memory::physical_memory_offset();
    let mut done = 0usize;
    while done < src.len() {
        let vaddr = user_addr + done as u64;
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(vaddr));
        let page_off = (vaddr & 0xFFF) as usize;
        let chunk = core::cmp::min(src.len() - done, 0x1000 - page_off);

        let Some(frame) = (unsafe { proc.address_space.translate_page(page) }) else { break; };
        let dst = (phys_offset + frame.start_address().as_u64() + page_off as u64).as_mut_ptr::<u8>();
        unsafe { core::ptr::copy_nonoverlapping(src[done..done + chunk].as_ptr(), dst, chunk); }
        done += chunk;
    }
    done
}

/// Copy from a blocked process's user buffer into `dst` (translated via
/// that process's own `AddressSpace`). Returns bytes actually copied.
unsafe fn copy_from_user(proc: &Process, user_addr: u64, dst: &mut [u8]) -> usize {
    super::ensure_user_pages_mapped(proc, user_addr, dst.len() as u64);

    let phys_offset = crate::memory::physical_memory_offset();
    let mut done = 0usize;
    while done < dst.len() {
        let vaddr = user_addr + done as u64;
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(vaddr));
        let page_off = (vaddr & 0xFFF) as usize;
        let chunk = core::cmp::min(dst.len() - done, 0x1000 - page_off);

        let Some(frame) = (unsafe { proc.address_space.translate_page(page) }) else { break; };
        let src = (phys_offset + frame.start_address().as_u64() + page_off as u64).as_mut_ptr::<u8>();
        unsafe { core::ptr::copy_nonoverlapping(src, dst[done..done + chunk].as_mut_ptr(), chunk); }
        done += chunk;
    }
    done
}

/// Find `pid` in the wait queue (must be Blocked), run `f` on it to compute
/// its syscall return value, then wake it. `f` returns the `rax` value.
fn deliver_and_wake(pid: usize, f: impl FnOnce(&super::Process) -> u64) {
    let mut sched = super::scheduler::local_scheduler();
    if let Some(idx) = sched.wait_queue.iter().position(|p| {
        p.pid.0 == pid && matches!(p.state, ProcessState::Blocked)
    }) {
        let rax = f(&sched.wait_queue[idx]);
        sched.wait_queue[idx].trapframe.rax = rax;
    }
    sched.wake(pid);
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
    let mut sched = super::scheduler::local_scheduler();
    let mut got = 0usize;
    if let Some(idx) = sched.wait_queue.iter().position(|p| {
        p.pid.0 == waiter.pid && matches!(p.state, ProcessState::Blocked)
    }) {
        got = unsafe { copy_from_user(&sched.wait_queue[idx], waiter.user_buf, &mut dst[..want]) };
        sched.wait_queue[idx].trapframe.rax = got as u64;
    }
    sched.wake(waiter.pid);
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

        let mut pb = self.buf.lock();

        if pb.len > 0 {
            let n = pb.try_read(buf);
            let waiter = pb.write_waiter.take();
            drop(pb);

            if let Some(w) = waiter {
                // Space freed — pull bytes straight from the blocked
                // writer's buffer and stash them for future reads.
                let mut tmp = [0u8; PIPE_CAPACITY];
                let got = collect_from_writer(w, &mut tmp);
                if got > 0 {
                    self.buf.lock().try_write(&tmp[..got]);
                }
            }
            return Ok(n);
        }

        if pb.writers == 0 {
            return Ok(0); // EOF
        }

        let pid = super::scheduler::current_pid().unwrap_or(0);
        pb.read_waiter = Some(PipeWaiter {
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

        let mut pb = self.buf.lock();

        if pb.readers == 0 {
            return Err(FileError::BrokenPipe);
        }

        let n = pb.try_write(buf);
        if n > 0 {
            let waiter = pb.read_waiter.take();
            let delivered = waiter.map(|w| {
                let mut tmp = [0u8; PIPE_CAPACITY];
                let got = pb.try_read(&mut tmp);
                (w, tmp, got)
            });
            drop(pb);
            if let Some((w, tmp, got)) = delivered {
                wake_reader(w, &tmp[..got]);
            }
            return Ok(n);
        }

        // Buffer full — block until a reader frees space.
        let pid = super::scheduler::current_pid().unwrap_or(0);
        pb.write_waiter = Some(PipeWaiter {
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
            let waiter = pb.write_waiter.take();
            drop(pb);
            if let Some(w) = waiter {
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
            let waiter = pb.read_waiter.take();
            drop(pb);
            if let Some(w) = waiter {
                wake_reader(w, &[]); // EOF
            }
        }
    }
}
