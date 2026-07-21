// kernel/src/process/syscall/signal.rs
//
// sigaction(13) / sigprocmask(14) / sigreturn(15).

use crate::process::TrapFrame;
use super::{errno, SyscallResult, with_current_process, validate_user_buffer, current_tf_ptr};

const SIG_DFL: u64 = 0;
const SIG_IGN: u64 = 1;

/// rt_sigaction(13): int sigaction(int sig, const struct sigaction *act, struct sigaction *oldact)
///
/// Simplified ABI: `act`/`oldact` are read/written as a single `u64`
/// handler address at offset 0 (matches `sa_handler`'s position in the
/// real `struct sigaction`; `sa_mask`/`sa_flags`/`sa_restorer` are ignored)
/// rather than the full struct — this kernel's userspace test programs use
/// a matching minimal ABI (see `userspace/src/syscall.rs::sigaction`).
pub(super) fn sys_sigaction(sig: u32, act_ptr: u64, oldact_ptr: u64) -> SyscallResult {
    if sig == 0 || sig as usize >= crate::process::signal::NUM_SIGNALS
        || sig == crate::process::signal::SIGKILL || sig == crate::process::signal::SIGSTOP {
        return errno::EINVAL;
    }
    if act_ptr != 0 {
        if let Err(e) = validate_user_buffer(act_ptr, 8) { return e; }
    }
    if oldact_ptr != 0 {
        if let Err(e) = validate_user_buffer(oldact_ptr, 8) { return e; }
    }

    with_current_process(|proc| {
        let old = proc.signal_handlers[sig as usize];
        if act_ptr != 0 {
            let handler_addr = unsafe { *(act_ptr as *const u64) };
            proc.signal_handlers[sig as usize] = match handler_addr {
                SIG_DFL => crate::process::SignalAction::Default,
                SIG_IGN => crate::process::SignalAction::Ignore,
                addr => crate::process::SignalAction::Handler(addr),
            };
        }
        if oldact_ptr != 0 {
            let old_addr = match old {
                crate::process::SignalAction::Default => SIG_DFL,
                crate::process::SignalAction::Ignore => SIG_IGN,
                crate::process::SignalAction::Handler(addr) => addr,
            };
            unsafe { *(oldact_ptr as *mut u64) = old_addr; }
        }
        0
    })
}

const SIG_BLOCK: i32 = 0;
const SIG_UNBLOCK: i32 = 1;
const SIG_SETMASK: i32 = 2;

/// rt_sigprocmask(14): int sigprocmask(int how, const sigset_t *set, sigset_t *oldset)
///
/// `sigset_t` here is a single `u64` bitmask (this kernel supports 32
/// signals, so no wider representation is needed).
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
            let set = unsafe { *(set_ptr as *const u64) };
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
            unsafe { *(oldset_ptr as *mut u64) = old_mask; }
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

    with_current_process(|proc| {
        unsafe { crate::process::signal::pop_signal_frame(proc, tf_ptr, user_rsp) };
        unsafe { (*tf_ptr).rax as i64 }
    })
}
