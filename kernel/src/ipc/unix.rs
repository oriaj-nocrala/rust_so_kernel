// kernel/src/ipc/unix.rs
//
// AF_UNIX adapter: the kernel half of the `usock` crate.
//
// `usock` owns every socket state machine — queues, backlogs, half-close,
// the abstract namespace, SCM_RIGHTS — and is host-tested (`cd usock &&
// cargo test`). What cannot leave the kernel lives here:
//
//   • the one global socket table, and its interrupt discipline;
//   • `UnixSocketHandle`, the `FileHandle` that puts a socket behind an fd
//     (so `read`/`write`/`dup`/`close` work on one like any other file);
//   • blocking: parking a process on a socket and waking it again.
//
// ── Interrupt discipline ────────────────────────────────────────────────
//
// `SOCKETS` is a `diag::IrqMutex`, not a `spin::Mutex`, for the reason
// CLAUDE.md's "Key Design Invariants" records for `BUDDY`/`SLAB_ALLOCATOR`:
// it is taken on paths that allocate, and it has no `lock()` at all — only
// `with`/`try_with`, which disable interrupts *first*. A plain mutex here
// would be a latent version of the ~1-in-10 boot hang that took months to
// find: a process holding the table when the timer fires, preempted, while
// the next process spins for the same lock with interrupts off.
//
// Lock order is `SOCKETS` → (release) → `SCHEDULER`, never nested — the
// same rule `pipe.rs` and `sys_futex` already follow. Every function here
// that has to wake somebody computes what to do inside `SOCKETS.with(...)`,
// lets the guard drop, and only then takes the scheduler.
//
// ── How blocking works, and why it differs from pipe.rs ─────────────────
//
// `pipe.rs` blocks by having the *waker* finish the sleeper's work: it
// translates the blocked process's user buffer through that process's own
// `AddressSpace`, copies the bytes, writes `rax`, and wakes it. That is
// necessary there because a blocked process's kernel stack is abandoned —
// only its saved user TrapFrame is ever restored.
//
// Sockets take the other road: **restart the syscall**. `block_on` rewinds
// the saved TrapFrame's `rip` by 2 (the width of `syscall`) before parking
// the process, so when it is woken it re-executes the syscall from the
// beginning and re-evaluates everything. `rax` still holds the syscall
// number the stub pushed — it is only overwritten on a normal return, which
// this path never takes — and the argument registers were never touched.
//
// Two reasons this is the better fit here, not just a shortcut:
//
//   1. `accept`, `connect`, `sendmsg`, `recvfrom`… each have a different
//      completion (install an fd; encode a `sockaddr`; consume an iovec).
//      Reproducing all of that from inside another process's address space
//      would be the bulk of this file, written twice.
//   2. The old channel code had exactly one global `ACCEPT_WAITER` and one
//      `RECV_WAITER` slot: a *second* process blocking on the same
//      operation silently overwrote the first, which then never woke up.
//      Restarting needs no per-waiter state beyond a pid, so any number of
//      processes can wait on one socket.
//
// Signals keep working: a woken process passes through
// `scheduler::resolve_signals` on its way back to user mode, so a pending
// handler runs first and the rewound syscall re-executes after its
// `sigreturn` — which is precisely Linux's `SA_RESTART` behavior.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use diag::IrqMutex;
use usock::{PollMask, SockError, SocketId, SocketTable, UnixAddr, Wakes};

use crate::allocator::KernelIrq;
use crate::process::file::{FileError, FileHandle, FileResult};

/// Every AF_UNIX socket in the system.
///
/// `Box<dyn FileHandle>` is the `SCM_RIGHTS` payload: a descriptor in flight
/// is an owned handle sitting in a queue, exactly like one sitting in a
/// `FileDescriptorTable`, and is installed into the receiver's table
/// verbatim when it arrives.
pub static SOCKETS: IrqMutex<SocketTable<Box<dyn FileHandle>>, KernelIrq> =
    IrqMutex::new(SocketTable::new());

/// Processes parked on a socket, waiting for it to become ready.
///
/// A waiter is just `(pid, socket)`: waking one means making it runnable so
/// it can re-execute its syscall (see the module comment). `spin::Mutex` is
/// enough here — this is only ever touched with interrupts already off, and
/// nothing inside the critical section allocates except the `Vec` itself.
static WAITERS: IrqMutex<Vec<Waiter>, KernelIrq> = IrqMutex::new(Vec::new());

