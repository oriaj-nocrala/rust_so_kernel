// kernel/src/process/signal.rs
//
// Minimal POSIX-ish signal delivery: SIGKILL, SIGTERM, SIGSEGV, SIGPIPE,
// SIGINT, SIGQUIT (all default-terminate), SIGCHLD/SIGCONT (default-ignore),
// SIGUSR1/SIGUSR2 (default-terminate, meant for installing custom handlers
// in tests). SIGSTOP/SIGTSTP default-stop (job control — see
// `SignalOutcome::Stop` and `Scheduler::stop_and_switch_tf`/`wake_stopped`).
// Linux's `rt_sigaction` ABI: handlers get `(sig, siginfo*, ucontext*)`,
// `SA_RESTORER`, `SA_SIGINFO`, `SA_ONSTACK` (with `sigaltstack`),
// `SA_NODEFER`, `SA_RESETHAND` and `sa_mask` are honoured. No real-time
// signal queueing. Only signals sent to a process are delivered to
// handlers, plus the hardware faults of user code (`deliver_fault`: SIGSEGV
// for a page fault or #GP, SIGILL for #UD, SIGFPE for #DE) when a handler
// can take them; without one the process is killed, as before.
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
// For a caught signal (`SignalAction::Handler`), an `RtFrame` (a Linux-layout
// `ucontext_t` and `siginfo_t`, then this kernel's private copy of the FPU
// state and TrapFrame) is written onto the process's own user stack, or its
// alternate stack (its page table is always already active at every call
// site — see call site comments), below a return address that is the
// handler's `sa_restorer` (or, without `SA_RESTORER`, a fixed
// one-instruction trampoline page mapped into every user address space by
// `elf_loader.rs`), and the live TrapFrame is redirected to the handler.
// `rt_sigreturn` (`syscall.rs`) reads the registers and the mask back from
// the `ucontext` — so a handler that edits it changes where the process
// resumes, as on Linux — and the FPU state from the private copy.

use super::fpu::FpuState;
use super::{Process, TrapFrame};
use crate::memory::signal_trampoline::TRAMPOLINE_VA;

pub const SIGHUP: u32 = 1;
pub const SIGINT: u32 = 2;
pub const SIGQUIT: u32 = 3;
pub const SIGILL: u32 = 4;
pub const SIGFPE: u32 = 8;
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
pub const SIGURG: u32 = 23;
pub const SIGWINCH: u32 = 28;

// 64, not 32: `pending_signals`/`blocked_signals` are `u64` bitmasks, so 64
// is the natural width — and mlibc's pthread subsystem unconditionally
// installs a SIGCANCEL(34, this port's abi-bits/signal.h) handler at
// program startup for every process, which needs a slot to land in even
// though this kernel never actually raises it.
pub const NUM_SIGNALS: usize = 64;

/// A user `sigset_t` (Linux layout: bit N-1 = signal N, what mlibc's
/// `sigaddset` writes) to this kernel's internal mask (bit N = signal N).
/// `sigprocmask`/`rt_sigsuspend` used to take the user's word as-is, so
/// every mask a C program set named the signal one below the one it meant:
/// blocking SIGCHLD blocked signal 16, blocking SIGUSR1 blocked SIGKILL
/// (then dropped as unblockable). Signal 64 has no internal slot.
pub fn mask_from_user(set: u64) -> u64 {
    set << 1
}

/// Inverse of `mask_from_user`.
pub fn mask_to_user(mask: u64) -> u64 {
    mask >> 1
}

pub const SA_SIGINFO: u32 = 0x4;
pub const SA_RESTORER: u32 = 0x0400_0000;
pub const SA_ONSTACK: u32 = 0x0800_0000;
pub const SA_RESTART: u32 = 0x1000_0000;
pub const SA_NODEFER: u32 = 0x4000_0000;
pub const SA_RESETHAND: u32 = 0x8000_0000;

