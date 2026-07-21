// kernel/src/process/syscall/mod.rs
//
// All syscalls use `with_current_process` or `with_scheduler` helpers
// that guarantee cli before lock, lock dropped before sti — both now
// enforced by RAII (`process::irq_guard`), not hand-paired `asm!("cli")`/
// `asm!("sti")` calls. See `irq_guard.rs`'s module doc comment for why:
// that manual pattern caused a real, reproducible full-kernel hang once
// (see `fs::sys_close`'s doc comment).
//
// with_current_process uses scheduler.running_mut() for O(1) access.
//
// HISTORY:
//   - sys_exit now performs an immediate full context switch via
//     kill_and_switch_tf + jump_to_trapframe, instead of entering
//     a hlt loop and waiting up to 10ms for the timer to preempt.
//
// MODULE LAYOUT: this used to be one 4200+ line file. It's now split by
// subsystem, matching the section dividers the original file already had:
//   fs           — read/write/open/close/stat family/getdents64/lseek/mmap/
//                  munmap/pipe/dup/dup2/fcntl/ioctl/writev/access/rename/
//                  mkdir/rmdir/unlink/symlink/readlink/chmod/fchmod/statvfs/
//                  getcwd/chdir, plus the stdin blocking-read machinery.
//   process_ctl  — fork/clone/exec/exit/waitpid/kill/getpid/setpgid/getpgid/
//                  setsid/yield/nanosleep/arch_prctl/set_tid_address.
//   signal       — sigaction/sigprocmask/sigreturn.
//   ipc          — socket/connect/accept/bind/sendmsg/recvmsg.
//   sync         — futex.
//   poll         — poll/epoll_create/epoll_ctl/epoll_wait.
//   misc         — uptime/meminfo/kdebug_ctl/clock_gettime.
// Everything below is dispatch plumbing + helpers shared by all of them.

mod fs;
mod process_ctl;
mod signal;
mod ipc;
mod sync;
mod poll;
mod misc;

pub(crate) use fs::{send_to_group, stdin_wakeup};
pub(crate) use process_ctl::cancel_all_waiters;
pub(crate) use poll::{poll_wakeup_for_fd0, poll_clear_on_timeout};

use core::arch::global_asm;
use core::sync::atomic::{AtomicU64, Ordering};
use super::TrapFrame;

// Scratch storage for syscall_entry_fast.
// Single-CPU only; safe because SFMASK clears IF on syscall entry,
// so this is never re-entered before we switch to the kernel stack.
#[no_mangle]
static mut SYSCALL_USER_RFLAGS: u64 = 0;