/// Bumped by every `dispatch_wakes` that wakes anything, *before* it scans
/// `WAITERS`. Closes the gap between a socket operation failing with
/// `Again` and its caller registering in `WAITERS` (stage 7 of
/// docs/smp/smp-plan.md): a wakeup run entirely inside that gap, on another
/// CPU, finds no waiter — but it moves this counter, which the caller read
/// before its operation and checks again after registering (`register_retry`),
/// and then retries instead of sleeping on a wakeup that already happened.
static WAKE_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Read before a socket operation that may have to block; hand the value to
/// `block_on`/`register_retry`. See `WAKE_EPOCH`.
pub fn wake_epoch() -> u64 {
    WAKE_EPOCH.load(Ordering::SeqCst)
}

struct Waiter {
    pid: usize,
    sock: SocketId,
}

// ────────────────────────────────────────────────────────────────────────
// Error mapping
// ────────────────────────────────────────────────────────────────────────

/// `usock`'s error → this kernel's negative-errno syscall return.
pub fn errno_of(e: SockError) -> i64 {
    -(e.errno() as i64)
}

// ────────────────────────────────────────────────────────────────────────
// The fd-facing handle
// ────────────────────────────────────────────────────────────────────────

/// A socket behind a file descriptor.
///
/// `read`/`write` are real: a connected AF_UNIX socket is readable and
/// writable with the ordinary syscalls, which is how `dup2`-ing one onto
/// stdin/stdout works. They follow `pipe.rs`'s contract exactly — never
/// block internally, return `WouldBlock` and let `sys_read`/`sys_write` do
/// the parking, because blocking here would strand the fd-table mutex the
/// caller is holding.
pub struct UnixSocketHandle {
    id: SocketId,
    /// `O_NONBLOCK`. Shared across `dup()` because it is a property of the
    /// open file description, not of the descriptor — same reason
    /// `RamFileHandle`'s offset is an `Arc`.
    nonblock: Arc<AtomicBool>,
}

impl UnixSocketHandle {
    pub fn new(id: SocketId) -> Self {
        Self { id, nonblock: Arc::new(AtomicBool::new(false)) }
    }

    /// Set `O_NONBLOCK` before the handle is installed (what `socket()`'s
    /// `SOCK_NONBLOCK` flag does).
    pub fn set_nonblocking(&self, on: bool) {
        self.nonblock.store(on, Ordering::Relaxed);
    }

    fn peer_or_self(&self) -> SocketId {
        SOCKETS
            .with(|t| t.get(self.id).and_then(|s| s.peer_id()))
            .unwrap_or(self.id)
    }
}

impl FileHandle for UnixSocketHandle {
    fn read(&mut self, buf: &mut [u8]) -> FileResult<usize> {
        loop {
        let epoch = wake_epoch();
        let (result, wakes) = SOCKETS.with(|t| {
            let mut wakes = Wakes::default();
            let r = t.recv(self.id, buf, false).map(|o| {
                wakes = o.wakes;
                // A plain read() has nowhere to put ancillary descriptors;
                // Linux closes them rather than losing them silently.
                (o.n, o.fds)
            });
            (r, wakes)
        });
        dispatch_wakes(&wakes);

        return match result {
            Ok((n, fds)) => {
                drop(fds); // closes any SCM_RIGHTS a read() could not report
                Ok(n)
            }
            Err(SockError::Again) if self.nonblock.load(Ordering::Relaxed) => {
                Err(FileError::Again)
            }
            Err(SockError::Again) => {
                if !register_retry(self.id, epoch) {
                    continue;
                }
                Err(FileError::WouldBlock)
            }
            Err(e) => Err(file_error_of(e)),
        };
        }
    }

    fn write(&mut self, buf: &[u8]) -> FileResult<usize> {
        loop {
        let epoch = wake_epoch();
        let mut fds = Vec::new();
        let (result, wakes) = SOCKETS.with(|t| {
            let r = t.send(self.id, buf, &mut fds, None);
            let wakes = match &r {
                Ok(o) => o.wakes.clone(),
                Err(_) => Wakes::default(),
            };
            (r.map(|o| o.written), wakes)
        });
        dispatch_wakes(&wakes);

        return match result {
            Ok(n) => Ok(n),
            Err(SockError::Again) if self.nonblock.load(Ordering::Relaxed) => {
                Err(FileError::Again)
            }
            Err(SockError::Again) => {
                // Wait on the *peer's* queue: that is the one that has to
                // drain before this send can make progress.
                if !register_retry(self.peer_or_self(), epoch) {
                    continue;
                }
                Err(FileError::WouldBlock)
            }
            Err(e) => Err(file_error_of(e)),
        };
        }
    }