/// What `sigaction` stores beside the handler address, per signal.
#[derive(Clone, Copy)]
pub struct SigExtra {
    /// The `sa_flags` as given (Linux's values, `SA_RESTORER` included).
    pub flags: u32,
    /// `sa_restorer`: where the handler returns to. Only used with `SA_RESTORER`.
    pub restorer: u64,
    /// `sa_mask`, in this kernel's bit-N numbering (`mask_from_user` already applied).
    pub mask: u64,
}

impl SigExtra {
    pub const NONE: SigExtra = SigExtra { flags: 0, restorer: 0, mask: 0 };
}

/// The alternate signal stack (`sigaltstack`): `size == 0` means none.
#[derive(Clone, Copy)]
pub struct AltStack {
    pub sp: u64,
    pub size: u64,
}

impl AltStack {
    pub const NONE: AltStack = AltStack { sp: 0, size: 0 };

    /// Is `rsp` on it?
    pub fn contains(&self, rsp: u64) -> bool {
        self.size != 0 && rsp >= self.sp && rsp < self.sp.saturating_add(self.size)
    }
}

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

/// SIGCHLD, SIGCONT, SIGURG and SIGWINCH default to Ignore, as in Linux;
/// everything else defaults to Terminate *except* SIGSTOP/SIGTSTP, which
/// `deliver_pending` checks before ever consulting this (see there).
/// SIGWINCH and SIGURG used to terminate: a terminal resize would have
/// killed every foreground program that had not installed a handler.
fn default_terminates(sig: u32) -> bool {
    !matches!(sig, SIGCHLD | SIGCONT | SIGURG | SIGWINCH)
}

/// Whether sending `sig` must resume a stopped target: `SIGCONT`, and
/// `SIGKILL`, which cannot wait for a `SIGCONT` that may never come. Until
/// 2026-09-25 only `SIGCONT` did, so `kill -9` of a stopped job queued a
/// signal a process that never ran again could never act on, and a
/// `waitpid` for it blocked forever (`pty_test` case E found it).
pub fn resumes_stopped(sig: u32) -> bool {
    sig == SIGCONT || sig == SIGKILL
}

/// `si_code` values (`siginfo_t`, Linux's numbers).
pub const SI_USER: i32 = 0;
pub const SI_KERNEL: i32 = 0x80;
pub const SI_TKILL: i32 = -6;
pub const CLD_EXITED: i32 = 1;
pub const CLD_KILLED: i32 = 2;
pub const CLD_STOPPED: i32 = 5;
pub const CLD_CONTINUED: i32 = 6;

/// Who sent a pending signal, as `siginfo_t` reports it: `si_code`, `si_pid` (a thread-group id) and, for `SIGCHLD`,
/// `si_status`. There is no uid model, so `si_uid` is always 0. One per signal number, as standard signals do not queue: the
/// first sender of a still-pending signal is the one a handler sees (`queue_signal_from`).
#[derive(Clone, Copy)]
pub struct SigOrigin {
    pub code: i32,
    pub pid: u32,
    pub status: i32,
}

impl SigOrigin {
    /// Raised by the kernel itself (a terminal's Ctrl-C, job control, a group kill on `exit_group`): `SI_KERNEL`, no sender.
    pub const KERNEL: SigOrigin = SigOrigin { code: SI_KERNEL, pid: 0, status: 0 };
    /// `kill(2)` from thread group `pid`.
    pub const fn user(pid: usize) -> SigOrigin { SigOrigin { code: SI_USER, pid: pid as u32, status: 0 } }
    /// `tkill(2)`/`tgkill(2)` (so `raise`) from thread group `pid`.
    pub const fn tkill(pid: usize) -> SigOrigin { SigOrigin { code: SI_TKILL, pid: pid as u32, status: 0 } }
    /// `SIGCHLD` for child `pid`: `code` is one of `CLD_*`, `status` its exit code or the signal.
    pub const fn child(code: i32, pid: usize, status: i32) -> SigOrigin { SigOrigin { code, pid: pid as u32, status } }
}