// syscall_entry_fast — kernel entry point for the `syscall` instruction.
//
// On entry (CPU-set):  RCX=user RIP, R11=user RFLAGS, RSP=user RSP, IF=0
//
// Strategy:
//  1. Save R11 (user RFLAGS) to static SYSCALL_USER_RFLAGS.
//  2. Move user RSP into R11, switch RSP to KERNEL_RSP0.
//  3. Build a 5-field iretq frame so we reuse the existing TrapFrame
//     layout and return via iretq (no sysretq complexity).
//  4. Push 15 GPRs, call handler, restore, iretq.
//
// Clobbers: RCX (user RIP) and R11 (user RFLAGS) — identical to what the
// `syscall` instruction itself clobbers; userspace wrappers already declare both.
// AT&T syntax so that SYMBOL(%rip) generates R_X86_64_PC32 (PC-relative),
// required for PIE linking.  Intel-mode bare [SYMBOL] generates R_X86_64_32S.
global_asm!(
    ".global syscall_entry_fast",
    "syscall_entry_fast:",

    // On entry (CPU): %rcx=user RIP, %r11=user RFLAGS, %rsp=user RSP, IF=0.

    // 1. Save user RFLAGS (%r11) before repurposing %r11 for user RSP.
    "movq %r11, SYSCALL_USER_RFLAGS(%rip)",

    // 2. %r11 ← user RSP; switch %rsp to kernel stack.
    "movq %rsp, %r11",
    "movq KERNEL_RSP0(%rip), %rsp",

    // 3. Build 5-field iretq frame: SS, user-RSP, RFLAGS, CS, user-RIP.
    "pushq $0x1b",                             // user SS  (ring-3 data: 0x18|3)
    "pushq %r11",                              // user RSP
    "movq SYSCALL_USER_RFLAGS(%rip), %r11",   // reload user RFLAGS
    "pushq %r11",                              // user RFLAGS
    "pushq $0x23",                             // user CS  (ring-3 code: 0x20|3)
    "pushq %rcx",                              // user RIP

    // 4. Save 15 GPRs — same layout as TrapFrame.
    //    %rcx=user RIP, %r11=user RFLAGS: both architecturally clobbered by
    //    `syscall`; userspace wrappers already declare out("rcx")_ / out("r11")_.
    //    %r10: original user value preserved (never touched above).
    "pushq %rax",
    "pushq %rbx",
    "pushq %rcx",
    "pushq %rdx",
    "pushq %rsi",
    "pushq %rdi",
    "pushq %rbp",
    "pushq %r8",
    "pushq %r9",
    "pushq %r10",
    "pushq %r11",
    "pushq %r12",
    "pushq %r13",
    "pushq %r14",
    "pushq %r15",

    "movq %rsp, %rdi",
    "call syscall_handler_asm",

    // Write return value to saved %rax slot: [%rsp+0]=r15 … [%rsp+112]=rax.
    "movq %rax, 112(%rsp)",

    "popq %r15",
    "popq %r14",
    "popq %r13",
    "popq %r12",
    "popq %r11",
    "popq %r10",
    "popq %r9",
    "popq %r8",
    "popq %rbp",
    "popq %rdi",
    "popq %rsi",
    "popq %rdx",
    "popq %rcx",
    "popq %rbx",
    "popq %rax",

    "iretq",
    options(att_syntax),
);

#[repr(C)]
struct SavedRegisters {
    r15: u64, r14: u64, r13: u64, r12: u64,
    r11: u64, r10: u64, r9: u64, r8: u64,
    rbp: u64, rdi: u64, rsi: u64, rdx: u64,
    rcx: u64, rbx: u64, rax: u64,
}

// ============================================================================
// CURRENT-SYSCALL TRAPFRAME
// ============================================================================

/// Address of the full TrapFrame on the kernel stack at syscall entry.
/// SavedRegisters is the first 15 fields of TrapFrame; the hardware iretq
/// fields (rip, cs, rflags, rsp, ss) follow immediately in memory.
/// Single-CPU — safe under cli.
static CURRENT_SYSCALL_TF: AtomicU64 = AtomicU64::new(0);

/// The current syscall's on-stack TrapFrame pointer — for blocking file
/// implementations (e.g. `pipe.rs`) that need it outside this module,
/// mirroring how `sync::sys_futex`/`process_ctl::sys_nanosleep` use it
/// internally.
pub(crate) fn current_tf_ptr() -> *const TrapFrame {
    CURRENT_SYSCALL_TF.load(Ordering::Relaxed) as *const TrapFrame
}

// WAIT_WAITER has been removed — per-process waiting_for field in Process is used instead.
// This supports multiple concurrent waitpid() callers (e.g. shell + ipc_ping).

#[no_mangle]
extern "C" fn syscall_handler_asm(regs: &SavedRegisters) -> i64 {
    // Store the TrapFrame pointer (SavedRegisters shares the same layout as
    // the first 15 fields of TrapFrame; hardware pushed rip/cs/rflags/rsp/ss
    // immediately after on the kernel stack).
    CURRENT_SYSCALL_TF.store(regs as *const SavedRegisters as u64, Ordering::Relaxed);
    let ret = syscall_handler(regs.rax, regs.rdi, regs.rsi, regs.rdx, regs.r10, regs.r8, regs.r9);

    // Deliver pending signals before returning to user mode. This is the
    // one "about to iretq into a process" point with no natural
    // `jump_to_trapframe` call to hang the check off of (the asm caller
    // just pops registers and `iretq`s directly), so it's handled here
    // instead of via `trapframe::jump_to_user` — see that function's doc
    // comment for the general design this mirrors.
    let tf_ptr = CURRENT_SYSCALL_TF.load(Ordering::Relaxed) as *mut TrapFrame;
    unsafe { (*tf_ptr).rax = ret as u64; }

    let irq = super::irq_guard::InterruptGuard::new();
    let resolved_tf = {
        let mut sched = super::scheduler::local_scheduler();
        let tf = sched.resolve_signals(tf_ptr as *const TrapFrame);
        // Same reasoning as `trapframe::jump_to_user` — must run regardless
        // of which branch below fires, since the signal-killed-and-rescheduled
        // branch jumps via a raw `jump_to_trapframe` that bypasses
        // `jump_to_user` entirely.
        sched.resolve_wait_status();
        tf
    };

    if resolved_tf != tf_ptr as *const TrapFrame {
        // A default-terminate signal was pending: `resolve_signals` already
        // killed this process and picked a different one to run instead.
        // `irq` is deliberately never dropped on this path (no `sti` runs) —
        // the target process's own `iretq` restores its own RFLAGS.IF.
        unsafe { super::trapframe::jump_to_trapframe(resolved_tf) }
    }

    drop(irq);
    ret
}

