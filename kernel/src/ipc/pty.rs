// kernel/src/ipc/pty.rs
//
// Pseudo-terminals: `/dev/ptmx` and `/dev/pts/<n>` (phase 3.3 of
// docs/gui/gui-plan.md).
//
// WHAT LIVES WHERE
//
// Every rule is in the host-tested `tty` crate: the line discipline, a
// pair's semantics (hangup, EIO/POLLHUP, SIGWINCH) and job control
// (`tty::jobctl`). This file is the adapter, in the shape of
// `ipc/unix.rs`: the global table, the `FileHandle` for each end, blocking
// and waking, and turning `tty::pty::Effects` into signals.
//
// BLOCKING RESTARTS THE SYSCALL
//
// As for sockets, not as for pipes: a blocked reader or writer registers,
// rewinds its saved `rip` onto the `syscall` instruction and sleeps; the
// waker only makes it runnable, and the call runs again from the top. That
// matters more here than for sockets: a restarted slave read goes through
// job control again, so a job that was moved to the background while it
// slept stops with SIGTTIN instead of stealing the foreground's input. The
// WAKE_EPOCH counter closes the gap between failing and registering, as in
// `unix.rs` (see its comment).
//
// JOB CONTROL
//
// A background read, or a background write with TOSTOP, or a background
// TCSETS/TIOCSPGRP, sends SIGTTIN/SIGTTOU to the caller's group and
// restarts the call — so after SIGCONT it runs again and checks again, as
// Linux's -ERESTARTSYS does. A read or write does it by blocking on a wait
// the pending signal interrupts at once (`block_current` refuses to sleep
// with an actionable signal); an ioctl, which cannot block, rewinds `rip`
// and returns the syscall number, which is what `rax` must hold for the
// `syscall` instruction to run again.
//
// LOCKS
//
// `PTYS` and `WAITERS` are `diag::IrqMutex`es, as `SOCKETS` is. Order:
// SCHEDULER is never taken while either is held — facts about the caller
// are read before, effects are applied after. `dup` (called by `fork`
// under SCHEDULER) takes `PTYS` inside it, which is that same order.
//
// LIMITS
//
// 16 pairs. VTIME is not timed: a read that would wait for it returns what
// is there (possibly 0). When a session leader dies its terminal is not
// hung up (Linux sends its foreground group SIGHUP); the master closing
// is the hangup this kernel implements.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ::tty::jobctl::{self, Access, Verdict};
use ::tty::ldisc::Read;
use ::tty::pty::{Effects, Flush, PollMask, Pty, Target};
use ::tty::termios::{Termios, Winsize, TOSTOP};
use ::tty::{Signal, TtyError};
use diag::IrqMutex;

use crate::allocator::KernelIrq;
use crate::fs::types::{Errno, Stat};
use crate::process::file::{FileError, FileHandle, FileResult};
use crate::process::syscall::errno;

pub const MAX_PTYS: usize = 16;

/// Inode numbers of the slaves (`/dev/pts/<n>`) start here.
pub const PTS_INO_BASE: u64 = 200_000;

struct Slot {
    pty: Pty,
    /// Open master handles (`dup`/`fork` share the master).
    masters: u32,
}

static PTYS: IrqMutex<[Option<Slot>; MAX_PTYS], KernelIrq> =
    IrqMutex::new([const { None }; MAX_PTYS]);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Queue {
    MasterRead,
    MasterWrite,
    SlaveRead,
    SlaveWrite,
}

struct Waiter {
    pid: usize,
    index: usize,
    queue: Queue,
    cell: Arc<crate::process::wait::WaitCell>,
}

static WAITERS: IrqMutex<Vec<Waiter>, KernelIrq> = IrqMutex::new(Vec::new());

/// See `ipc/unix.rs`'s `WAKE_EPOCH`: bumped by every wakeup before it scans
/// `WAITERS`.
static WAKE_EPOCH: AtomicU64 = AtomicU64::new(0);

// ────────────────────────────────────────────────────────────────────────
// Facts about the caller
// ────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct Caller {
    pid: u32,
    pgid: u32,
    sid: u32,
    ctty: Option<usize>,
    ttin_ignored_or_blocked: bool,
    ttou_ignored_or_blocked: bool,
}

