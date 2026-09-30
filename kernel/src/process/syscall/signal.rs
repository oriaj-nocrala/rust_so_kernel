// kernel/src/process/syscall/signal.rs
//
// sigaction(13) / sigprocmask(14) / sigreturn(15) / rt_sigsuspend(130).

use crate::process::TrapFrame;
use super::{errno, SyscallResult, with_current_process, validate_user_buffer, current_tf_ptr};

const SIG_DFL: u64 = 0;
const SIG_IGN: u64 = 1;

/// rt_sigaction(13): `int rt_sigaction(int sig, const struct kernel_sigaction *act, struct kernel_sigaction *oldact, size_t sigsetsize)`
///
/// Linux's layout: `{ handler: u64 @0, flags: u64 @8, restorer: u64 @16, mask: u64 @24 }`, `sigsetsize` 8, `mask` in Linux's
/// `sigset_t` layout (bit N-1 = signal N). `SA_SIGINFO`, `SA_RESTORER`, `SA_ONSTACK`, `SA_RESTART`, `SA_NODEFER`,
/// `SA_RESETHAND` and `sa_mask` are stored (`SigExtra`) and act at delivery (`signal::push_signal_frame`); `SA_NOCLDSTOP` and
/// `SA_NOCLDWAIT` are stored and ignored.
pub(super) fn sys_sigaction(sig: u32, act_ptr: u64, oldact_ptr: u64, sigsetsize: u64) -> SyscallResult {
    use crate::process::signal::{mask_from_user, mask_to_user, SigExtra, SA_RESTART};
    const ACT_LEN: usize = 32;

    if sigsetsize != 8 {
        return errno::EINVAL;
    }
    if sig == 0 || sig as usize >= crate::process::signal::NUM_SIGNALS
        || (act_ptr != 0 && (sig == crate::process::signal::SIGKILL || sig == crate::process::signal::SIGSTOP)) {
        return errno::EINVAL;
    }
    if act_ptr != 0 {
        if let Err(e) = validate_user_buffer(act_ptr, ACT_LEN) { return e; }
    }
    if oldact_ptr != 0 {
        if let Err(e) = validate_user_buffer(oldact_ptr, ACT_LEN) { return e; }
    }
    // Read before taking the scheduler lock: a fault under it would deadlock.
    let new = if act_ptr != 0 {
        let word = |off: u64| unsafe { core::ptr::read_unaligned((act_ptr + off) as *const u64) };
        Some((word(0), word(8) as u32, word(16), mask_from_user(word(24))))
    } else {
        None
    };

    with_current_process(|proc| {
        let bit = 1u64 << sig;
        let old = proc.signal_handlers[sig as usize];
        let old_extra = proc.sig_extra[sig as usize];
        if let Some((handler_addr, flags, restorer, mask)) = new {
            proc.signal_handlers[sig as usize] = match handler_addr {
                SIG_DFL => crate::process::SignalAction::Default,
                SIG_IGN => crate::process::SignalAction::Ignore,
                addr => crate::process::SignalAction::Handler(addr),
            };
            proc.sig_extra[sig as usize] = SigExtra { flags, restorer, mask };
            // Decides whether a wait this signal interrupts is re-executed
            // or fails with EINTR (`process::wait`).
            if flags & SA_RESTART != 0 {
                proc.sig_restart |= bit;
            } else {
                proc.sig_restart &= !bit;
            }
        }
        if oldact_ptr != 0 {
            let old_addr = match old {
                crate::process::SignalAction::Default => SIG_DFL,
                crate::process::SignalAction::Ignore => SIG_IGN,
                crate::process::SignalAction::Handler(addr) => addr,
            };
            unsafe {
                let put = |off: u64, v: u64| core::ptr::write_unaligned((oldact_ptr + off) as *mut u64, v);
                put(0, old_addr);
                put(8, old_extra.flags as u64);
                put(16, old_extra.restorer);
                put(24, mask_to_user(old_extra.mask));
            }
        }
        0
    })
}

const SIG_BLOCK: i32 = 0;
const SIG_UNBLOCK: i32 = 1;
const SIG_SETMASK: i32 = 2;

