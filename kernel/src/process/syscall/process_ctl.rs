// kernel/src/process/syscall/process_ctl.rs
//
// Process lifecycle + control syscalls: fork/clone/exec/exit/waitpid/kill/
// getpid/setpgid/getpgid/setsid/yield/nanosleep/arch_prctl/set_tid_address.

use crate::process::signal::SigOrigin;
use super::with_current_process;
use crate::sync::Mutex;
use crate::serial_println;
use crate::process::TrapFrame;
use super::{
    errno, SyscallResult, with_scheduler, validate_user_buffer, resolve_path,
    current_tf_ptr,
};

// ── arch_prctl(158) ────────────────────────────────────────────────────────

/// arch_prctl(158): int arch_prctl(int code, unsigned long addr)
///
/// Only ARCH_SET_FS (0x1002) is implemented: writes the FS.base MSR so
/// that TLS (thread-local storage) works.  mlibc calls this via sys_tcb_set.
pub(super) fn sys_arch_prctl(code: i32, addr: u64) -> SyscallResult {
    const ARCH_SET_FS: i32 = 0x1002;
    const ARCH_GET_FS: i32 = 0x1003;
    const IA32_FS_BASE: u32 = 0xC000_0100;

    match code {
        ARCH_SET_FS => {
            unsafe {
                core::arch::asm!(
                    "wrmsr",
                    in("ecx") IA32_FS_BASE,
                    in("eax") (addr & 0xFFFF_FFFF) as u32,
                    in("edx") (addr >> 32) as u32,
                    options(nostack, preserves_flags),
                );
            }
            // Also persist in the current process's saved state so the
            // value is restored on every context switch via TSS RSP0 path.
            // (For now it survives as long as the process keeps running;
            // full save/restore needs FS.base in TrapFrame — future work.)
            0
        }
        ARCH_GET_FS => {
            if validate_user_buffer(addr, 8).is_err() { return errno::EFAULT; }
            let mut lo: u32;
            let mut hi: u32;
            unsafe {
                core::arch::asm!(
                    "rdmsr",
                    in("ecx") IA32_FS_BASE,
                    out("eax") lo,
                    out("edx") hi,
                    options(nostack, preserves_flags),
                );
                *(addr as *mut u64) = (hi as u64) << 32 | lo as u64;
            }
            0
        }
        _ => errno::EINVAL,
    }
}


// ── set_tid_address(218) ───────────────────────────────────────────────────

/// set_tid_address(218): pid_t set_tid_address(int *tidptr)
///
/// Registers the address that gets a 0 and a futex wake when this thread
/// exits (`Process::clear_child_tid`); returns the tid.
pub(super) fn sys_set_tid_address(tidptr: u64) -> SyscallResult {
    with_scheduler(|scheduler| match scheduler.running_mut() {
        Some(proc) => {
            proc.clear_child_tid = tidptr;
            proc.pid.0 as SyscallResult
        }
        None => 0,
    })
}

/// sys_yield — voluntary context switch.
///
/// Reuses the same `switch_to_next` the timer ISR uses for preemption: puts
/// the caller back at the tail of its run queue (as Ready) and switches to
/// the next Ready process. If nothing else is Ready, `switch_to_next`
/// returns the caller's own TrapFrame unchanged and this is a no-op.
pub(super) fn sys_yield() -> SyscallResult {
    let tf_ptr = current_tf_ptr();

    // `_irq` is deliberately never dropped: this always ends in
    // `jump_to_user` (`-> !`), so interrupts intentionally stay off across
    // the jump — see `sys_read`'s WouldBlock arm for the same reasoning.
    let _irq = crate::process::irq_guard::InterruptGuard::new();

    let next_tf = {
        let mut scheduler = crate::process::scheduler::local_scheduler();
        // Pre-set rax=0 in the on-stack frame *before* switch_to_next copies
        // it into the process's saved TrapFrame, so that whenever this
        // process runs again, the syscall returns 0.
        unsafe { (*(tf_ptr as *mut TrapFrame)).rax = 0; }
        scheduler.switch_to_next(tf_ptr)
    };

    unsafe { crate::process::trapframe::jump_to_user(next_tf) }
}

/// nanosleep(35): int nanosleep(const struct timespec *req, struct timespec *rem)
///
/// The Linux ABI: `req` is a `timespec`, and if a handler ends the sleep early `rem` (optional) gets the time left.
pub(super) fn sys_nanosleep(req: u64, rem: u64) -> SyscallResult {
    match read_sleep_args(req, rem) {
        Ok(ns) if ns == 0 => 0,
        Ok(ns) => sleep_until(relative_expiry(ns), 35, rem, true),
        Err(e) => e,
    }
}

/// Validate a sleep's `req` (a `timespec`, `EINVAL` if malformed) and optional `rem` (writable) and return the request in ns.
fn read_sleep_args(req: u64, rem: u64) -> Result<u64, SyscallResult> {
    if validate_user_buffer(req, 16).is_err() || (rem != 0 && validate_user_buffer(rem, 16).is_err()) {
        return Err(errno::EFAULT);
    }
    // SAFETY: validated as a user-space range; a fault demand-pages or kills the caller, as for any user store.
    let (sec, nsec) = unsafe { (*(req as *const i64), *((req + 8) as *const i64)) };
    timespec_ns(sec, nsec).ok_or(errno::EINVAL)
}

/// clock_nanosleep(230): int clock_nanosleep(clockid_t, int flags, const struct timespec *req, struct timespec *rem)
///
/// `TIMER_ABSTIME` sleeps until an absolute time on the clock named: the monotonic ones are `ktime`, `CLOCK_REALTIME`
/// is `ktime` shifted by the wall clock (the same reading `clock_gettime` gives). A time already past returns 0 at once.
/// `rem` is written for a relative sleep a handler ended (`EINTR`), never for an absolute one: Rust's `thread::sleep` retries
/// with it.
pub(super) fn sys_clock_nanosleep(clock: u64, flags: u32, req: u64, rem: u64) -> SyscallResult {
    const TIMER_ABSTIME: u32 = 1;
    const CLOCK_REALTIME: u64 = 0;
    const CLOCK_MONOTONIC: u64 = 1;
    const CLOCK_MONOTONIC_RAW: u64 = 4;
    const CLOCK_BOOTTIME: u64 = 7;
    if flags & !TIMER_ABSTIME != 0 {
        return errno::EINVAL;
    }
    if !matches!(clock, CLOCK_REALTIME | CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_BOOTTIME) {
        return errno::EINVAL;
    }
    let ns = match read_sleep_args(req, rem) {
        Ok(ns) => ns,
        Err(e) => return e,
    };
    let up = crate::time::ktime_get();
    let expiry = if flags & TIMER_ABSTIME == 0 {
        up.saturating_add(ns)
    } else if clock == CLOCK_REALTIME {
        // `ktime` at which the wall clock reads `ns`: the wall clock is `BOOT_UNIX_SECS * 1e9 + up`
        let wall_now = crate::time::now_unix_secs().saturating_mul(1_000_000_000).saturating_add(up % 1_000_000_000);
        up.saturating_add(ns.saturating_sub(wall_now))
    } else {
        ns
    };
    if expiry <= up {
        return 0;
    }
    let relative = flags & TIMER_ABSTIME == 0;
    sleep_until(if relative { relative_expiry(ns) } else { expiry }, 230, rem, relative)
}

/// The deadline of a relative sleep of `ns` from now — or, if this call is the restart of a sleep a stop/continue interrupted,
/// the deadline that sleep already had (`Process::sleep_resume`).
fn relative_expiry(ns: u64) -> u64 {
    let resume = crate::process::irq_guard::SchedGuard::lock().running_mut().and_then(|p| p.sleep_resume.take());
    resume.unwrap_or_else(|| crate::time::ktime_get().saturating_add(ns))
}

/// A `timespec` as nanoseconds; `None` for a negative or out-of-range field (`EINVAL` in Linux).
fn timespec_ns(sec: i64, nsec: i64) -> Option<u64> {
    if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
        return None;
    }
    Some((sec as u64).saturating_mul(1_000_000_000).saturating_add(nsec as u64))
}