fn with_sched<R>(f: impl FnOnce(&mut crate::process::scheduler::Scheduler) -> R) -> R {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut s = crate::process::scheduler::local_scheduler();
        f(&mut s)
    })
}

fn caller() -> Option<Caller> {
    use crate::process::signal::{SignalAction, SIGTTIN, SIGTTOU};
    with_sched(|s| {
        let p = s.running_ref()?;
        let off = |sig: u32| {
            matches!(p.signal_handlers[sig as usize], SignalAction::Ignore)
                || p.blocked_signals & (1u64 << sig) != 0
        };
        Some(Caller {
            pid: p.pid.0 as u32,
            pgid: p.pgid,
            sid: p.sid,
            ctty: p.ctty,
            ttin_ignored_or_blocked: off(SIGTTIN),
            ttou_ignored_or_blocked: off(SIGTTOU),
        })
    })
}

fn signal_number(s: Signal) -> u32 {
    use crate::process::signal as sig;
    match s {
        Signal::Hup => sig::SIGHUP,
        Signal::Int => sig::SIGINT,
        Signal::Quit => sig::SIGQUIT,
        Signal::Tstp => sig::SIGTSTP,
        Signal::Ttin => sig::SIGTTIN,
        Signal::Ttou => sig::SIGTTOU,
        Signal::Cont => sig::SIGCONT,
        Signal::Winch => sig::SIGWINCH,
    }
}

fn errno_of(e: TtyError) -> i64 {
    match e {
        TtyError::Again => errno::EAGAIN,
        TtyError::Io => errno::EIO,
        TtyError::Perm => errno::EPERM,
        TtyError::NotTty => errno::ENOTTY,
    }
}

fn file_error_of(e: TtyError) -> FileError {
    match e {
        TtyError::Again => FileError::Again,
        TtyError::Io => FileError::IOError,
        TtyError::Perm | TtyError::NotTty => FileError::InvalidArgument,
    }
}

// ────────────────────────────────────────────────────────────────────────
// Effects: wakeups and signals, applied with PTYS released
// ────────────────────────────────────────────────────────────────────────

fn apply(index: usize, e: Effects) {
    let w = e.wakes;
    if w.any() {
        WAKE_EPOCH.fetch_add(1, Ordering::SeqCst);
        let mut pids: Vec<usize> = Vec::new();
        WAITERS.with(|list| {
            list.retain(|x| {
                let hit = x.index == index
                    && match x.queue {
                        Queue::MasterRead => w.master_readable,
                        Queue::MasterWrite => w.master_writable,
                        Queue::SlaveRead => w.slave_readable,
                        Queue::SlaveWrite => w.slave_writable,
                    };
                if hit && x.cell.claim() {
                    pids.push(x.pid);
                }
                !hit
            });
        });
        if !pids.is_empty() {
            with_sched(|s| {
                for pid in pids {
                    s.wake_or_defer(pid, crate::process::WakePending::Restart);
                }
            });
        }
        x86_64::instructions::interrupts::without_interrupts(|| {
            crate::process::syscall::poll_wakeup_for_pty(index);
        });
    }
    if !e.signals.is_empty() || e.detach_session.is_some() {
        with_sched(|s| {
            for (target, sig) in e.signals {
                let n = signal_number(sig);
                match target {
                    Target::Group(g) => s.signal_group(g, n),
                    Target::Process(p) => s.signal_pid(p as usize, n),
                }
            }
            if e.detach_session.is_some() {
                s.clear_ctty(index);
            }
        });
    }
}

/// Run `f` on pair `index` and apply what it reports. `None` if the pair
/// is gone (cannot happen while a handle holds it).
fn with_pty<R>(index: usize, f: impl FnOnce(&mut Pty) -> (R, Effects)) -> Option<R> {
    let out = PTYS.with(|t| t[index].as_mut().map(|slot| f(&mut slot.pty)));
    out.map(|(r, e)| {
        apply(index, e);
        r
    })
}

// ────────────────────────────────────────────────────────────────────────
// Blocking
// ────────────────────────────────────────────────────────────────────────

