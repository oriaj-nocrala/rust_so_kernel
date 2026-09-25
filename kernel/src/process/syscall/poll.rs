// kernel/src/process/syscall/poll.rs
//
use alloc::collections::BTreeMap;
use crate::sync::Mutex;
use crate::process::TrapFrame;
use super::{errno, SyscallResult, validate_user_buffer, current_tf_ptr};
use usock::SocketId;
use crate::ipc::unix;

/// Upper bound on pids tracked by the per-pid side tables below.
/// Must match `FileDescriptorTable`'s own `MAX_FILES`.
pub(super) const MAX_FILES_PER_PROC: usize = 16;

/// What `poll` needs to know about one fd without its handle: what can make
/// it ready. Anything else is always ready (`/dev/null`, regular files).
#[derive(Clone, Copy, PartialEq, Eq)]
enum PollSource {
    Other,
    Socket(SocketId),
    /// An evdev device: which queue feeds it (`drivers::evdev::QUEUE_*`),
    /// and whether the handle held records of its own at snapshot time.
    Input { queue: usize, buffered: bool },
    /// One end of a pseudo-terminal (`ipc::pty`).
    Pty { index: usize, master: bool },
}

/// A process's fd → `PollSource` mapping, snapshotted at the moment it blocks.
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
/// Input devices joined it for the same reason (`FileHandle::event_source`,
/// phase 2.2 of `docs/gui/gui-plan.md`): before, every device fd but stdin
/// was "always ready", and a compositor polling the mouse spun at 100 %.
///
/// A handle's `buffered` records cannot change while its process sleeps in
/// `poll` (only a read takes them), so a snapshot saying "buffered" makes
/// the fast path return and the process never blocks on a stale answer.
type SocketMap = [PollSource; MAX_FILES_PER_PROC];

const NO_SOCKETS: SocketMap = [PollSource::Other; MAX_FILES_PER_PROC];

/// Resolve every fd of the *running* process to what can make it ready.
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
            *slot = if let Some(id) = h.socket_id() {
                PollSource::Socket(id)
            } else if let Some(src) = h.event_source() {
                PollSource::Input { queue: src.queue, buffered: src.buffered }
            } else if let Some(end) = h.pty_end() {
                PollSource::Pty { index: end.index, master: end.master }
            } else {
                PollSource::Other
            };
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
//   - Wakeup hooks: `poll_wakeup_for_fd0` (keyboard ISR),
//     `poll_wakeup_for_input` (evdev producers) and
//     `poll_wakeup_for_socket` (the socket layer), all `poll_wake_where`.
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
const EPOLLHUP:      u32 = 0x0000_0010;
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

static EPOLL_INSTANCES: crate::sync::IrqLock<EpollInstanceTable> = crate::sync::IrqLock::new(EpollInstanceTable::new());

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
#[derive(Clone)]
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
    /// The wait's cell (`process::wait`), set by `block_poll_waiter`:
    /// claimed by whichever of an event and the timeout comes first,
    /// cancelled by a signal.
    cell:     Option<alloc::sync::Arc<crate::process::wait::WaitCell>>,
}

impl PollWaiter {
    fn claim(&self) -> bool {
        self.cell.as_ref().is_some_and(|c| c.claim())
    }
}