/// Block the caller until `ktime` reaches `expiry` (an hrtimer wake), or a signal ends the wait.
///
/// LOCKING (see hrtimer.rs for full analysis):
///   cli → scheduler lock → QUEUE lock (hrtimer::start) → QUEUE released →
///   block_current → never returns here.
fn sleep_until(expiry: u64, syscall_nr: u64, rem: u64, relative: bool) -> SyscallResult {
    let tf_ptr = current_tf_ptr();

    // `_irq` is deliberately never dropped — see sys_yield above.
    let _irq = crate::process::irq_guard::InterruptGuard::new();

    let next_tf = {
        let mut scheduler = crate::process::scheduler::local_scheduler();

        // Set the wakeup return value in the saved TrapFrame so that when the
        // process is woken by hrtimer::tick() the syscall returns 0.
        unsafe {
            (*(tf_ptr as *mut TrapFrame)).rax = 0;
        }

        let pid = scheduler.current_pid().map(|p| p.0).unwrap_or(0);
        crate::ktrace!(crate::debug::SCHED, "sleep PID {} until {} (syscall {})", pid, expiry, syscall_nr);

        // Register the hrtimer.  QUEUE lock is acquired and released inside
        // start(); we still hold the scheduler lock, which is safe because
        // the ISR path acquires QUEUE first then the scheduler — and ISRs
        // cannot fire while cli is in effect.
        let cell = scheduler.begin_wait();
        let timer = crate::time::hrtimer::start(expiry, crate::time::hrtimer::HrTimerAction::Wake { pid, cell });

        // A signal ends the sleep with EINTR once a handler runs, and `rem` (if given) gets the time left.
        let ret_rip = unsafe { (*tf_ptr).rip };
        let mut wait = crate::process::wait::Wait::cell(
            syscall_nr, ret_rip, crate::process::wait::RestartPolicy::NoHandlerOnly,
            crate::process::wait::Cleanup::Timer(timer),
        );
        // A relative sleep carries its deadline (the time left goes to `rem` if given; a restart resumes to it).
        if relative {
            wait = wait.with_rem(crate::process::wait::SleepRem { expiry, rem_ptr: rem });
        }
        scheduler.block_current(tf_ptr, wait)
        // scheduler lock dropped here
    };

    unsafe { crate::process::trapframe::jump_to_user(next_tf) }
}

/// pidfd_open(434): a descriptor for process `pid` (a thread-group leader; `EINVAL` for a thread's tid, `ESRCH` if there is no such
/// process), readable once it has exited. `PIDFD_NONBLOCK` (= `O_NONBLOCK`) is the only flag; the fd is always close-on-exec.
pub(super) fn sys_pidfd_open(pid: i64, flags: u32) -> SyscallResult {
    const PIDFD_NONBLOCK: u32 = 0x800;
    if pid <= 0 || flags & !PIDFD_NONBLOCK != 0 {
        return errno::EINVAL;
    }
    let pid = pid as usize;
    // (leader, zombie) of the process, from the scheduler; syscalls start with IF=0.
    let found = {
        let sched = crate::process::scheduler::local_scheduler();
        let r = sched.iter_all().find(|p| p.pid.0 == pid && pid != 0).map(|p| (!p.is_thread, matches!(p.state, crate::process::ProcessState::Zombie)));
        r
    };
    let Some((leader, zombie)) = found else { return errno::ESRCH };
    if !leader {
        return errno::EINVAL;
    }
    let handle = crate::process::pidfd::create(pid, zombie, flags & PIDFD_NONBLOCK != 0);
    super::with_files(|files| {
        match files.allocate(alloc::boxed::Box::new(handle)) {
            Ok(fd) => {
                let _ = files.set_cloexec(fd, true);
                fd as SyscallResult
            }
            Err(_) => errno::EMFILE,
        }
    })
}

/// pidfd_send_signal(424): `kill` through a pidfd (`info` must be NULL, `flags` 0). `ESRCH` once the process has been reaped.
pub(super) fn sys_pidfd_send_signal(pidfd: i32, sig: u32, info: u64, flags: u32) -> SyscallResult {
    if info != 0 || flags != 0 {
        return errno::EINVAL;
    }
    let files = match crate::process::scheduler::local_scheduler().running_ref() {
        Some(p) => p.files.clone(),
        None => return errno::ESRCH,
    };
    let pid = {
        let guard = files.lock();
        if pidfd < 0 {
            return errno::EBADF;
        }
        match guard.get(pidfd as usize) {
            Ok(h) => match h.pidfd_pid() {
                Some(pid) => pid,
                None => return errno::EBADF,
            },
            Err(_) => return errno::EBADF,
        }
    };
    // As `kill`: a zombie still takes (and ignores) the signal, a reaped process is `ESRCH`.
    sys_kill(pid as i64, sig)
}

/// prctl(157): only `PR_SET_NAME` (15) and `PR_GET_NAME` (16), the thread's `comm` (Rust's `thread::Builder::name`, tokio's worker
/// threads). Any other option is `EINVAL`.
pub(super) fn sys_prctl(option: i32, arg2: u64) -> SyscallResult {
    const PR_SET_NAME: i32 = 15;
    const PR_GET_NAME: i32 = 16;
    match option {
        PR_SET_NAME => {
            if validate_user_buffer(arg2, 1).is_err() {
                return errno::EFAULT;
            }
            // Up to 15 bytes, stopping at NUL, read one at a time (the name may end right at an unmapped page).
            let mut name = [0u8; 16];
            let mut n = 0;
            while n < 15 {
                if validate_user_buffer(arg2 + n as u64, 1).is_err() {
                    return errno::EFAULT;
                }
                let b = unsafe { *((arg2 + n as u64) as *const u8) };
                if b == 0 { break; }
                name[n] = b;
                n += 1;
            }
            with_scheduler(|s| {
                if let Some(p) = s.running_mut() {
                    p.name = name;
                }
                0
            })
        }
        PR_GET_NAME => {
            if validate_user_buffer(arg2, 16).is_err() {
                return errno::EFAULT;
            }
            let name = crate::process::irq_guard::SchedGuard::lock().running_ref().map(|p| p.name).unwrap_or([0; 16]);
            unsafe { core::ptr::copy_nonoverlapping(name.as_ptr(), arg2 as *mut u8, 16); }
            0
        }
        _ => errno::EINVAL,
    }
}

/// getpid(39): the thread-group id (`Process::tgid`): the same in every thread of a process, and the pid `kill`/`waitpid` and a child's
/// `getppid` use. The per-thread id is `gettid`.
pub(super) fn sys_getpid() -> SyscallResult {
    with_scheduler(|scheduler| {
        scheduler.running_ref().map(|p| p.tgid as SyscallResult).unwrap_or(0)
    })
}

/// gettid(186). A thread is a process here (`sys_clone` gives it its own pid), so its tid is that pid.
pub(super) fn sys_gettid() -> SyscallResult {
    with_scheduler(|scheduler| {
        scheduler.current_pid().map(|pid| pid.0 as SyscallResult).unwrap_or(0)
    })
}

/// tkill(200) / tgkill(234): a signal to one thread, not to whichever thread of the group takes it (`kill_impl` with `exact`).
pub(super) fn sys_tkill(tid: i64, sig: u32) -> SyscallResult {
    if tid <= 0 {
        return errno::EINVAL;
    }
    kill_impl(tid, sig, true, None)
}

/// `tgkill` also names the thread group, and ESRCH if `tid` is not in it (a tid reused by another process cannot be hit).
pub(super) fn sys_tgkill(tgid: i64, tid: i64, sig: u32) -> SyscallResult {
    if tgid <= 0 || tid <= 0 {
        return errno::EINVAL;
    }
    kill_impl(tid, sig, true, Some(tgid as usize))
}

/// getppid(110): the parent's pid — `Process::parent_pid`, which
/// `reparent_children` points at PID 1 when the parent dies, so an orphan
/// reads 1 exactly as on Linux. 0 for a process with no parent (PID 1).
pub(super) fn sys_getppid() -> SyscallResult {
    with_scheduler(|scheduler| {
        scheduler.running_ref().and_then(|p| p.parent_pid).map(|pid| pid.0 as SyscallResult).unwrap_or(0)
    })
}