/// `unix.rs`'s `register_retry`, for a pty queue: register, rewind, arm.
/// `false` (nothing registered) if a wakeup ran since `epoch`: retry now.
/// Every caller that gets `true` must block (return `WouldBlock`).
fn register_retry(index: usize, queue: Queue, epoch: u64) -> bool {
    let tf = crate::process::syscall::current_tf_ptr() as *mut crate::process::TrapFrame;
    if tf.is_null() {
        return true;
    }
    let pid = crate::process::scheduler::current_pid().unwrap_or(0);
    let cell = Arc::new(crate::process::wait::WaitCell::new());
    WAITERS.with(|w| {
        w.retain(|x| !(x.pid == pid && x.index == index && x.queue == queue));
        w.push(Waiter { pid, index, queue, cell: cell.clone() });
    });
    if WAKE_EPOCH.load(Ordering::SeqCst) != epoch && cell.cancel() {
        WAITERS.with(|w| w.retain(|x| !(x.pid == pid && x.index == index && x.queue == queue)));
        return false;
    }
    crate::process::scheduler::arm_wait(cell);
    unsafe {
        (*tf).rip -= 2;
    }
    true
}

/// Job control said stop: send `sig` to the caller's group and make the
/// read/write it is in re-run after the signal (see the module comment).
/// Returns what the `FileHandle` method returns.
fn stop_and_restart<T>(c: &Caller, sig: Signal) -> FileResult<T> {
    let n = signal_number(sig);
    with_sched(|s| s.signal_group(c.pgid, n));
    let tf = crate::process::syscall::current_tf_ptr() as *mut crate::process::TrapFrame;
    if tf.is_null() {
        return Err(FileError::IOError);
    }
    crate::process::scheduler::arm_wait(Arc::new(crate::process::wait::WaitCell::new()));
    unsafe {
        (*tf).rip -= 2;
    }
    Err(FileError::WouldBlock)
}

/// The ioctl counterpart of `stop_and_restart`: an ioctl cannot block, so
/// it rewinds onto `syscall` and returns the syscall number, which the
/// return path puts in `rax` — the call runs again once the signal has
/// been dealt with.
fn stop_and_restart_ioctl(c: &Caller, sig: Signal) -> i64 {
    let n = signal_number(sig);
    with_sched(|s| s.signal_group(c.pgid, n));
    let tf = crate::process::syscall::current_tf_ptr() as *mut crate::process::TrapFrame;
    if tf.is_null() {
        return errno::EIO;
    }
    unsafe {
        let nr = (*tf).rax as i64;
        (*tf).rip -= 2;
        nr
    }
}

/// Job control for an operation on slave `index`: `None` to go ahead,
/// `Some(verdict)` otherwise. `check` is `check_read` or `check_change`.
fn job_verdict(c: &Caller, index: usize, read: bool) -> Option<Verdict> {
    let fg = PTYS.with(|t| t[index].as_ref().and_then(|s| s.pty.foreground()));
    let mut a = Access {
        is_ctty: c.ctty == Some(index),
        pgid: c.pgid,
        foreground: fg,
        signal_ignored_or_blocked: if read { c.ttin_ignored_or_blocked } else { c.ttou_ignored_or_blocked },
        orphaned: false,
    };
    let check = if read { jobctl::check_read } else { jobctl::check_change };
    match check(&a) {
        Verdict::Allow => None,
        Verdict::Stop(_) => {
            // Only now worth a scan of every process.
            a.orphaned = with_sched(|s| s.group_orphaned(c.pgid, c.sid));
            Some(check(&a))
        }
        v => Some(v),
    }
}

// ────────────────────────────────────────────────────────────────────────
// Opening
// ────────────────────────────────────────────────────────────────────────

/// `/dev/ptmx`: a new pair, its master.
pub fn open_master() -> Result<Box<dyn FileHandle>, Errno> {
    let index = PTYS.with(|t| {
        let i = t.iter().position(|s| s.is_none())?;
        t[i] = Some(Slot { pty: Pty::new(), masters: 1 });
        Some(i)
    });
    let index = index.ok_or(Errno::ENOSPC)?;
    crate::ktrace!(crate::debug::FS, "pty: new pair {}", index);
    Ok(Box::new(PtyHandle::new(index, true)))
}

