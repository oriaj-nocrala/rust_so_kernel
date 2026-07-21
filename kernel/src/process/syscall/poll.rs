// kernel/src/process/syscall/poll.rs
//
use spin::Mutex;
use core::sync::atomic::Ordering;
use crate::process::TrapFrame;
use super::{errno, SyscallResult, validate_user_buffer, CURRENT_SYSCALL_TF};
use crate::ipc::channel::{ChannelId, CHANNELS};
use super::ipc::{MAX_PROCS, MAX_FILES_PER_PROC, FD_CHANNEL_MAP};

// ============================================================================
// POLL / EPOLL SYSCALLS
// ============================================================================
//
// poll(7), epoll_create(213), epoll_ctl(233), epoll_wait(232)
//
// Architecture:
//   - `fd_check_ready(pid, fd, events)` checks FD readiness without consuming data.
//   - `POLL_WAITERS[pid]` stores a blocked process's buffer info for wakeup delivery.
//   - `EPOLL_INSTANCES` holds per-epoll-fd watch lists.
//   - `EPOLL_FD_MAP[pid][fd]` maps epoll FDs to EpollInstanceIds (same pattern as FD_CHANNEL_MAP).
//   - Wakeup hooks: `poll_wakeup_for_fd0` (keyboard ISR) and
//     `poll_wakeup_for_channel` (sys_sendmsg).
//
// LOCKING ORDER (cli must be held):
//   POLL_WAITERS → EPOLL_INSTANCES → FD_CHANNEL_MAP → CHANNELS → (release) → SCHEDULER
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
static EPOLL_FD_MAP: Mutex<[[EpollInstanceId; MAX_FILES_PER_PROC]; MAX_PROCS]> =
    Mutex::new([[0; MAX_FILES_PER_PROC]; MAX_PROCS]);

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
    if pid < MAX_PROCS && fd < MAX_FILES_PER_PROC {
        EPOLL_FD_MAP.lock()[pid][fd]
    } else {
        0
    }
}

fn set_epoll_fd(pid: usize, fd: usize, epoll_id: EpollInstanceId) {
    if pid < MAX_PROCS && fd < MAX_FILES_PER_PROC {
        EPOLL_FD_MAP.lock()[pid][fd] = epoll_id;
    }
}

pub(super) fn clear_epoll_fd_all(pid: usize) {
    if pid < MAX_PROCS {
        let mut map = EPOLL_FD_MAP.lock();
        map[pid] = [0; MAX_FILES_PER_PROC];
    }
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
}

/// One slot per PID — a process can only have one outstanding poll/epoll_wait.
static POLL_WAITERS: Mutex<[Option<PollWaiter>; MAX_PROCS]> =
    Mutex::new([None; MAX_PROCS]);

// ── FD readiness ───────────────────────────────────────────────────────────