    fn name(&self) -> &str {
        "<socket>"
    }

    fn socket_id(&self) -> Option<usize> {
        Some(self.id)
    }

    fn dup(&self) -> Option<Box<dyn FileHandle>> {
        SOCKETS.with(|t| t.retain(self.id)).ok()?;
        Some(Box::new(UnixSocketHandle { id: self.id, nonblock: self.nonblock.clone() }))
    }

    fn stat(&self) -> Option<vfs::types::Stat> {
        Some(vfs::types::Stat::socket(self.id as u64))
    }

    fn nonblocking(&self) -> bool {
        self.nonblock.load(Ordering::Relaxed)
    }

    fn set_nonblocking(&self, on: bool) -> bool {
        self.nonblock.store(on, Ordering::Relaxed);
        true
    }
}

impl Drop for UnixSocketHandle {
    // Reference counting lives in `Drop`, not in `FileHandle::close()`:
    // `FileDescriptorTable::close` calls `close()` *and then* drops the box,
    // so splitting the work across both would decrement twice. Same choice
    // `pipe.rs` made for the same reason.
    fn drop(&mut self) {
        let (fds, wakes) = SOCKETS.with(|t| {
            let out = t.close(self.id);
            (out.fds, out.wakes)
        });
        // Dropped outside the closure on purpose: one of these handles may
        // itself be a socket, whose own `Drop` re-enters `SOCKETS` — and
        // `IrqMutex` is not reentrant.
        drop(fds);
        dispatch_wakes(&wakes);
    }
}

fn file_error_of(e: SockError) -> FileError {
    match e {
        SockError::Again => FileError::Again,
        SockError::Pipe => FileError::BrokenPipe,
        SockError::Inval | SockError::NotConn | SockError::NotSock => FileError::InvalidArgument,
        SockError::BadF => FileError::BadFileDescriptor,
        _ => FileError::IOError,
    }
}

// ────────────────────────────────────────────────────────────────────────
// fd → socket
// ────────────────────────────────────────────────────────────────────────

/// The socket behind `fd` in the current process, or `ENOTSOCK`.
///
/// Goes through `FileHandle::socket_id()` — no pid-indexed side table, no
/// downcasting. The scheduler lock is taken and released here and nowhere
/// else in the lookup, so callers can go on to take `SOCKETS` safely.
pub fn socket_of_fd(fd: i32) -> Result<SocketId, i64> {
    if fd < 0 {
        return Err(-(SockError::BadF.errno() as i64));
    }
    let files = {
        let guard = crate::process::irq_guard::SchedGuard::lock();
        match guard.running_ref() {
            Some(proc) => proc.files.clone(),
            None => return Err(-3), // ESRCH
        }
    };
    let id = {
        let guard = files.lock();
        match guard.get(fd as usize) {
            Ok(h) => h.socket_id(),
            Err(_) => return Err(-(SockError::BadF.errno() as i64)),
        }
    };
    id.ok_or(-(SockError::NotSock.errno() as i64))
}

/// Whether `fd` is a socket opened `O_NONBLOCK`.
pub fn fd_is_nonblocking(fd: i32) -> bool {
    let files = {
        let guard = crate::process::irq_guard::SchedGuard::lock();
        match guard.running_ref() {
            Some(proc) => proc.files.clone(),
            None => return false,
        }
    };
    let guard = files.lock();
    match guard.get(fd as usize) {
        Ok(h) => h.nonblocking(),
        Err(_) => false,
    }
}

// ────────────────────────────────────────────────────────────────────────
// Blocking and waking
// ────────────────────────────────────────────────────────────────────────