/// `/dev/pts/<index>`. A session leader without a controlling terminal
/// that opens a free slave without `O_NOCTTY` acquires it, as in Linux.
pub fn open_slave(index: usize, noctty: bool) -> Result<Box<dyn FileHandle>, Errno> {
    if index >= MAX_PTYS {
        return Err(Errno::ENOENT);
    }
    let c = caller();
    let acquired = PTYS.with(|t| {
        let slot = t[index].as_mut().ok_or(Errno::ENOENT)?;
        slot.pty.open_slave().map_err(|_| Errno::EIO)?;
        let acquire = c.is_some_and(|c| {
            jobctl::acquires_on_open(c.pid, c.sid, c.ctty.is_some(), slot.pty.session(), noctty)
        });
        if acquire {
            let c = c.unwrap();
            slot.pty.set_session(Some(c.sid), c.pgid);
        }
        Ok::<bool, Errno>(acquire)
    })?;
    if acquired {
        with_sched(|s| {
            if let Some(p) = s.running_mut() {
                p.ctty = Some(index);
            }
        });
    }
    Ok(Box::new(PtyHandle::new(index, false)))
}

/// `/dev/tty`: the caller's controlling terminal, a new slave handle on it.
pub fn open_controlling() -> Result<Box<dyn FileHandle>, Errno> {
    let index = caller().and_then(|c| c.ctty).ok_or(Errno::ENXIO)?;
    PTYS.with(|t| {
        let slot = t[index].as_mut().ok_or(Errno::ENXIO)?;
        slot.pty.open_slave().map_err(|_| Errno::EIO)
    })?;
    Ok(Box::new(PtyHandle::new(index, false)))
}

/// Pairs whose slave can be looked up (`/dev/pts` lists them): the master
/// is still open, as Linux's devpts shows a pair until then.
pub fn live_indices() -> Vec<usize> {
    PTYS.with(|t| {
        t.iter()
            .enumerate()
            .filter(|(_, s)| s.as_ref().is_some_and(|s| s.pty.master_open()))
            .map(|(i, _)| i)
            .collect()
    })
}

pub fn exists(index: usize) -> bool {
    index < MAX_PTYS && live_indices().contains(&index)
}

/// For poll/epoll: what an end is ready for now.
pub fn poll_mask(index: usize, master: bool) -> Option<PollMask> {
    PTYS.with(|t| {
        t.get(index)?.as_ref().map(|s| if master { s.pty.poll_master() } else { s.pty.poll_slave() })
    })
}

/// Forget every wait registered by `pid` (it died).
pub fn cancel_waiters_for(pid: usize) {
    WAITERS.with(|w| w.retain(|x| x.pid != pid));
}

// ────────────────────────────────────────────────────────────────────────
// The handle
// ────────────────────────────────────────────────────────────────────────

pub struct PtyHandle {
    index: usize,
    master: bool,
    /// `O_NONBLOCK`, shared across `dup()` (see `UnixSocketHandle`).
    nonblock: Arc<AtomicBool>,
}

impl PtyHandle {
    fn new(index: usize, master: bool) -> Self {
        Self { index, master, nonblock: Arc::new(AtomicBool::new(false)) }
    }

    fn nonblocking_now(&self) -> bool {
        self.nonblock.load(Ordering::Relaxed)
    }

    fn read_master(&mut self, buf: &mut [u8]) -> FileResult<usize> {
        loop {
            let epoch = WAKE_EPOCH.load(Ordering::SeqCst);
            let r = with_pty(self.index, |p| p.master_read(buf)).ok_or(FileError::IOError)?;
            return match r {
                Ok(n) => Ok(n),
                Err(TtyError::Again) if self.nonblocking_now() => Err(FileError::Again),
                Err(TtyError::Again) => {
                    if !register_retry(self.index, Queue::MasterRead, epoch) {
                        continue;
                    }
                    Err(FileError::WouldBlock)
                }
                Err(e) => Err(file_error_of(e)),
            };
        }
    }

    fn write_master(&mut self, buf: &[u8]) -> FileResult<usize> {
        loop {
            let epoch = WAKE_EPOCH.load(Ordering::SeqCst);
            let r = with_pty(self.index, |p| p.master_write(buf)).ok_or(FileError::IOError)?;
            return match r {
                Ok(n) => Ok(n),
                Err(TtyError::Again) if self.nonblocking_now() => Err(FileError::Again),
                Err(TtyError::Again) => {
                    if !register_retry(self.index, Queue::MasterWrite, epoch) {
                        continue;
                    }
                    Err(FileError::WouldBlock)
                }
                Err(e) => Err(file_error_of(e)),
            };
        }
    }

