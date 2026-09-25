// kernel/src/process/syscall/poll.rs
//
use alloc::collections::BTreeMap;
use spin::Mutex;
use crate::process::TrapFrame;
use super::{errno, SyscallResult, validate_user_buffer, current_tf_ptr};
use usock::SocketId;
use crate::ipc::unix;

/// Upper bound on pids tracked by the per-pid side tables below.
/// Must match `FileDescriptorTable`'s own `MAX_FILES`.
pub(super) const MAX_FILES_PER_PROC: usize = 16;

/// A process's fd → socket mapping, snapshotted at the moment it blocks.
///
/// Readiness for a blocked process has to be re-checked by whoever wakes it,
/// from *their* context — and another process's fd table is not reachable
/// without taking the scheduler lock, which the wakeup path (sometimes an
/// ISR) cannot do at that point. Snapshotting the mapping into the waiter
/// itself sidesteps that entirely.
///
/// This replaces the old global `FD_CHANNEL_MAP[pid][fd]`, which had to be
/// hand-maintained at every fd-allocating call site and silently did nothing
/// for pids past its bound. `FileHandle::socket_id()` is the source of truth
/// now; this is only a cache of it, valid for the duration of one block.
type SocketMap = [SocketId; MAX_FILES_PER_PROC];

const NO_SOCKETS: SocketMap = [0; MAX_FILES_PER_PROC];

/// Resolve every fd of the *running* process to a socket id (0 = not one).
fn snapshot_sockets() -> SocketMap {
    let mut map = NO_SOCKETS;
    let files = {
        let sched = crate::process::scheduler::local_scheduler();
        match sched.running_ref() {
            Some(proc) => proc.files.clone(),
            None => return map,
        }
    };
    let guard = files.lock();
    for (fd, slot) in map.iter_mut().enumerate() {
        if let Ok(h) = guard.get(fd) {
            *slot = h.socket_id().unwrap_or(0);
        }
    }
    map
}

// ============================================================================
// POLL / EPOLL SYSCALLS
// ============================================================================
//
// poll(7), epoll_create(213), epoll_ctl(233), epoll_wait(232)
//
// Architecture:
//   - `fd_check_ready(socks, fd, events)` checks FD readiness without consuming data.
//   - `POLL_WAITERS` (pid → waiter) stores a blocked process's buffer info for wakeup delivery.
//   - `EPOLL_INSTANCES` holds per-epoll-fd watch lists.
//   - `EPOLL_FD_MAP` (pid → [fd]) maps epoll FDs to EpollInstanceIds.
//   Both are keyed by pid with no bound. They were `[_; 32]` arrays that
//   silently skipped pid >= 32: a `poll()` with no timeout from such a pid
//   blocked without registering and was never woken, and `epoll_*` said
//   ESRCH.
//   - Wakeup hooks: `poll_wakeup_for_fd0` (keyboard ISR) and
//     `poll_wakeup_for_socket` (the socket layer).
//
// LOCKING ORDER (cli must be held):
//   POLL_WAITERS → EPOLL_INSTANCES → SOCKETS → (release) → SCHEDULER
//   SCHEDULER is always acquired last.

// ── Poll bitmasks (POSIX ABI) ──────────────────────────────────────────────

const POLLIN:   i16 = 0x0001;
const POLLOUT:  i16 = 0x0004;
const POLLERR:  i16 = 0x0008;
#[allow(dead_code)]
const POLLHUP:  i16 = 0x0010;
const POLLNVAL: i16 = 0x0020;

// ── Epoll bitmasks / ops (Linux ABI) ──────────────────────────────────────

const EPOLLIN:       u32 = 0x0000_0001;
const EPOLLOUT:      u32 = 0x0000_0004;
const EPOLLERR:      u32 = 0x0000_0008;
const EPOLLET:       u32 = 0x8000_0000;

const EPOLL_CTL_ADD: i32 = 1;
const EPOLL_CTL_DEL: i32 = 2;
const EPOLL_CTL_MOD: i32 = 3;

// ── Structures ──────────────────────────────────────────────────────────────