/// sys_exit — terminate the calling process and switch immediately.
///
/// Performs an immediate full context switch via kill_and_switch_tf +
/// jump_to_trapframe.  This restores ALL registers of the next process
/// and never returns.
pub(super) fn sys_exit(status: i32) -> SyscallResult {
    use alloc::format;

    let reason = format!("exit({})", status);

    let _irq = crate::process::irq_guard::InterruptGuard::new();

    let (dead_pid, parent_to_notify, tf_ptr, old_files) = {
        let mut scheduler = crate::process::scheduler::local_scheduler();
        // Swap in a fresh, empty fd table *before* the process becomes a
        // zombie (or gets reaped immediately, if it's a thread) — this
        // closes any pipe ends it held right now instead of leaving them
        // open until some future waitpid() reaps the zombie. The old Arc
        // is returned out of this block (not dropped here): dropping it
        // can run a pipe end's Drop impl, which needs to lock SCHEDULER to
        // wake a peer — doing that while this scope's `scheduler` guard is
        // still held would self-deadlock (spin::Mutex isn't reentrant).
        //
        // Only a real process (not a pthread) exiting notifies its parent
        // via SIGCHLD — matches POSIX (individual thread exits are not a
        // waitpid()/SIGCHLD event, only the process as a whole exiting is).
        let (old_files, parent_to_notify) = if let Some(proc) = scheduler.running_mut() {
            proc.exit_status = status;
            let parent = if proc.is_thread { None } else { proc.parent_pid };
            let old = core::mem::replace(
                &mut proc.files,
                alloc::sync::Arc::new(Mutex::new(crate::process::file::FileDescriptorTable::new())),
            );
            (old, parent)
        } else {
            (alloc::sync::Arc::new(Mutex::new(crate::process::file::FileDescriptorTable::new())), None)
        };
        let dead_pid = scheduler.current_pid().map(|p| p.0).unwrap_or(0);
        let ptr = scheduler.kill_and_switch_tf(&reason);
        serial_println!("  → Process exited, switching immediately (full TrapFrame restore)");
        (dead_pid, parent_to_notify, ptr, old_files)
    };

    // Safe to drop now: SCHEDULER is released, interrupts are still off
    // (`irq` hasn't dropped yet), so any pipe-end wake this triggers can
    // lock SCHEDULER without racing anything else on this single core.
    drop(old_files);

    // Queue SIGCHLD on the parent (default action Ignore — purely additive,
    // no observable change unless the parent installed a handler) and wake
    // it if it's blocked in waitpid() for exactly this pid, delivering the
    // real wait status. One locked section for both — `find_process_mut`
    // only searches run_queues/wait_queue (deliberately excludes `running`,
    // see its doc comment); the parent may already *be* `running` here if
    // `kill_and_switch_tf` above just picked it as the next process to
    // schedule, so `notify_child_death` checks that case itself.
    crate::process::scheduler::local_scheduler().notify_child_death(dead_pid, parent_to_notify);

    // Cancel any pending poll/epoll wait and clear side tables. Deliberately
    // runs with interrupts STILL OFF (`irq` is not dropped until the RSP
    // switch inside `jump_to_user`): all three cleanups are non-blocking
    // spin-lock critical sections over pid-keyed maps (POLL_WAITERS /
    // EPOLL_FD_MAP / FUTEX_WAITERS) — nothing here needs IF=1. Keeping IF=0
    // closes the bug-2 window where a timer tick lands while the CPU is still
    // on the dying process's kernel stack (queued for deferred free by
    // `kill_and_switch_tf`), freeing that stack out from under this epilogue
    // and, on the switch path, saving the next process's trapframe with a
    // dangling RSP.
    cancel_all_waiters(dead_pid);

    // Deliberately no `drop(irq)` and no explicit `sti` before the switch:
    // either one re-enables interrupts here, re-opening exactly the window
    // above (the interrupt guard's Drop does `sti`). `jump_to_user` does its
    // own `cli`, and its iretq restores the target process's own saved
    // RFLAGS (IF set), so interrupts come back on from the resumed process's
    // state and the guard's Drop never needs to run — this function diverges.
    unsafe {
        crate::process::trapframe::jump_to_user(tf_ptr);
    }
}

/// exit_group(231): end every thread of the calling process, then exit.
///
/// A thread group is the set of processes sharing one address space (there is no tgid here). The others get SIGKILL; the caller
/// exits with `status`. Called from a spawned thread, the *leader* is what the parent waits for: it is told to report
/// `exit(status)` (`group_exited`) although SIGKILL is what ends it.
pub(super) fn sys_exit_group(status: i32) -> SyscallResult {
    with_scheduler(|sched| {
        let Some(me) = sched.running_mut() else { return 0 };
        // The caller ends the process with `status`: the SIGKILLs below make its threads die "of a signal", and each such death
        // tags the still-unreaped leader (the caller, if it is one) with SIGKILL through `kill_thread_group`.
        me.group_exited = true;
        let (my_pid, space) = (me.pid.0, me.address_space.clone());
        let others: alloc::vec::Vec<(usize, bool)> = sched
            .iter_all()
            .filter(|p| p.pid.0 != my_pid && p.pid.0 != 0 && alloc::sync::Arc::ptr_eq(&p.address_space, &space))
            .map(|p| (p.pid.0, p.is_thread))
            .collect();
        for (pid, is_thread) in others {
            if !is_thread {
                if let Some(leader) = sched.find_process_mut(pid) {
                    leader.exit_status = status;
                    leader.group_exited = true;
                }
            }
            sched.signal_pid(pid, crate::process::signal::SIGKILL);
        }
        0
    });
    sys_exit(status)
}

/// Cancel every side-table registration a dying process might be holding
/// (pending poll/epoll waits, futex waiters). Must run for *every* death
/// path, not just `sys_exit`'s: `resolve_signals`'s uncaught-signal
/// Terminate path (`Scheduler::kill_and_switch_tf`, driven by hardware
/// faults and now routinely by job-control signals like `kill(-pgid,
/// SIGTERM)`) used to skip this entirely, leaking a stale `POLL_WAITERS`/
/// `EPOLL_FD_MAP`/`FUTEX_WAITERS` slot for that pid forever — harmless by
/// itself (pids are never reused), but a real hazard whenever any of those
/// tables index by a small fixed slot number rather than pid: enough leaked
/// entries can spuriously wake or otherwise affect a *different*, later,
/// completely unrelated process that happens to land in the same slot.
/// Found via `kill(-pgid, SIGTERM)` on a process busy-nanosleeping in a
/// nanosleep loop (`jobctl_test.c`'s group-kill test) followed immediately
/// by starting an interactive `ash` — its own `poll()`-based input loop
/// intermittently died after 1-2 characters, traced back to exactly this.
pub(crate) fn cancel_all_waiters(pid: usize) {
    super::poll::poll_cancel_waiter(pid);
    super::sync::futex_cancel_waiter(pid);
    // A socket waiter left behind would later wake whatever process
    // inherits this pid number.
    crate::ipc::unix::cancel_waiters_for(pid);
    crate::ipc::pty::cancel_waiters_for(pid);
}

pub(super) fn sys_fork() -> SyscallResult {
    fork_impl(0, None, false)
}

/// `fork`, with `clone`'s two extras: the child resumes on `child_stack` if that is nonzero, and with `tls` as its FS base if given.
fn fork_impl(child_stack: u64, tls: Option<u64>, vfork: bool) -> SyscallResult {
    let tf_ptr = current_tf_ptr();

    let _irq = crate::process::irq_guard::InterruptGuard::new();

    // Real fork() semantics: the child gets a copy of the parent's *live*
    // FPU/SSE registers, not whatever was last stashed in the parent's own
    // `Process::fpu_state` (stale as of its last preemption — this process
    // has been running uninterrupted since then, up to and including
    // whatever FP code ran right before calling fork()). A fresh
    // `fpu::save()` here captures the actual current hardware state.
    let mut parent_fpu_state = crate::process::fpu::default_state();
    unsafe { crate::process::fpu::save(&mut parent_fpu_state); }

    // Collect what we need from the running process
    let (child_as, parent_pid, parent_fs_base, files_arc, child_tf, parent_cwd, (parent_pgid, parent_sid, parent_ctty), parent_exe_name, parent_signals, (parent_comm, parent_cmdline, parent_creds)) = {
        let scheduler = crate::process::scheduler::local_scheduler();
        match scheduler.running_ref() {
            Some(proc) => {
                // Build child TrapFrame: same as parent but rax=0 (fork returns 0 in child)
                let mut tf_copy = unsafe { *tf_ptr };
                tf_copy.rax = 0;
                if child_stack != 0 {
                    tf_copy.rsp = child_stack;
                }

                match unsafe { proc.address_space.fork() } {
                    // FS base from the MSR, not `proc.fs_base`: that field is
                    // only refreshed when the parent is switched out, so it is
                    // stale if `arch_prctl` ran since — same reasoning as the
                    // live `fpu::save` above.
                    Ok(child_as) => (child_as, crate::process::Pid(proc.tgid), crate::process::scheduler::read_fs_base(), proc.files.clone(), tf_copy, proc.cwd.clone(), (proc.pgid, proc.sid, proc.ctty), proc.exe_name.clone(),
                        (proc.signal_handlers, proc.sig_restart, proc.blocked_signals, proc.sig_extra, proc.altstack), (proc.name, proc.cmdline.clone(), proc.creds.clone())),
                    Err(e) => {
                        serial_println!("fork: address_space.fork() failed: {}", e);
                        return errno::ENOMEM;
                    }
                }
            }
            None => return errno::ESRCH,
        }
    };

    // The fd table is copied (each handle `dup`ed) with `SCHEDULER` released: see `with_fd_table` for the lock order.
    let files = files_arc.lock().clone();
    drop(files_arc);

    let kernel_stack = crate::init::processes::allocate_kernel_stack();

    let (child_pid, vfork_next) = {
        let mut scheduler = crate::process::scheduler::local_scheduler();
        let pid = scheduler.allocate_pid();
        let caller_tid = scheduler.current_pid().map(|p| p.0);

        let mut child = alloc::boxed::Box::new(
            crate::process::Process::new_user_from_fork(
                pid, parent_pid, alloc::boxed::Box::new(child_tf),
                kernel_stack, child_as, files, parent_cwd, parent_pgid, parent_sid, parent_exe_name,
                alloc::boxed::Box::new(parent_fpu_state),
            )
        );
        child.fs_base = tls.unwrap_or(parent_fs_base); // inherit TLS base from parent unless clone gave one
        child.ctty = parent_ctty;
        // POSIX fork(): dispositions (with their SA_RESTART) and the signal
        // mask are inherited; pending signals are not. Every child used to
        // start with all-default handlers and an empty mask, so a signal
        // between fork and the child's own sigaction ran the default action
        // (SIGINT killed a child its shell had set to ignore it) and one
        // the parent had blocked around fork arrived in the child anyway.
        (child.signal_handlers, child.sig_restart, child.blocked_signals, child.sig_extra, child.altstack) = parent_signals;
        // Linux: a child keeps its parent's `comm` and command line until
        // it execs. Every child used to be named "child", so `ps` showed a
        // shell's subshells, and any program that forks without exec'ing,
        // as `[child]`.
        child.name = parent_comm;
        child.cmdline = parent_cmdline;
        child.creds = parent_creds;
        if vfork {
            child.vfork_parent = caller_tid;
        }
        scheduler.add_process(child);
        // `CLONE_VFORK`: the parent sleeps until the child execs or dies. Added and blocked under one hold of the lock, so
        // the child cannot finish first; whoever ends the child's hold on the parent wakes it with the child's pid.
        let next = if vfork {
            unsafe { (*(tf_ptr as *mut TrapFrame)).rax = pid.0 as u64; }
            Some(scheduler.block_current(tf_ptr, crate::process::wait::Wait::uninterruptible()))
        } else {
            None
        };
        (pid.0 as SyscallResult, next)
    };

    if let Some(next) = vfork_next {
        // Diverges, like every blocking syscall: `_irq` stays held on purpose (see `sys_yield`).
        unsafe { crate::process::trapframe::jump_to_user(next) }
    }
    child_pid  // parent sees child PID
}