/// rt_sigprocmask(14): int sigprocmask(int how, const sigset_t *set, sigset_t *oldset)
///
/// `sigset_t` is a single `u64` in Linux's layout (bit N-1 = signal N),
/// converted at this boundary — see `signal::mask_from_user`.
pub(super) fn sys_sigprocmask(how: i32, set_ptr: u64, oldset_ptr: u64) -> SyscallResult {
    if set_ptr != 0 {
        if let Err(e) = validate_user_buffer(set_ptr, 8) { return e; }
    }
    if oldset_ptr != 0 {
        if let Err(e) = validate_user_buffer(oldset_ptr, 8) { return e; }
    }

    with_current_process(|proc| {
        let old_mask = proc.blocked_signals;
        if set_ptr != 0 {
            let set = crate::process::signal::mask_from_user(unsafe { *(set_ptr as *const u64) });
            // SIGKILL can never be blocked.
            let set = set & !(1u64 << crate::process::signal::SIGKILL);
            proc.blocked_signals = match how {
                SIG_BLOCK => old_mask | set,
                SIG_UNBLOCK => old_mask & !set,
                SIG_SETMASK => set,
                _ => return errno::EINVAL,
            };
        }
        if oldset_ptr != 0 {
            unsafe { *(oldset_ptr as *mut u64) = crate::process::signal::mask_to_user(old_mask); }
        }
        0
    })
}

/// rt_sigreturn(15): only ever reached via the trampoline page a caught
/// signal redirects execution through — never called directly by normal
/// userspace code. Restores the TrapFrame `deliver_pending` saved before
/// redirecting to the handler; see `signal::pop_signal_frame` and
/// `signal.rs`'s module doc comment for the full frame layout/rationale.
pub(super) fn sys_sigreturn() -> SyscallResult {
    let tf_ptr = current_tf_ptr() as *mut TrapFrame;
    let user_rsp = unsafe { (*tf_ptr).rsp };
    crate::ktrace!(
        crate::debug::PROC,
        "sigreturn: entry PID {} tf={:p} user rsp={:#x} rip={:#x}",
        crate::process::scheduler::current_pid_fast(), tf_ptr, user_rsp, unsafe { (*tf_ptr).rip }
    );

    with_current_process(|proc| {
        unsafe { crate::process::signal::pop_signal_frame(proc, tf_ptr, user_rsp) };
        unsafe { (*tf_ptr).rax as i64 }
    })
}

/// rt_sigsuspend(130): int sigsuspend(const sigset_t *mask)
///
/// Replace the signal mask with `*mask` and sleep until a signal arrives
/// that runs a handler, terminates or stops the process; then put the old
/// mask back and return `EINTR` (it never returns anything else). The
/// swap, the check and the block happen under one hold of the scheduler
/// lock — the same lock every signal sender holds while queueing and then
/// calling `Scheduler::interrupt_blocked` — so a signal sent from another
/// CPU is either seen by the check or finds this process Blocked.
///
/// The old mask travels in `Process::saved_sigmask`: a handler frame
/// pushed on the way out saves it (so the handler's `sigreturn` restores
/// it), and if none is pushed `signal::deliver_pending` restores it.
/// That is what makes BusyBox ash's `waitproc` work: it blocks every
/// signal, then `sigsuspend`s with the old mask to wait for SIGCHLD.
pub(super) fn sys_rt_sigsuspend(mask_ptr: u64, sigsetsize: u64) -> SyscallResult {
    if sigsetsize != 8 {
        return errno::EINVAL;
    }
    if let Err(e) = validate_user_buffer(mask_ptr, 8) { return e; }
    let new_mask = crate::process::signal::mask_from_user(unsafe { core::ptr::read_unaligned(mask_ptr as *const u64) })
        & !(1u64 << crate::process::signal::SIGKILL)
        & !(1u64 << crate::process::signal::SIGSTOP);
    suspend(Some(new_mask))
}

/// pause(34): sleep until a signal that runs a handler, terminates or
/// stops the process, then return `EINTR` — `sigsuspend` with the mask
/// the process already has, which is exactly how Linux defines it.
pub(super) fn sys_pause() -> SyscallResult {
    suspend(None)
}