/// Park the running process until `sock` becomes ready, then re-execute its
/// syscall from the `syscall` instruction itself.
///
/// Never returns: it ends in `jump_to_user`, like every other blocking path
/// in this kernel. Interrupts must already be off (the caller holds an
/// `InterruptGuard`), and no lock may be held — this abandons the current
/// kernel stack, so a live guard would never run its `Drop`.
///
/// # Safety contract
/// The current TrapFrame must be a real syscall entry frame: `rip` pointing
/// just past a 2-byte `syscall`, `rax` still holding the syscall number.
/// That is true for every caller (`CURRENT_SYSCALL_TF` is set by
/// `syscall_handler_asm` and only read here and in the other blocking
/// syscalls).
pub fn block_on(sock: SocketId, epoch: u64) -> ! {
    // Deliberately never dropped: this function ends in `jump_to_user`, so
    // interrupts stay off across the jump and are restored by the next
    // process's own `iretq` — the same shape every other blocking syscall
    // path here uses.
    let _irq = crate::process::irq_guard::InterruptGuard::new();
    let tf_ptr = crate::process::syscall::current_tf_ptr();
    if !register_retry(sock, epoch) {
        // A wakeup already came and went: re-execute the syscall now
        // instead of sleeping (`rax` still holds its number, see
        // `register_retry`).
        unsafe {
            (*(tf_ptr as *mut crate::process::TrapFrame)).rip -= 2;
            crate::process::trapframe::jump_to_user(tf_ptr)
        }
    }

    let next_tf = {
        let mut sched = crate::process::scheduler::local_scheduler();
        // A wakeup that lands after the registration but before this block
        // is left pending on the process (`Scheduler::wake_or_defer`), and
        // `block_current` then returns this same, rewound frame.
        sched.block_current(tf_ptr)
    };
    unsafe { crate::process::trapframe::jump_to_user(next_tf) }
}

/// Register the running process as waiting on `sock`, and rewind its saved
/// `rip` so the syscall it is in re-executes when it is woken.
///
/// Split out of [`block_on`] for `FileHandle::read`/`write`: those cannot
/// block themselves (they run with the caller's fd-table mutex held — see
/// `pipe.rs`'s module comment for the hazard), so they mark the retry here
/// and return `WouldBlock`, and `sys_read`/`sys_write` perform the actual
/// park afterwards.
///
/// **Every caller must genuinely block afterwards.** Rewinding `rip` and
/// then returning normally would make the process re-enter `syscall` with
/// `rax` holding a *return value* instead of a syscall number. Both call
/// sites satisfy this: `sys_read`/`sys_write`'s `WouldBlock` arms end in
/// `jump_to_user` and never return.
///
/// Returns `false`, with nothing registered and the frame left alone, if a
/// wakeup ran since `epoch` was read (`WAKE_EPOCH`): the caller must retry
/// its operation rather than sleep.
///
/// Known wart, inherited rather than introduced: a multi-iovec `writev()`
/// that fills the socket partway through re-executes from the first iovec
/// after the retry, re-sending what already went out. The same call is
/// already broken for a pipe today (it loses its running total instead),
/// and fixing it properly means making `sys_writev` a single `write()` of a
/// gathered buffer — out of scope here.
fn register_retry(sock: SocketId, epoch: u64) -> bool {
    let tf = crate::process::syscall::current_tf_ptr() as *mut crate::process::TrapFrame;
    if tf.is_null() {
        // No syscall frame: the caller is kernel code driving a socket
        // directly, not a process in a syscall (`hw_tests`'s adapter test
        // is the one such caller today). There is nothing to rewind and no
        // process to wake, and `WouldBlock` is still the right answer for
        // the caller to see — so record nothing rather than rewinding a
        // frame that does not exist. Found by that test: this used to
        // dereference the null pointer unconditionally.
        return true;
    }

    let pid = crate::process::scheduler::current_pid().unwrap_or(0);
    WAITERS.with(|w| {
        if !w.iter().any(|x| x.pid == pid && x.sock == sock) {
            w.push(Waiter { pid, sock });
        }
    });
    if WAKE_EPOCH.load(Ordering::SeqCst) != epoch {
        WAITERS.with(|w| w.retain(|x| !(x.pid == pid && x.sock == sock)));
        return false;
    }

    unsafe {
        // Rewind onto the `syscall` instruction (0F 05, two bytes) so the
        // whole call happens again when this process next runs. `rax` is
        // untouched and still holds the syscall number: the entry stub only
        // overwrites that slot when the handler returns normally.
        (*tf).rip -= 2;
    }
    true
}