/// POSIX `struct pollfd` — 8 bytes.
#[repr(C)]
#[derive(Clone, Copy)]
struct PollFd {
    fd:      i32,
    events:  i16,
    revents: i16,
}

/// Linux `struct epoll_event` (packed, 12 bytes on x86_64).
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct EpollEvent {
    events: u32,
    data:   u64,
}

/// One watched FD inside an epoll instance.
#[derive(Clone, Copy)]
struct EpollWatch {
    fd:             i32,
    events:         u32,   // EPOLLIN | EPOLLOUT | …
    data:           u64,   // opaque user data returned in events
    edge_triggered: bool,
    #[allow(dead_code)]
    et_delivered:   bool,
}

/// A single epoll instance (the object behind an epoll FD).
#[derive(Clone, Copy)]
struct EpollInstance {
    watches:   [Option<EpollWatch>; 16],
    owner_pid: usize,
}

pub type EpollInstanceId = usize; // 0 = invalid

struct EpollInstanceTable {
    slots: [Option<EpollInstance>; 16],
}

impl EpollInstanceTable {
    const fn new() -> Self {
        Self { slots: [None; 16] }
    }

    fn alloc(&mut self, owner_pid: usize) -> Option<EpollInstanceId> {
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(EpollInstance { watches: [None; 16], owner_pid });
                return Some(i + 1); // 1-based IDs; 0 = invalid
            }
        }
        None
    }

    fn free(&mut self, id: EpollInstanceId) {
        if id >= 1 && id <= 16 {
            self.slots[id - 1] = None;
        }
    }

    fn get(&self, id: EpollInstanceId) -> Option<&EpollInstance> {
        if id >= 1 && id <= 16 { self.slots[id - 1].as_ref() } else { None }
    }

    fn get_mut(&mut self, id: EpollInstanceId) -> Option<&mut EpollInstance> {
        if id >= 1 && id <= 16 { self.slots[id - 1].as_mut() } else { None }
    }
}

static EPOLL_INSTANCES: Mutex<EpollInstanceTable> = Mutex::new(EpollInstanceTable::new());

/// pid×fd → EpollInstanceId side table (0 = not an epoll fd).
static EPOLL_FD_MAP: Mutex<BTreeMap<usize, [EpollInstanceId; MAX_FILES_PER_PROC]>> =
    Mutex::new(BTreeMap::new());

/// FileHandle marker stored in the FD table for epoll FDs.
struct EpollHandle {
    epoll_id: EpollInstanceId,
}

impl crate::process::file::FileHandle for EpollHandle {
    fn read(&mut self, _buf: &mut [u8]) -> crate::process::file::FileResult<usize> {
        Err(crate::process::file::FileError::NotSupported)
    }
    fn write(&mut self, _buf: &[u8]) -> crate::process::file::FileResult<usize> {
        Err(crate::process::file::FileError::NotSupported)
    }
    fn close(&mut self) -> crate::process::file::FileResult<()> {
        EPOLL_INSTANCES.lock().free(self.epoll_id);
        Ok(())
    }
    fn name(&self) -> &str { "epoll" }
}

// ── EPOLL_FD_MAP helpers ───────────────────────────────────────────────────

fn get_epoll_fd(pid: usize, fd: usize) -> EpollInstanceId {
    if fd < MAX_FILES_PER_PROC {
        EPOLL_FD_MAP.lock().get(&pid).map_or(0, |fds| fds[fd])
    } else {
        0
    }
}

fn set_epoll_fd(pid: usize, fd: usize, epoll_id: EpollInstanceId) {
    if fd < MAX_FILES_PER_PROC {
        let mut map = EPOLL_FD_MAP.lock();
        if epoll_id != 0 {
            map.entry(pid).or_insert([0; MAX_FILES_PER_PROC])[fd] = epoll_id;
        } else if let Some(fds) = map.get_mut(&pid) {
            fds[fd] = 0;
        }
    }
}

pub(super) fn clear_epoll_fd_all(pid: usize) {
    EPOLL_FD_MAP.lock().remove(&pid);
}

// ── Poll waiter ────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum PollWaiterKind {
    Poll      { nfds: u32 },
    EpollWait { epoll_id: EpollInstanceId, maxevents: usize },
}