/// The body of `rt_sigsuspend`/`pause`: `new_mask` replaces the mask for
/// the duration of the sleep, `None` keeps the current one.
fn suspend(new_mask: Option<u64>) -> SyscallResult {
    let tf_ptr = current_tf_ptr();
    let irq = crate::process::irq_guard::InterruptGuard::new();

    let next_tf = {
        let mut scheduler = crate::process::scheduler::local_scheduler();
        let Some(proc) = scheduler.running_mut() else {
            drop(scheduler);
            drop(irq);
            return errno::EINTR;
        };
        let new_mask = new_mask.unwrap_or(proc.blocked_signals);
        proc.saved_sigmask = Some(proc.blocked_signals);
        proc.blocked_signals = new_mask;
        let immediate = crate::process::signal::has_actionable(proc);
        crate::ktrace!(
            crate::debug::PROC,
            "sigsuspend: PID {} mask {:#x} (was {:#x}) pending {:#x} -> {}",
            proc.pid.0, new_mask, proc.saved_sigmask.unwrap_or(0), proc.pending_signals,
            if immediate { "return" } else { "block" }
        );
        if immediate {
            // Already pending: return at once; the syscall-return path
            // delivers it with the temporary mask still in force.
            None
        } else {
            proc.in_sigsuspend = true;
            unsafe { (*(tf_ptr as *mut TrapFrame)).rax = errno::EINTR as u64; }
            // Not a `process::wait` wait: `interrupt_blocked` wakes it by
            // `in_sigsuspend`, with rax preset to EINTR above.
            let next = scheduler.block_current(tf_ptr, crate::process::wait::Wait::uninterruptible());
            if next == tf_ptr {
                // A stale `wake_pending` let `block_current` return without
                // blocking: a spurious wakeup, which sigsuspend's callers
                // loop on anyway.
                if let Some(proc) = scheduler.running_mut() {
                    proc.in_sigsuspend = false;
                }
                None
            } else {
                Some(next)
            }
        }
    };

    match next_tf {
        None => {
            drop(irq);
            errno::EINTR
        }
        // Diverges; interrupts stay off across the jump (see `sys_waitpid`).
        Some(next) => unsafe { crate::process::trapframe::jump_to_user(next) },
    }
}

// ── sigaltstack(131) ───────────────────────────────────────────────────────

/// sigaltstack(131): int sigaltstack(const stack_t *ss, stack_t *old_ss)
///
/// Linux's `stack_t`: `{ ss_sp: u64 @0, ss_flags: i32 @8, ss_size: u64 @16 }`. Per thread. A handler installed with
/// `SA_ONSTACK` runs on it (`signal::push_signal_frame`). `SS_DISABLE` turns it off; a size under `MINSIGSTKSZ` (2048) is
/// `ENOMEM`; changing it while running on it is `EPERM`. `old_ss.ss_flags` has `SS_ONSTACK` while the caller is on it.
pub(super) fn sys_sigaltstack(ss: u64, old_ss: u64) -> SyscallResult {
    use crate::process::signal::AltStack;
    const SS_ONSTACK: i32 = 1;
    const SS_DISABLE: i32 = 2;
    const MINSIGSTKSZ: u64 = 2048;
    if old_ss != 0 && validate_user_buffer(old_ss, 24).is_err() {
        return errno::EFAULT;
    }
    // SAFETY (both reads): validated as a user-space range; done before the scheduler lock, where a fault must not happen.
    let new = if ss != 0 {
        if validate_user_buffer(ss, 24).is_err() {
            return errno::EFAULT;
        }
        let (sp, flags, size) = unsafe { (*(ss as *const u64), *((ss + 8) as *const i32), *((ss + 16) as *const u64)) };
        if flags & !(SS_DISABLE | SS_ONSTACK) != 0 {
            return errno::EINVAL;
        }
        Some((sp, flags, size))
    } else {
        None
    };
    let rsp = unsafe { (*current_tf_ptr()).rsp };

    // The old value and the outcome, out of the lock by value: the lock's closure returns only an errno.
    let mut old = AltStack::NONE;
    let result = with_current_process(|proc| {
        old = proc.altstack;
        let mut result = 0;
        if let Some((sp, flags, size)) = new {
            if old.contains(rsp) {
                result = errno::EPERM;
            } else if flags & SS_DISABLE != 0 {
                proc.altstack = AltStack::NONE;
            } else if size < MINSIGSTKSZ {
                result = errno::ENOMEM;
            } else {
                proc.altstack = AltStack { sp, size };
            }
        }
        result
    });
    if result == 0 && old_ss != 0 {
        let flags = if old.contains(rsp) { SS_ONSTACK } else if old.size == 0 { SS_DISABLE } else { 0 };
        // SAFETY: validated above as a user-space range.
        unsafe {
            *(old_ss as *mut u64) = old.sp;
            *((old_ss + 8) as *mut i32) = flags;
            *((old_ss + 16) as *mut u64) = old.size;
        }
    }
    result
}