/// Check which requested events are currently ready for `fd`.
///
/// cli must be in effect when called (called from blocking paths where cli
/// is already set, and from ISR/wakeup context).
///
/// Rules:
///   - IPC channel fd: POLLIN if rx has messages; POLLOUT if peer's rx is not full.
///   - stdin (fd=0): POLLIN if keyboard buffer has data.
///   - All other device fds: always ready for the requested events.
fn fd_check_ready(pid: usize, fd: i32, events: i16) -> i16 {
    if fd < 0 { return POLLNVAL; }
    let fd_usize = fd as usize;

    // IPC channel?
    if fd_usize < MAX_FILES_PER_PROC && pid < MAX_PROCS {
        let channel_id = FD_CHANNEL_MAP.lock()[pid][fd_usize];
        if channel_id != 0 {
            let tbl = CHANNELS.lock();
            let mut rev: i16 = 0;
            if events & POLLIN != 0 {
                if tbl.get(channel_id).map(|ch| ch.has_messages()).unwrap_or(false) {
                    rev |= POLLIN;
                }
            }
            if events & POLLOUT != 0 {
                // POLLOUT ready if peer's rx buffer is not full
                let peer_not_full = tbl.get(channel_id)
                    .and_then(|ch| ch.peer)
                    .and_then(|peer_id| tbl.get(peer_id))
                    .map(|peer| !peer.is_rx_full())
                    .unwrap_or(false);
                if peer_not_full { rev |= POLLOUT; }
            }
            return rev;
        }
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
    let pid = waiter.pid;
    match waiter.kind {
        PollWaiterKind::Poll { nfds } => {
            // phys_buf → array of PollFd structs (8 bytes each)
            let base = (phys_offset + waiter.phys_buf) as *mut PollFd;
            let mut ready = 0usize;
            for i in 0..nfds as usize {
                let pfd = unsafe { *base.add(i) };
                let rev = fd_check_ready(pid, pfd.fd, pfd.events);
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
                    let rev = fd_check_ready(pid, watch.fd, poll_ev);
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

/// Check if a poll waiter is watching a specific IPC channel for POLLIN.
/// Called while POLL_WAITERS is held.
fn poll_waiter_watches_channel(
    waiter: &PollWaiter,
    channel_id: ChannelId,
    phys_offset: u64,
) -> bool {
    let pid = waiter.pid;
    if pid >= MAX_PROCS { return false; }
    match waiter.kind {
        PollWaiterKind::Poll { nfds } => {
            let map = FD_CHANNEL_MAP.lock();
            let base = (phys_offset + waiter.phys_buf) as *const PollFd;
            for i in 0..nfds as usize {
                let pfd = unsafe { *base.add(i) };
                if pfd.fd >= 0 && (pfd.fd as usize) < MAX_FILES_PER_PROC {
                    if map[pid][pfd.fd as usize] == channel_id && (pfd.events & POLLIN) != 0 {
                        return true;
                    }
                }
            }
            false
        }
        PollWaiterKind::EpollWait { epoll_id, .. } => {
            // POLL_WAITERS → EPOLL_INSTANCES → FD_CHANNEL_MAP
            let instances = EPOLL_INSTANCES.lock();
            let map = FD_CHANNEL_MAP.lock();
            if let Some(inst) = instances.get(epoll_id) {
                for watch in inst.watches.iter().flatten() {
                    if watch.fd >= 0 && (watch.fd as usize) < MAX_FILES_PER_PROC {
                        if map[pid][watch.fd as usize] == channel_id
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
        let mut found = None;
        for (i, slot) in waiters.iter().enumerate() {
            if let Some(w) = slot {
                if poll_waiter_watches_stdin(w, phys_offset) {
                    found = Some(i);
                    break;
                }
            }
        }
        found.and_then(|i| waiters[i].take())
    };

    let Some(waiter) = waiter else { return; };

    let count = deliver_poll_result_phys(&waiter, phys_offset);
    if count == 0 {
        if waiter.pid < MAX_PROCS {
            POLL_WAITERS.lock()[waiter.pid] = Some(waiter);
        }
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

/// Called from sys_sendmsg after enqueuing a message (CHANNELS released).
///
/// Wakes any process blocked in poll/epoll_wait watching `channel_id` for POLLIN.
pub(crate) fn poll_wakeup_for_channel(channel_id: ChannelId) {
    let _irq = crate::process::irq_guard::InterruptGuard::new();

    let phys_offset = crate::memory::physical_memory_offset().as_u64();

    let waiter = {
        let mut waiters = POLL_WAITERS.lock();
        let mut found = None;
        for (i, slot) in waiters.iter().enumerate() {
            if let Some(w) = slot {
                if poll_waiter_watches_channel(w, channel_id, phys_offset) {
                    found = Some(i);
                    break;
                }
            }
        }
        found.and_then(|i| waiters[i].take())
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
    if pid >= MAX_PROCS { return; }
    let waiter = {
        let mut waiters = POLL_WAITERS.lock();
        waiters[pid].take()
    };
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
    if pid >= MAX_PROCS { return; }
    POLL_WAITERS.lock()[pid] = None;
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
    pid: usize,
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
            let rev = fd_check_ready(pid, watch.fd, poll_ev);
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

    // Fast path: check all fds for immediate readiness
    let mut ready = 0i32;
    for i in 0..nfds as usize {
        let rev = fd_check_ready(pid, fds[i].fd, fds[i].events);
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
    let tf_ptr = CURRENT_SYSCALL_TF.load(Ordering::Relaxed) as *const TrapFrame;

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
    if pid < MAX_PROCS {
        POLL_WAITERS.lock()[pid] = Some(PollWaiter {
            pid,
            phys_buf,
            phys_len: buf_size,
            kind: PollWaiterKind::Poll { nfds },
            timer_id,
        });
    }

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
    if pid >= MAX_PROCS { return errno::ESRCH; }
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
    if pid >= MAX_PROCS { return errno::ESRCH; }
    if epfd < 0 || (epfd as usize) >= MAX_FILES_PER_PROC { return errno::EBADF; }

    let epoll_id = get_epoll_fd(pid, epfd as usize);
    if epoll_id == 0 { return errno::EBADF; }

    // `irq` is deliberately never dropped on the slow (blocking) path below
    // — it ends in `jump_to_user` (`-> !`), so interrupts intentionally
    // stay off across that jump; see `sys_read`'s WouldBlock arm.
    let irq = crate::process::irq_guard::InterruptGuard::new();

    // Fast path: check readiness now
    let ready = check_epoll_ready_uva(epoll_id, pid, events_ptr, maxevents as usize);

    if ready > 0 || timeout_ms == 0 {
        drop(irq);
        return ready as SyscallResult;
    }

    // ── Slow path: block ──────────────────────────────────────────────────
    let tf_ptr = CURRENT_SYSCALL_TF.load(Ordering::Relaxed) as *const TrapFrame;

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

    if pid < MAX_PROCS {
        POLL_WAITERS.lock()[pid] = Some(PollWaiter {
            pid,
            phys_buf,
            phys_len: buf_size,
            kind: PollWaiterKind::EpollWait { epoll_id, maxevents: maxevents as usize },
            timer_id,
        });
    }

    let next_tf = {
        let mut sched = crate::process::scheduler::local_scheduler();
        sched.block_current(tf_ptr)
    };
    unsafe { crate::process::trapframe::jump_to_user(next_tf) }
}