/// Describes a process blocked in poll() or epoll_wait().
#[derive(Clone, Copy)]
struct PollWaiter {
    pid:      usize,
    /// Physical address of the user result buffer (pre-translated at block time).
    phys_buf: u64,
    #[allow(dead_code)]
    phys_len: usize,
    kind:     PollWaiterKind,
    /// hrtimer ID for timeout; None = wait forever.
    timer_id: Option<u32>,
    /// This process's fd → socket mapping at block time — see `SocketMap`.
    socks:    SocketMap,
}

/// One entry per PID — a process can only have one outstanding poll/epoll_wait.
static POLL_WAITERS: Mutex<BTreeMap<usize, PollWaiter>> = Mutex::new(BTreeMap::new());

// ── FD readiness ───────────────────────────────────────────────────────────

/// Check which requested events are currently ready for `fd`.
///
/// cli must be in effect when called (called from blocking paths where cli
/// is already set, and from ISR/wakeup context).
///
/// Rules:
///   - socket fd: whatever `usock` says about it — data queued or a
///     reportable EOF is POLLIN, room to send is POLLOUT, a dead peer is
///     POLLHUP. A listening socket with a pending connection is POLLIN,
///     which is what makes `poll()`-before-`accept()` work.
///   - stdin (fd=0): POLLIN if keyboard buffer has data.
///   - All other device fds: always ready for the requested events.
fn fd_check_ready(socks: &SocketMap, fd: i32, events: i16) -> i16 {
    if fd < 0 { return POLLNVAL; }
    let fd_usize = fd as usize;

    // Socket?
    if fd_usize < MAX_FILES_PER_PROC && socks[fd_usize] != 0 {
        let Some(mask) = unix::poll_mask(socks[fd_usize]) else { return POLLNVAL };
        let mut rev: i16 = 0;
        if events & POLLIN != 0 && mask.readable { rev |= POLLIN; }
        if events & POLLOUT != 0 && mask.writable { rev |= POLLOUT; }
        // POLLHUP and POLLERR are reported whether or not they were asked
        // for, exactly as poll(2) specifies.
        if mask.hup { rev |= POLLHUP; }
        if mask.err { rev |= POLLERR; }
        return rev;
    }

    // stdin
    if fd_usize == 0 {
        let mut rev: i16 = 0;
        if events & POLLIN != 0 && crate::keyboard::read_key_peek() {
            rev |= POLLIN;
        }
        return rev;
    }

    // All other device FDs (always ready)
    events & (POLLIN | POLLOUT)
}

// ── deliver_poll_result_phys ───────────────────────────────────────────────

/// Write poll/epoll results into the pre-translated physical buffer.
///
/// For Poll: updates revents fields in the PollFd array at phys_buf.
/// For EpollWait: writes ready EpollEvent structs starting at phys_buf.
/// Returns the number of ready fds/events.
///
/// Called with cli held, after POLL_WAITERS has been released.
fn deliver_poll_result_phys(waiter: &PollWaiter, phys_offset: u64) -> usize {
    let socks = &waiter.socks;
    match waiter.kind {
        PollWaiterKind::Poll { nfds } => {
            // phys_buf → array of PollFd structs (8 bytes each)
            let base = (phys_offset + waiter.phys_buf) as *mut PollFd;
            let mut ready = 0usize;
            for i in 0..nfds as usize {
                let pfd = unsafe { *base.add(i) };
                let rev = fd_check_ready(socks, pfd.fd, pfd.events);
                unsafe { (*base.add(i)).revents = rev; }
                if rev != 0 { ready += 1; }
            }
            ready
        }
        PollWaiterKind::EpollWait { epoll_id, maxevents } => {
            // phys_buf → array of EpollEvent structs (12 bytes each, packed)
            let base = phys_offset + waiter.phys_buf;
            let instances = EPOLL_INSTANCES.lock();
            let inst = match instances.get(epoll_id) {
                Some(i) => i,
                None => return 0,
            };
            let mut written = 0usize;
            for watch_opt in inst.watches.iter() {
                if written >= maxevents { break; }
                if let Some(watch) = watch_opt {
                    let mut poll_ev: i16 = 0;
                    if watch.events & EPOLLIN  != 0 { poll_ev |= POLLIN; }
                    if watch.events & EPOLLOUT != 0 { poll_ev |= POLLOUT; }
                    let rev = fd_check_ready(socks, watch.fd, poll_ev);
                    let mut epoll_rev: u32 = 0;
                    if rev & POLLIN  != 0 { epoll_rev |= EPOLLIN; }
                    if rev & POLLOUT != 0 { epoll_rev |= EPOLLOUT; }
                    if rev & POLLERR != 0 { epoll_rev |= EPOLLERR; }
                    if epoll_rev != 0 {
                        let ev = EpollEvent { events: epoll_rev, data: watch.data };
                        let dst = (base + written as u64 * 12) as *mut EpollEvent;
                        unsafe { core::ptr::write_unaligned(dst, ev); }
                        written += 1;
                    }
                }
            }
            written
        }
    }
}