/// Set `sig`'s pending bit, noting who sent it. Pending state is independent of whether the
/// signal is currently blocked — blocking only defers delivery, matching
/// POSIX `sigprocmask` semantics.
pub fn queue_signal_from(proc: &mut Process, sig: u32, origin: SigOrigin) {
    if sig == 0 || sig as usize >= NUM_SIGNALS {
        return;
    }
    if proc.pending_signals & (1u64 << sig) == 0 {
        proc.sig_origin[sig as usize] = origin;
    }
    proc.pending_signals |= 1u64 << sig;
}

/// `queue_signal_from` a kernel-raised signal (`SigOrigin::KERNEL`).
pub fn queue_signal(proc: &mut Process, sig: u32) {
    queue_signal_from(proc, sig, SigOrigin::KERNEL);
}

/// Check `proc`'s pending & unblocked signals against its handler table and
/// act on the lowest-numbered one, if any. `tf` must point at whatever
/// TrapFrame will actually be restored into user mode next — not
/// necessarily `proc.trapframe` (see call sites: the live on-stack syscall
/// frame during `syscall_handler_asm`, `proc.trapframe` everywhere else).
pub fn deliver_pending(proc: &mut Process, tf: *mut TrapFrame) -> SignalOutcome {
    // On its way back to user mode, so no longer in `rt_sigsuspend` — even
    // if something other than a signal woke it. A flag left set would let
    // `interrupt_blocked` end some later, unrelated block.
    proc.in_sigsuspend = false;
    if let Some(i) = proc.interrupted.take() {
        finish_interrupted_call(proc, tf, i);
    }
    let outcome = deliver_one(proc, tf);
    // Returning to user mode without a handler frame to carry it: whatever
    // `rt_sigsuspend` replaced goes back now (a pushed frame took it with
    // `saved_sigmask.take()`; Terminate makes it moot).
    if !matches!(outcome, SignalOutcome::Delivered | SignalOutcome::Terminate(_)) {
        if let Some(mask) = proc.saved_sigmask.take() {
            proc.blocked_signals = mask;
        }
    }
    outcome
}

/// Whether `proc` has a pending, unblocked signal it would act on — run a
/// handler, terminate or stop. An ignored one (explicitly, or SIGCHLD/SIGCONT
/// by default) does not count: it would not end an `rt_sigsuspend` in Linux
/// either, where such signals are discarded when sent.
pub fn has_actionable(proc: &Process) -> bool {
    let mut deliverable = proc.pending_signals & !proc.blocked_signals;
    while deliverable != 0 {
        let sig = deliverable.trailing_zeros();
        deliverable &= deliverable - 1;
        let acts = match proc.signal_handlers[sig as usize] {
            _ if sig == SIGSTOP => true,
            SignalAction::Ignore => false,
            SignalAction::Default => {
                sig == SIGTSTP || sig == SIGTTIN || sig == SIGTTOU || default_terminates(sig)
            }
            SignalAction::Handler(_) => true,
        };
        if acts {
            return true;
        }
    }
    false
}

/// What delivering `sig` does, without delivering it.
fn effect_of(proc: &Process, sig: u32) -> sched::SignalEffect {
    use sched::SignalEffect;
    match proc.signal_handlers[sig as usize] {
        _ if sig == SIGSTOP => SignalEffect::Stop,
        SignalAction::Ignore => SignalEffect::Nothing,
        SignalAction::Default => {
            if sig == SIGTSTP || sig == SIGTTIN || sig == SIGTTOU {
                SignalEffect::Stop
            } else if default_terminates(sig) {
                SignalEffect::Terminate
            } else {
                SignalEffect::Nothing
            }
        }
        SignalAction::Handler(_) => SignalEffect::Handler { sa_restart: proc.sig_restart & (1u64 << sig) != 0 },
    }
}

/// What the next delivery will do: the first deliverable signal that is
/// not ignored (`deliver_one` discards the ignored ones on the way).
fn next_effect(proc: &Process) -> sched::SignalEffect {
    let mut deliverable = proc.pending_signals & !proc.blocked_signals;
    while deliverable != 0 {
        let sig = deliverable.trailing_zeros();
        deliverable &= deliverable - 1;
        let e = effect_of(proc, sig);
        if e != sched::SignalEffect::Nothing {
            return e;
        }
    }
    sched::SignalEffect::Nothing
}