// ── clone(56) ──────────────────────────────────────────────────────────────

/// clone(56): long clone(unsigned long flags, void *stack, int *ptid, int *ctid, void *tls) — Linux's x86-64 argument order.
///
/// - `CLONE_THREAD` (with `CLONE_VM|CLONE_SIGHAND`): a new schedulable Process that SHARES the caller's `AddressSpace` (the same
///   `Arc`, no page-table copy) and file table. It resumes where the caller does, after the `syscall`, with `rax = 0`, the
///   caller's registers and FPU state, `rsp = stack` (if nonzero) and FS base = `tls` under `CLONE_SETTLS`. The caller gets the
///   new thread's pid, its tid. `CLONE_PARENT_SETTID`/`CLONE_CHILD_SETTID` store it at `ptid`/`ctid` before the thread can run;
///   `CLONE_CHILD_CLEARTID` registers `ctid` for `Process::clear_child_tid`.
/// - Without `CLONE_THREAD`: a COW fork (`fork_impl`) on `stack`, whatever `CLONE_VM`/`CLONE_VFORK` say. That is what musl's
///   `posix_spawn` asks for; the parent is not suspended until the child execs, which is safe because musl reports an exec
///   failure through a close-on-exec pipe, not shared memory. (close-on-exec is not honoured yet, so a spawn would block.)
///
/// A thread is a process with its own pid: there is no tgid, so `getpid()` in a thread returns the tid. It never zombie-parks on
/// exit (see `Process::is_thread`, `Scheduler::kill_current`). A `Huge2M` VMA under `stack` is recorded as the thread's own and
/// freed when it dies (mlibc never unmaps its thread stacks).
pub(super) fn sys_clone(flags: u64, stack: u64, ptid: u64, ctid: u64, tls: u64) -> SyscallResult {
    const CLONE_VM: u64 = 0x100;
    const CLONE_SIGHAND: u64 = 0x800;
    const CLONE_VFORK: u64 = 0x4000;
    const CLONE_THREAD: u64 = 0x1_0000;
    const CLONE_SETTLS: u64 = 0x8_0000;
    const CLONE_PARENT_SETTID: u64 = 0x10_0000;
    const CLONE_CHILD_CLEARTID: u64 = 0x20_0000;
    const CLONE_CHILD_SETTID: u64 = 0x100_0000;

    if flags & CLONE_THREAD == 0 {
        return fork_impl(stack, (flags & CLONE_SETTLS != 0).then_some(tls), flags & CLONE_VFORK != 0);
    }
    if flags & CLONE_VM == 0 || flags & CLONE_SIGHAND == 0 {
        return errno::EINVAL;
    }
    for (flag, addr) in [(CLONE_PARENT_SETTID, ptid), (CLONE_CHILD_SETTID, ctid), (CLONE_CHILD_CLEARTID, ctid)] {
        if flags & flag != 0 && validate_user_buffer(addr, 4).is_err() {
            return errno::EFAULT;
        }
    }

    let tf_ptr = current_tf_ptr();
    let mut child_tf = unsafe { *tf_ptr };
    child_tf.rax = 0;
    if stack != 0 {
        child_tf.rsp = stack;
    }
    // Like `fork`: the live registers, not what the last preemption stashed.
    let mut fpu_state = crate::process::fpu::default_state();
    unsafe { crate::process::fpu::save(&mut fpu_state); }
    let parent_fs_base = crate::process::scheduler::read_fs_base();

    let ((tgid, group_parent), address_space, files, parent_cwd, (parent_pgid, parent_sid, parent_ctty), parent_exe_name, parent_signals, (parent_comm, parent_cmdline, parent_creds)) = {
        let sched = crate::process::scheduler::local_scheduler();
        match sched.running_ref() {
            Some(proc) => ((proc.tgid, proc.parent_pid), proc.address_space.clone(), proc.files.clone(), proc.cwd.clone(), (proc.pgid, proc.sid, proc.ctty), proc.exe_name.clone(),
                (proc.signal_handlers, proc.sig_restart, proc.blocked_signals, proc.sig_extra), (proc.name, proc.cmdline.clone(), proc.creds.clone())),
            None => return errno::ESRCH,
        }
    };

    let owned_stack_vma = if stack == 0 {
        None
    } else {
        address_space.find_vma(stack).and_then(|vma| {
            (vma.kind == crate::memory::vma::VmaKind::Huge2M).then_some((vma.start, vma.size_pages))
        })
    };

    let kernel_stack = crate::init::processes::allocate_kernel_stack();

    let mut scheduler = crate::process::irq_guard::SchedGuard::lock();
    let pid = scheduler.allocate_pid();
    let tid_bytes = (pid.0 as u32).to_ne_bytes();
    // Before the thread exists for the scheduler: it must find its tid in place. (Lock order: scheduler → address space.)
    for (flag, addr) in [(CLONE_PARENT_SETTID, ptid), (CLONE_CHILD_SETTID, ctid)] {
        if flags & flag != 0 && unsafe { address_space.copy_to_user(addr, &tid_bytes) } != 4 {
            return errno::EFAULT;
        }
    }

    let mut thread = alloc::boxed::Box::new(
        crate::process::Process::new_thread(
            pid, tgid, group_parent,
            x86_64::VirtAddr::new(child_tf.rip), x86_64::VirtAddr::new(child_tf.rsp),
            kernel_stack, address_space, files, owned_stack_vma, parent_cwd, parent_pgid, parent_sid, parent_exe_name,
        )
    );
    *thread.trapframe = child_tf;
    thread.fpu_state = alloc::boxed::Box::new(fpu_state);
    thread.fs_base = if flags & CLONE_SETTLS != 0 { tls } else { parent_fs_base };
    if flags & CLONE_CHILD_CLEARTID != 0 {
        thread.clear_child_tid = ctid;
    }
    // A copy, not Linux's shared table (CLONE_SIGHAND): enough that a
    // signal landing on a new thread runs the handler its process
    // installed, rather than the default action.
    (thread.signal_handlers, thread.sig_restart, thread.blocked_signals, thread.sig_extra) = parent_signals;
    thread.ctty = parent_ctty;
    // A thread starts with its creator's name and command line, as on Linux.
    thread.name = parent_comm;
    thread.cmdline = parent_cmdline;
    thread.creds = parent_creds;
    scheduler.add_process(thread);
    pid.0 as SyscallResult
}