// ── Waiter-scan helpers ────────────────────────────────────────────────────

/// Check if a poll waiter is watching fd=0 (stdin) for POLLIN.
/// Called while POLL_WAITERS is held (poll_waiter is borrowed from it).
fn poll_waiter_watches_stdin(waiter: &PollWaiter, phys_offset: u64) -> bool {
    match waiter.kind {
        PollWaiterKind::Poll { nfds } => {
            let base = (phys_offset + waiter.phys_buf) as *const PollFd;
            for i in 0..nfds as usize {
                let pfd = unsafe { *base.add(i) };
                if pfd.fd == 0 && (pfd.events & POLLIN) != 0 {
                    return true;
                }
            }
            false
        }
        PollWaiterKind::EpollWait { epoll_id, .. } => {
            // POLL_WAITERS → EPOLL_INSTANCES is the allowed nesting
            let instances = EPOLL_INSTANCES.lock();
            if let Some(inst) = instances.get(epoll_id) {
                for watch in inst.watches.iter().flatten() {
                    if watch.fd == 0 && (watch.events & EPOLLIN) != 0 {
                        return true;
                    }
                }
            }
            false
        }
    }
}

/// Check if a poll waiter is watching `sock` for POLLIN.
/// Called while POLL_WAITERS is held.
fn poll_waiter_watches_socket(
    waiter: &PollWaiter,
    sock: SocketId,
    phys_offset: u64,
) -> bool {
    match waiter.kind {
        PollWaiterKind::Poll { nfds } => {
            let base = (phys_offset + waiter.phys_buf) as *const PollFd;
            for i in 0..nfds as usize {
                let pfd = unsafe { *base.add(i) };
                if pfd.fd >= 0 && (pfd.fd as usize) < MAX_FILES_PER_PROC {
                    if waiter.socks[pfd.fd as usize] == sock && (pfd.events & POLLIN) != 0 {
                        return true;
                    }
                }
            }
            false
        }
        PollWaiterKind::EpollWait { epoll_id, .. } => {
            // POLL_WAITERS → EPOLL_INSTANCES
            let instances = EPOLL_INSTANCES.lock();
            if let Some(inst) = instances.get(epoll_id) {
                for watch in inst.watches.iter().flatten() {
                    if watch.fd >= 0 && (watch.fd as usize) < MAX_FILES_PER_PROC {
                        if waiter.socks[watch.fd as usize] == sock
                            && (watch.events & EPOLLIN) != 0
                        {
                            return true;
                        }
                    }
                }
            }
            false
        }
    }
}

// ── Wakeup hooks ───────────────────────────────────────────────────────────