/// A signal ended this process's wait (`process::wait`): make the call it
/// was in return `EINTR`, or re-execute it, according to what the signal
/// is about to do — before `deliver_one` saves the frame into a handler's
/// signal frame, so the handler's `sigreturn` lands on the right one.
fn finish_interrupted_call(proc: &mut Process, tf: *mut TrapFrame, i: super::wait::Interrupted) {
    const EINTR: i64 = -4;
    let restart = sched::wait::restarts(i.policy, next_effect(proc));
    crate::ktrace!(
        crate::debug::PROC,
        "interrupted: PID {} syscall {} -> {}",
        proc.pid.0, i.nr, if restart { "restart" } else { "EINTR" }
    );
    // A relative sleep that restarts (stopped, then continued) must sleep only what it had left, not the whole time again.
    if let (true, Some(r)) = (restart, i.rem) {
        proc.sleep_resume = Some(r.expiry);
    }
    // A sleep ended by a handler reports the time it had left (`nanosleep`/`clock_nanosleep`'s `rem`).
    if let (false, Some(r), true) = (restart, i.rem, i.rem.is_some_and(|r| r.rem_ptr != 0)) {
        let left = r.expiry.saturating_sub(crate::time::ktime_get());
        let mut ts = [0u8; 16];
        ts[..8].copy_from_slice(&((left / 1_000_000_000) as i64).to_ne_bytes());
        ts[8..].copy_from_slice(&((left % 1_000_000_000) as i64).to_ne_bytes());
        unsafe { proc.address_space.copy_to_user(r.rem_ptr, &ts); }
    }
    unsafe {
        if restart {
            // Back onto the `syscall` instruction with its number in rax,
            // the arguments still in their saved registers.
            (*tf).rip = i.ret_rip - 2;
            (*tf).rax = i.nr;
        } else {
            (*tf).rip = i.ret_rip;
            (*tf).rax = EINTR as u64;
        }
    }
}

fn deliver_one(proc: &mut Process, tf: *mut TrapFrame) -> SignalOutcome {
    // Ignored signals are discarded on the way to the first one that does
    // something. Stopping at an ignored one (as this did) left an
    // actionable one behind it pending until the next return to user mode.
    let mut sig;
    loop {
        let deliverable = proc.pending_signals & !proc.blocked_signals;
        if deliverable == 0 {
            return SignalOutcome::None;
        }
        sig = deliverable.trailing_zeros();
        proc.pending_signals &= !(1u64 << sig);
        if effect_of(proc, sig) != sched::SignalEffect::Nothing {
            break;
        }
    }

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
            if unsafe { push_signal_frame(proc, tf, sig, addr, None) } {
                SignalOutcome::Delivered
            } else {
                // No room for the frame: SIGSEGV, unconditionally, as Linux's force_sigsegv.
                SignalOutcome::Terminate(SIGSEGV)
            }
        }
    }
}

/// The kernel's private part of a signal frame: what `ucontext` cannot carry.
///
/// `fpu` is the interrupted code's XSAVE image (FXSAVE layout in its first
/// 512 bytes, then the header and the ymm upper halves), as Linux keeps it in its
/// `rt_sigframe`: the handler is ordinary code free to use XMM registers
/// (mlibc's `memcpy`/`printf` do, and so does every Rust program since the
/// userspace target gained SSE), and without it the interrupted code would
/// resume with the handler's values in them. `FpuState` is 64-byte aligned
/// (`xsave` needs it), so the whole frame is, and so is `frame_base` below.
/// `saved_tf` supplies what `sigreturn` does not take from the `ucontext`
/// (the segment selectors).
#[repr(C)]
struct SignalFrame {
    fpu: FpuState,
    saved_tf: TrapFrame,
}