#[derive(Debug, Clone, Copy)]
#[repr(u64)]
pub enum SyscallNumber {
    Read = 0,
    Write = 1,
    Open = 2,
    Close = 3,
    Stat = 4,
    Fstat = 5,
    Lstat = 6,
    Sigaction = 13,
    Sigprocmask = 14,
    Sigreturn = 15,
    Poll = 7,
    Lseek = 8,
    Mmap = 9,
    Getcwd = 79,
    Chdir = 80,
    Rename = 82,
    Mkdir = 83,
    Rmdir = 84,
    Unlink = 87,
    Readlink = 89,
    Symlink = 88,
    Access = 21,
    Chmod = 90,
    Fchmod = 91,
    Dup = 32,
    Dup2 = 33,
    Fcntl = 72,
    Pipe = 22,
    Munmap = 11,
    Brk = 12,
    Ioctl = 16,
    Writev = 20,
    Yield = 24,
    Nanosleep = 35,
    GetPid = 39,
    Socket = 41,
    Connect = 42,
    Accept = 43,
    Sendmsg = 46,
    Recvmsg = 47,
    Bind = 49,
    Clone = 56,
    Fork = 57,
    Exec = 59,
    Exit = 60,
    Waitpid = 61,
    Kill = 62,
    Setpgid = 109,
    Setsid = 112,
    Getpgid = 121,
    ArchPrctl = 158,
    Futex = 202,
    EpollCreate = 213,
    GetDents64 = 217,
    SetTidAddress = 218,
    ClockGettime = 228,
    EpollWait = 232,
    EpollCtl = 233,
    // Custom kernel syscalls (above Linux range)
    UptimeMs = 400,
    UptimeSec = 401,
    MemInfoKb = 402,
    KdebugCtl = 403,
    Statvfs = 404,
}