/// Called by the keyboard ISR (after stdin_wakeup) with IF=0.
///
/// Delivers POLLIN on fd=0 to any process blocked in poll/epoll_wait that
/// is watching stdin.
///
/// Unlike the serial ISR (which only calls this when `tty::feed_input` says
/// a byte was really queued), the PS/2 keyboard ISR calls this on *every*
/// raw scancode — including key-release codes and modifier presses, which
/// push nothing into `KEYBOARD_BUFFER` (see `keyboard::process_scancode`).
/// A real keypress is always followed by its release scancode shortly
/// after; if that release lands while a process is already blocked in a
/// *fresh* `poll()` call (e.g. waiting for the *next* keystroke), this must
/// not wake it with a spurious "0 fds ready" — that's indistinguishable
/// from a real timeout to the caller (confirmed root cause of BusyBox
/// ash's line editor exiting after ~2 keystrokes: `poll()` returning 0 is
/// read as EOF by `libbb/read_key.c`). So: only actually wake the process
/// once `deliver_poll_result_phys` finds something genuinely ready; put an
/// otherwise-untouched waiter back so a real future event or its own
/// timeout still wakes it normally.
pub(crate) fn poll_wakeup_for_fd0() {
    let phys_offset = crate::memory::physical_memory_offset().as_u64();

    // Take the waiter (if any) watching fd=0 for POLLIN.
    let waiter = {
        let mut waiters = POLL_WAITERS.lock();
        let found = waiters.iter()
            .find(|(_, w)| poll_waiter_watches_stdin(w, phys_offset))
            .map(|(&pid, _)| pid);
        found.and_then(|pid| waiters.remove(&pid))
    };

    let Some(waiter) = waiter else { return; };

    let count = deliver_poll_result_phys(&waiter, phys_offset);
    if count == 0 {
        POLL_WAITERS.lock().insert(waiter.pid, waiter);
        return;
    }

    // Cancel timeout timer (if any)
    if let Some(tid) = waiter.timer_id {
        crate::time::hrtimer::cancel(tid);
    }

    let mut sched = crate::process::scheduler::local_scheduler();
    sched.wake_with_retval(waiter.pid, count as u64);
    // sched guard dropped; caller (keyboard ISR) still holds IF=0
}

/// Called by the socket layer after a socket became readable (`SOCKETS`
/// already released — see `ipc/unix.rs`'s lock order).
///
/// Wakes any process blocked in poll/epoll_wait watching `sock` for POLLIN.
pub(crate) fn poll_wakeup_for_socket(sock: SocketId) {
    // Save/restore rather than cli+sti: this runs under `dispatch_wakes`,
    // which can be reached from a socket's `Drop` inside `sys_exit`, where
    // re-enabling interrupts would let a timer tick free the kernel stack
    // this code is standing on. See `ipc/unix.rs::dispatch_wakes`.
    x86_64::instructions::interrupts::without_interrupts(poll_wakeup_for_socket_inner_call(sock));
}

fn poll_wakeup_for_socket_inner_call(sock: SocketId) -> impl FnOnce() {
    move || poll_wakeup_for_socket_inner(sock)
}

fn poll_wakeup_for_socket_inner(sock: SocketId) {
    let phys_offset = crate::memory::physical_memory_offset().as_u64();

    let waiter = {
        let mut waiters = POLL_WAITERS.lock();
        let found = waiters.iter()
            .find(|(_, w)| poll_waiter_watches_socket(w, sock, phys_offset))
            .map(|(&pid, _)| pid);
        found.and_then(|pid| waiters.remove(&pid))
    };

    let Some(waiter) = waiter else { return; };

    if let Some(tid) = waiter.timer_id {
        crate::time::hrtimer::cancel(tid);
    }

    let count = deliver_poll_result_phys(&waiter, phys_offset);
    let mut sched = crate::process::scheduler::local_scheduler();
    sched.wake_with_retval(waiter.pid, count as u64);
}

/// Cancel a pending poll/epoll waiter for a process (called on exit).
pub(super) fn poll_cancel_waiter(pid: usize) {
    let waiter = POLL_WAITERS.lock().remove(&pid);
    if let Some(w) = waiter {
        if let Some(tid) = w.timer_id {
            crate::time::hrtimer::cancel(tid);
        }
    }
}

/// Clear the POLL_WAITERS slot after an hrtimer timeout woke the process.
///
/// Called from the timer ISR (timer_preempt) AFTER the scheduler lock is
/// released, satisfying the lock order: POLL_WAITERS → SCHEDULER.
/// The timer has already fired so there is nothing to cancel.
pub(crate) fn poll_clear_on_timeout(pid: usize) {
    POLL_WAITERS.lock().remove(&pid);
}

// ── Helper: translate user VA → phys + page-boundary check ────────────────