/// Max entries `sys_exec` will read out of an argv/envp array — past this,
/// exec fails with `E2BIG` rather than silently truncating (a program
/// silently missing half its arguments is a worse failure mode than a
/// loud one).
const MAX_EXEC_ARGS: usize = 64;
/// Max bytes per argv/envp string (including the caller's NUL, which isn't
/// copied). Same 255-byte cap `read_user_str` already uses for paths.
const MAX_EXEC_ARG_LEN: usize = 255;

/// Read a NULL-terminated array of C-string pointers (`char *const argv[]`)
/// out of the *calling* process's user memory into owned kernel buffers.
///
/// Must run and finish *before* `load_elf`/the address-space swap: once
/// `sys_exec` replaces `proc.address_space`, the caller's old user pointers
/// (including `ptr` itself) stop being valid to dereference.
///
/// `ptr == 0` means "no array" → returns empty, so old callers that never
/// learned about this ABI extension (e.g. the Rust userspace's original
/// `exec(name)`, which still passes 0 for argv/envp) keep working exactly
/// as before (argc=0).
fn read_user_str_array(ptr: usize) -> Result<alloc::vec::Vec<alloc::vec::Vec<u8>>, i64> {
    use alloc::vec::Vec;
    let mut out = Vec::new();
    if ptr == 0 {
        return Ok(out);
    }
    for i in 0..MAX_EXEC_ARGS {
        let slot_addr = ptr as u64 + (i as u64) * 8;
        validate_user_buffer(slot_addr, 8)?;
        let str_ptr = unsafe { *(slot_addr as *const u64) };
        if str_ptr == 0 {
            return Ok(out); // NULL terminator reached
        }
        validate_user_buffer(str_ptr, 1)?;
        let s = unsafe {
            let p = str_ptr as *const u8;
            let mut len = 0usize;
            while len < MAX_EXEC_ARG_LEN {
                if *p.add(len) == 0 { break; }
                len += 1;
            }
            core::slice::from_raw_parts(p, len).to_vec()
        };
        out.push(s);
    }
    // MAX_EXEC_ARGS entries consumed and still no NULL terminator in sight.
    Err(errno::E2BIG)
}

pub(super) fn sys_exec(path_ptr: usize, argv_ptr: usize, envp_ptr: usize) -> SyscallResult {
    if let Err(e) = validate_user_buffer(path_ptr as u64, 64) {
        return e;
    }

    // Read the program name from user memory (process page table still active)
    let name_bytes = unsafe {
        let ptr = path_ptr as *const u8;
        let mut len = 0usize;
        while len < 64 {
            if *ptr.add(len) == 0 { break; }
            len += 1;
        }
        core::slice::from_raw_parts(ptr, len)
    };

    let name = match core::str::from_utf8(name_bytes) {
        Ok(s) => s,
        Err(_) => return errno::EINVAL,
    };

    // Both must be read out of the caller's memory now — load_elf below
    // swaps in a fresh address space, after which argv_ptr/envp_ptr (and
    // any pointers *inside* those arrays) no longer resolve to anything
    // meaningful in this process's page table.
    let argv = match read_user_str_array(argv_ptr) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let envp = match read_user_str_array(envp_ptr) {
        Ok(v) => v,
        Err(e) => return e,
    };

    serial_println!("sys_exec: loading '{}' (argc={}, envc={})", name, argv.len(), envp.len());

    // `comm` is the basename of the path as the caller named it, before
    // symlinks are followed — Linux's `kbasename(bprm->filename)`. BusyBox
    // applets are symlinks to one binary: `/tmp/bin/cat` must show as
    // `cat`, not `busybox`. (`/proc/<pid>/exe` is the resolved path.)
    let comm = alloc::string::String::from(&name[name.rfind('/').map_or(0, |i| i + 1)..]);
    // `/proc/<pid>/cmdline`: every argument followed by its NUL, as Linux
    // lays out the argument area. A call without argv (the legacy
    // `exec(name)`) gets the name as its only argument.
    let cmdline: alloc::sync::Arc<[u8]> = {
        let mut c = alloc::vec::Vec::new();
        if argv.is_empty() {
            c.extend_from_slice(name.as_bytes());
            c.push(0);
        }
        for a in &argv {
            c.extend_from_slice(a.strip_suffix(&[0]).unwrap_or(a));
            c.push(0);
        }
        c.into()
    };
    let resolved_path = match resolve_exec_path(name) {
        Ok(p) => p,
        Err(e) => {
            serial_println!("sys_exec: '{}' not found", name);
            return e;
        }
    };
    serial_println!("sys_exec: resolved '{}' -> '{}'", name, resolved_path);

    let elf_owned = {
        let mut handle = match crate::fs::vfs::open(&resolved_path, crate::fs::types::OpenFlags::RDONLY) {
            Ok(h) => h,
            Err(e) => {
                serial_println!("sys_exec: '{}' not found", name);
                return e.as_i64();
            }
        };
        let mut buf = alloc::vec::Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match handle.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(_) => return errno::EIO,
            }
        }
        buf
    };

    // Load ELF without any lock — may take time and allocates frames.
    // Every process's stack starts small and grows on demand (see
    // memory::elf_loader::STACK_PAGES/STACK_MAX_PAGES) — no per-program
    // size to pass in.
    let loaded = match unsafe { crate::memory::elf_loader::load_elf(&elf_owned, 0, &argv, &envp) } {
        Ok(l) => l,
        Err(e) => {
            serial_println!("sys_exec: load_elf failed: {}", e);
            return if e == "ELF loader: argv/envp too large for the initial stack page" {
                errno::E2BIG
            } else {
                errno::ENOMEM
            };
        }
    };
    // `load_elf` copies whatever it needs from `elf_owned` into the new
    // address space's own frames (`LoadedElf` holds no borrow into it) —
    // drop it explicitly here rather than let it fall out of scope
    // naturally. This function ends by jumping into the new process via
    // `jump_to_user`/`jump_to_trapframe` (`-> !`, a raw `iretq`, not a
    // normal Rust return), so any local still alive at that point never
    // gets its destructor run — this Vec (the whole ELF file's bytes,
    // rounded up to the Buddy allocator's nearest order — ~1 MiB for
    // busybox) was leaking on *every single successful exec()*, exactly
    // the ~1 MiB-per-fork+exec leak that hung the kernel under heavier
    // busybox use this session (confirmed via free_bytes() bracketing a
    // single fork/exec/wait/reap cycle — see the now-removed [MEM-DEBUG]
    // instrumentation this fix was diagnosed with).
    drop(elf_owned);

    // `execve` closes the close-on-exec descriptors — only now, once the image has loaded: a failed exec must leave them open.
    // The handles are taken out under the table's lock and closed after it, with no scheduler lock held (a `Drop` may take it).
    // The interrupt guard below is dropped at the end of this block, before exec's own `cli`.
    let files = crate::process::irq_guard::SchedGuard::lock().running_ref().map(|p| p.files.clone());
    if let Some(files) = files {
        // IF=0 for the closes: a pipe or socket end wakes its peer through `local_scheduler()`, which insists on it.
        let _irq = crate::process::irq_guard::InterruptGuard::new();
        let closing = files.lock().take_cloexec();
        for mut h in closing {
            let _ = h.close();
        }
    }

    crate::ktrace!(crate::debug::SCHED, "exec: load_elf done, going cli");
    // `_irq` is deliberately never dropped on the success path — this
    // function always ends in `jump_to_user` (`-> !`), so interrupts
    // intentionally stay off across that jump; see `sys_read`'s WouldBlock
    // arm for the same reasoning. On the `None` early-return below, the
    // guard drops automatically and does re-enable interrupts, same as
    // before this was RAII.
    let _irq = crate::process::irq_guard::InterruptGuard::new();
    crate::ktrace!(crate::debug::SCHED, "exec: cli done, taking scheduler lock");

    let next_tf = {
        let mut scheduler = crate::process::scheduler::local_scheduler();
        crate::ktrace!(crate::debug::SCHED, "exec: scheduler locked, swapping address space");
        match scheduler.running_mut() {
            Some(proc) => {
                let vfork_parent = proc.vfork_parent.take().map(|p| (p, proc.pid.0));
                // Rename to the new image's basename, the way real
                // `execve()` resets `comm`. Without it a process kept
                // whatever it was called when it was created — and since
                // the only way to reach `exec` is through `fork()`, which
                // names every child "child", that meant *every* program
                // this kernel ever ran showed up as "child" in `ps`, in
                // `top`, in the scheduler's traces and in the on-screen
                // kill notice. Set before `resolved_path` is
                // moved into `exe_name` just below (a borrow of
                // `proc.exe_name` afterwards would collide with
                // `set_name`'s `&mut self`). From the name as given, not
                // `resolved_path`: see `comm` above.
                proc.set_name(&comm);
                proc.exe_name = resolved_path;
                proc.cmdline = cmdline;
                // POSIX `execve`: a caught signal goes back to SIG_DFL (its
                // handler's address means nothing in the new image), an
                // ignored one stays ignored, and the mask and pending set
                // carry over. Keeping the handlers ran the old image's code
                // at that address in the new one: ash's `sh -c 'true &
                // sleep 1'` execs `sleep` in place, `true`'s SIGCHLD then
                // jumped into ash's handler inside `sleep`, before ash's
                // globals existed — SIGSEGV writing to 0x45, every time.
                for (sig, action) in proc.signal_handlers.iter_mut().enumerate() {
                    if let crate::process::SignalAction::Handler(_) = action {
                        *action = crate::process::SignalAction::Default;
                        proc.sig_extra[sig] = crate::process::signal::SigExtra::NONE;
                    }
                }
                proc.sig_restart = 0;
                // The alternate stack lives in the old image too.
                proc.altstack = crate::process::signal::AltStack::NONE;
                // `set_tid_address`'s pointer belongs to the old image.
                proc.clear_child_tid = 0;
                crate::ktrace!(crate::debug::SCHED, "exec: dropping old AS");
                // Replace address space with freshly loaded one. This drops
                // this Process's Arc reference to whatever it had before —
                // if that was a shared (thread) address space, the actual
                // page table/pages are only freed once every other thread
                // sharing it has also exited (Arc refcount reaches 0).
                //
                // Not dropped *here*, though: this CPU's CR3 still points at
                // the old page table, and a PML4 freed while loaded is a
                // frame another CPU can allocate and overwrite at once —
                // this CPU then fetches its next instruction through
                // garbage (found by stage 7 of docs/smp/smp-plan.md: a
                // silent triple fault under concurrent execs). Kept alive
                // until the new one is active, below.
                let old_space = core::mem::replace(
                    &mut proc.address_space,
                    alloc::sync::Arc::new(loaded.address_space),
                );
                crate::ktrace!(crate::debug::SCHED, "exec: new AS in place");
                crate::debug::inc_execs();

                // The page-fault fast path caches a raw pointer to the
                // process's AddressSpace (see scheduler::refresh_current_fast);
                // it must be re-synced now that the field above points at a
                // brand-new Arc allocation, or every fault after this exec()
                // will look up VMAs in the stale pre-exec address space.
                crate::process::scheduler::refresh_current_fast(proc);

                // Reset TrapFrame to new entry point
                proc.trapframe.rip    = loaded.entry_point.as_u64();
                proc.trapframe.rsp    = loaded.user_stack_top.as_u64();
                proc.trapframe.cs     = 0x23;
                proc.trapframe.ss     = 0x1b;
                proc.trapframe.rflags = 0x202; // IF, plus bit 1 (reserved, always 1)
                proc.trapframe.rax = 0; proc.trapframe.rbx = 0; proc.trapframe.rcx = 0;
                proc.trapframe.rdx = 0; proc.trapframe.rsi = 0; proc.trapframe.rdi = 0;
                proc.trapframe.rbp = 0; proc.trapframe.r8  = 0; proc.trapframe.r9  = 0;
                proc.trapframe.r10 = 0; proc.trapframe.r11 = 0; proc.trapframe.r12 = 0;
                proc.trapframe.r13 = 0; proc.trapframe.r14 = 0; proc.trapframe.r15 = 0;

                // Reset TLS — the new image will set it via arch_prctl if needed.
                proc.fs_base = 0;
                unsafe {
                    core::arch::asm!(
                        "wrmsr",
                        in("ecx") 0xC000_0100u32,
                        in("eax") 0u32,
                        in("edx") 0u32,
                        options(nostack, preserves_flags),
                    );
                }

                // Reset FPU/SSE state too — real `execve()` resets it, and
                // there's no reason for a brand-new program image to see
                // whatever XMM/x87 garbage the previous one left behind.
                // Written directly to live hardware (like the MSR write
                // above): this continues running on the same CPU without
                // an intervening context switch, so `proc.fpu_state`
                // itself would never get consulted before the exec'd
                // program runs — but keep it in sync anyway so the next
                // real context switch (which reads `proc.fpu_state`, not
                // hardware) doesn't stash a stale pre-exec image over it.
                *proc.fpu_state = crate::process::fpu::default_state();
                unsafe { crate::process::fpu::restore(&proc.fpu_state); }

                crate::ktrace!(crate::debug::SCHED, "exec: activating new CR3");
                unsafe { proc.address_space.activate(); }
                drop(old_space);
                crate::ktrace!(crate::debug::SCHED, "exec: CR3 active, jumping to entry={:#x}", proc.trapframe.rip);
                // This direct field-by-field rewrite (above) discards
                // whatever trapframe content this process had before exec —
                // and the very next thing this function does is jump
                // straight into it via `jump_to_user`, with no intervening
                // scheduler SAVE/RESUME. Tag it as a save+resume pair for
                // `tf_note_save`/`tf_note_resume`'s bookkeeping (see
                // scheduler.rs's 2026-07-25 hang-hunt section) so this
                // process's `tf_seq`/`tf_awaiting_resume` stay consistent
                // for whatever scheduler SAVE/RESUME comes next — e.g. if a
                // timer interrupt preempts mid-ELF-load (interrupts are
                // enabled during that phase, only `cli`'d right before this
                // block), the resulting SAVE/RESUME pair around *that*
                // preemption already ran before this point and is unrelated.
                crate::process::scheduler::tf_note_save(proc, "sys_exec");
                crate::process::scheduler::tf_note_resume(proc, "sys_exec");
                (&*proc.trapframe as *const TrapFrame, vfork_parent)
            }
            None => return errno::ESRCH,
        }
    };
    let (next_tf, vfork_parent) = next_tf;
    // The image is replaced: a `vfork` parent waiting for that goes on (Linux releases it at exec as well as at exit).
    if let Some((parent, child)) = vfork_parent {
        crate::process::scheduler::local_scheduler().wake_with_retval(parent, child as u64);
    }

    crate::ktrace!(crate::debug::SCHED, "exec: jump_to_trapframe");
    // Jump to the new program — never returns
    unsafe { crate::process::trapframe::jump_to_user(next_tf) }
}

