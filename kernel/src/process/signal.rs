// kernel/src/process/signal.rs
//
// Minimal POSIX-ish signal delivery: SIGKILL, SIGTERM, SIGSEGV, SIGPIPE,
// SIGINT, SIGQUIT (all default-terminate), SIGCHLD/SIGCONT (default-ignore),
// SIGUSR1/SIGUSR2 (default-terminate, meant for installing custom handlers
// in tests). SIGSTOP/SIGTSTP default-stop (job control — see
// `SignalOutcome::Stop` and `Scheduler::stop_and_switch_tf`/`wake_stopped`).
// void (*)(int) handlers only — no siginfo, no altstack, no real-time
// signals.
//
// DELIVERY
//
// `deliver_pending` is called at every point this kernel is about to return
// to user mode (see `trapframe::jump_to_user`, `syscall::syscall_handler_asm`,
// `timer_preempt::timer_preempt_handler`) with a raw pointer to whichever
// TrapFrame will actually be restored. It's a raw pointer rather than `&mut
// TrapFrame` specifically so callers can pass `proc.trapframe`'s contents
// *and* still hold `&mut Process` at the same time — a safe reference to a
// field while also holding `&mut` to the parent struct doesn't borrow-check
// across a function call boundary, but a raw pointer sidesteps that; the
// aliasing is sound here because nothing else touches that memory in this
// single-core, cli-disciplined kernel while this runs.
//
// For a caught signal (`SignalAction::Handler`), the interrupted TrapFrame
// is copied onto the process's own user stack (its page table is always
// already active at every call site — see call site comments) below a
// fixed one-instruction trampoline page (mapped into every user address
// space by `elf_loader.rs`), and the live TrapFrame is redirected to the
// handler. `rt_sigreturn` (`syscall.rs`) reverses this exactly.

use super::{Process, TrapFrame};
use crate::memory::signal_trampoline::TRAMPOLINE_VA;

pub const SIGINT: u32 = 2;
pub const SIGQUIT: u32 = 3;
pub const SIGKILL: u32 = 9;
pub const SIGUSR1: u32 = 10;
pub const SIGSEGV: u32 = 11;
pub const SIGUSR2: u32 = 12;
pub const SIGPIPE: u32 = 13;
pub const SIGTERM: u32 = 15;
pub const SIGCHLD: u32 = 17;
pub const SIGCONT: u32 = 18;
pub const SIGSTOP: u32 = 19;
pub const SIGTSTP: u32 = 20;
pub const SIGTTIN: u32 = 21;
pub const SIGTTOU: u32 = 22;

// 64, not 32: `pending_signals`/`blocked_signals` are `u64` bitmasks, so 64
// is the natural width — and mlibc's pthread subsystem unconditionally
// installs a SIGCANCEL(34, this port's abi-bits/signal.h) handler at
// program startup for every process, which needs a slot to land in even
// though this kernel never actually raises it.
pub const NUM_SIGNALS: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SignalAction {
    Default,
    Ignore,
    Handler(u64),
}

/// What the caller must do after `deliver_pending` returns.
pub enum SignalOutcome {
    /// Nothing pending/deliverable — TrapFrame untouched.
    None,
    /// A handler frame was pushed; the (already-redirected) TrapFrame is
    /// ready to `iretq` into the handler.
    Delivered,
    /// This signal's default action is to terminate the process; the
    /// caller must kill it (e.g. via `Scheduler::kill_and_switch_tf`) and
    /// pick a different TrapFrame to run instead.
    Terminate(u32),
    /// This signal's default action is to stop the process (job control);
    /// the caller must park it as `ProcessState::Stopped` (e.g. via
    /// `Scheduler::stop_and_switch_tf`) and pick a different TrapFrame.
    Stop(u32),
}

/// SIGCHLD and SIGCONT default to Ignore; everything else this kernel
/// raises defaults to Terminate *except* SIGSTOP/SIGTSTP, which
/// `deliver_pending` checks before ever consulting this (see there).
fn default_terminates(sig: u32) -> bool {
    sig != SIGCHLD && sig != SIGCONT
}

/// Set `sig`'s pending bit. Pending state is independent of whether the
/// signal is currently blocked — blocking only defers delivery, matching
/// POSIX `sigprocmask` semantics.
pub fn queue_signal(proc: &mut Process, sig: u32) {
    if sig == 0 || sig as usize >= NUM_SIGNALS {
        return;
    }
    proc.pending_signals |= 1u64 << sig;
}