/// Translate a user virtual address to a physical address and verify the
/// buffer fits within a single 4K page (required for our single-page pre-translation).
///
/// cli must be held.  Returns None on error (EFAULT).
fn translate_user_buf_phys(user_va: u64, size: usize) -> Option<u64> {
    use x86_64::{VirtAddr, structures::paging::{Page, Size4KiB}};
    let page   = Page::<Size4KiB>::containing_address(VirtAddr::new(user_va));
    let offset = user_va & 0xFFF;
    // Reject buffers that straddle a page boundary
    if offset + size as u64 > 0x1000 { return None; }
    let sched = crate::process::scheduler::local_scheduler();
    sched.running_ref()
        .and_then(|proc| unsafe { proc.address_space.translate_page(page) })
        .map(|frame| frame.start_address().as_u64() + offset)
}

// ── Helper: check epoll readiness and write directly to user VA ───────────

fn check_epoll_ready_uva(
    epoll_id: EpollInstanceId,
    socks: &SocketMap,
    events_ptr: u64,
    maxevents: usize,
) -> usize {
    let instances = EPOLL_INSTANCES.lock();
    let inst = match instances.get(epoll_id) {
        Some(i) => i,
        None    => return 0,
    };
    let mut written = 0usize;
    for watch_opt in inst.watches.iter() {
        if written >= maxevents { break; }
        if let Some(watch) = watch_opt {
            let mut poll_ev: i16 = 0;
            if watch.events & EPOLLIN  != 0 { poll_ev |= POLLIN; }
            if watch.events & EPOLLOUT != 0 { poll_ev |= POLLOUT; }
            let rev = fd_check_ready(socks, watch.fd, poll_ev);
            let mut epoll_rev: u32 = 0;
            if rev & POLLIN  != 0 { epoll_rev |= EPOLLIN; }
            if rev & POLLOUT != 0 { epoll_rev |= EPOLLOUT; }
            if rev & POLLERR != 0 { epoll_rev |= EPOLLERR; }
            if epoll_rev != 0 {
                let ev = EpollEvent { events: epoll_rev, data: watch.data };
                unsafe {
                    core::ptr::write_unaligned(
                        (events_ptr + written as u64 * 12) as *mut EpollEvent,
                        ev,
                    );
                }
                written += 1;
            }
        }
    }
    written
}

// ── sys_poll ───────────────────────────────────────────────────────────────

/// poll(7) — wait for events on a set of file descriptors.
///
/// `fds_ptr`   — user pointer to array of `struct pollfd`.
/// `nfds`      — number of entries (max 16).
/// `timeout_ms`— milliseconds to wait (-1 = forever, 0 = non-blocking).
pub(super) fn sys_poll(fds_ptr: u64, nfds: u32, timeout_ms: i32) -> SyscallResult {
    if nfds > 16 { return errno::EINVAL; }
    let buf_size = nfds as usize * 8; // sizeof(PollFd)
    if buf_size > 0 {
        if let Err(e) = validate_user_buffer(fds_ptr, buf_size) { return e; }
    }

    // Read PollFd array from user memory (user page table active)
    let mut fds = [PollFd { fd: -1, events: 0, revents: 0 }; 16];
    for i in 0..nfds as usize {
        fds[i] = unsafe { *((fds_ptr + i as u64 * 8) as *const PollFd) };
    }

    // `irq` is deliberately never dropped on the slow (blocking) path below
    // — it ends in `jump_to_user` (`-> !`), so interrupts intentionally
    // stay off across that jump; see `sys_read`'s WouldBlock arm.
    let irq = crate::process::irq_guard::InterruptGuard::new();

    let pid = crate::process::scheduler::current_pid().unwrap_or(0);
    let socks = snapshot_sockets();

    // Fast path: check all fds for immediate readiness
    let mut ready = 0i32;
    for i in 0..nfds as usize {
        let rev = fd_check_ready(&socks, fds[i].fd, fds[i].events);
        fds[i].revents = rev;
        if rev != 0 { ready += 1; }
    }

    if ready > 0 || timeout_ms == 0 {
        drop(irq);
        // Write revents back to user memory
        for i in 0..nfds as usize {
            unsafe { *((fds_ptr + i as u64 * 8) as *mut PollFd) = fds[i]; }
        }
        return ready as SyscallResult;
    }

    // ── Slow path: block ──────────────────────────────────────────────────
    let tf_ptr = current_tf_ptr();

    // Pre-translate user buffer to physical address
    let phys_buf = match translate_user_buf_phys(fds_ptr, buf_size) {
        Some(pa) => pa,
        None => return errno::EFAULT,
    };

    // Pre-set rax=0 (timeout return value)
    unsafe { (*(tf_ptr as *mut TrapFrame)).rax = 0; }

    // Register hrtimer if timeout_ms > 0
    let timer_id = if timeout_ms > 0 {
        let expiry = crate::time::ktime_get() + timeout_ms as u64 * 1_000_000;
        Some(crate::time::hrtimer::start(
            expiry,
            crate::time::hrtimer::HrTimerAction::WakePid(pid),
        ))
    } else {
        None // timeout_ms < 0 → wait forever
    };

    // Store waiter
    POLL_WAITERS.lock().insert(pid, PollWaiter {
        pid,
        phys_buf,
        phys_len: buf_size,
        kind: PollWaiterKind::Poll { nfds },
        timer_id,
        socks,
    });

    let next_tf = {
        let mut sched = crate::process::scheduler::local_scheduler();
        sched.block_current(tf_ptr)
    };
    unsafe { crate::process::trapframe::jump_to_user(next_tf) }
}