/// Resolves `name` (whatever `exec()`'s caller passed as the program path —
/// a bareword, a `./`-relative path, or an absolute path like `/bin/ls`) to
/// its canonical, fully symlink-resolved absolute path — real VFS
/// traversal, not a special-cased string match.
///
/// This kernel has no real `/bin`, `/usr/bin`, etc. as *separate mounts* —
/// `/bin` is a real subdirectory of the single `/` `InitramfsFs` mount (see
/// `fs::mod`'s doc comment and `fs::initramfs`) — so a real shell's own
/// `$PATH` search (e.g. BusyBox `ash` with `FEATURE_SH_STANDALONE`, trying
/// `/bin/hello` before giving up) resolves correctly through the ordinary
/// VFS mount table, same as an explicit `./ls`. A *bareword* like `hello`
/// only resolves if the caller already searched `$PATH` itself — the
/// kernel does not do `$PATH` search here; `ash` is the only shell now
/// (see `userspace/src/bin/shell.rs`, a minimal `fork`+exec+respawn init
/// loop, not an interactive shell) and it already does this correctly.
/// `/proc/self/exe` — which `ash` re-execs for any applet that
/// isn't `NOFORK`/`NOEXEC` (most of them; `echo`/`ls` are exceptions) —
/// is just another symlink under this same mechanism now (see
/// `fs::procfs::ProcExeInode`, backed by `Process::exe_name`): no
/// special-casing left here at all, any symlink anywhere gets followed
/// the same way.
///
/// Manual loop (not `fs::vfs::resolve`'s own internal following) because
/// the *canonical path string* is what needs to survive to become the new
/// `Process::exe_name` — an `Inode` alone doesn't carry the path that
/// reached it.
fn resolve_exec_path(name: &str) -> Result<alloc::string::String, i64> {
    let mut path = resolve_path(name);
    for _ in 0..8 {
        let inode = crate::fs::vfs::resolve_no_follow(&path).map_err(|e| e.as_i64())?;
        if inode.file_type() != crate::fs::types::FileType::Symlink {
            return Ok(path);
        }
        let target = inode.readlink().map_err(|e| e.as_i64())?;
        path = if target.starts_with('/') {
            target
        } else {
            crate::fs::vfs::normalize_path(&path, &target)
        };
    }
    Err(errno::ELOOP)
}