    fn read_slave(&mut self, buf: &mut [u8]) -> FileResult<usize> {
        let c = caller().ok_or(FileError::IOError)?;
        match job_verdict(&c, self.index, true) {
            None => {}
            Some(Verdict::Stop(sig)) => return stop_and_restart(&c, sig),
            Some(Verdict::Error(e)) => return Err(file_error_of(e)),
            Some(Verdict::Allow) => {}
        }
        loop {
            let epoch = WAKE_EPOCH.load(Ordering::SeqCst);
            let r = with_pty(self.index, |p| {
                // VTIME is not timed here (see LIMITS): a timed wait is
                // answered as if its time had run out.
                match p.slave_read(buf, false) {
                    (Read::Wait(::tty::ldisc::Wait::Timeout(_)), _) => p.slave_read(buf, true),
                    other => other,
                }
            })
            .ok_or(FileError::IOError)?;
            return match r {
                Read::Data(n) => Ok(n),
                Read::Eof => Ok(0),
                Read::Wait(_) if self.nonblocking_now() => Err(FileError::Again),
                Read::Wait(_) => {
                    if !register_retry(self.index, Queue::SlaveRead, epoch) {
                        continue;
                    }
                    Err(FileError::WouldBlock)
                }
            };
        }
    }

    fn write_slave(&mut self, buf: &[u8]) -> FileResult<usize> {
        let tostop = PTYS.with(|t| {
            t[self.index].as_ref().is_some_and(|s| s.pty.termios().lflag(TOSTOP))
        });
        if tostop {
            let c = caller().ok_or(FileError::IOError)?;
            match job_verdict(&c, self.index, false) {
                Some(Verdict::Stop(sig)) => return stop_and_restart(&c, sig),
                Some(Verdict::Error(e)) => return Err(file_error_of(e)),
                _ => {}
            }
        }
        loop {
            let epoch = WAKE_EPOCH.load(Ordering::SeqCst);
            let r = with_pty(self.index, |p| p.slave_write(buf)).ok_or(FileError::IOError)?;
            return match r {
                Ok(n) => Ok(n),
                Err(TtyError::Again) if self.nonblocking_now() => Err(FileError::Again),
                Err(TtyError::Again) => {
                    if !register_retry(self.index, Queue::SlaveWrite, epoch) {
                        continue;
                    }
                    Err(FileError::WouldBlock)
                }
                Err(e) => Err(file_error_of(e)),
            };
        }
    }

    /// Job control for a change made through the slave (`TCSETS*`,
    /// `TIOCSPGRP`): `Some(return value)` if it must not go ahead.
    fn change_blocked(&self, c: &Caller) -> Option<i64> {
        if self.master {
            return None;
        }
        match job_verdict(c, self.index, false) {
            Some(Verdict::Stop(sig)) => Some(stop_and_restart_ioctl(c, sig)),
            Some(Verdict::Error(e)) => Some(errno_of(e)),
            _ => None,
        }
    }