// ── sys_epoll_create ───────────────────────────────────────────────────────

/// epoll_create(213) — create an epoll instance.
///
/// `size` is ignored (Linux ≥ 2.6.8 ignores it too, kept for ABI).
/// Returns a file descriptor referring to the new epoll instance.
pub(super) fn sys_epoll_create(_size: i32) -> SyscallResult {
    let epoll_id = {
        let pid = crate::process::scheduler::current_pid().unwrap_or(0);
        let mut instances = EPOLL_INSTANCES.lock();
        match instances.alloc(pid) {
            Some(id) => id,
            None => return errno::ENOMEM,
        }
    };

    let handle = alloc::boxed::Box::new(EpollHandle { epoll_id });

    let _irq = crate::process::irq_guard::InterruptGuard::new();
    let mut sched = crate::process::scheduler::local_scheduler();
    match sched.running_mut() {
        Some(proc) => {
            let pid = proc.pid.0;
            // See sys_socket's comment: the lock guard must not outlive
            // this `let`, since the arms below drop `sched`.
            let alloc_result = proc.files.lock().allocate(handle);
            match alloc_result {
                Ok(fd) => {
                    drop(sched);
                    set_epoll_fd(pid, fd, epoll_id);
                    fd as i64
                }
                Err(_) => {
                    drop(sched);
                    EPOLL_INSTANCES.lock().free(epoll_id);
                    errno::EINVAL
                }
            }
        }
        None => {
            drop(sched);
            EPOLL_INSTANCES.lock().free(epoll_id);
            errno::ESRCH
        }
    }
}

// ── sys_epoll_ctl ──────────────────────────────────────────────────────────