/// waitpid(61): long waitpid(pid_t pid, int *status, int options)
///
/// `pid`: `>0` = exactly that pid; `0` = any child in the caller's own
/// process group; `-1` = any child at all; `<-1` = any child in group
/// `-pid` — the real POSIX overloads (this kernel used to accept only a
/// single exact pid). Only actual children of the caller ever match
/// (checked via `parent_pid`), same as real `waitpid()`; if the target
/// selector matches no live-or-zombie child of the caller at all, this
/// returns `ECHILD` instead of blocking forever.
///
/// `options` (Linux's values): `WNOHANG` (1) returns 0 immediately instead of blocking when
/// nothing is reapable yet. `WUNTRACED` (2) also matches a `Stopped` child
/// (job control), reporting it once (see `Process::stop_reported`) without
/// removing it from the wait queue — a later real exit, or another
/// stop/continue cycle, can still be observed. `WCONTINUED` (8) reports a child resumed by `SIGCONT` once, with status
/// `0xffff` (`Process::continued_pending`).
pub(super) fn sys_waitpid(pid_arg: i64, status_ptr: usize, options: i32) -> SyscallResult {
    const WNOHANG: i32 = 1;
    const WUNTRACED: i32 = 2;
    const WCONTINUED: i32 = 8;

    if status_ptr != 0 {
        if let Err(e) = validate_user_buffer(status_ptr as u64, 4) { return e; }
    }

    let tf_ptr = current_tf_ptr();

    let irq = crate::process::irq_guard::InterruptGuard::new();

    enum Outcome {
        Return(SyscallResult),
        Block(*const TrapFrame),
    }

    // Everything that decides Return-vs-Block must finish inside this one
    // locked block, so `sti` never runs while `scheduler` is still held —
    // a guard alive past `sti` opens a window where a timer tick could land
    // inside the critical section and spin on `local_scheduler()` forever,
    // since the only thing that could release it (this call, mid-return)
    // can't resume until that spin gives up, which it never does. Same bug
    // class as `sys_kill`'s doc comment describes.
    let outcome = {
        let mut scheduler = crate::process::scheduler::local_scheduler();

        let caller_pid = scheduler.running_ref().map(|p| crate::process::Pid(p.tgid));
        let caller_pgid = scheduler.running_ref().map(|p| p.pgid).unwrap_or(0);

        let target = match pid_arg {
            p if p > 0 => crate::process::WaitTarget::Pid(p as usize),
            0 => crate::process::WaitTarget::Pgid(caller_pgid),
            -1 => crate::process::WaitTarget::AnyChild,
            p => crate::process::WaitTarget::Pgid((-p) as u32),
        };

        let zombie_pos = scheduler.wait_queue().iter().position(|p| {
            matches!(p.state, crate::process::ProcessState::Zombie)
                && p.parent_pid == caller_pid
                && target.matches(p.pid.0, p.pgid)
        });
        let stopped_pos = if zombie_pos.is_none() && options & WUNTRACED != 0 {
            scheduler.wait_queue().iter().position(|p| {
                matches!(p.state, crate::process::ProcessState::Stopped)
                    && !p.stop_reported
                    && p.parent_pid == caller_pid
                    && target.matches(p.pid.0, p.pgid)
            })
        } else {
            None
        };

        // A child resumed by SIGCONT lives in a run queue (or on a CPU), not in `wait_queue`.
        let continued_pid = if zombie_pos.is_none() && stopped_pos.is_none() && options & WCONTINUED != 0 {
            scheduler.iter_all()
                .find(|p| p.continued_pending && !p.is_thread && p.parent_pid == caller_pid && target.matches(p.pid.0, p.pgid))
                .map(|p| p.pid.0)
        } else {
            None
        };

        if let Some(pos) = zombie_pos {
            // Safe to write the status straight into `status_ptr` right
            // here: we're running on the *parent's* stack in the parent's
            // own address space (this is its own waitpid() syscall). The
            // zombie's kernel stack is another matter since stage 7: the
            // child's `sys_exit` may still be unwinding on it on another
            // CPU, so it goes through the same deferred free as a thread's
            // (freed by a later tick once no CPU is on it).
            let proc = scheduler.wait_queue_mut().remove(pos).unwrap();
            let status = proc.wait_status_word();
            let pid = proc.pid.0;
            scheduler.credit_reaped(&proc);
            scheduler.defer_stack_free(proc.kernel_stack);
            crate::debug::inc_reaps();
            if status_ptr != 0 {
                // write_unaligned, not write: `validate_user_buffer` only
                // checks that this pointer falls inside the user canonical
                // range, not that it's actually 4-byte aligned or even
                // mapped — a buggy/malicious caller can hand us anything
                // that passes that check. `write` panics via Rust's
                // alignment UB precondition on a misaligned pointer, which
                // takes the whole kernel down; `write_unaligned` doesn't
                // care about alignment and degrades to (at worst) a page
                // fault the demand-paging handler can still route sanely.
                unsafe { core::ptr::write_unaligned(status_ptr as *mut i32, status); }
            }
            Outcome::Return(pid as SyscallResult)
        } else if let Some(pos) = stopped_pos {
            let status = scheduler.wait_queue()[pos].stop_status_word();
            let pid = scheduler.wait_queue()[pos].pid.0;
            scheduler.wait_queue_mut()[pos].stop_reported = true;
            if status_ptr != 0 {
                // write_unaligned: see the zombie_pos branch above for why.
                unsafe { core::ptr::write_unaligned(status_ptr as *mut i32, status); }
            }
            Outcome::Return(pid as SyscallResult)
        } else if let Some(pid) = continued_pid {
            if let Some(c) = scheduler.find_process_mut(pid) {
                c.continued_pending = false;
            }
            if status_ptr != 0 {
                unsafe { core::ptr::write_unaligned(status_ptr as *mut i32, 0xffff); }
            }
            Outcome::Return(pid as SyscallResult)
        } else {
            // ECHILD before WNOHANG, as in Linux: "no such child" is an
            // error even when not blocking. BusyBox ash's `wait` keeps
            // reaping with WNOHANG until it sees ECHILD — a 0 here sent it
            // back to sigsuspend with nothing left to wake it.
            let has_any = scheduler.iter_all()
                .any(|p| !p.is_thread && p.parent_pid == caller_pid && target.matches(p.pid.0, p.pgid));
            if !has_any {
                Outcome::Return(errno::ECHILD)
            } else if options & WNOHANG != 0 {
                Outcome::Return(0)
            } else {
                // Not reapable yet — record what we are waiting for (and
                // where to eventually write its status — see `Process::
                // waiting_status_ptr`'s doc comment for why that write can't
                // happen from `notify_child_death`/`notify_child_stopped`
                // directly) in the Process struct (supports multiple
                // concurrent waitpid callers: shell + ipc_ping etc.) and
                // block until a matching child exits or (if WUNTRACED) stops.
                if let Some(proc) = scheduler.running_mut() {
                    proc.waiting_for = Some(target);
                    proc.waiting_options = options;
                    proc.waiting_status_ptr = status_ptr;
                }
                // Interruptible: a signal clears `waiting_for` instead
                // (`process::wait`).
                let ret_rip = unsafe { (*tf_ptr).rip };
                Outcome::Block(scheduler.block_current(tf_ptr, crate::process::wait::Wait {
                    how: crate::process::wait::Interruptible::WaitPid,
                    nr: 61,
                    ret_rip,
                    policy: crate::process::wait::RestartPolicy::SaRestart,
                    rem: None,
                }))
            }
        }
    };

    crate::ktrace!(
        crate::debug::PROC,
        "waitpid: PID {} pid={} options={:#x} -> {}{}",
        crate::process::scheduler::current_pid_fast(), pid_arg, options,
        match outcome { Outcome::Return(v) => v, Outcome::Block(_) => 0 },
        if matches!(outcome, Outcome::Block(_)) { " (block)" } else { "" }
    );
    match outcome {
        Outcome::Return(v) => {
            drop(irq);
            v
        }
        // `irq` deliberately never dropped here — diverges via
        // `jump_to_user` (`-> !`); interrupts intentionally stay off across
        // the jump, same reasoning as `sys_read`'s WouldBlock arm.
        Outcome::Block(next_tf) => unsafe { crate::process::trapframe::jump_to_user(next_tf) },
    }
}


// ============================================================================
// kill(62) / setpgid(109) / getpgid(121) / setsid(112)
//
// (sigaction/sigprocmask/sigreturn now live in `syscall::signal` — the
// original file grouped kill() in with those under one "SIGNALS" banner,
// but it belongs with the rest of process control here instead.)
// ============================================================================