    fn tty_ioctl(&mut self, request: u64, argp: u64) -> i64 {
        const TCGETS: u64 = 0x5401;
        const TCSETS: u64 = 0x5402;
        const TCSETSW: u64 = 0x5403;
        const TCSETSF: u64 = 0x5404;
        const TCFLSH: u64 = 0x540B;
        const TIOCSCTTY: u64 = 0x540E;
        const TIOCGPGRP: u64 = 0x540F;
        const TIOCSPGRP: u64 = 0x5410;
        const TIOCGWINSZ: u64 = 0x5413;
        const TIOCSWINSZ: u64 = 0x5414;
        const FIONREAD: u64 = 0x541B;
        const FIONBIO: u64 = 0x5421;
        const TIOCNOTTY: u64 = 0x5422;
        const TIOCGSID: u64 = 0x5429;
        const TIOCGPTN: u64 = 0x8004_5430;
        const TIOCSPTLCK: u64 = 0x4004_5431;

        let index = self.index;
        let user = |len: usize| crate::process::syscall::validate_user_buffer(argp, len);

        match request {
            TCGETS => {
                // A null pointer is `isatty()` probing: nothing to write.
                if argp != 0 {
                    if let Err(e) = user(core::mem::size_of::<Termios>()) { return e; }
                    let t = PTYS.with(|t| t[index].as_ref().map(|s| *s.pty.termios()));
                    let Some(t) = t else { return errno::EIO };
                    unsafe { core::ptr::write_unaligned(argp as *mut Termios, t) };
                }
                0
            }
            TCSETS | TCSETSW | TCSETSF => {
                if let Err(e) = user(core::mem::size_of::<Termios>()) { return e; }
                let Some(c) = caller() else { return errno::ESRCH };
                if let Some(r) = self.change_blocked(&c) { return r; }
                let t = unsafe { core::ptr::read_unaligned(argp as *const Termios) };
                // TCSETSW's "after output drains" is immediate: the
                // output queue belongs to the master's reader, which this
                // call cannot wait for without blocking an ioctl.
                with_pty(index, |p| ((), p.set_termios(t, request == TCSETSF)));
                0
            }
            TCFLSH => {
                let which = match argp {
                    0 => Flush::Input,
                    1 => Flush::Output,
                    2 => Flush::Both,
                    _ => return errno::EINVAL,
                };
                with_pty(index, |p| ((), p.flush(which)));
                0
            }
            TIOCGWINSZ => {
                if let Err(e) = user(8) { return e; }
                let ws = PTYS.with(|t| t[index].as_ref().map(|s| s.pty.winsize())).unwrap_or_default();
                unsafe { core::ptr::write_unaligned(argp as *mut Winsize, ws) };
                0
            }
            TIOCSWINSZ => {
                if let Err(e) = user(8) { return e; }
                let ws = unsafe { core::ptr::read_unaligned(argp as *const Winsize) };
                with_pty(index, |p| ((), p.set_winsize(ws)));
                0
            }
            FIONREAD => {
                if let Err(e) = user(4) { return e; }
                let n = PTYS.with(|t| {
                    t[index].as_ref().map(|s| if self.master { s.pty.master_pending() } else { s.pty.slave_pending() })
                }).unwrap_or(0);
                unsafe { core::ptr::write_unaligned(argp as *mut i32, n as i32) };
                0
            }
            FIONBIO => {
                if let Err(e) = user(4) { return e; }
                let on = unsafe { core::ptr::read_unaligned(argp as *const i32) } != 0;
                self.nonblock.store(on, Ordering::Relaxed);
                0
            }
            TIOCGPTN if self.master => {
                if let Err(e) = user(4) { return e; }
                unsafe { core::ptr::write_unaligned(argp as *mut u32, index as u32) };
                0
            }
            TIOCSPTLCK if self.master => {
                if let Err(e) = user(4) { return e; }
                let lock = unsafe { core::ptr::read_unaligned(argp as *const i32) } != 0;
                PTYS.with(|t| {
                    if let Some(s) = t[index].as_mut() {
                        s.pty.set_locked(lock);
                    }
                });
                0
            }
            TIOCSCTTY => {
                let Some(c) = caller() else { return errno::ESRCH };
                let r = PTYS.with(|t| {
                    let slot = t[index].as_mut().ok_or(TtyError::Io)?;
                    let already = jobctl::check_set_ctty(c.pid, c.sid, c.ctty.is_some(), slot.pty.session(), argp == 1)?;
                    if !already {
                        slot.pty.set_session(Some(c.sid), c.pgid);
                    }
                    Ok(already)
                });
                match r {
                    Ok(false) => {
                        // Stolen from another session: its members lose it.
                        with_sched(|s| {
                            s.clear_ctty(index);
                            if let Some(p) = s.running_mut() {
                                p.ctty = Some(index);
                            }
                        });
                        0
                    }
                    Ok(true) => 0,
                    Err(e) => errno_of(e),
                }
            }
            TIOCNOTTY => {
                let Some(c) = caller() else { return errno::ESRCH };
                if c.ctty != Some(index) {
                    return errno::ENOTTY;
                }
                with_sched(|s| {
                    if let Some(p) = s.running_mut() {
                        p.ctty = None;
                    }
                });
                // A session leader giving it up detaches the whole session
                // and hangs up its foreground group, as Linux's
                // `disassociate_ctty` does.
                if c.pid == c.sid {
                    let fg = PTYS.with(|t| {
                        t[index].as_mut().and_then(|s| {
                            let fg = s.pty.foreground();
                            s.pty.set_session(None, 0);
                            fg
                        })
                    });
                    let mut e = Effects::default();
                    if let Some(fg) = fg {
                        e.signals.push((Target::Group(fg), Signal::Hup));
                        e.signals.push((Target::Group(fg), Signal::Cont));
                    }
                    e.detach_session = Some(c.sid);
                    apply(index, e);
                }
                0
            }
            TIOCGPGRP | TIOCGSID => {
                if let Err(e) = user(4) { return e; }
                let Some(c) = caller() else { return errno::ESRCH };
                // Through the slave, only a process's own controlling
                // terminal answers; the master always does (Linux).
                if !self.master && c.ctty != Some(index) {
                    return errno::ENOTTY;
                }
                let v = PTYS.with(|t| {
                    t[index].as_ref().and_then(|s| if request == TIOCGPGRP { s.pty.foreground() } else { s.pty.session() })
                });
                let Some(v) = v else { return errno::ENOTTY };
                unsafe { core::ptr::write_unaligned(argp as *mut i32, v as i32) };
                0
            }
            TIOCSPGRP => {
                if let Err(e) = user(4) { return e; }
                let Some(c) = caller() else { return errno::ESRCH };
                let is_ctty = c.ctty == Some(index);
                if !is_ctty {
                    return errno::ENOTTY;
                }
                if let Some(r) = self.change_blocked(&c) { return r; }
                let pgid = unsafe { core::ptr::read_unaligned(argp as *const i32) };
                if pgid <= 0 {
                    return errno::EINVAL;
                }
                let in_session = with_sched(|s| s.group_in_session(pgid as u32, c.sid));
                if let Err(e) = jobctl::check_set_foreground(is_ctty, in_session) {
                    return errno_of(e);
                }
                PTYS.with(|t| {
                    if let Some(s) = t[index].as_mut() {
                        s.pty.set_foreground(pgid as u32);
                    }
                });
                0
            }
            _ => errno::ENOTTY,
        }
    }
}