impl SyscallNumber {
    pub fn from_u64(n: u64) -> Option<Self> {
        match n {
            0  => Some(Self::Read),
            1  => Some(Self::Write),
            2  => Some(Self::Open),
            3  => Some(Self::Close),
            4  => Some(Self::Stat),
            5  => Some(Self::Fstat),
            6  => Some(Self::Lstat),
            13 => Some(Self::Sigaction),
            14 => Some(Self::Sigprocmask),
            15 => Some(Self::Sigreturn),
            7  => Some(Self::Poll),
            8  => Some(Self::Lseek),
            9  => Some(Self::Mmap),
            79 => Some(Self::Getcwd),
            80 => Some(Self::Chdir),
            82 => Some(Self::Rename),
            83 => Some(Self::Mkdir),
            84 => Some(Self::Rmdir),
            87 => Some(Self::Unlink),
            89 => Some(Self::Readlink),
            88 => Some(Self::Symlink),
            21 => Some(Self::Access),
            90 => Some(Self::Chmod),
            91 => Some(Self::Fchmod),
            32 => Some(Self::Dup),
            33 => Some(Self::Dup2),
            72 => Some(Self::Fcntl),
            22 => Some(Self::Pipe),
            11 => Some(Self::Munmap),
            12 => Some(Self::Brk),
            16 => Some(Self::Ioctl),
            20 => Some(Self::Writev),
            24 => Some(Self::Yield),
            35 => Some(Self::Nanosleep),
            39 => Some(Self::GetPid),
            41 => Some(Self::Socket),
            42 => Some(Self::Connect),
            43 => Some(Self::Accept),
            46 => Some(Self::Sendmsg),
            47 => Some(Self::Recvmsg),
            49 => Some(Self::Bind),
            56 => Some(Self::Clone),
            57 => Some(Self::Fork),
            59 => Some(Self::Exec),
            60 => Some(Self::Exit),
            61 => Some(Self::Waitpid),
            62 => Some(Self::Kill),
            109 => Some(Self::Setpgid),
            112 => Some(Self::Setsid),
            121 => Some(Self::Getpgid),
            158 => Some(Self::ArchPrctl),
            202 => Some(Self::Futex),
            213 => Some(Self::EpollCreate),
            217 => Some(Self::GetDents64),
            218 => Some(Self::SetTidAddress),
            228 => Some(Self::ClockGettime),
            232 => Some(Self::EpollWait),
            233 => Some(Self::EpollCtl),
            400 => Some(Self::UptimeMs),
            401 => Some(Self::UptimeSec),
            402 => Some(Self::MemInfoKb),
            403 => Some(Self::KdebugCtl),
            404 => Some(Self::Statvfs),
            _ => None,
        }
    }
}

pub type SyscallResult = i64;

#[allow(dead_code)]
pub mod errno {
    pub const EPERM: i64 = -1;
    pub const ENOENT: i64 = -2;
    pub const ESRCH: i64 = -3;
    pub const ECHILD: i64 = -10;
    pub const EINTR: i64 = -4;
    pub const EIO: i64 = -5;
    pub const ENXIO: i64 = -6;
    pub const E2BIG: i64 = -7;
    pub const EBADF: i64 = -9;
    pub const ENOMEM: i64 = -12;
    pub const EACCES: i64 = -13;
    pub const EFAULT: i64 = -14;
    pub const ENOTBLK: i64 = -15;
    pub const EBUSY: i64 = -16;
    pub const EEXIST: i64 = -17;
    pub const ENOTDIR: i64 = -20;
    pub const EINVAL: i64 = -22;
    pub const ENOTTY: i64 = -25;
    pub const ESPIPE: i64 = -29;
    pub const ENOSPC: i64 = -28;
    pub const ERANGE: i64 = -34;
    pub const ENOSYS: i64 = -38;
    pub const ELOOP: i64 = -40;
    pub const EAGAIN: i64 = -11;
    pub const EWOULDBLOCK: i64 = -11;
    pub const EPIPE: i64 = -32;
    pub const ENOTSOCK: i64 = -88;
    pub const ENOTCONN: i64 = -107;
    pub const ETIMEDOUT: i64 = -110;
    pub const ECONNREFUSED: i64 = -111;
}

// ============================================================================
// SAFE HELPERS
// ============================================================================

fn with_current_process<F>(f: F) -> SyscallResult
where
    F: FnOnce(&mut super::Process) -> SyscallResult,
{
    let mut guard = super::irq_guard::SchedGuard::lock();
    match guard.running_mut() {
        Some(proc) => f(proc),
        None => errno::ESRCH,
    }
}

fn with_scheduler<F>(f: F) -> SyscallResult
where
    F: FnOnce(&mut super::scheduler::Scheduler) -> SyscallResult,
{
    let mut guard = super::irq_guard::SchedGuard::lock();
    f(&mut guard)
}

// ============================================================================
// MEMORY VALIDATION
// ============================================================================

/// Read a null-terminated C string from user space (max 255 chars).
///
/// Caller must have already validated `ptr` via `validate_user_buffer`.
/// Returns an empty string slice on encoding errors (safe fallback).
fn read_user_str(ptr: usize) -> &'static str {
    unsafe {
        let mut len = 0usize;
        let p = ptr as *const u8;
        while len < 255 && *p.add(len) != 0 {
            len += 1;
        }
        let bytes = core::slice::from_raw_parts(p, len);
        core::str::from_utf8(bytes).unwrap_or("")
    }
}