/// Check `proc`'s pending & unblocked signals against its handler table and
/// act on the lowest-numbered one, if any. `tf` must point at whatever
/// TrapFrame will actually be restored into user mode next — not
/// necessarily `proc.trapframe` (see call sites: the live on-stack syscall
/// frame during `syscall_handler_asm`, `proc.trapframe` everywhere else).
pub fn deliver_pending(proc: &mut Process, tf: *mut TrapFrame) -> SignalOutcome {
    let deliverable = proc.pending_signals & !proc.blocked_signals;
    if deliverable == 0 {
        return SignalOutcome::None;
    }
    let sig = deliverable.trailing_zeros();
    proc.pending_signals &= !(1u64 << sig);

    match proc.signal_handlers[sig as usize] {
        // SIGSTOP can never be caught/ignored (sys_sigaction rejects
        // attempts to change its disposition) and SIGTSTP's *default*
        // action is always to stop even if `signal_handlers[SIGTSTP]` was
        // never touched — checked ahead of the `SignalAction` match so a
        // stray `Ignore`/`Handler` entry for SIGSTOP specifically (which
        // sigaction should never produce) can't accidentally suppress it.
        _ if sig == SIGSTOP => SignalOutcome::Stop(sig),
        SignalAction::Ignore => SignalOutcome::None,
        SignalAction::Default => {
            if sig == SIGTSTP || sig == SIGTTIN || sig == SIGTTOU {
                // Real POSIX default action for all three is to stop the
                // process — not terminate it. This matters concretely: a
                // job-control shell's own tty negotiation (e.g. ash's
                // `setjobctl()`) calls `killpg(0, SIGTTIN)` on *itself*
                // whenever it isn't yet the foreground process group, fully
                // expecting to just be stopped (then later resumed via
                // SIGCONT once it becomes foreground) — treating this as
                // Terminate would silently kill an interactive shell the
                // first time its own job-control setup ever raced with the
                // foreground group not matching yet.
                SignalOutcome::Stop(sig)
            } else if default_terminates(sig) {
                SignalOutcome::Terminate(sig)
            } else {
                SignalOutcome::None
            }
        }
        SignalAction::Handler(addr) => {
            unsafe { push_signal_frame(proc, tf, sig, addr) };
            SignalOutcome::Delivered
        }
    }
}

/// Saved onto the user stack so `rt_sigreturn` can restore everything
/// exactly, including the signal mask in effect before delivery.
#[repr(C)]
struct SignalFrame {
    saved_mask: u64,
    saved_tf: TrapFrame,
}

/// Redirect `tf` to run `handler_addr(sig)`, saving the interrupted context
/// on the user stack below the trampoline's return address.
///
/// # Safety
/// `tf` must point at a valid, live TrapFrame whose `rsp` is a valid user
/// stack pointer in `proc`'s *currently active* address space (true at
/// every call site — see module doc comment).
unsafe fn push_signal_frame(proc: &mut Process, tf: *mut TrapFrame, sig: u32, handler_addr: u64) {
    let old_tf = unsafe { core::ptr::read(tf) };

    // Layout (low -> high addresses): [tramp_slot: u64][SignalFrame].
    // `tramp_slot % 16 == 8` so the handler sees the same stack alignment
    // it would after a normal `call` instruction.
    let frame_size = core::mem::size_of::<SignalFrame>() as u64;
    let base = old_tf.rsp.saturating_sub(128) & !0xF; // clear the SysV red zone, 16-align
    let frame_base = (base - frame_size) & !0xF;
    let tramp_slot = frame_base - 8;

    let frame = SignalFrame {
        saved_mask: proc.blocked_signals,
        saved_tf: old_tf,
    };

    // The target stack region may dip below anything this process has
    // actually touched yet (e.g. its first-ever signal, delivered while
    // still near the top of a freshly-mapped stack) — demand-page it now,
    // since this write happens from kernel-mode code, which this kernel's
    // fault handler never demand-pages on its own (see the function this
    // helper lives next to in mod.rs for why).
    super::ensure_user_pages_mapped(proc, tramp_slot, frame_size + 8);

    unsafe {
        core::ptr::write(frame_base as *mut SignalFrame, frame);
        core::ptr::write(tramp_slot as *mut u64, TRAMPOLINE_VA);

        (*tf).rdi = sig as u64;
        (*tf).rip = handler_addr;
        (*tf).rsp = tramp_slot;
    }

    // POSIX default: the signal being handled is blocked for the duration
    // of its own handler (no nested re-entry on repeated delivery).
    proc.blocked_signals |= 1u64 << sig;
}

/// Reverse `push_signal_frame`: read the `SignalFrame` back from `user_rsp`
/// (the syscall-entry `rsp` of the `rt_sigreturn` call, which is exactly
/// `frame_base` — the handler's `ret` already popped the 8-byte trampoline
/// slot) and restore it into `tf` verbatim, plus the pre-signal mask.
///
/// # Safety
/// `user_rsp` must be exactly the `frame_base` a prior `push_signal_frame`
/// call computed — true whenever this is reached via the trampoline, which
/// is the only place that sets rsp to that value.
pub unsafe fn pop_signal_frame(proc: &mut Process, tf: *mut TrapFrame, user_rsp: u64) {
    let frame = unsafe { core::ptr::read(user_rsp as *const SignalFrame) };
    proc.blocked_signals = frame.saved_mask;
    unsafe { core::ptr::write(tf, frame.saved_tf) };
}