/// Wake every process parked on any of the sockets named in `wakes`.
///
/// Must be called with `SOCKETS` already released: it takes the scheduler,
/// and the one-way lock order is `SOCKETS` → `SCHEDULER`.
///
/// **Interrupts are saved and restored, never unconditionally re-enabled.**
/// `SchedGuard`/`InterruptGuard` would `sti` on the way out, and one caller
/// is `UnixSocketHandle::drop` — which runs inside `sys_exit`, on the kernel
/// stack of a process already queued for deferred free. Re-enabling
/// interrupts there lets a timer tick land on that stack and free it from
/// under this code; CLAUDE.md's "sys_exit must keep IF=0 all the way to the
/// iretq" invariant is exactly this hazard, and it was not theoretical here:
/// the first boot of this code panicked in `timer_preempt`'s corrupt-frame
/// detector the moment a process with an open socket exited.
pub fn dispatch_wakes(wakes: &Wakes) {
    if wakes.is_empty() {
        return;
    }
    // Before the scan, never after: see `WAKE_EPOCH`.
    WAKE_EPOCH.fetch_add(1, Ordering::SeqCst);

    let mut pids: Vec<usize> = Vec::new();
    WAITERS.with(|w| {
        w.retain(|waiter| {
            let hit = wakes.readable.contains(&waiter.sock)
                || wakes.writable.contains(&waiter.sock)
                || wakes.acceptable.contains(&waiter.sock);
            if hit {
                pids.push(waiter.pid);
            }
            !hit
        });
    });

    if !pids.is_empty() {
        x86_64::instructions::interrupts::without_interrupts(|| {
            let mut sched = crate::process::scheduler::local_scheduler();
            for pid in pids {
                // The waiter may still be on its way to `block_current` on
                // another CPU: then the wakeup waits for it there.
                sched.wake_or_defer(pid, crate::process::WakePending::Restart);
            }
        });
    }

    // poll()/epoll_wait() sleepers are tracked separately, by fd.
    for id in wakes.readable.iter().chain(wakes.acceptable.iter()) {
        crate::process::syscall::poll_wakeup_for_socket(*id);
    }
}

/// Forget every socket wait registered by `pid` — called when a process dies
/// so a stale entry can't wake a pid that has been recycled.
pub fn cancel_waiters_for(pid: usize) {
    WAITERS.with(|w| w.retain(|x| x.pid != pid));
}

// ────────────────────────────────────────────────────────────────────────
// Readiness, for poll/epoll
// ────────────────────────────────────────────────────────────────────────

pub fn poll_mask(id: SocketId) -> Option<PollMask> {
    SOCKETS.with(|t| t.poll(id).ok())
}

// ────────────────────────────────────────────────────────────────────────
// Address helpers shared by bind/connect/sendto
// ────────────────────────────────────────────────────────────────────────

/// Resolve the filesystem side of a pathname address.
///
/// A pathname socket has two halves: a node in the filesystem (so `ls`,
/// `stat` and `unlink` see it) and an entry in `SOCKETS`'s bind registry
/// (which is what `connect` actually resolves). This checks the first half
/// the way Linux does — `connect()` to a path with no node is `ENOENT`, not
/// `ECONNREFUSED`, and that distinction is how a client tells "the server
/// was never started" from "the server is gone".
pub fn path_node_exists(path: &str) -> bool {
    crate::fs::vfs::resolve(path).is_ok()
}

/// Create the filesystem node for a pathname `bind()`.
pub fn create_path_node(path: &str) -> Result<(), i64> {
    use vfs::types::Errno;

    match crate::fs::vfs::mksocket(path) {
        Ok(()) => Ok(()),
        // An existing name is EADDRINUSE from bind()'s point of view, even
        // though the filesystem calls it EEXIST.
        Err(Errno::EEXIST) => Err(-(SockError::AddrInUse.errno() as i64)),
        Err(e) => Err(e.as_i64()),
    }
}

/// Normalize a user-supplied address against the caller's cwd, so a
/// relative `bind("sock")` lands where the process actually is.
pub fn normalize(addr: UnixAddr, cwd: &str) -> UnixAddr {
    match addr {
        UnixAddr::Path(p) => UnixAddr::Path(crate::fs::vfs::normalize_path(cwd, &p)),
        other => other,
    }
}