/// kill(62): long kill(pid_t pid, int sig)
///
/// `pid > 0`: single target, as before. `pid == 0`: every process in the
/// caller's own process group. `pid < -1`: every process in group `-pid`.
/// `pid == -1` (broadcast to every signalable process) is not supported —
/// this kernel has no permission model to bound it, so it just returns
/// `EINVAL` rather than doing something surprising.
///
/// Queues the signal, then `Scheduler::interrupt_blocked` ends the wait of
/// any target Blocked in an interruptible one (`process::wait`: sleeps,
/// pipes, sockets, futex, poll, waitpid, stdin, sigsuspend) that the
/// signal would act on. `SIGCONT` against a `Stopped` target also
/// force-wakes it via `wake_stopped` — that's the *only* wakeup a stopped
/// process ever gets (see `Process::state`'s `Stopped` doc comment) — in
/// addition to (not instead of) the normal `queue_signal`, so a handler
/// installed for SIGCONT still runs once the process resumes.
pub(super) fn sys_kill(target_pid: i64, sig: u32) -> SyscallResult {
    kill_impl(target_pid, sig, false, None)
}

/// `kill`, `tkill` and `tgkill`. `pid > 0` names a thread group for `kill` (any of its threads' ids will do): the signal goes
/// to the thread that can take it (`Scheduler::resolve_signal_target`). With `exact` (`tkill`/`tgkill`) it names one thread;
/// `in_group` makes `tgkill` fail with ESRCH when that thread belongs to another group.
fn kill_impl(target_pid: i64, sig: u32, exact: bool, in_group: Option<usize>) -> SyscallResult {
    if sig as usize >= crate::process::signal::NUM_SIGNALS {
        return errno::EINVAL;
    }
    if sig == 0 {
        // POSIX: no signal is sent, only the checks are made — does the target exist? (BusyBox's `timeout` probes with it.)
        if target_pid > 0 {
            return with_scheduler(|sched| {
                if sched.resolve_signal_target(target_pid as usize, sig, exact, in_group).is_some() { 0 } else { errno::ESRCH }
            });
        }
        return 0;
    }
    if target_pid == -1 {
        return errno::EINVAL;
    }

    // `SchedGuard` guarantees the scheduler lock and interrupts both drop
    // together at this closure's end, on every path — holding a
    // spin::Mutex guard past `sti` opens a window where a timer tick can
    // land inside it and spin forever on `local_scheduler()`, since the
    // interrupted code (the only thing that could ever release the lock)
    // can't resume until that same spin gives up — it never does. An
    // earlier hand-written version of this function held the lock for the
    // whole function body instead of scoping it tightly and deadlocked the
    // first time a signal landed on a Ready (not self, not Blocked) target.
    with_scheduler(|sched| {
        // `si_code`/`si_pid` a handler will see: `SI_USER` (`kill`) or `SI_TKILL` (`tkill`, `tgkill`, so `raise`) from this group.
        let sender = sched.running_ref().map_or(0, |p| p.tgid);
        let origin = if exact { SigOrigin::tkill(sender) } else { SigOrigin::user(sender) };
        if target_pid == 0 || target_pid < -1 {
            let pgid = if target_pid == 0 {
                sched.running_ref().map(|p| p.pgid).unwrap_or(0)
            } else {
                (-target_pid) as u32
            };
            sched.signal_group_from(pgid, sig, origin);
            0
        } else {
            let Some(target_pid) = sched.resolve_signal_target(target_pid as usize, sig, exact, in_group) else {
                return errno::ESRCH;
            };
            let is_self = sched.current_pid().map(|p| p.0) == Some(target_pid);
            if is_self {
                if let Some(proc) = sched.running_mut() {
                    crate::process::signal::queue_signal_from(proc, sig, origin);
                }
                0
            } else {
                // Queue, then interrupt the target's wait if it is in an
                // interruptible one. It used to be queue-only: a generic
                // `wake()` resumed the target with a stale `rax` and left its
                // registration behind for the real wakeup to act on (a pipe
                // reader woken this way read back "" — `mlibc_signal_test`),
                // so a SIGKILL to `sleep 100` waited out the 100 s. A
                // wait's one-shot cell now decides which of the signal and
                // the waker ends it (`process::wait`).
                if crate::process::signal::resumes_stopped(sig) {
                    sched.wake_stopped(target_pid, sig);
                }
                match sched.find_process_mut(target_pid) {
                    Some(proc) => crate::process::signal::queue_signal_from(proc, sig, origin),
                    None => return errno::ESRCH,
                }
                sched.interrupt_blocked();
                0
            }
        }
    })
}

// ── setpgid(109) / getpgid(121) / getsid(124) / setsid(112) ───────────────────────────────

/// setpgid(109): int setpgid(pid_t pid, pid_t pgid)
///
/// `pid == 0` means "the caller"; `pgid == 0` means "use `pid`'s own pid as
/// its new group id" (become a group leader). POSIX's rules, now that
/// sessions exist (phase 3.2 of `docs/gui/gui-plan.md`):
///
/// - the target is the caller or one of its children (`ESRCH` otherwise);
/// - a child must be in the caller's session, and no session leader may
///   change group (`EPERM`);
/// - joining an existing group needs that group to be in the caller's
///   session (`EPERM`).
///
/// Not enforced: `EACCES` for a child that has already `exec`'d (nothing
/// records that). ash calls `setpgid` from both sides of a `fork` and
/// ignores the loser's error, so allowing the late call changes nothing.
pub(super) fn sys_setpgid(pid: i64, pgid: i64) -> SyscallResult {
    if pid < 0 || pgid < 0 {
        return errno::EINVAL;
    }

    with_scheduler(|sched| {
        let (caller_pid, caller_sid) = match sched.running_ref() {
            Some(p) => (p.tgid, p.sid),
            None => return errno::ESRCH,
        };
        let target_pid = if pid == 0 { caller_pid } else { pid as usize };
        let new_pgid = if pgid == 0 { target_pid as u32 } else { pgid as u32 };

        let (target_sid, target_parent) = if target_pid == caller_pid {
            (caller_sid, None)
        } else {
            match sched.iter_all().find(|p| p.pid.0 == target_pid) {
                Some(p) => (p.sid, p.parent_pid.map(|pp| pp.0)),
                None => return errno::ESRCH,
            }
        };
        if target_pid != caller_pid && target_parent != Some(caller_pid) {
            return errno::ESRCH;
        }
        if target_sid != caller_sid || target_sid == target_pid as u32 {
            return errno::EPERM;
        }
        if new_pgid != target_pid as u32
            && !sched.iter_all().any(|p| p.pgid == new_pgid && p.sid == caller_sid)
        {
            return errno::EPERM;
        }

        let target = if target_pid == caller_pid {
            sched.running_mut()
        } else {
            sched.find_process_mut(target_pid)
        };
        match target {
            Some(proc) => { proc.pgid = new_pgid; 0 }
            None => errno::ESRCH,
        }
    })
}

/// getpgid(121): pid_t getpgid(pid_t pid)
pub(super) fn sys_getpgid(pid: i64) -> SyscallResult {
    lookup_id(pid, |p| p.pgid)
}

/// getsid(124): pid_t getsid(pid_t pid)
pub(super) fn sys_getsid(pid: i64) -> SyscallResult {
    lookup_id(pid, |p| p.sid)
}

/// `pid` (0 = the caller) → `field` of it, or `ESRCH`.
fn lookup_id(pid: i64, field: fn(&crate::process::Process) -> u32) -> SyscallResult {
    if pid < 0 {
        return errno::EINVAL;
    }

    with_scheduler(|sched| {
        let caller_pid = sched.running_ref().map(|p| p.tgid).unwrap_or(0);
        let target_pid = if pid == 0 { caller_pid } else { pid as usize };

        if target_pid == caller_pid {
            sched.running_ref().map(|p| field(p) as SyscallResult).unwrap_or(errno::ESRCH)
        } else {
            sched.iter_all().find(|p| p.pid.0 == target_pid)
                .map(|p| field(p) as SyscallResult).unwrap_or(errno::ESRCH)
        }
    })
}

/// setsid(112): pid_t setsid(void)
///
/// A new session and a new process group, both named after the caller,
/// with no controlling terminal. `EPERM` if a process group with the
/// caller's pid already exists — the caller leads one, or one it led
/// outlived it — since the new group could not be told apart from it.
/// The caller loses its controlling terminal.
/// Until 2026-09-25 this only made the caller a group leader, and no
/// session id existed at all.
pub(super) fn sys_setsid() -> SyscallResult {
    with_scheduler(|sched| {
        let pid = match sched.running_ref() {
            Some(p) => p.pid.0 as u32,
            None => return errno::ESRCH,
        };
        if sched.iter_all().any(|p| p.pgid == pid) {
            return errno::EPERM;
        }
        match sched.running_mut() {
            Some(proc) => {
                proc.pgid = pid;
                proc.sid = pid;
                proc.ctty = None;
                pid as SyscallResult
            }
            None => errno::ESRCH,
        }
    })
}