/// Read the running process's cwd (briefly takes the scheduler lock, same
/// cli/sti discipline as `with_current_process`).
fn current_cwd() -> alloc::string::String {
    let mut guard = super::irq_guard::SchedGuard::lock();
    guard.running_mut()
        .map(|p| p.cwd.clone())
        .unwrap_or_else(|| alloc::string::String::from("/"))
}

/// Normalize a raw user-supplied path against the current process's cwd.
/// Every syscall taking a filesystem path must route it through here so
/// relative paths (and `.`/`..` inside absolute ones, e.g. `/a/../b`) work —
/// `vfs::resolve` itself assumes an already-clean absolute path.
fn resolve_path(raw: &str) -> alloc::string::String {
    crate::fs::vfs::normalize_path(&current_cwd(), raw)
}

fn validate_user_buffer(addr: u64, size: usize) -> Result<(), i64> {
    if addr == 0 {
        return Err(errno::EFAULT);
    }

    let end = addr.checked_add(size as u64)
        .ok_or(errno::EFAULT)?;

    const USER_SPACE_MAX: u64 = 0x0000_8000_0000_0000;
    if addr >= USER_SPACE_MAX || end > USER_SPACE_MAX {
        return Err(errno::EFAULT);
    }

    Ok(())
}

// ============================================================================
// SYSCALL DISPATCH
// ============================================================================