/// Everything written to the user stack for one delivery, at `frame_base`
/// (64-aligned): a Linux x86-64 `ucontext_t` (304 bytes, padded), a
/// `siginfo_t` (128 bytes), then the private part at a 64-aligned offset.
/// The handler is entered with `rsp` on the 8 bytes just below it (the return
/// address) and receives `rsi = &info`, `rdx = &uc`.
#[repr(C, align(64))]
struct RtFrame {
    uc: [u64; UC_WORDS],
    info: [u64; 16],
    private: SignalFrame,
}

const UC_WORDS: usize = 40;
/// `ucontext_t` word indexes: `uc_flags`, `uc_link`, `uc_stack` (`ss_sp`,
/// `ss_flags`, `ss_size`), `gregs[23]` from 5, `fpregs` pointer, reserved,
/// `uc_sigmask`.
const UC_STACK: usize = 2;
const UC_GREGS: usize = 5;
const UC_SIGMASK: usize = 37;
/// `gregs` order (`REG_R8` … `REG_CR2`): the kernel's registers by slot.
const REG_R8: usize = 0;
const REG_RDI: usize = 8;
const REG_RSP: usize = 15;
const REG_RIP: usize = 16;
const REG_EFL: usize = 17;
const REG_CSGSFS: usize = 18;
/// The flags a handler may change on return: CF PF AF ZF SF DF OF. IF stays on, TF and IOPL stay off.
const USER_EFLAGS_MASK: u64 = 0x0CD5;

fn gregs_from_tf(tf: &TrapFrame) -> [u64; 23] {
    let mut g = [0u64; 23];
    g[REG_R8] = tf.r8;
    g[REG_R8 + 1] = tf.r9;
    g[REG_R8 + 2] = tf.r10;
    g[REG_R8 + 3] = tf.r11;
    g[REG_R8 + 4] = tf.r12;
    g[REG_R8 + 5] = tf.r13;
    g[REG_R8 + 6] = tf.r14;
    g[REG_R8 + 7] = tf.r15;
    g[REG_RDI] = tf.rdi;
    g[REG_RDI + 1] = tf.rsi;
    g[REG_RDI + 2] = tf.rbp;
    g[REG_RDI + 3] = tf.rbx;
    g[REG_RDI + 4] = tf.rdx;
    g[REG_RDI + 5] = tf.rax;
    g[REG_RDI + 6] = tf.rcx;
    g[REG_RSP] = tf.rsp;
    g[REG_RIP] = tf.rip;
    g[REG_EFL] = tf.rflags;
    g[REG_CSGSFS] = tf.cs | (tf.ss << 48);
    g
}

fn tf_from_gregs(tf: &mut TrapFrame, g: &[u64]) {
    tf.r8 = g[REG_R8];
    tf.r9 = g[REG_R8 + 1];
    tf.r10 = g[REG_R8 + 2];
    tf.r11 = g[REG_R8 + 3];
    tf.r12 = g[REG_R8 + 4];
    tf.r13 = g[REG_R8 + 5];
    tf.r14 = g[REG_R8 + 6];
    tf.r15 = g[REG_R8 + 7];
    tf.rdi = g[REG_RDI];
    tf.rsi = g[REG_RDI + 1];
    tf.rbp = g[REG_RDI + 2];
    tf.rbx = g[REG_RDI + 3];
    tf.rdx = g[REG_RDI + 4];
    tf.rax = g[REG_RDI + 5];
    tf.rcx = g[REG_RDI + 6];
    tf.rsp = g[REG_RSP];
    tf.rip = g[REG_RIP];
    tf.rflags = (g[REG_EFL] & USER_EFLAGS_MASK) | 0x202;
}