impl FileHandle for PtyHandle {
    fn read(&mut self, buf: &mut [u8]) -> FileResult<usize> {
        if self.master { self.read_master(buf) } else { self.read_slave(buf) }
    }

    fn write(&mut self, buf: &[u8]) -> FileResult<usize> {
        if self.master { self.write_master(buf) } else { self.write_slave(buf) }
    }

    fn ioctl(&mut self, request: u64, arg: u64) -> Option<i64> {
        Some(self.tty_ioctl(request, arg))
    }

    fn stat(&self) -> Option<Stat> {
        // ptmx is one device; each slave its own.
        let ino = if self.master { PTS_INO_BASE - 1 } else { PTS_INO_BASE + self.index as u64 };
        Some(Stat::chardev(ino))
    }

    fn dup(&self) -> Option<Box<dyn FileHandle>> {
        PTYS.with(|t| {
            let slot = t[self.index].as_mut()?;
            if self.master {
                slot.masters += 1;
            } else {
                slot.pty.dup_slave();
            }
            Some(())
        })?;
        Some(Box::new(PtyHandle { index: self.index, master: self.master, nonblock: self.nonblock.clone() }))
    }

    fn name(&self) -> &str {
        if self.master { "pty-master" } else { "pty-slave" }
    }

    fn pty_end(&self) -> Option<vfs::file::PtyEnd> {
        Some(vfs::file::PtyEnd { index: self.index, master: self.master })
    }

    fn nonblocking(&self) -> bool {
        self.nonblocking_now()
    }

    fn set_nonblocking(&self, on: bool) -> bool {
        self.nonblock.store(on, Ordering::Relaxed);
        true
    }
}

impl Drop for PtyHandle {
    // As in `UnixSocketHandle`: the count lives in `Drop`. Effects are
    // applied with interrupts saved and restored (`with_sched`,
    // `without_interrupts`), never re-enabled: this runs inside `sys_exit`.
    fn drop(&mut self) {
        let index = self.index;
        let master = self.master;
        let out = PTYS.with(|t| {
            let slot = t[index].as_mut()?;
            let e = if master {
                slot.masters -= 1;
                if slot.masters == 0 { slot.pty.close_master() } else { Effects::default() }
            } else {
                slot.pty.close_slave()
            };
            let dead = slot.pty.is_dead();
            if dead {
                t[index] = None;
            }
            Some((e, dead))
        });
        if let Some((e, dead)) = out {
            apply(index, e);
            if dead {
                crate::ktrace!(crate::debug::FS, "pty: pair {} freed", index);
            }
        }
    }
}