pub fn syscall_handler(
    syscall_num: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
    arg5: u64,
    _arg6: u64,
) -> SyscallResult {
    // // Debug: log all syscalls from PID >= 2 (ipc_ping + client)
    // {
    //     let pid = crate::process::scheduler::current_pid().unwrap_or(0);
    //     if pid >= 2 {
    //         serial_println!("[SYSCALL] PID {} nr={}", pid, syscall_num);
    //     }
    // }

    let syscall = match SyscallNumber::from_u64(syscall_num) {
        Some(s) => s,
        None => return errno::ENOSYS,
    };

    match syscall {
        SyscallNumber::Read => fs::sys_read(arg1 as i32, arg2 as usize, arg3 as usize),
        SyscallNumber::Write => fs::sys_write(arg1 as i32, arg2 as usize, arg3 as usize),
        SyscallNumber::Open => fs::sys_open(arg1 as usize, arg2 as i32),
        SyscallNumber::Close => fs::sys_close(arg1 as i32),
        SyscallNumber::Stat => fs::sys_stat(arg1 as usize, arg2 as usize),
        SyscallNumber::Fstat => fs::sys_fstat(arg1 as i32, arg2 as usize),
        SyscallNumber::Lstat => fs::sys_lstat(arg1 as usize, arg2 as usize),
        SyscallNumber::Sigaction => signal::sys_sigaction(arg1 as u32, arg2, arg3),
        SyscallNumber::Sigprocmask => signal::sys_sigprocmask(arg1 as i32, arg2, arg3),
        SyscallNumber::Sigreturn => signal::sys_sigreturn(),
        SyscallNumber::Poll => poll::sys_poll(arg1, arg2 as u32, arg3 as i32),
        SyscallNumber::Lseek => fs::sys_lseek(arg1 as i32, arg2 as i64, arg3 as i32),
        SyscallNumber::Mmap => fs::sys_mmap(arg1, arg2, arg3 as u32, arg4 as u32, arg5 as i32),
        SyscallNumber::Getcwd => fs::sys_getcwd(arg1 as usize, arg2 as usize),
        SyscallNumber::Chdir => fs::sys_chdir(arg1 as usize),
        SyscallNumber::Rename => fs::sys_rename(arg1 as usize, arg2 as usize),
        SyscallNumber::Mkdir => fs::sys_mkdir(arg1 as usize),
        SyscallNumber::Rmdir => fs::sys_rmdir(arg1 as usize),
        SyscallNumber::Unlink => fs::sys_unlink(arg1 as usize),
        SyscallNumber::Readlink => fs::sys_readlink(arg1 as usize, arg2 as usize, arg3 as usize),
        SyscallNumber::Symlink => fs::sys_symlink(arg1 as usize, arg2 as usize),
        SyscallNumber::Access => fs::sys_access(arg1 as usize, arg2 as i32),
        SyscallNumber::Chmod => fs::sys_chmod(arg1 as usize, arg2 as u32),
        SyscallNumber::Fchmod => fs::sys_fchmod(arg1 as i32),
        SyscallNumber::Dup => fs::sys_dup(arg1 as i32),
        SyscallNumber::Dup2 => fs::sys_dup2(arg1 as i32, arg2 as i32),
        SyscallNumber::Fcntl => fs::sys_fcntl(arg1 as i32, arg2 as i32, arg3),
        SyscallNumber::Pipe => fs::sys_pipe(arg1),
        SyscallNumber::Munmap => fs::sys_munmap(arg1, arg2),
        SyscallNumber::Brk => fs::sys_brk(arg1),
        SyscallNumber::Ioctl => fs::sys_ioctl(arg1 as i32, arg2 as u64, arg3),
        SyscallNumber::Writev => fs::sys_writev(arg1 as i32, arg2, arg3 as usize),
        SyscallNumber::Yield => process_ctl::sys_yield(),
        SyscallNumber::Nanosleep => process_ctl::sys_nanosleep(arg1),
        SyscallNumber::GetPid => process_ctl::sys_getpid(),
        SyscallNumber::Socket  => ipc::sys_socket_impl(),
        SyscallNumber::Connect => ipc::sys_connect(arg1 as i32, arg2 as usize, arg3 as usize),
        SyscallNumber::Accept  => ipc::sys_accept(arg1 as i32),
        SyscallNumber::Sendmsg => ipc::sys_sendmsg(arg1 as i32, arg2, arg3 as u32),
        SyscallNumber::Recvmsg => ipc::sys_recvmsg(arg1 as i32, arg2, arg3 as u32),
        SyscallNumber::Bind    => ipc::sys_bind_impl(arg1 as i32, arg2 as usize, arg3 as usize),
        SyscallNumber::Clone => process_ctl::sys_clone(arg1, arg2, arg3),
        SyscallNumber::Fork => process_ctl::sys_fork(),
        SyscallNumber::Exec => process_ctl::sys_exec(arg1 as usize, arg2 as usize, arg3 as usize),
        SyscallNumber::Exit => process_ctl::sys_exit(arg1 as i32),
        SyscallNumber::Waitpid => process_ctl::sys_waitpid(arg1 as i64, arg2 as usize, arg3 as i32),
        SyscallNumber::Kill => process_ctl::sys_kill(arg1 as i64, arg2 as u32),
        SyscallNumber::Setpgid => process_ctl::sys_setpgid(arg1 as i64, arg2 as i64),
        SyscallNumber::Setsid => process_ctl::sys_setsid(),
        SyscallNumber::Getpgid => process_ctl::sys_getpgid(arg1 as i64),
        SyscallNumber::ArchPrctl => process_ctl::sys_arch_prctl(arg1 as i32, arg2),
        SyscallNumber::Futex => sync::sys_futex(arg1, arg2 as i32, arg3 as i32, arg4),
        SyscallNumber::SetTidAddress => process_ctl::sys_set_tid_address(arg1),
        SyscallNumber::EpollCreate => poll::sys_epoll_create(arg1 as i32),
        SyscallNumber::GetDents64 => fs::sys_getdents64(arg1 as i32, arg2 as usize, arg3 as usize),
        SyscallNumber::ClockGettime => misc::sys_clock_gettime(arg1, arg2),
        SyscallNumber::EpollWait => poll::sys_epoll_wait(arg1 as i32, arg2, arg3 as i32, arg4 as i32),
        SyscallNumber::EpollCtl => poll::sys_epoll_ctl(arg1 as i32, arg2 as i32, arg3 as i32, arg4),
        SyscallNumber::UptimeMs => misc::sys_uptime_ms(),
        SyscallNumber::UptimeSec => misc::sys_uptime_sec(),
        SyscallNumber::MemInfoKb => misc::sys_meminfo_kb(),
        SyscallNumber::KdebugCtl => misc::sys_kdebug_ctl(arg1, arg2, arg3),
        SyscallNumber::Statvfs => fs::sys_statvfs(arg1 as usize, arg2 as usize),
    }
}