/// Redirect `tf` to run `handler_addr(sig, &info, &uc)`, saving the
/// interrupted context on the user stack (or the alternate stack, for
/// `SA_ONSTACK`) as an `RtFrame`.
///
/// # Safety
/// `tf` must point at a valid, live TrapFrame whose `rsp` is a valid user
/// stack pointer in `proc`'s *currently active* address space (true at
/// every call site — see module doc comment).
unsafe fn push_signal_frame(proc: &mut Process, tf: *mut TrapFrame, sig: u32, handler_addr: u64, fault: Option<(i32, u64)>) -> bool {
    let old_tf = unsafe { core::ptr::read(tf) };
    let extra = proc.sig_extra[sig as usize];

    // Where the frame goes: on the alternate stack if the handler asked for
    // it and one is set and we are not on it already, else below the
    // interrupted stack pointer, past the SysV red zone.
    let on_alt = proc.altstack.contains(old_tf.rsp);
    let (base, uc_stack) = if extra.flags & SA_ONSTACK != 0 && proc.altstack.size != 0 && !on_alt {
        ((proc.altstack.sp + proc.altstack.size) & !0xF, proc.altstack)
    } else {
        (old_tf.rsp.saturating_sub(128) & !0xF, proc.altstack)
    };
    // `frame_base` 64-aligned so the ret slot below it is 8 mod 16: the
    // handler sees the alignment it would after a normal `call`.
    let frame_size = core::mem::size_of::<RtFrame>() as u64;
    let frame_base = (base - frame_size) & !0x3F;
    let ret_slot = frame_base - 8;

    // The interrupted code's FPU/SSE state is the live one: every caller
    // runs for the process `resolve_signals` is about to return to, after
    // any switch into it has already restored its state, and the
    // kernel itself is soft-float.
    let mut fpu = FpuState::zeroed();
    unsafe { super::fpu::save(&mut fpu) };

    // After `rt_sigsuspend`, the handler's `sigreturn` must restore the
    // caller's mask, not sigsuspend's temporary one.
    let saved_mask = proc.saved_sigmask.take().unwrap_or(proc.blocked_signals);

    let mut uc = [0u64; UC_WORDS];
    uc[UC_STACK] = uc_stack.sp;
    uc[UC_STACK + 1] = if on_alt { 1 } else if uc_stack.size == 0 { 2 } else { 0 }; // SS_ONSTACK / SS_DISABLE
    uc[UC_STACK + 2] = uc_stack.size;
    uc[UC_GREGS..UC_GREGS + 23].copy_from_slice(&gregs_from_tf(&old_tf));
    uc[UC_SIGMASK] = mask_to_user(saved_mask);

    // siginfo: si_signo, si_errno 0, then `si_code` and, for a fault, `si_addr`; otherwise the sender recorded when the signal
    // was queued: `si_pid`/`si_uid` at 16, and for SIGCHLD `si_status` at 24.
    let mut info = [0u64; 16];
    info[0] = sig as u64;
    if let Some((code, addr)) = fault {
        info[1] = code as u32 as u64;
        info[2] = addr;
    } else {
        let o = proc.sig_origin[sig as usize];
        info[1] = o.code as u32 as u64;
        info[2] = o.pid as u64;
        if sig == SIGCHLD {
            info[3] = o.status as u32 as u64;
        }
    }

    let frame = RtFrame { uc, info, private: SignalFrame { fpu, saved_tf: old_tf } };

    // Make the target stack region mapped and privately writable before
    // writing it through its virtual address: it may dip below anything
    // this process has touched yet (its first-ever signal, near the top of
    // a fresh stack), or still be COW-shared. The fault handler would
    // resolve either, but this runs under the scheduler lock, where a
    // fault is best avoided.
    // A stack that cannot take the frame (a stack overflow with no alternate stack, a wild `rsp`) is the one case where
    // delivering is impossible: the caller kills the process, as Linux does.
    if !proc.address_space.prepare_user_write(ret_slot, frame_size + 8) {
        return false;
    }

    crate::ktrace!(
        crate::debug::PROC,
        "signal: PID {} sig {} -> handler {:#x}; saved rip={:#x} rsp={:#x} cs={:#x}; frame at {:#x}{}",
        proc.pid.0, sig, handler_addr, old_tf.rip, old_tf.rsp, old_tf.cs, frame_base,
        if base != old_tf.rsp.saturating_sub(128) & !0xF { " (altstack)" } else { "" }
    );

    let restorer = if extra.flags & SA_RESTORER != 0 { extra.restorer } else { TRAMPOLINE_VA };
    unsafe {
        core::ptr::write(frame_base as *mut RtFrame, frame);
        core::ptr::write(ret_slot as *mut u64, restorer);

        (*tf).rdi = sig as u64;
        (*tf).rsi = frame_base + core::mem::offset_of!(RtFrame, info) as u64;
        (*tf).rdx = frame_base;
        (*tf).rax = 0;
        (*tf).rip = handler_addr;
        (*tf).rsp = ret_slot;
        (*tf).rflags &= !0x400; // DF clear on entry, as the ABI says
    }

    // The signal being handled is blocked for the duration of its own
    // handler, unless `SA_NODEFER`; `sa_mask` adds to that.
    proc.blocked_signals |= extra.mask & !(1u64 << SIGKILL);
    if extra.flags & SA_NODEFER == 0 {
        proc.blocked_signals |= 1u64 << sig;
    }
    // `SA_RESETHAND`: one-shot.
    if extra.flags & SA_RESETHAND != 0 {
        proc.signal_handlers[sig as usize] = SignalAction::Default;
        proc.sig_extra[sig as usize] = SigExtra::NONE;
        proc.sig_restart &= !(1u64 << sig);
    }
    true
}