/// One entry per PID — a process can only have one outstanding poll/epoll_wait.
static POLL_WAITERS: crate::sync::IrqLock<BTreeMap<usize, PollWaiter>> = crate::sync::IrqLock::new(BTreeMap::new());

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
///   - evdev device: POLLIN if its handle holds records or its queue is
///     non-empty; POLLOUT always (writes are accepted, as Linux's
///     `evdev_poll` answers).
///   - stdin (fd=0): POLLIN if keyboard buffer has data.
///   - All other device fds: always ready for the requested events.
fn fd_check_ready(socks: &SocketMap, fd: i32, events: i16) -> i16 {
    if fd < 0 { return POLLNVAL; }
    let fd_usize = fd as usize;
    let source = socks.get(fd_usize).copied().unwrap_or(PollSource::Other);

    if let PollSource::Input { queue, buffered } = source {
        let ready = buffered || crate::drivers::evdev::queue_ready(queue);
        let rev = events & POLLOUT;
        return if events & POLLIN != 0 && ready { rev | POLLIN } else { rev };
    }

    if let PollSource::Pty { index, master } = source {
        let Some(mask) = crate::ipc::pty::poll_mask(index, master) else { return POLLNVAL };
        let mut rev: i16 = 0;
        if events & POLLIN != 0 && mask.readable { rev |= POLLIN; }
        if events & POLLOUT != 0 && mask.writable { rev |= POLLOUT; }
        if mask.hup { rev |= POLLHUP; }
        return rev;
    }

    // Socket?
    if let PollSource::Socket(sock) = source {
        let Some(mask) = unix::poll_mask(sock) else { return POLLNVAL };
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
/// Called with cli held, after POLL_WAITERS has been released. With
/// `write` false it only counts: a waker that has not claimed the wait's
/// cell yet must not write into memory the process may have moved on
/// from (`process::wait`).
fn deliver_poll_result_phys(waiter: &PollWaiter, phys_offset: u64, write: bool) -> usize {
    let socks = &waiter.socks;
    match waiter.kind {
        PollWaiterKind::Poll { nfds } => {
            // phys_buf → array of PollFd structs (8 bytes each)
            let base = (phys_offset + waiter.phys_buf) as *mut PollFd;
            let mut ready = 0usize;
            for i in 0..nfds as usize {
                let pfd = unsafe { *base.add(i) };
                let rev = fd_check_ready(socks, pfd.fd, pfd.events);
                if write {
                    unsafe { (*base.add(i)).revents = rev; }
                }
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
                    let epoll_rev = epoll_revents(rev);
                    if epoll_rev != 0 {
                        let ev = EpollEvent { events: epoll_rev, data: watch.data };
                        let dst = (base + written as u64 * 12) as *mut EpollEvent;
                        if write {
                            unsafe { core::ptr::write_unaligned(dst, ev); }
                        }
                        written += 1;
                    }
                }
            }
            written
        }
    }
}

/// `poll` revents → epoll events. `POLLHUP` is reported whether or not it
/// was asked for, as `POLLERR` is and as Linux does: a pty master whose
/// last slave closed is *only* `POLLHUP` (it has no data), and dropping it
/// here left an `epoll_wait` on the master asleep for good — the windowed
/// terminal never saw its shell exit.
fn epoll_revents(rev: i16) -> u32 {
    let mut e = 0;
    if rev & POLLIN  != 0 { e |= EPOLLIN; }
    if rev & POLLOUT != 0 { e |= EPOLLOUT; }
    if rev & POLLERR != 0 { e |= EPOLLERR; }
    if rev & POLLHUP != 0 { e |= EPOLLHUP; }
    e
}

// ── Waiter-scan helpers ────────────────────────────────────────────────────

/// Whether `waiter` asked for POLLIN on some fd `wanted` accepts.
/// Called while POLL_WAITERS is held (the waiter is borrowed from it);
/// POLL_WAITERS → EPOLL_INSTANCES is the allowed nesting.
fn poll_waiter_watches(
    waiter: &PollWaiter,
    phys_offset: u64,
    wanted: &impl Fn(&PollWaiter, i32) -> bool,
) -> bool {
    match waiter.kind {
        PollWaiterKind::Poll { nfds } => {
            let base = (phys_offset + waiter.phys_buf) as *const PollFd;
            (0..nfds as usize).any(|i| {
                let pfd = unsafe { *base.add(i) };
                pfd.events & POLLIN != 0 && wanted(waiter, pfd.fd)
            })
        }
        PollWaiterKind::EpollWait { epoll_id, .. } => {
            let instances = EPOLL_INSTANCES.lock();
            instances.get(epoll_id).is_some_and(|inst| {
                inst.watches.iter().flatten()
                    .any(|w| w.events & EPOLLIN != 0 && wanted(waiter, w.fd))
            })
        }
    }
}

/// The fd's source in the waiter's snapshot (`Other` for an fd out of range).
fn waiter_source(waiter: &PollWaiter, fd: i32) -> PollSource {
    usize::try_from(fd).ok()
        .and_then(|fd| waiter.socks.get(fd).copied())
        .unwrap_or(PollSource::Other)
}

// ── Wakeup hooks ───────────────────────────────────────────────────────────

/// How many waiters one wakeup can serve. More processes than this polling
/// the same source at once leaves the rest for the next event — the
/// scan used to stop at the first one, always.
const MAX_WAKE_PER_EVENT: usize = 8;

/// Wake every process blocked in poll/epoll_wait on an fd `wanted` accepts
/// that now has something ready. Called with IF=0 (an ISR, or under
/// `without_interrupts`); takes POLL_WAITERS, releases it, then takes the
/// scheduler lock per wakeup — the documented order.
///
/// A waiter whose fds turn out *not* ready goes back untouched, so a real
/// future event or its own timeout still wakes it. That matters: the PS/2
/// keyboard ISR calls this on *every* raw scancode — key releases and
/// modifier presses included, which push nothing into `KEYBOARD_BUFFER`
/// (see `keyboard::process_scancode`) — and a process woken with a
/// spurious "0 fds ready" reads it as a timeout (the confirmed cause of
/// BusyBox ash's line editor exiting after ~2 keystrokes: `poll()`
/// returning 0 is read as EOF by `libbb/read_key.c`).
///
/// The same pid may meanwhile have been woken by its timeout and blocked in
/// a *new* poll with a new waiter; putting the old one back must not
/// replace that, hence `or_insert`.
fn poll_wake_where(wanted: impl Fn(&PollWaiter, i32) -> bool) {
    let phys_offset = crate::memory::physical_memory_offset().as_u64();

    let mut taken: [Option<PollWaiter>; MAX_WAKE_PER_EVENT] = Default::default();
    {
        let mut waiters = POLL_WAITERS.lock();
        let mut pids = [0usize; MAX_WAKE_PER_EVENT];
        let mut n = 0;
        for (&pid, w) in waiters.iter() {
            if n == MAX_WAKE_PER_EVENT { break; }
            if poll_waiter_watches(w, phys_offset, &wanted) {
                pids[n] = pid;
                n += 1;
            }
        }
        for (slot, pid) in taken.iter_mut().zip(&pids[..n]) {
            *slot = waiters.remove(pid);
        }
    }

    for waiter in taken.into_iter().flatten() {
        // Count first, claim second, write third: nothing may be written
        // into a waiter's memory before its wait is won (a signal may have
        // ended it), and a 0 count must leave the wait armed.
        if deliver_poll_result_phys(&waiter, phys_offset, false) == 0 {
            if waiter.cell.as_ref().is_some_and(|c| c.is_armed()) {
                POLL_WAITERS.lock().entry(waiter.pid).or_insert(waiter);
            }
            continue;
        }
        if !waiter.claim() {
            continue; // stale: a signal (or the timeout) ended that wait
        }
        if let Some(tid) = waiter.timer_id {
            crate::time::hrtimer::cancel(tid);
        }
        let count = deliver_poll_result_phys(&waiter, phys_offset, true);
        let mut sched = crate::process::scheduler::local_scheduler();
        sched.wake_with_retval(waiter.pid, count as u64);
    }
}

/// Called by the keyboard ISR (after stdin_wakeup) with IF=0: POLLIN on
/// fd=0 for any process polling stdin.
pub(crate) fn poll_wakeup_for_fd0() {
    poll_wake_where(|_, fd| fd == 0);
}

/// Called by an input producer after pushing to evdev queue `queue`
/// (`drivers::evdev::QUEUE_*`): the keyboard ISR and the USB poll for the
/// keyboard, the IRQ12 ISR and the USB poll for the mouse. IF=0.
pub(crate) fn poll_wakeup_for_input(queue: usize) {
    poll_wake_where(|w, fd| {
        matches!(waiter_source(w, fd), PollSource::Input { queue: q, .. } if q == queue)
    });
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
    x86_64::instructions::interrupts::without_interrupts(|| {
        poll_wake_where(|w, fd| waiter_source(w, fd) == PollSource::Socket(sock))
    });
}

/// Called by `ipc::pty` after either end of pair `index` changed (with
/// `PTYS` released, IF=0): wakes poll/epoll sleepers watching it.
pub(crate) fn poll_wakeup_for_pty(index: usize) {
    poll_wake_where(|w, fd| matches!(waiter_source(w, fd), PollSource::Pty { index: i, .. } if i == index));
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

/// Clear the POLL_WAITERS slot of a poll/epoll_wait whose timeout `timer`
/// just fired — only if it is still that wait's: an event may have woken
/// the process already, and on another CPU it may already be blocked in a
/// new poll with a new waiter under the same pid.
///
/// Called from the timer ISR (timer_preempt) *before* the process is woken,
/// with the scheduler lock not held: lock order POLL_WAITERS → SCHEDULER.
/// The timer has already fired so there is nothing to cancel.
pub(crate) fn poll_clear_on_timeout(pid: usize, timer: u32) {
    let mut waiters = POLL_WAITERS.lock();
    if waiters.get(&pid).map_or(false, |w| w.timer_id == Some(timer)) {
        waiters.remove(&pid);
    }
}

// ── Helper: translate user VA → phys + page-boundary check ────────────────

/// Translate a user virtual address to a physical address and verify the
/// buffer fits within a single 4K page (required for our single-page pre-translation).
///
/// The waker writes the result through that physical address later, from
/// another context, so the page is first made privately writable
/// (`AddressSpace::prepare_user_write`): translating a COW-shared page or
/// the zero frame would hand the waker a frame that other address spaces
/// read — the result would appear in the fork parent's copy, or in every
/// untouched anonymous page in the system. What this cannot cover is the
/// page being shared again *while* the caller sleeps, by a sibling
/// thread's `fork()`; that needs the waker to write through the address
/// space instead of a saved physical address.
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
        .and_then(|proc| unsafe {
            if !proc.address_space.prepare_user_write(user_va, size as u64) {
                return None;
            }
            proc.address_space.translate_page(page)
        })
        .map(|frame| frame.start_address().as_u64() + offset)
}

// ── Helper: check epoll readiness and write directly to user VA ───────────

fn check_epoll_ready_uva(
    epoll_id: EpollInstanceId,
    socks: &SocketMap,
    events_ptr: u64,
    maxevents: usize,
) -> usize {
    epoll_ready(epoll_id, socks, Some(events_ptr), maxevents)
}

/// How many of `epoll_id`'s watches are ready (at most `maxevents`),
/// writing each as a `struct epoll_event` to `events_ptr` (a user VA of the
/// running process) when given — `None` only counts, which is safe under
/// the scheduler lock, where touching user memory is not.
fn epoll_ready(
    epoll_id: EpollInstanceId,
    socks: &SocketMap,
    events_ptr: Option<u64>,
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
            let epoll_rev = epoll_revents(rev);
            if epoll_rev != 0 {
                if let Some(events_ptr) = events_ptr {
                    let ev = EpollEvent { events: epoll_rev, data: watch.data };
                    unsafe {
                        core::ptr::write_unaligned(
                            (events_ptr + written as u64 * 12) as *mut EpollEvent,
                            ev,
                        );
                    }
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

    let ready_now = |socks: &SocketMap| {
        (0..nfds as usize).any(|i| fd_check_ready(socks, fds[i].fd, fds[i].events) != 0)
    };
    let waiter = PollWaiter {
        pid,
        phys_buf,
        phys_len: buf_size,
        kind: PollWaiterKind::Poll { nfds },
        timer_id: None,
        socks,
        cell: None,
    };
    block_poll_waiter(tf_ptr, waiter, timeout_ms, ready_now)
}

/// The blocking half shared by `poll` and `epoll_wait`: register `waiter`
/// (and its timeout), then block — or, if `ready_now` finds something ready
/// after all, restart the syscall instead.
///
/// Registering and blocking happen under the scheduler lock, as one step
/// (stage 7 of docs/smp/smp-plan.md). A waker takes `POLL_WAITERS` and only
/// then the scheduler lock, so once it can see this waiter the process is
/// already Blocked — it can no longer be running on another CPU, between
/// registering and blocking, where `wake_with_retval` would miss it. The
/// same goes for the timeout's hrtimer, which the tick also turns into a
/// wakeup under the scheduler lock.
///
/// The re-check closes the gap before that: an event on another CPU between
/// the caller's fast-path check and this registration found no waiter to
/// wake. Checked again once a waker would find us; if something is ready
/// the waiter is withdrawn and the syscall re-executed (`rip -= 2`, `rax`
/// still its number), which reports it through the fast path.
fn block_poll_waiter(
    tf_ptr: *const TrapFrame,
    mut waiter: PollWaiter,
    timeout_ms: i32,
    ready_now: impl FnOnce(&SocketMap) -> bool,
) -> SyscallResult {
    let pid = waiter.pid;
    let socks = waiter.socks;
    let next_tf = {
        let mut sched = crate::process::scheduler::local_scheduler();
        let cell = sched.begin_wait();

        // Register hrtimer if timeout_ms > 0 (< 0 waits forever).
        waiter.timer_id = if timeout_ms > 0 {
            let expiry = crate::time::ktime_get() + timeout_ms as u64 * 1_000_000;
            Some(crate::time::hrtimer::start(
                expiry,
                crate::time::hrtimer::HrTimerAction::Wake { pid, cell: cell.clone() },
            ))
        } else {
            None
        };
        waiter.cell = Some(cell);
        let timer_id = waiter.timer_id;
        POLL_WAITERS.lock().insert(pid, waiter);

        if ready_now(&socks) {
            POLL_WAITERS.lock().remove(&pid);
            if let Some(tid) = timer_id {
                crate::time::hrtimer::cancel(tid);
            }
            sched.abandon_wait();
            None
        } else {
            // A signal ends the wait with EINTR once a handler runs.
            let (nr, ret_rip) = unsafe { ((*tf_ptr).rax, (*tf_ptr).rip) };
            // Pre-set rax=0 (timeout return value)
            unsafe { (*(tf_ptr as *mut TrapFrame)).rax = 0; }
            let cleanup = match timer_id {
                Some(id) => crate::process::wait::Cleanup::Timer(id),
                None => crate::process::wait::Cleanup::None,
            };
            Some(sched.block_current(tf_ptr, crate::process::wait::Wait::cell(
                nr, ret_rip, crate::process::wait::RestartPolicy::NoHandlerOnly, cleanup,
            )))
        }
    };
    match next_tf {
        Some(tf) => unsafe { crate::process::trapframe::jump_to_user(tf) },
        None => unsafe {
            (*(tf_ptr as *mut TrapFrame)).rip -= 2;
            crate::process::trapframe::jump_to_user(tf_ptr)
        },
    }
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

    let waiter = PollWaiter {
        pid,
        phys_buf,
        phys_len: buf_size,
        kind: PollWaiterKind::EpollWait { epoll_id, maxevents: maxevents as usize },
        timer_id: None,
        socks,
        cell: None,
    };
    block_poll_waiter(tf_ptr, waiter, timeout_ms, |socks| {
        epoll_ready(epoll_id, socks, None, maxevents as usize) > 0
    })
}