/// epoll_ctl(233) — modify an epoll instance's interest list.
pub(super) fn sys_epoll_ctl(epfd: i32, op: i32, fd: i32, event_ptr: u64) -> SyscallResult {
    let pid = crate::process::scheduler::current_pid().unwrap_or(0);
    if epfd < 0 || (epfd as usize) >= MAX_FILES_PER_PROC { return errno::EBADF; }

    let epoll_id = get_epoll_fd(pid, epfd as usize);
    if epoll_id == 0 { return errno::EBADF; }

    // Read EpollEvent from user memory (not needed for EPOLL_CTL_DEL)
    let event = if op != EPOLL_CTL_DEL {
        if let Err(e) = validate_user_buffer(event_ptr, 12) { return e; }
        Some(unsafe { core::ptr::read_unaligned(event_ptr as *const EpollEvent) })
    } else {
        None
    };

    let mut instances = EPOLL_INSTANCES.lock();
    let inst = match instances.get_mut(epoll_id) {
        Some(i) => i,
        None    => return errno::EBADF,
    };

    match op {
        EPOLL_CTL_ADD => {
            match inst.watches.iter_mut().find(|s| s.is_none()) {
                Some(slot) => {
                    let ev = event.unwrap();
                    *slot = Some(EpollWatch {
                        fd,
                        events: ev.events,
                        data:   ev.data,
                        edge_triggered: (ev.events & EPOLLET) != 0,
                        et_delivered:   false,
                    });
                    0
                }
                None => errno::ENOMEM,
            }
        }
        EPOLL_CTL_DEL => {
            match inst.watches.iter_mut().find(|s| s.as_ref().map(|w| w.fd == fd).unwrap_or(false)) {
                Some(slot) => { *slot = None; 0 }
                None       => errno::ENOENT,
            }
        }
        EPOLL_CTL_MOD => {
            match inst.watches.iter_mut().find(|s| s.as_ref().map(|w| w.fd == fd).unwrap_or(false)) {
                Some(slot) => {
                    let ev = event.unwrap();
                    if let Some(w) = slot {
                        w.events         = ev.events;
                        w.data           = ev.data;
                        w.edge_triggered = (ev.events & EPOLLET) != 0;
                    }
                    0
                }
                None => errno::ENOENT,
            }
        }
        _ => errno::EINVAL,
    }
}

// ── sys_epoll_wait ─────────────────────────────────────────────────────────

/// epoll_wait(232) — wait for events on an epoll instance.
///
/// `epfd`       — epoll file descriptor.
/// `events_ptr` — user pointer to array of `struct epoll_event`.
/// `maxevents`  — max events to return (1..=16).
/// `timeout_ms` — -1 = forever, 0 = non-blocking, >0 = ms.
pub(super) fn sys_epoll_wait(epfd: i32, events_ptr: u64, maxevents: i32, timeout_ms: i32) -> SyscallResult {
    if maxevents <= 0 || maxevents > 16 { return errno::EINVAL; }
    let buf_size = maxevents as usize * 12; // sizeof(EpollEvent)
    if let Err(e) = validate_user_buffer(events_ptr, buf_size) { return e; }

    let pid = crate::process::scheduler::current_pid().unwrap_or(0);
    if epfd < 0 || (epfd as usize) >= MAX_FILES_PER_PROC { return errno::EBADF; }

    let epoll_id = get_epoll_fd(pid, epfd as usize);
    if epoll_id == 0 { return errno::EBADF; }

    // `irq` is deliberately never dropped on the slow (blocking) path below
    // — it ends in `jump_to_user` (`-> !`), so interrupts intentionally
    // stay off across that jump; see `sys_read`'s WouldBlock arm.
    let irq = crate::process::irq_guard::InterruptGuard::new();

    // Fast path: check readiness now
    let socks = snapshot_sockets();
    let ready = check_epoll_ready_uva(epoll_id, &socks, events_ptr, maxevents as usize);

    if ready > 0 || timeout_ms == 0 {
        drop(irq);
        return ready as SyscallResult;
    }

    // ── Slow path: block ──────────────────────────────────────────────────
    let tf_ptr = current_tf_ptr();

    let phys_buf = match translate_user_buf_phys(events_ptr, buf_size) {
        Some(pa) => pa,
        None => return errno::EFAULT,
    };

    // Pre-set rax=0 (timeout)
    unsafe { (*(tf_ptr as *mut TrapFrame)).rax = 0; }

    let timer_id = if timeout_ms > 0 {
        let expiry = crate::time::ktime_get() + timeout_ms as u64 * 1_000_000;
        Some(crate::time::hrtimer::start(
            expiry,
            crate::time::hrtimer::HrTimerAction::WakePid(pid),
        ))
    } else {
        None
    };

    POLL_WAITERS.lock().insert(pid, PollWaiter {
        pid,
        phys_buf,
        phys_len: buf_size,
        kind: PollWaiterKind::EpollWait { epoll_id, maxevents: maxevents as usize },
        timer_id,
        socks,
    });

    let next_tf = {
        let mut sched = crate::process::scheduler::local_scheduler();
        sched.block_current(tf_ptr)
    };
    unsafe { crate::process::trapframe::jump_to_user(next_tf) }
}