/// A hardware fault in user code (`init::devices`' entries): run the handler for `sig` with `si_code`/`si_addr`, if the process
/// has one and has not blocked the signal (a fault inside a `SIGSEGV` handler finds it blocked and kills, as on Linux).
/// Returns whether a frame was pushed; if not the caller kills the process.
///
/// # Safety
/// As `push_signal_frame`: `tf` is the faulting user context, live and mapped in the active address space.
pub unsafe fn deliver_fault(proc: &mut Process, tf: *mut TrapFrame, sig: u32, si_code: i32, addr: u64) -> bool {
    if proc.blocked_signals & (1u64 << sig) != 0 {
        return false;
    }
    match proc.signal_handlers[sig as usize] {
        SignalAction::Handler(h) => unsafe { push_signal_frame(proc, tf, sig, h, Some((si_code, addr))) },
        _ => false,
    }
}

/// Reverse `push_signal_frame`: read the `RtFrame` back from `user_rsp`
/// (the syscall-entry `rsp` of the `rt_sigreturn` call, which is exactly
/// `frame_base` — the handler's `ret` already popped the 8-byte return
/// address) and restore the registers and the mask from its `ucontext`
/// (`USER_EFLAGS_MASK` of the flags; the segment selectors are the
/// kernel's), and the FPU state from the private copy.
///
/// # Safety
/// `user_rsp` must be exactly the `frame_base` a prior `push_signal_frame`
/// call computed — true whenever this is reached from a handler that
/// returned normally.
pub unsafe fn pop_signal_frame(proc: &mut Process, tf: *mut TrapFrame, user_rsp: u64) {
    // Unaligned: `user_rsp` is whatever the process had in rsp when it made
    // the call, and `RtFrame` demands 64.
    let frame = unsafe { core::ptr::read_unaligned(user_rsp as *const RtFrame) };
    crate::ktrace!(
        crate::debug::PROC,
        "sigreturn: PID {} frame at {:#x} -> rip={:#x} rsp={:#x}",
        proc.pid.0, user_rsp, frame.uc[UC_GREGS + REG_RIP], frame.uc[UC_GREGS + REG_RSP]
    );
    proc.blocked_signals = mask_from_user(frame.uc[UC_SIGMASK]) & !(1u64 << SIGKILL);
    let mut new_tf = frame.private.saved_tf;
    tf_from_gregs(&mut new_tf, &frame.uc[UC_GREGS..UC_GREGS + 23]);
    unsafe { core::ptr::write(tf, new_tf) };
    // Straight into the live registers, as `sys_exec` does: this process
    // returns to user mode on this CPU without another switch, and a
    // preemption before that saves what this loads.
    let mut fpu = frame.private.fpu;
    super::fpu::sanitize(&mut fpu);
    unsafe { super::fpu::restore(&fpu) };
}
