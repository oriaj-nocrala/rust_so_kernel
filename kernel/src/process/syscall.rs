// kernel/src/process/syscall.rs
//
// All syscalls use `with_current_process` or `with_scheduler` helpers
// that guarantee cli before lock, lock dropped before sti.
//
// with_current_process uses scheduler.running_mut() for O(1) access.
//
// HISTORY:
//   - sys_exit now performs an immediate full context switch via
//     kill_and_switch_tf + jump_to_trapframe, instead of entering
//     a hlt loop and waiting up to 10ms for the timer to preempt.

use core::arch::global_asm;
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;
use crate::serial_println;
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
// STDIN / WAITPID BLOCKING GLOBALS
// ============================================================================

/// Address of the full TrapFrame on the kernel stack at syscall entry.
/// SavedRegisters is the first 15 fields of TrapFrame; the hardware iretq
/// fields (rip, cs, rflags, rsp, ss) follow immediately in memory.
/// Single-CPU — safe under cli.
static CURRENT_SYSCALL_TF: AtomicU64 = AtomicU64::new(0);

/// The current syscall's on-stack TrapFrame pointer — for blocking file
/// implementations (e.g. `pipe.rs`) that need it outside this module,
/// mirroring how `sys_futex`/`sys_nanosleep` use it internally.
pub(crate) fn current_tf_ptr() -> *const TrapFrame {
    CURRENT_SYSCALL_TF.load(Ordering::Relaxed) as *const TrapFrame
}

struct StdinWaiter {
    pid: usize,
    user_buf: u64,
}

static STDIN_WAITER: Mutex<Option<StdinWaiter>> = Mutex::new(None);

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

    unsafe { core::arch::asm!("cli"); }
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
        unsafe { super::trapframe::jump_to_trapframe(resolved_tf) }
    }

    unsafe { core::arch::asm!("sti"); }
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
    pub const ERANGE: i64 = -34;
    pub const ENOSYS: i64 = -38;
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
    unsafe { core::arch::asm!("cli"); }

    let result = {
        let mut scheduler = super::scheduler::local_scheduler();
        match scheduler.running_mut() {
            Some(proc) => f(proc),
            None => errno::ESRCH,
        }
    };

    unsafe { core::arch::asm!("sti"); }
    result
}

fn with_scheduler<F>(f: F) -> SyscallResult
where
    F: FnOnce(&mut super::scheduler::Scheduler) -> SyscallResult,
{
    unsafe { core::arch::asm!("cli"); }

    let result = {
        let mut scheduler = super::scheduler::local_scheduler();
        f(&mut scheduler)
    };

    unsafe { core::arch::asm!("sti"); }
    result
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
    unsafe { core::arch::asm!("cli"); }
    let cwd = {
        let mut scheduler = super::scheduler::local_scheduler();
        scheduler.running_mut()
            .map(|p| p.cwd.clone())
            .unwrap_or_else(|| alloc::string::String::from("/"))
    };
    unsafe { core::arch::asm!("sti"); }
    cwd
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
        SyscallNumber::Read => sys_read(arg1 as i32, arg2 as usize, arg3 as usize),
        SyscallNumber::Write => sys_write(arg1 as i32, arg2 as usize, arg3 as usize),
        SyscallNumber::Open => sys_open(arg1 as usize, arg2 as i32),
        SyscallNumber::Close => sys_close(arg1 as i32),
        SyscallNumber::Stat => sys_stat(arg1 as usize, arg2 as usize),
        SyscallNumber::Fstat => sys_fstat(arg1 as i32, arg2 as usize),
        SyscallNumber::Lstat => sys_stat(arg1 as usize, arg2 as usize), // no symlinks yet
        SyscallNumber::Sigaction => sys_sigaction(arg1 as u32, arg2, arg3),
        SyscallNumber::Sigprocmask => sys_sigprocmask(arg1 as i32, arg2, arg3),
        SyscallNumber::Sigreturn => sys_sigreturn(),
        SyscallNumber::Poll => sys_poll(arg1, arg2 as u32, arg3 as i32),
        SyscallNumber::Lseek => sys_lseek(arg1 as i32, arg2 as i64, arg3 as i32),
        SyscallNumber::Mmap => sys_mmap(arg1, arg2, arg3 as u32, arg4 as u32, arg5 as i32),
        SyscallNumber::Getcwd => sys_getcwd(arg1 as usize, arg2 as usize),
        SyscallNumber::Chdir => sys_chdir(arg1 as usize),
        SyscallNumber::Rename => sys_rename(arg1 as usize, arg2 as usize),
        SyscallNumber::Mkdir => sys_mkdir(arg1 as usize),
        SyscallNumber::Rmdir => sys_rmdir(arg1 as usize),
        SyscallNumber::Unlink => sys_unlink(arg1 as usize),
        SyscallNumber::Dup => sys_dup(arg1 as i32),
        SyscallNumber::Dup2 => sys_dup2(arg1 as i32, arg2 as i32),
        SyscallNumber::Fcntl => sys_fcntl(arg1 as i32, arg2 as i32, arg3),
        SyscallNumber::Pipe => sys_pipe(arg1),
        SyscallNumber::Munmap => sys_munmap(arg1, arg2),
        SyscallNumber::Brk => sys_brk(arg1),
        SyscallNumber::Ioctl => sys_ioctl(arg1 as i32, arg2 as u64, arg3),
        SyscallNumber::Writev => sys_writev(arg1 as i32, arg2, arg3 as usize),
        SyscallNumber::Yield => sys_yield(),
        SyscallNumber::Nanosleep => sys_nanosleep(arg1),
        SyscallNumber::GetPid => sys_getpid(),
        SyscallNumber::Socket  => sys_socket_impl(),
        SyscallNumber::Connect => sys_connect(arg1 as i32, arg2 as usize, arg3 as usize),
        SyscallNumber::Accept  => sys_accept(arg1 as i32),
        SyscallNumber::Sendmsg => sys_sendmsg(arg1 as i32, arg2, arg3 as u32),
        SyscallNumber::Recvmsg => sys_recvmsg(arg1 as i32, arg2, arg3 as u32),
        SyscallNumber::Bind    => sys_bind_impl(arg1 as i32, arg2 as usize, arg3 as usize),
        SyscallNumber::Clone => sys_clone(arg1, arg2, arg3),
        SyscallNumber::Fork => sys_fork(),
        SyscallNumber::Exec => sys_exec(arg1 as usize, arg2 as usize, arg3 as usize),
        SyscallNumber::Exit => sys_exit(arg1 as i32),
        SyscallNumber::Waitpid => sys_waitpid(arg1 as i64, arg2 as usize, arg3 as i32),
        SyscallNumber::Kill => sys_kill(arg1 as i64, arg2 as u32),
        SyscallNumber::Setpgid => sys_setpgid(arg1 as i64, arg2 as i64),
        SyscallNumber::Setsid => sys_setsid(),
        SyscallNumber::Getpgid => sys_getpgid(arg1 as i64),
        SyscallNumber::ArchPrctl => sys_arch_prctl(arg1 as i32, arg2),
        SyscallNumber::Futex => sys_futex(arg1, arg2 as i32, arg3 as i32, arg4),
        SyscallNumber::SetTidAddress => sys_set_tid_address(arg1),
        SyscallNumber::EpollCreate => sys_epoll_create(arg1 as i32),
        SyscallNumber::GetDents64 => sys_getdents64(arg1 as i32, arg2 as usize, arg3 as usize),
        SyscallNumber::ClockGettime => sys_clock_gettime(arg1, arg2),
        SyscallNumber::EpollWait => sys_epoll_wait(arg1 as i32, arg2, arg3 as i32, arg4 as i32),
        SyscallNumber::EpollCtl => sys_epoll_ctl(arg1 as i32, arg2 as i32, arg3 as i32, arg4),
        SyscallNumber::UptimeMs => sys_uptime_ms(),
        SyscallNumber::UptimeSec => sys_uptime_sec(),
        SyscallNumber::MemInfoKb => sys_meminfo_kb(),
    }
}

// ============================================================================
// SYSCALL IMPLEMENTATIONS
// ============================================================================

fn sys_read(fd: i32, buf: usize, count: usize) -> SyscallResult {
    if count == 0 {
        return 0;
    }
    if let Err(e) = validate_user_buffer(buf as u64, count) {
        return e;
    }

    if fd == 0 {
        // stdin: try to read from keyboard buffer; block if empty.
        //
        // cli prevents a race between the buffer-empty check and setting
        // STDIN_WAITER — the keyboard ISR could fire between them otherwise.
        unsafe { core::arch::asm!("cli"); }

        if let Some(c) = crate::keyboard::read_key() {
            unsafe { core::arch::asm!("sti"); }
            // Process's page table is active — write directly to user VA.
            unsafe { *(buf as *mut u8) = c as u8; }
            return 1;
        }

        // Buffer empty — register waiter and block.
        let pid = crate::process::scheduler::current_pid().unwrap_or(0);
        *STDIN_WAITER.lock() = Some(StdinWaiter { pid, user_buf: buf as u64 });
        let tf_ptr = CURRENT_SYSCALL_TF.load(Ordering::Relaxed) as *const TrapFrame;
        // cli is still in effect; block_stdin_read never returns.
        block_stdin_read(tf_ptr)
    } else {
        // Continuous cli from before the fd lookup through either the fast
        // return or the block — same shape as sys_futex's FUTEX_WAIT. This
        // matters because a pipe's `read()` may return WouldBlock, at which
        // point this function does the actual block_current/jump_to_trapframe
        // itself; that must never happen while SCHEDULER or the fd table are
        // still held (SCHEDULER: self-deadlock, spin::Mutex isn't reentrant;
        // fd table: jump_to_trapframe diverges, so a guard alive across it
        // would never run its Drop and stays locked forever — see sys_close's
        // doc comment for the same class of hazard).
        unsafe { core::arch::asm!("cli"); }

        let files = {
            let scheduler = super::scheduler::local_scheduler();
            match scheduler.running_ref() {
                Some(proc) => proc.files.clone(),
                None => { unsafe { core::arch::asm!("sti"); } return errno::ESRCH; }
            }
        };

        let result = {
            let mut files_guard = files.lock();
            match files_guard.get_mut(fd as usize) {
                Ok(file) => {
                    let buffer = unsafe {
                        core::slice::from_raw_parts_mut(buf as *mut u8, count)
                    };
                    file.read(buffer)
                }
                Err(_) => { unsafe { core::arch::asm!("sti"); } return errno::EBADF; }
            }
        };

        match result {
            Ok(n) => { unsafe { core::arch::asm!("sti"); } n as i64 }
            Err(super::file::FileError::WouldBlock) => {
                let tf_ptr = current_tf_ptr();
                let next_tf = {
                    let mut scheduler = super::scheduler::local_scheduler();
                    scheduler.block_current(tf_ptr)
                };
                unsafe { super::trapframe::jump_to_user(next_tf) }
            }
            Err(_) => { unsafe { core::arch::asm!("sti"); } errno::EIO }
        }
    }
}

/// Block the calling process waiting for keyboard input.
///
/// cli must already be in effect when this is called.
/// Saves the current TrapFrame into the process Box, moves the process to the
/// wait_queue, and jumps to the next Ready process.  Never returns.
fn block_stdin_read(current_tf: *const TrapFrame) -> ! {
    let next_tf = {
        let mut sched = super::scheduler::local_scheduler();
        sched.block_current(current_tf)
        // Lock dropped here; sti happens via iretq of the next process.
    };
    unsafe { super::trapframe::jump_to_user(next_tf) }
}

/// Called by the keyboard ISR after a key is pushed into the buffer.
///
/// If a process is blocked on stdin, delivers the character to its user
/// buffer (via physical-memory translation), sets rax=1 in its saved
/// TrapFrame, and moves it back to the run queue.
/// Deliver `sig` to every process in group `pgid` — used by the tty line
/// discipline (Ctrl-C/Ctrl-Z, see `tty::feed_input`) from ISR context,
/// where interrupts are already off, so (like `stdin_wakeup` below) this
/// locks the scheduler directly with no explicit cli/sti.
pub(crate) fn send_to_group(pgid: u32, sig: u32) {
    super::scheduler::local_scheduler().queue_signal_to_group(pgid, sig);
}

pub(crate) fn stdin_wakeup() {
    // Take the waiter atomically — if no one is waiting, return immediately.
    let waiter = {
        let mut w = STDIN_WAITER.lock();
        w.take()
    };
    let Some(waiter) = waiter else { return; };

    // Consume the character that was just pushed by the keyboard ISR.
    let Some(c) = crate::keyboard::read_key() else {
        // Shouldn't happen (ISR pushed it just before calling us), but be safe.
        *STDIN_WAITER.lock() = Some(waiter);
        return;
    };

    let phys_offset = crate::memory::physical_memory_offset();
    let user_buf = waiter.user_buf;
    let pid = waiter.pid;

    let mut sched = super::scheduler::local_scheduler();

    // Find the blocked process, translate its user buffer to a kernel VA,
    // write the character, and set rax=1 as the syscall return value.
    for proc in sched.wait_queue.iter_mut() {
        if proc.pid.0 == pid && matches!(proc.state, crate::process::ProcessState::Blocked) {
            use x86_64::{VirtAddr, structures::paging::{Page, Size4KiB}};

            let page = Page::<Size4KiB>::containing_address(VirtAddr::new(user_buf));
            let offset = user_buf & 0xFFF;

            if let Some(frame) = unsafe { proc.address_space.translate_page(page) } {
                let dst = phys_offset + frame.start_address().as_u64() + offset;
                unsafe { *(dst.as_mut_ptr::<u8>()) = c as u8; }
                proc.trapframe.rax = 1; // syscall return value: 1 byte read
            }
            break;
        }
    }

    sched.wake(pid);
}

/// sys_write — same non-reentrant shape as sys_read's fd>0 branch (see its
/// comment): the fd-table lock must be released before any potential block,
/// since `file.write()` (e.g. a full pipe) may need to register a waiter and
/// return `WouldBlock`, at which point *this* function does the actual
/// cli/block_current/jump_to_trapframe dance itself.
fn sys_write(fd: i32, buf: usize, count: usize) -> SyscallResult {
    if let Err(e) = validate_user_buffer(buf as u64, count) {
        return e;
    }

    unsafe { core::arch::asm!("cli"); }

    let files = {
        let scheduler = super::scheduler::local_scheduler();
        match scheduler.running_ref() {
            Some(proc) => proc.files.clone(),
            None => { unsafe { core::arch::asm!("sti"); } return errno::ESRCH; }
        }
    };

    let result = {
        let mut files_guard = files.lock();
        match files_guard.get_mut(fd as usize) {
            Ok(file) => {
                let buffer = unsafe {
                    core::slice::from_raw_parts(buf as *const u8, count)
                };
                file.write(buffer)
            }
            Err(_) => { unsafe { core::arch::asm!("sti"); } return errno::EBADF; }
        }
    };

    match result {
        Ok(n) => { unsafe { core::arch::asm!("sti"); } n as i64 }
        Err(super::file::FileError::BrokenPipe) => { unsafe { core::arch::asm!("sti"); } errno::EPIPE }
        Err(super::file::FileError::WouldBlock) => {
            let tf_ptr = current_tf_ptr();
            let next_tf = {
                let mut scheduler = super::scheduler::local_scheduler();
                scheduler.block_current(tf_ptr)
            };
            unsafe { super::trapframe::jump_to_user(next_tf) }
        }
        Err(_) => { unsafe { core::arch::asm!("sti"); } errno::EIO }
    }
}

fn sys_open(path_ptr: usize, flags: i32) -> SyscallResult {
    // Validation BEFORE cli — no lock needed
    if let Err(e) = validate_user_buffer(path_ptr as u64, 256) {
        return e;
    }

    let path = read_user_str(path_ptr);
    if path.is_empty() { return errno::EINVAL; }
    let path = resolve_path(path);

    // Resolve through VFS: /dev/* → drivers, /bin/* → initramfs, …
    // Box allocation uses Slab (different lock from SCHEDULER).
    let handle = match crate::fs::vfs::open(&path, crate::fs::types::OpenFlags(flags)) {
        Ok(h)  => h,
        Err(e) => return e.as_i64(),
    };

    // Only take scheduler lock for the FD table insertion
    with_current_process(|proc| {
        match proc.files.lock().allocate(handle) {
            Ok(fd) => fd as i64,
            Err(_) => errno::EINVAL,
        }
    })
}

fn sys_stat(path_ptr: usize, stat_ptr: usize) -> SyscallResult {
    use crate::fs::types::Stat;
    if let Err(e) = validate_user_buffer(path_ptr as u64, 1) { return e; }
    if let Err(e) = validate_user_buffer(stat_ptr as u64, core::mem::size_of::<Stat>()) { return e; }

    let path = read_user_str(path_ptr);
    let path = resolve_path(path);
    match crate::fs::vfs::stat(&path) {
        Err(e)   => e.as_i64(),
        Ok(stat) => {
            unsafe { core::ptr::write(stat_ptr as *mut Stat, stat); }
            0
        }
    }
}

fn sys_fstat(fd: i32, stat_ptr: usize) -> SyscallResult {
    use crate::fs::types::Stat;
    if let Err(e) = validate_user_buffer(stat_ptr as u64, core::mem::size_of::<Stat>()) { return e; }

    // Retrieve stat outside with_current_process to avoid holding the scheduler lock
    // while doing a potentially expensive write.
    let stat_result: Option<Stat> = {
        let mut sched = super::scheduler::local_scheduler();
        sched.running_mut().and_then(|proc| {
            proc.files.lock().get(fd as usize).ok().and_then(|f| f.stat())
        })
    };

    match stat_result {
        None       => errno::EBADF,
        Some(stat) => {
            unsafe { core::ptr::write(stat_ptr as *mut Stat, stat); }
            0
        }
    }
}

/// mkdir(83): long mkdir(const char *path) — no `mode` param, matching
/// this kernel's `open()` (which also drops the POSIX `mode` argument):
/// nothing here enforces permission bits, so there's nothing to store it
/// in.
fn sys_mkdir(path_ptr: usize) -> SyscallResult {
    if let Err(e) = validate_user_buffer(path_ptr as u64, 1) { return e; }
    let path = read_user_str(path_ptr);
    if path.is_empty() { return errno::EINVAL; }
    let path = resolve_path(path);
    match crate::fs::vfs::mkdir(&path) {
        Ok(())  => 0,
        Err(e)  => e.as_i64(),
    }
}

/// rmdir(84): long rmdir(const char *path)
fn sys_rmdir(path_ptr: usize) -> SyscallResult {
    if let Err(e) = validate_user_buffer(path_ptr as u64, 1) { return e; }
    let path = read_user_str(path_ptr);
    if path.is_empty() { return errno::EINVAL; }
    let path = resolve_path(path);
    match crate::fs::vfs::rmdir(&path) {
        Ok(())  => 0,
        Err(e)  => e.as_i64(),
    }
}

/// unlink(87): long unlink(const char *path)
fn sys_unlink(path_ptr: usize) -> SyscallResult {
    if let Err(e) = validate_user_buffer(path_ptr as u64, 1) { return e; }
    let path = read_user_str(path_ptr);
    if path.is_empty() { return errno::EINVAL; }
    let path = resolve_path(path);
    match crate::fs::vfs::unlink(&path) {
        Ok(())  => 0,
        Err(e)  => e.as_i64(),
    }
}

/// rename(82): long rename(const char *old_path, const char *new_path)
fn sys_rename(old_path_ptr: usize, new_path_ptr: usize) -> SyscallResult {
    if let Err(e) = validate_user_buffer(old_path_ptr as u64, 1) { return e; }
    if let Err(e) = validate_user_buffer(new_path_ptr as u64, 1) { return e; }
    let old_path = read_user_str(old_path_ptr);
    let new_path = read_user_str(new_path_ptr);
    if old_path.is_empty() || new_path.is_empty() { return errno::EINVAL; }
    let old_path = resolve_path(old_path);
    let new_path = resolve_path(new_path);
    match crate::fs::vfs::rename(&old_path, &new_path) {
        Ok(())  => 0,
        Err(e)  => e.as_i64(),
    }
}

/// getcwd(79): long getcwd(char *buffer, size_t size)
///
/// This kernel's raw-syscall convention (unlike glibc's libc-level
/// `getcwd()`, which returns a `char*`) matches Linux's actual syscall:
/// returns the number of bytes written to `buffer` (including the NUL) on
/// success, or a negative errno. `ERANGE` if `size` is too small to hold
/// the current path + NUL.
fn sys_getcwd(buf_ptr: usize, size: usize) -> SyscallResult {
    if size == 0 { return errno::EINVAL; }
    if let Err(e) = validate_user_buffer(buf_ptr as u64, size) { return e; }

    let cwd = current_cwd();
    let needed = cwd.len() + 1; // + NUL
    if needed > size {
        return errno::ERANGE;
    }

    unsafe {
        core::ptr::copy_nonoverlapping(cwd.as_ptr(), buf_ptr as *mut u8, cwd.len());
        *(buf_ptr as *mut u8).add(cwd.len()) = 0;
    }
    needed as SyscallResult
}

/// chdir(80): long chdir(const char *path)
///
/// Resolves `path` (relative to the current cwd if not absolute) and, if it
/// names an existing directory, replaces the process's cwd with the clean
/// normalized form — never the raw user string, so a later `getcwd()` never
/// echoes back `..`/`.`/double-slashes the caller happened to type.
fn sys_chdir(path_ptr: usize) -> SyscallResult {
    if let Err(e) = validate_user_buffer(path_ptr as u64, 1) { return e; }
    let path = read_user_str(path_ptr);
    if path.is_empty() { return errno::EINVAL; }
    let path = resolve_path(path);

    let inode = match crate::fs::vfs::resolve(&path) {
        Ok(i)  => i,
        Err(e) => return e.as_i64(),
    };
    if inode.file_type() != crate::fs::types::FileType::Directory {
        return errno::ENOTDIR;
    }

    with_current_process(|proc| {
        proc.cwd = path;
        0
    })
}

fn sys_getdents64(fd: i32, buf_ptr: usize, count: usize) -> SyscallResult {
    if let Err(e) = validate_user_buffer(buf_ptr as u64, count) { return e; }

    with_current_process(|proc| {
        match proc.files.lock().get_mut(fd as usize) {
            Err(_) => errno::EBADF,
            Ok(f)  => {
                let buf = unsafe {
                    core::slice::from_raw_parts_mut(buf_ptr as *mut u8, count)
                };
                f.getdents64(buf)
            }
        }
    })
}

/// sys_close — close a file descriptor.
///
/// Deliberately does NOT use `with_current_process`: that helper holds the
/// SCHEDULER lock across the whole closure, but closing a pipe end can drop
/// a `Box<dyn FileHandle>` whose `Drop` impl needs to wake a peer blocked on
/// the other end of the pipe (via `local_scheduler()` + `wake()`). Dropping
/// the handle while SCHEDULER is already held would self-deadlock (spin
/// locks aren't reentrant). Instead: clone the `Arc<Mutex<FileDescriptorTable>>`
/// under a short scheduler-lock scope, release it, then close outside any
/// scheduler lock — same shape sys_fork/sys_exec use for lock-crossing work.
fn sys_close(fd: i32) -> SyscallResult {
    let files = {
        unsafe { core::arch::asm!("cli"); }
        let scheduler = super::scheduler::local_scheduler();
        let result = match scheduler.running_ref() {
            Some(proc) => proc.files.clone(),
            None => { unsafe { core::arch::asm!("sti"); } return errno::ESRCH; }
        };
        unsafe { core::arch::asm!("sti"); }
        result
    };

    // cli here too: closing a pipe end can run its Drop impl (deallocating,
    // possibly waking a peer via a fresh, independent SCHEDULER lock/unlock
    // — safe, since no lock is already held across this). Without cli, nothing
    // stops a timer tick from preempting mid-close, saving this process's
    // trapframe with cs = kernel (0x08) instead of user (0x23) — and later
    // treating that stale kernel-mode snapshot as a live user context (e.g.
    // for signal delivery, which needs a genuine user rsp/rip) corrupts
    // whatever that kernel rsp actually pointed at.
    unsafe { core::arch::asm!("cli"); }
    let result = files.lock().close(fd as usize);
    unsafe { core::arch::asm!("sti"); }
    match result {
        Ok(_) => 0,
        Err(_) => errno::EBADF,
    }
}

/// dup(32): long dup(int fd)
///
/// Never closes anything (always lands on a *free* slot), so — unlike
/// dup2/close — there's no pipe-Drop-while-locked hazard here; plain
/// `with_current_process` is fine.
fn sys_dup(fd: i32) -> SyscallResult {
    if fd < 0 { return errno::EBADF; }
    with_current_process(|proc| {
        match proc.files.lock().dup(fd as usize, 0) {
            Ok(newfd) => newfd as SyscallResult,
            Err(_) => errno::EBADF,
        }
    })
}

/// dup2(33): long dup2(int oldfd, int newfd)
///
/// Same lock-dropping shape as `sys_close` (see its doc comment): if
/// `newfd` is already open, installing the dup closes whatever was there
/// first, which can run a pipe's Drop impl and deadlock if SCHEDULER were
/// still held.
fn sys_dup2(oldfd: i32, newfd: i32) -> SyscallResult {
    if oldfd < 0 || newfd < 0 { return errno::EBADF; }

    let files = {
        unsafe { core::arch::asm!("cli"); }
        let scheduler = super::scheduler::local_scheduler();
        let result = match scheduler.running_ref() {
            Some(proc) => proc.files.clone(),
            None => { unsafe { core::arch::asm!("sti"); } return errno::ESRCH; }
        };
        unsafe { core::arch::asm!("sti"); }
        result
    };

    unsafe { core::arch::asm!("cli"); }
    let result = files.lock().dup2(oldfd as usize, newfd as usize);
    unsafe { core::arch::asm!("sti"); }
    match result {
        Ok(nf) => nf as SyscallResult,
        Err(_) => errno::EBADF,
    }
}

// fcntl(2) commands this kernel understands — real Linux x86-64 values.
const F_DUPFD: i32 = 0;
const F_GETFD: i32 = 1;
const F_SETFD: i32 = 2;
const F_GETFL: i32 = 3;
const F_SETFL: i32 = 4;
const F_DUPFD_CLOEXEC: i32 = 1030;

/// fcntl(72): long fcntl(int fd, int cmd, unsigned long arg)
///
/// Only F_DUPFD/F_DUPFD_CLOEXEC actually do something, and they do the
/// same thing: this kernel has no per-fd close-on-exec flag anywhere, so
/// there's nothing for the CLOEXEC half to set differently. F_GETFD/
/// F_SETFD/F_GETFL/F_SETFL are stubbed — `FileDescriptorTable` has no
/// per-fd flags storage to back real answers with, so the getters always
/// report 0 and the setters silently accept anything (after checking `fd`
/// is actually open). Good enough for callers that only care whether the
/// call succeeded, not a real flags implementation.
fn sys_fcntl(fd: i32, cmd: i32, arg: u64) -> SyscallResult {
    if fd < 0 { return errno::EBADF; }
    match cmd {
        F_DUPFD | F_DUPFD_CLOEXEC => {
            with_current_process(|proc| {
                match proc.files.lock().dup(fd as usize, arg as usize) {
                    Ok(newfd) => newfd as SyscallResult,
                    Err(_) => errno::EBADF,
                }
            })
        }
        F_GETFD | F_SETFD | F_GETFL | F_SETFL => {
            with_current_process(|proc| {
                match proc.files.lock().get(fd as usize) {
                    Ok(_)  => 0,
                    Err(_) => errno::EBADF,
                }
            })
        }
        _ => errno::EINVAL,
    }
}

/// pipe(22): long pipe(int pipefd[2])
///
/// pipefd[0] = read end, pipefd[1] = write end (matches Linux). Both fds
/// start with one open reference; `fork()` duplicates them (see
/// `FileHandle::dup` / `FileDescriptorTable::clone`), `clone()` (threads)
/// shares them automatically via the shared fd table.
fn sys_pipe(pipefd_ptr: u64) -> SyscallResult {
    if let Err(e) = validate_user_buffer(pipefd_ptr, 8) {
        return e;
    }

    let (read_end, write_end) = super::pipe::create();

    with_current_process(|proc| {
        let mut files = proc.files.lock();
        let rfd = match files.allocate(alloc::boxed::Box::new(read_end)) {
            Ok(fd) => fd,
            Err(_) => return errno::EINVAL,
        };
        let wfd = match files.allocate(alloc::boxed::Box::new(write_end)) {
            Ok(fd) => fd,
            Err(_) => {
                // Rolling back by dropping the read end here (while SCHEDULER
                // is held via with_current_process) is safe ONLY because this
                // pipe was just created in this same call and has never been
                // exposed to another process — its write_waiter is always
                // None, so PipeReadEnd::drop() cannot reach the wake path
                // that would need to re-lock SCHEDULER. Don't reuse this
                // pattern for closing an fd a process has actually had open.
                let _ = files.close(rfd);
                return errno::EINVAL;
            }
        };
        drop(files);

        unsafe {
            let ptr = pipefd_ptr as *mut i32;
            ptr.write(rfd as i32);
            ptr.add(1).write(wfd as i32);
        }
        0
    })
}

/// mmap(9): void *mmap(void *addr, size_t length, int prot, int flags, int fd, off_t offset)
///
/// Only MAP_ANONYMOUS (0x20) is supported.  fd must be -1.
/// Returns the mapped virtual address on success, or ENOMEM / EINVAL.
fn sys_mmap(addr: u64, length: u64, prot: u32, flags: u32, fd: i32) -> SyscallResult {
    const MAP_ANONYMOUS: u32 = 0x20;
    if flags & MAP_ANONYMOUS == 0 || fd != -1 {
        return errno::EINVAL;
    }
    with_current_process(|proc| {
        match proc.address_space.sys_mmap_anon(addr, length, prot) {
            Ok(vaddr) => vaddr as i64,
            Err(_)    => errno::ENOMEM,
        }
    })
}

/// munmap(11): int munmap(void *addr, size_t length)
///
/// Removes the VMA at `addr` and frees any demand-paged frames.
/// Requires exact match on addr and length (no partial unmap).
fn sys_munmap(addr: u64, length: u64) -> SyscallResult {
    with_current_process(|proc| {
        match unsafe { proc.address_space.sys_munmap(addr, length) } {
            Ok(())  => 0,
            Err(_)  => errno::EINVAL,
        }
    })
}

// ── lseek(8) ───────────────────────────────────────────────────────────────

/// lseek(8): off_t lseek(int fd, off_t offset, int whence)
///
/// Seeking on character devices (console, keyboard) is not meaningful;
/// return ESPIPE just like Linux does for pipes.  When we have a VFS,
/// this will delegate to the file's seek method.
fn sys_lseek(fd: i32, _offset: i64, _whence: i32) -> SyscallResult {
    if fd < 0 || fd >= 16 { return errno::EBADF; }
    // All current fds are character devices — not seekable.
    errno::ESPIPE
}

// ── brk(12) ────────────────────────────────────────────────────────────────

/// brk(12): int brk(void *addr)
///
/// Returning 0 (failure, current break unchanged) tells mlibc to fall
/// back to mmap(MAP_ANONYMOUS) for heap allocation, which we support.
fn sys_brk(_addr: u64) -> SyscallResult {
    0
}

// ── ioctl(16) ──────────────────────────────────────────────────────────────

/// ioctl(16): int ioctl(int fd, unsigned long request, ...)
///
/// Backs mlibc's `sys_isatty` (via TCGETS with a null pointer — kept
/// working exactly as before), the real `tcgetattr`/`tcsetattr` sysdeps
/// hooks (which this port implements as thin TCGETS/TCSETS* wrappers, same
/// as real glibc does — see `mlibc-port/.../generic.cpp::sys_tcgetattr`),
/// `tcgetpgrp`/`tcsetpgrp` (TIOCGPGRP/TIOCSPGRP — mlibc calls `ioctl()`
/// directly for these, not a sysdeps hook), and terminal-size queries.
fn sys_ioctl(fd: i32, request: u64, argp: u64) -> SyscallResult {
    const TCGETS: u64 = 0x5401;
    const TCSETS: u64 = 0x5402;
    const TCSETSW: u64 = 0x5403;
    const TCSETSF: u64 = 0x5404;
    const TIOCGWINSZ: u64 = 0x5413;
    const TIOCGPGRP: u64 = 0x540F;
    const TIOCSPGRP: u64 = 0x5410;

    if fd < 0 { return errno::EBADF; }

    // A handle counts as a tty if it's actually backed by the console
    // driver (serial or framebuffer) — checked by the handle's identity,
    // not by fd number. A fixed "fd <= 2" check breaks the moment a tty fd
    // gets dup'd to something higher, which is exactly what real job
    // control setup does: ash's `setjobctl()` (shell/ash.c) opens/falls
    // back to the console, then `fcntl(fd, F_DUPFD_CLOEXEC, 10)`s it to a
    // fd >= 10 before calling `tcgetpgrp()` on *that* fd — confirmed live,
    // this was silently sending ash down its "can't access tty, job
    // control turned off" fallback path.
    let is_tty = {
        unsafe { core::arch::asm!("cli"); }
        let result = {
            let mut sched = super::scheduler::local_scheduler();
            sched.running_mut().map(|proc| {
                proc.files.lock().get(fd as usize).ok()
                    .map(|f| matches!(f.name(), "serial" | "fb"))
                    .unwrap_or(false)
            }).unwrap_or(false)
        };
        unsafe { core::arch::asm!("sti"); }
        result
    };

    match request {
        TCGETS => {
            if !is_tty { return errno::ENOTTY; }
            // `argp == 0` is `sys_isatty`'s "just probe the return code"
            // call — nothing to write, and that's fine.
            const SZ: usize = core::mem::size_of::<crate::tty::Termios>();
            if argp != 0 && validate_user_buffer(argp, SZ).is_ok() {
                let t = *crate::tty::TERMIOS.lock();
                unsafe { core::ptr::write(argp as *mut crate::tty::Termios, t); }
            }
            0
        }
        TCSETS | TCSETSW | TCSETSF => {
            if !is_tty { return errno::ENOTTY; }
            const SZ: usize = core::mem::size_of::<crate::tty::Termios>();
            if let Err(e) = validate_user_buffer(argp, SZ) { return e; }
            // TCSETSW/TCSETSF (drain-first / flush-first) collapse to the
            // same immediate apply as TCSETS: there's no real output queue
            // to drain and no queued-but-unread input beyond
            // `keyboard_buffer::KEYBOARD_BUFFER` worth discarding.
            let t = unsafe { core::ptr::read(argp as *const crate::tty::Termios) };
            *crate::tty::TERMIOS.lock() = t;
            0
        }
        TIOCGWINSZ => {
            if argp != 0 && validate_user_buffer(argp, 8).is_ok() {
                // struct winsize { ws_row, ws_col, ws_xpixel, ws_ypixel }
                // Use 25 rows × 80 cols as a reasonable default.
                let ws = argp as *mut u16;
                unsafe {
                    *ws.add(0) = 25;  // rows
                    *ws.add(1) = 80;  // cols
                    *ws.add(2) = 0;
                    *ws.add(3) = 0;
                }
            }
            0
        }
        TIOCGPGRP => {
            if !is_tty { return errno::ENOTTY; }
            if let Err(e) = validate_user_buffer(argp, 4) { return e; }
            let pgid = crate::tty::FOREGROUND_PGID.load(core::sync::atomic::Ordering::Relaxed);
            unsafe { *(argp as *mut i32) = pgid as i32; }
            0
        }
        TIOCSPGRP => {
            if !is_tty { return errno::ENOTTY; }
            if let Err(e) = validate_user_buffer(argp, 4) { return e; }
            let pgid = unsafe { *(argp as *const i32) };
            if pgid <= 0 { return errno::EINVAL; }
            crate::tty::FOREGROUND_PGID.store(pgid as u32, core::sync::atomic::Ordering::Relaxed);
            0
        }
        _ => errno::EINVAL,
    }
}

// ── writev(20) ─────────────────────────────────────────────────────────────

/// writev(20): ssize_t writev(int fd, const struct iovec *iov, int iovcnt)
///
/// Loops over the iovec array and calls sys_write for each segment.
/// struct iovec = { void *iov_base (8 bytes), size_t iov_len (8 bytes) }
fn sys_writev(fd: i32, iov_ptr: u64, iovcnt: usize) -> SyscallResult {
    if iovcnt > 1024 { return errno::EINVAL; }
    if validate_user_buffer(iov_ptr, iovcnt * 16).is_err() {
        return errno::EFAULT;
    }

    let mut total: i64 = 0;
    for i in 0..iovcnt {
        let entry = (iov_ptr + i as u64 * 16) as *const u64;
        let (base, len) = unsafe { (*entry, *entry.add(1)) };
        if len == 0 { continue; }
        let n = sys_write(fd, base as usize, len as usize);
        if n < 0 { return n; }
        total += n;
    }
    total
}

// ── arch_prctl(158) ────────────────────────────────────────────────────────

/// arch_prctl(158): int arch_prctl(int code, unsigned long addr)
///
/// Only ARCH_SET_FS (0x1002) is implemented: writes the FS.base MSR so
/// that TLS (thread-local storage) works.  mlibc calls this via sys_tcb_set.
fn sys_arch_prctl(code: i32, addr: u64) -> SyscallResult {
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

// ── futex(202) ─────────────────────────────────────────────────────────────

/// futex(202): long futex(uint32_t *uaddr, int futex_op, uint32_t val,
///                        const struct timespec *timeout, ...)
///
/// WAIT blocks the caller if `*uaddr == val` until a matching WAKE (timeouts
/// are not supported — `_timeout` is ignored, matching the previous stub).
/// WAKE wakes up to `val` waiters registered on the same `uaddr`.
///
/// Waiters are scoped by (uaddr, address space) — not raw uaddr alone —
/// because every process's anonymous mmap region starts at the same fixed
/// base (see USER_MMAP_BASE), so two unrelated processes can easily end up
/// with numerically identical uaddrs for e.g. mlibc's internal malloc lock.
/// Without this scoping a WAKE in one process could wake a waiter in a
/// completely unrelated one. There is no real thread-sharing yet (sys_clone
/// is ENOSYS), so today this is one-waiter-per-address-space in practice,
/// but the scoping is what makes it correct once real threads land.
fn sys_futex(uaddr: u64, futex_op: i32, val: i32, _timeout: u64) -> SyscallResult {
    const FUTEX_WAIT: i32 = 0;
    const FUTEX_WAKE: i32 = 1;
    const FUTEX_PRIVATE_FLAG: i32 = 128;

    let op = futex_op & !FUTEX_PRIVATE_FLAG;

    match op {
        FUTEX_WAIT => {
            if validate_user_buffer(uaddr, 4).is_err() { return errno::EFAULT; }
            let current = unsafe { *(uaddr as *const i32) };
            if current != val {
                return errno::EAGAIN;
            }

            let tf_ptr = CURRENT_SYSCALL_TF.load(Ordering::Relaxed) as *const TrapFrame;

            unsafe { core::arch::asm!("cli"); }

            let (pid, as_id) = {
                let sched = super::scheduler::local_scheduler();
                match sched.running_ref() {
                    Some(proc) => (proc.pid.0, proc.address_space.root_frame().start_address().as_u64()),
                    None => { unsafe { core::arch::asm!("sti"); } return errno::ESRCH; }
                }
            };

            if pid < MAX_PROCS {
                FUTEX_WAITERS.lock()[pid] = Some(FutexWaiter { uaddr, as_id });
            }

            let next_tf = {
                let mut scheduler = super::scheduler::local_scheduler();
                unsafe { (*(tf_ptr as *mut TrapFrame)).rax = 0; }
                scheduler.block_current(tf_ptr)
            };
            unsafe { super::trapframe::jump_to_user(next_tf) }
        }
        FUTEX_WAKE => {
            unsafe { core::arch::asm!("cli"); }

            let as_id = {
                let sched = super::scheduler::local_scheduler();
                match sched.running_ref() {
                    Some(proc) => proc.address_space.root_frame().start_address().as_u64(),
                    None => { unsafe { core::arch::asm!("sti"); } return errno::ESRCH; }
                }
            };

            let max_wake = if val <= 0 { i32::MAX } else { val };
            let mut woken_pids = [0usize; 8];
            let mut woken_count = 0usize;
            {
                let mut waiters = FUTEX_WAITERS.lock();
                for (pid, slot) in waiters.iter_mut().enumerate() {
                    if woken_count >= woken_pids.len() || woken_count as i32 >= max_wake {
                        break;
                    }
                    if let Some(w) = slot {
                        if w.uaddr == uaddr && w.as_id == as_id {
                            woken_pids[woken_count] = pid;
                            woken_count += 1;
                            *slot = None;
                        }
                    }
                }
            }

            if woken_count > 0 {
                let mut sched = super::scheduler::local_scheduler();
                for &pid in &woken_pids[..woken_count] {
                    sched.wake_with_retval(pid, 0);
                }
            }
            unsafe { core::arch::asm!("sti"); }
            woken_count as i64
        }
        _ => errno::ENOSYS,
    }
}

#[derive(Clone, Copy)]
struct FutexWaiter {
    uaddr: u64,
    as_id: u64,
}

/// One outstanding FUTEX_WAIT per PID — mirrors POLL_WAITERS/RECV_WAITER.
static FUTEX_WAITERS: Mutex<[Option<FutexWaiter>; MAX_PROCS]> = Mutex::new([None; MAX_PROCS]);

/// Clear a pending futex wait for a process (called on exit).
fn futex_cancel_waiter(pid: usize) {
    if pid < MAX_PROCS {
        FUTEX_WAITERS.lock()[pid] = None;
    }
}

// ── set_tid_address(218) ───────────────────────────────────────────────────

/// set_tid_address(218): pid_t set_tid_address(int *tidptr)
///
/// Used by mlibc during thread startup to register a clear-child-tid pointer.
/// In our single-threaded model we just return the current PID.
fn sys_set_tid_address(_tidptr: u64) -> SyscallResult {
    sys_getpid()
}

/// sys_yield — voluntary context switch.
///
/// Reuses the same `switch_to_next` the timer ISR uses for preemption: puts
/// the caller back at the tail of its run queue (as Ready) and switches to
/// the next Ready process. If nothing else is Ready, `switch_to_next`
/// returns the caller's own TrapFrame unchanged and this is a no-op.
fn sys_yield() -> SyscallResult {
    let tf_ptr = CURRENT_SYSCALL_TF.load(Ordering::Relaxed) as *const TrapFrame;

    unsafe { core::arch::asm!("cli"); }

    let next_tf = {
        let mut scheduler = super::scheduler::local_scheduler();
        // Pre-set rax=0 in the on-stack frame *before* switch_to_next copies
        // it into the process's saved TrapFrame, so that whenever this
        // process runs again, the syscall returns 0.
        unsafe { (*(tf_ptr as *mut TrapFrame)).rax = 0; }
        scheduler.switch_to_next(tf_ptr)
    };

    unsafe { super::trapframe::jump_to_user(next_tf) }
}

/// sys_nanosleep — block the calling process for at least `ns` nanoseconds.
///
/// Returns 0 when the sleep completes. Returns immediately (0) if ns == 0.
///
/// LOCKING (see hrtimer.rs for full analysis):
///   cli → scheduler lock → QUEUE lock (hrtimer::start) → QUEUE released →
///   block_current → never returns here.
fn sys_nanosleep(ns: u64) -> SyscallResult {
    if ns == 0 {
        return 0;
    }

    let now = crate::time::ktime_get();
    let expiry = now.saturating_add(ns);

    let tf_ptr = CURRENT_SYSCALL_TF.load(Ordering::Relaxed) as *const TrapFrame;

    unsafe { core::arch::asm!("cli"); }

    let next_tf = {
        let mut scheduler = super::scheduler::local_scheduler();

        // Set the wakeup return value in the saved TrapFrame so that when the
        // process is woken by hrtimer::tick() the syscall returns 0.
        unsafe {
            (*(tf_ptr as *mut TrapFrame)).rax = 0;
        }

        let pid = scheduler.current_pid().map(|p| p.0).unwrap_or(0);
        serial_println!("[DBG] nanosleep PID {} for {} ns (expiry={})", pid, ns, expiry);

        // Register the hrtimer.  QUEUE lock is acquired and released inside
        // start(); we still hold the scheduler lock, which is safe because
        // the ISR path acquires QUEUE first then the scheduler — and ISRs
        // cannot fire while cli is in effect.
        crate::time::hrtimer::start(expiry, crate::time::hrtimer::HrTimerAction::WakePid(pid));

        scheduler.block_current(tf_ptr)
        // scheduler lock dropped here
    };

    unsafe { super::trapframe::jump_to_user(next_tf) }
}

fn sys_getpid() -> SyscallResult {
    with_scheduler(|scheduler| {
        scheduler.current_pid().map(|pid| pid.0 as SyscallResult).unwrap_or(0)
    })
}

/// sys_exit — terminate the calling process and switch immediately.
///
/// Performs an immediate full context switch via kill_and_switch_tf +
/// jump_to_trapframe.  This restores ALL registers of the next process
/// and never returns.
fn sys_exit(status: i32) -> SyscallResult {
    use alloc::format;

    let reason = format!("exit({})", status);

    unsafe { core::arch::asm!("cli"); }

    let (dead_pid, parent_to_notify, tf_ptr, old_files) = {
        let mut scheduler = super::scheduler::local_scheduler();
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
                alloc::sync::Arc::new(Mutex::new(super::file::FileDescriptorTable::new())),
            );
            (old, parent)
        } else {
            (alloc::sync::Arc::new(Mutex::new(super::file::FileDescriptorTable::new())), None)
        };
        let dead_pid = scheduler.current_pid().map(|p| p.0).unwrap_or(0);
        let ptr = scheduler.kill_and_switch_tf(&reason);
        serial_println!("  → Process exited, switching immediately (full TrapFrame restore)");
        (dead_pid, parent_to_notify, ptr, old_files)
    };

    // Safe to drop now: SCHEDULER is released, cli is still in effect (we
    // haven't called sti yet), so any pipe-end wake this triggers can lock
    // SCHEDULER without racing anything else on this single core.
    drop(old_files);

    // Queue SIGCHLD on the parent (default action Ignore — purely additive,
    // no observable change unless the parent installed a handler) and wake
    // it if it's blocked in waitpid() for exactly this pid, delivering the
    // real wait status. One locked section for both — `find_process_mut`
    // only searches run_queues/wait_queue (deliberately excludes `running`,
    // see its doc comment); the parent may already *be* `running` here if
    // `kill_and_switch_tf` above just picked it as the next process to
    // schedule, so `notify_child_death` checks that case itself.
    unsafe { core::arch::asm!("cli"); }
    super::scheduler::local_scheduler().notify_child_death(dead_pid, parent_to_notify);
    unsafe { core::arch::asm!("sti"); }

    // Cancel any pending poll/epoll wait and clear side tables
    cancel_all_waiters(dead_pid);

    unsafe {
        core::arch::asm!("sti");
        crate::process::trapframe::jump_to_user(tf_ptr);
    }
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
    poll_cancel_waiter(pid);
    clear_epoll_fd_all(pid);
    futex_cancel_waiter(pid);
}

fn sys_fork() -> SyscallResult {
    let tf_ptr = CURRENT_SYSCALL_TF.load(Ordering::Relaxed) as *const TrapFrame;

    unsafe { core::arch::asm!("cli"); }

    // Collect what we need from the running process
    let (child_as, parent_pid, parent_fs_base, files, child_tf, parent_cwd, parent_pgid) = {
        let scheduler = super::scheduler::local_scheduler();
        match scheduler.running_ref() {
            Some(proc) => {
                // Build child TrapFrame: same as parent but rax=0 (fork returns 0 in child)
                let mut tf_copy = unsafe { *tf_ptr };
                tf_copy.rax = 0;

                match unsafe { proc.address_space.fork() } {
                    Ok(child_as) => (child_as, proc.pid, proc.fs_base, proc.files.lock().clone(), tf_copy, proc.cwd.clone(), proc.pgid),
                    Err(e) => {
                        serial_println!("fork: address_space.fork() failed: {}", e);
                        unsafe { core::arch::asm!("sti"); }
                        return errno::ENOMEM;
                    }
                }
            }
            None => {
                unsafe { core::arch::asm!("sti"); }
                return errno::ESRCH;
            }
        }
    };

    let kernel_stack = crate::init::processes::allocate_kernel_stack();

    let child_pid = {
        let mut scheduler = super::scheduler::local_scheduler();
        let pid = scheduler.allocate_pid();

        let mut child = alloc::boxed::Box::new(
            super::Process::new_user_from_fork(
                pid, parent_pid, alloc::boxed::Box::new(child_tf),
                kernel_stack, child_as, files, parent_cwd, parent_pgid,
            )
        );
        child.fs_base = parent_fs_base; // inherit TLS base from parent
        child.set_name("child");
        scheduler.add_process(child);
        pid.0 as SyscallResult
    };

    unsafe { core::arch::asm!("sti"); }
    child_pid  // parent sees child PID
}

// ── clone(56) ──────────────────────────────────────────────────────────────

/// clone(56): long clone(void *entry, void *stack, void *tcb)
///
/// Real threading: creates a new schedulable Process that SHARES the
/// caller's AddressSpace (same `Arc`, no COW page-table clone at all —
/// unlike fork()) instead of getting its own. The new thread starts
/// executing at `entry` with RSP=`stack`.
///
/// This is a custom ABI (not Linux's real `clone(2)` flags/signature) —
/// it matches exactly what this kernel's mlibc port's `sys_clone` calls
/// with: `entry` = `__mlibc_start_thread`, `stack` = the already-prepared
/// stack `sys_prepare_stack` built in userspace (carrying the real
/// entry/arg/tcb the assembly trampoline pops off it), `tcb` unused here —
/// mlibc's own `__mlibc_enter_thread` calls `sys_tcb_set(tcb)` itself once
/// the new thread actually starts running.
///
/// Returns the new thread's pid (used as its tid) to the caller.
///
/// The new thread shares the caller's `FileDescriptorTable` (`Arc<Mutex<..>>`,
/// see `Process::files`) — files one thread opens are visible to its
/// siblings, matching POSIX semantics. It also never zombie-parks on exit:
/// see `Process::is_thread` / `Scheduler::kill_current` for why (mlibc's
/// `pthread_join()` never calls `waitpid()` on a tid, so the kernel reaps a
/// thread's `Process` immediately instead of waiting for a collector that
/// will never come).
fn sys_clone(entry: u64, stack: u64, _tcb: u64) -> SyscallResult {
    let (parent_pid, address_space, files, parent_cwd, parent_pgid) = {
        let sched = super::scheduler::local_scheduler();
        match sched.running_ref() {
            Some(proc) => (proc.pid, proc.address_space.clone(), proc.files.clone(), proc.cwd.clone(), proc.pgid),
            None => return errno::ESRCH,
        }
    };

    // If `stack` falls inside a VMA that mlibc's sys_prepare_stack mmap'd
    // just for this thread (the common case — Huge2M, since mlibc's
    // default_stacksize is exactly 2 MiB, which sys_mmap_anon always backs
    // with a huge page), record it so the kernel can free it when this
    // thread dies. mlibc itself never does (see Process::owned_stack_vma's
    // doc comment) — a caller-supplied stack (pthread_attr_setstack) has no
    // matching VMA here and is correctly left alone.
    let owned_stack_vma = address_space.find_vma(stack).and_then(|vma| {
        if vma.kind == crate::memory::vma::VmaKind::Huge2M {
            Some((vma.start, vma.size_pages))
        } else {
            None
        }
    });

    let kernel_stack = crate::init::processes::allocate_kernel_stack();

    unsafe { core::arch::asm!("cli"); }

    let tid = {
        let mut scheduler = super::scheduler::local_scheduler();
        let pid = scheduler.allocate_pid();

        let mut thread = alloc::boxed::Box::new(
            super::Process::new_thread(
                pid, parent_pid,
                x86_64::VirtAddr::new(entry), x86_64::VirtAddr::new(stack),
                kernel_stack, address_space, files, owned_stack_vma, parent_cwd, parent_pgid,
            )
        );
        thread.set_name("thread");
        scheduler.add_process(thread);
        pid.0 as SyscallResult
    };

    unsafe { core::arch::asm!("sti"); }
    tid
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

fn sys_exec(path_ptr: usize, argv_ptr: usize, envp_ptr: usize) -> SyscallResult {
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

    let elf_bytes = match find_program_elf(name) {
        Some(b) => b,
        None => {
            serial_println!("sys_exec: '{}' not found", name);
            return errno::ENOENT;
        }
    };

    // Load ELF without any lock — may take time and allocates frames
    let loaded = match unsafe { crate::memory::elf_loader::load_elf(elf_bytes, 0, &argv, &envp) } {
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

    crate::serial_println_raw!("[EXEC] load_elf done, going cli");
    unsafe { core::arch::asm!("cli"); }
    crate::serial_println_raw!("[EXEC] cli done, taking scheduler lock");

    let next_tf = {
        let mut scheduler = super::scheduler::local_scheduler();
        crate::serial_println_raw!("[EXEC] scheduler locked, swapping address space");
        match scheduler.running_mut() {
            Some(proc) => {
                crate::serial_println_raw!("[EXEC] dropping old AS");
                // Replace address space with freshly loaded one. This drops
                // this Process's Arc reference to whatever it had before —
                // if that was a shared (thread) address space, the actual
                // page table/pages are only freed once every other thread
                // sharing it has also exited (Arc refcount reaches 0).
                proc.address_space = alloc::sync::Arc::new(loaded.address_space);
                crate::serial_println_raw!("[EXEC] old AS dropped, new AS in place");

                // The page-fault fast path caches a raw pointer to the
                // process's AddressSpace (see scheduler::refresh_current_fast);
                // it must be re-synced now that the field above points at a
                // brand-new Arc allocation, or every fault after this exec()
                // will look up VMAs in the stale pre-exec address space.
                super::scheduler::refresh_current_fast(proc);

                // Reset TrapFrame to new entry point
                proc.trapframe.rip    = loaded.entry_point.as_u64();
                proc.trapframe.rsp    = loaded.user_stack_top.as_u64();
                proc.trapframe.cs     = 0x23;
                proc.trapframe.ss     = 0x1b;
                proc.trapframe.rflags = 0x200;
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

                crate::serial_println_raw!("[EXEC] activating new CR3");
                unsafe { proc.address_space.activate(); }
                crate::serial_println_raw!("[EXEC] CR3 active, jumping to entry={:#x}", proc.trapframe.rip);
                &*proc.trapframe as *const TrapFrame
            }
            None => {
                unsafe { core::arch::asm!("sti"); }
                return errno::ESRCH;
            }
        }
    };

    crate::serial_println_raw!("[EXEC] jump_to_trapframe");
    // Jump to the new program — never returns
    unsafe { super::trapframe::jump_to_user(next_tf) }
}

fn find_program_elf(name: &str) -> Option<&'static [u8]> {
    crate::fs::initramfs::bytes(name)
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
/// `options`: `WNOHANG` (2) returns 0 immediately instead of blocking when
/// nothing is reapable yet. `WUNTRACED` (4) also matches a `Stopped` child
/// (job control), reporting it once (see `Process::stop_reported`) without
/// removing it from the wait queue — a later real exit, or another
/// stop/continue cycle, can still be observed. No `WCONTINUED` support
/// (this kernel doesn't track SIGCONT-resume events for reporting).
fn sys_waitpid(pid_arg: i64, status_ptr: usize, options: i32) -> SyscallResult {
    const WNOHANG: i32 = 2;
    const WUNTRACED: i32 = 4;

    if status_ptr != 0 {
        if let Err(e) = validate_user_buffer(status_ptr as u64, 4) { return e; }
    }

    let tf_ptr = CURRENT_SYSCALL_TF.load(Ordering::Relaxed) as *const TrapFrame;

    unsafe { core::arch::asm!("cli"); }

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
        let mut scheduler = super::scheduler::local_scheduler();

        let caller_pid = scheduler.current_pid();
        let caller_pgid = scheduler.running_ref().map(|p| p.pgid).unwrap_or(0);

        let target = match pid_arg {
            p if p > 0 => super::WaitTarget::Pid(p as usize),
            0 => super::WaitTarget::Pgid(caller_pgid),
            -1 => super::WaitTarget::AnyChild,
            p => super::WaitTarget::Pgid((-p) as u32),
        };

        let zombie_pos = scheduler.wait_queue.iter().position(|p| {
            matches!(p.state, super::ProcessState::Zombie)
                && p.parent_pid == caller_pid
                && target.matches(p.pid.0, p.pgid)
        });
        let stopped_pos = if zombie_pos.is_none() && options & WUNTRACED != 0 {
            scheduler.wait_queue.iter().position(|p| {
                matches!(p.state, super::ProcessState::Stopped)
                    && !p.stop_reported
                    && p.parent_pid == caller_pid
                    && target.matches(p.pid.0, p.pgid)
            })
        } else {
            None
        };

        if let Some(pos) = zombie_pos {
            // Safe to free the zombie's kernel stack and write the status
            // straight into `status_ptr` right here: we're running on the
            // *parent's* stack in the parent's own address space (this is
            // its own waitpid() syscall), never the dead child's.
            let proc = scheduler.wait_queue.remove(pos).unwrap();
            let status = proc.wait_status_word();
            let pid = proc.pid.0;
            crate::init::processes::free_kernel_stack(proc.kernel_stack);
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
            let status = scheduler.wait_queue[pos].stop_status_word();
            let pid = scheduler.wait_queue[pos].pid.0;
            scheduler.wait_queue[pos].stop_reported = true;
            if status_ptr != 0 {
                // write_unaligned: see the zombie_pos branch above for why.
                unsafe { core::ptr::write_unaligned(status_ptr as *mut i32, status); }
            }
            Outcome::Return(pid as SyscallResult)
        } else if options & WNOHANG != 0 {
            Outcome::Return(0)
        } else {
            let has_any = scheduler.iter_all()
                .any(|p| p.parent_pid == caller_pid && target.matches(p.pid.0, p.pgid));
            if !has_any {
                Outcome::Return(errno::ECHILD)
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
                Outcome::Block(scheduler.block_current(tf_ptr))
            }
        }
    };

    match outcome {
        Outcome::Return(v) => {
            unsafe { core::arch::asm!("sti"); }
            v
        }
        Outcome::Block(next_tf) => unsafe { super::trapframe::jump_to_user(next_tf) },
    }
}

fn sys_uptime_ms() -> SyscallResult {
    crate::cpu::tsc::uptime_ms() as SyscallResult
}

/// sys_meminfo_kb (custom #402) — free physical memory, in KiB.
///
/// Mainly a debugging aid: run something in a loop (e.g. `sh` a script that
/// spawns/kills threads or processes many times) and watch this between
/// runs to catch a leak — see kernel_stack's `pending_stack_frees` /
/// `free_kernel_stack` for the leak this was added to verify.
fn sys_meminfo_kb() -> SyscallResult {
    (crate::allocator::buddy_allocator::BUDDY.lock().free_bytes() / 1024) as SyscallResult
}

/// sys_uptime_sec (custom #202) — seconds elapsed since kernel boot.
///
/// Uses the active clocksource (TSC when available).
fn sys_uptime_sec() -> SyscallResult {
    (crate::time::ktime_get() / 1_000_000_000) as SyscallResult
}

/// sys_clock_gettime (Linux #228) — write a `struct timespec` to user memory.
///
/// Supported clock IDs:
///   0 = CLOCK_REALTIME   — returns monotonic uptime (no RTC; boot = epoch)
///   1 = CLOCK_MONOTONIC  — same as REALTIME for now
///   7 = CLOCK_BOOTTIME   — same; included for glibc compatibility
///
/// `struct timespec { i64 tv_sec; i64 tv_nsec; }` (16 bytes, 8-byte aligned).
///
/// The process's own page table is active during the syscall, so we can
/// write directly to the user virtual address without physical translation.
fn sys_clock_gettime(clk_id: u64, tp_ptr: u64) -> SyscallResult {
    // Validate the user pointer (16 bytes = 2 × i64)
    if let Err(e) = validate_user_buffer(tp_ptr, 16) {
        return e;
    }

    // Accept only the clock IDs we can serve meaningfully.
    match clk_id {
        0 | 1 | 7 => {}
        _ => return errno::EINVAL,
    }

    let uptime_ns = crate::time::ktime_get();
    let tv_sec  = (uptime_ns / 1_000_000_000) as i64;
    let tv_nsec = (uptime_ns % 1_000_000_000) as i64;

    // Direct write into user VA — safe because:
    //   1. validate_user_buffer confirmed it is in user-space range.
    //   2. The running process's CR3 is still active (we're in the kernel
    //      but the user page tables haven't been switched away).
    //   3. If the page isn't mapped yet, the write faults and the page-fault
    //      handler demand-pages it (same as any user store instruction).
    unsafe {
        let ptr = tp_ptr as *mut i64;
        ptr.write(tv_sec);
        ptr.add(1).write(tv_nsec);
    }

    0
}

// ============================================================================
// SIGNALS: kill / sigaction / sigprocmask / sigreturn
// ============================================================================

/// kill(62): long kill(pid_t pid, int sig)
///
/// `pid > 0`: single target, as before. `pid == 0`: every process in the
/// caller's own process group. `pid < -1`: every process in group `-pid`.
/// `pid == -1` (broadcast to every signalable process) is not supported —
/// this kernel has no permission model to bound it, so it just returns
/// `EINVAL` rather than doing something surprising.
///
/// Only queues the signal on Blocked/Ready/Zombie targets — never
/// force-wakes them; see the doc comment inside for why. The one deliberate
/// exception is `SIGCONT` against a currently-`Stopped` target (single or
/// group): that's the *only* wakeup a stopped process ever gets (see
/// `Process::state`'s `Stopped` doc comment), so it's force-woken via
/// `wake_stopped` in addition to (not instead of) the normal
/// `queue_signal` — if a handler is installed for SIGCONT, it still runs
/// once the process resumes and passes through `deliver_pending`.
fn sys_kill(target_pid: i64, sig: u32) -> SyscallResult {
    if sig == 0 || sig as usize >= super::signal::NUM_SIGNALS {
        return errno::EINVAL;
    }
    if target_pid == -1 {
        return errno::EINVAL;
    }

    unsafe { core::arch::asm!("cli"); }

    // `sched` MUST be dropped before `sti` on every path — scoping it to
    // this inner block (instead of holding it for the whole function, as
    // an earlier version of this code did) guarantees that. Holding a
    // spin::Mutex guard past `sti` opens a window where a timer tick can
    // land inside it and spin forever on `local_scheduler()`, since the
    // interrupted code (the only thing that could ever release the lock)
    // can't resume until that same spin gives up — it never does. Every
    // other syscall in this file already follows this shape; this one
    // didn't, and deadlocked the first time a signal landed on a Ready
    // (not self, not Blocked) target during testing.
    let result = {
        let mut sched = super::scheduler::local_scheduler();

        if target_pid == 0 || target_pid < -1 {
            let pgid = if target_pid == 0 {
                sched.running_ref().map(|p| p.pgid).unwrap_or(0)
            } else {
                (-target_pid) as u32
            };
            if sig == super::signal::SIGCONT {
                let stopped: alloc::vec::Vec<usize> = sched.wait_queue.iter()
                    .filter(|p| p.pgid == pgid && matches!(p.state, super::ProcessState::Stopped))
                    .map(|p| p.pid.0)
                    .collect();
                for pid in stopped {
                    sched.wake_stopped(pid);
                }
            }
            sched.queue_signal_to_group(pgid, sig);
            0
        } else {
            let target_pid = target_pid as usize;
            let is_self = sched.current_pid().map(|p| p.0) == Some(target_pid);
            if is_self {
                if let Some(proc) = sched.running_mut() {
                    super::signal::queue_signal(proc, sig);
                }
                0
            } else {
                // Just queue the signal — never force-wake a Blocked target.
                // Whatever it's actually blocked on (pipe data, a futex, a
                // timer) has its own wakeup path that sets a *correct* return
                // value for that specific wait; a generic wake() here would
                // resume it with whatever stale rax was live before it blocked
                // (pipe/futex reads never preset one, unlike nanosleep), and —
                // worse — removes it from wait_queue before its real wakeup
                // gets a chance to find it there, silently losing whatever
                // that wakeup was about to deliver. Confirmed by
                // mlibc_signal_test.c: a kill()-woken pipe reader raced its
                // sibling's write() and read back "" instead of the message,
                // because deliver_and_wake's wait_queue scan found nothing —
                // kill() had already moved it to Ready. The tradeoff (no
                // instant SIGKILL for something blocked forever on a condition
                // that will never occur) is accepted for this minimal
                // implementation — delivery still happens the next time this
                // process wakes for its own real reason and passes through a
                // jump_to_user checkpoint. SIGCONT against a Stopped target is
                // the one exception (see this function's doc comment).
                if sig == super::signal::SIGCONT {
                    sched.wake_stopped(target_pid);
                }
                match sched.find_process_mut(target_pid) {
                    Some(proc) => { super::signal::queue_signal(proc, sig); 0 }
                    None => errno::ESRCH,
                }
            }
        }
    };

    unsafe { core::arch::asm!("sti"); }
    result
}

// ── setpgid(109) / getpgid(121) / setsid(112) ───────────────────────────────

/// setpgid(109): int setpgid(pid_t pid, pid_t pgid)
///
/// `pid == 0` means "the caller"; `pgid == 0` means "use `pid`'s own pid as
/// its new group id" (become a group leader) — matches real POSIX. No
/// session concept is tracked, so (unlike real POSIX) this never checks
/// "is `pid` a session leader" — every process can always repoint its pgid.
fn sys_setpgid(pid: i64, pgid: i64) -> SyscallResult {
    if pid < 0 || pgid < 0 {
        return errno::EINVAL;
    }

    unsafe { core::arch::asm!("cli"); }
    let result = {
        let mut sched = super::scheduler::local_scheduler();
        let caller_pid = sched.current_pid().map(|p| p.0).unwrap_or(0);
        let target_pid = if pid == 0 { caller_pid } else { pid as usize };
        let new_pgid = if pgid == 0 { target_pid as u32 } else { pgid as u32 };

        if target_pid == caller_pid {
            match sched.running_mut() {
                Some(proc) => { proc.pgid = new_pgid; 0 }
                None => errno::ESRCH,
            }
        } else {
            match sched.find_process_mut(target_pid) {
                Some(proc) => { proc.pgid = new_pgid; 0 }
                None => errno::ESRCH,
            }
        }
    };
    unsafe { core::arch::asm!("sti"); }
    result
}

/// getpgid(121): pid_t getpgid(pid_t pid)
fn sys_getpgid(pid: i64) -> SyscallResult {
    if pid < 0 {
        return errno::EINVAL;
    }

    unsafe { core::arch::asm!("cli"); }
    let result = {
        let mut sched = super::scheduler::local_scheduler();
        let caller_pid = sched.current_pid().map(|p| p.0).unwrap_or(0);
        let target_pid = if pid == 0 { caller_pid } else { pid as usize };

        if target_pid == caller_pid {
            sched.running_ref().map(|p| p.pgid as SyscallResult).unwrap_or(errno::ESRCH)
        } else {
            sched.find_process_mut(target_pid).map(|p| p.pgid as SyscallResult).unwrap_or(errno::ESRCH)
        }
    };
    unsafe { core::arch::asm!("sti"); }
    result
}

/// setsid(112): pid_t setsid(void)
///
/// No real session tracking exists — approximated as "become your own
/// process group leader", rejected with `EPERM` if already one (the real
/// POSIX rule: a process that's already a group leader can't `setsid()`).
fn sys_setsid() -> SyscallResult {
    unsafe { core::arch::asm!("cli"); }
    let result = {
        let mut sched = super::scheduler::local_scheduler();
        match sched.running_mut() {
            Some(proc) => {
                if proc.pgid == proc.pid.0 as u32 {
                    errno::EPERM
                } else {
                    proc.pgid = proc.pid.0 as u32;
                    proc.pid.0 as SyscallResult
                }
            }
            None => errno::ESRCH,
        }
    };
    unsafe { core::arch::asm!("sti"); }
    result
}

const SIG_DFL: u64 = 0;
const SIG_IGN: u64 = 1;

/// rt_sigaction(13): int sigaction(int sig, const struct sigaction *act, struct sigaction *oldact)
///
/// Simplified ABI: `act`/`oldact` are read/written as a single `u64`
/// handler address at offset 0 (matches `sa_handler`'s position in the
/// real `struct sigaction`; `sa_mask`/`sa_flags`/`sa_restorer` are ignored)
/// rather than the full struct — this kernel's userspace test programs use
/// a matching minimal ABI (see `userspace/src/syscall.rs::sigaction`).
fn sys_sigaction(sig: u32, act_ptr: u64, oldact_ptr: u64) -> SyscallResult {
    if sig == 0 || sig as usize >= super::signal::NUM_SIGNALS
        || sig == super::signal::SIGKILL || sig == super::signal::SIGSTOP {
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
                SIG_DFL => super::SignalAction::Default,
                SIG_IGN => super::SignalAction::Ignore,
                addr => super::SignalAction::Handler(addr),
            };
        }
        if oldact_ptr != 0 {
            let old_addr = match old {
                super::SignalAction::Default => SIG_DFL,
                super::SignalAction::Ignore => SIG_IGN,
                super::SignalAction::Handler(addr) => addr,
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
fn sys_sigprocmask(how: i32, set_ptr: u64, oldset_ptr: u64) -> SyscallResult {
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
            let set = set & !(1u64 << super::signal::SIGKILL);
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
fn sys_sigreturn() -> SyscallResult {
    let tf_ptr = current_tf_ptr() as *mut TrapFrame;
    let user_rsp = unsafe { (*tf_ptr).rsp };

    unsafe { core::arch::asm!("cli"); }
    let ret = {
        let mut scheduler = super::scheduler::local_scheduler();
        match scheduler.running_mut() {
            Some(proc) => {
                unsafe { super::signal::pop_signal_frame(proc, tf_ptr, user_rsp) };
                unsafe { (*tf_ptr).rax as i64 }
            }
            None => errno::ESRCH,
        }
    };
    unsafe { core::arch::asm!("sti"); }
    ret
}

// ============================================================================
// IPC SYSCALLS: socket / bind / connect / accept / sendmsg / recvmsg
// ============================================================================
//
// Each process stores its open channel FDs in its FileDescriptorTable using
// a thin SocketHandle wrapper that implements FileHandle.  The actual Channel
// state lives in ipc::CHANNELS (global table, protected by its own Mutex).
//
// LOCKING ORDER (must never be inverted):
//   cli → SCHEDULER → CHANNELS
//
// The ISR path only touches the SCHEDULER, not CHANNELS, so this is safe.

use crate::ipc::channel::{ChannelId, Message as IpcMessage, ServerState, CHANNELS};

// ============================================================================
// IPC BLOCKING WAITERS
// ============================================================================
//
// Pattern: same as STDIN_WAITER / WAIT_WAITER.
//   1. Syscall saves waiter info (pid + data pointers) in a global slot.
//   2. Syscall calls block_current + jump_to_trapframe → never returns here.
//   3. Wakeup code (from another process's syscall) writes the result directly
//      into the blocked process's trapframe.rax (and into the user buffer when
//      needed, via physical address translation).
//   4. sched.wake() makes the process runnable; it returns from the syscall
//      via iretq with rax already set to the correct value.

struct AcceptWaiter {
    pid:               usize,
    server_channel_id: ChannelId,
}
static ACCEPT_WAITER: Mutex<Option<AcceptWaiter>> = Mutex::new(None);

struct RecvWaiter {
    pid:        usize,
    channel_id: ChannelId,
    /// Physical address of the 64-byte Message buffer (pre-translated at
    /// block time so that delivery in sys_sendmsg skips the page-table walk).
    phys_buf:   u64,
}
static RECV_WAITER: Mutex<Option<RecvWaiter>> = Mutex::new(None);

// ——— SocketHandle — FileHandle wrapper around a channel ————————————————————

/// A FileHandle wrapper around a ChannelId.
/// `read`  ↔  recvmsg (returns one message; blocks if empty)
/// `write` ↔  sendmsg (sends one message to the peer)
///
/// Blocking inside read/write is not used here — callers use
/// sys_recvmsg / sys_sendmsg directly.  The handle is only kept in the FD
/// table so that close() frees the channel.
struct SocketHandle {
    channel_id: ChannelId,
}

impl crate::process::file::FileHandle for SocketHandle {
    fn read(&mut self, buf: &mut [u8]) -> crate::process::file::FileResult<usize> {
        let msg = CHANNELS.lock().get_mut(self.channel_id)
            .and_then(|ch| ch.dequeue());
        match msg {
            Some(m) => {
                let n = core::cmp::min(buf.len(), m.len as usize);
                buf[..n].copy_from_slice(&m.data[..n]);
                Ok(n)
            }
            None => Ok(0),   // non-blocking: no data yet
        }
    }

    fn write(&mut self, buf: &[u8]) -> crate::process::file::FileResult<usize> {
        let msg = IpcMessage::new(0, buf);
        let ok = {
            let mut tbl = CHANNELS.lock();
            let peer_id = tbl.get(self.channel_id).and_then(|ch| ch.peer);
            if let Some(pid) = peer_id {
                tbl.get_mut(pid).map(|ch| ch.enqueue(msg)).unwrap_or(false)
            } else {
                false
            }
        };
        if ok { Ok(buf.len()) } else { Err(crate::process::file::FileError::IOError) }
    }

    fn close(&mut self) -> crate::process::file::FileResult<()> {
        CHANNELS.lock().free(self.channel_id);
        Ok(())
    }

    fn name(&self) -> &str { "socket" }
}

// ——— fd → channel_id side table ———————————————————————————————————————————
//
// FileHandle is a trait object; we can't downcast to SocketHandle in no_std
// (no Any).  Solution: maintain a per-process fd→channel_id side table here.
//
// The challenge: FileHandle is a trait object; we can't downcast to SocketHandle
// in no_std (no Any).  Solution: maintain a per-process fd→channel_id side table
// in the IPC layer rather than in the FileDescriptorTable.
//
// We use a global array indexed by (pid * MAX_FILES + fd).

const MAX_PROCS: usize = 32;
const MAX_FILES_PER_PROC: usize = 16;

/// fd → channel_id mapping.  0 means "not a socket fd".
static FD_CHANNEL_MAP: Mutex<[[ChannelId; MAX_FILES_PER_PROC]; MAX_PROCS]> =
    Mutex::new([[0usize; MAX_FILES_PER_PROC]; MAX_PROCS]);

fn set_fd_channel(pid: usize, fd: usize, channel_id: ChannelId) {
    if pid < MAX_PROCS && fd < MAX_FILES_PER_PROC {
        FD_CHANNEL_MAP.lock()[pid][fd] = channel_id;
    }
}

fn get_fd_channel(pid: usize, fd: usize) -> Option<ChannelId> {
    if pid < MAX_PROCS && fd < MAX_FILES_PER_PROC {
        let id = FD_CHANNEL_MAP.lock()[pid][fd];
        if id != 0 { Some(id) } else { None }
    } else {
        None
    }
}

fn clear_fd_channel(pid: usize, fd: usize) {
    if pid < MAX_PROCS && fd < MAX_FILES_PER_PROC {
        FD_CHANNEL_MAP.lock()[pid][fd] = 0;
    }
}

// ——— sys_socket (revised) — store mapping ——————————————————————————————————

/// Internal helper: open a socket and record the fd→channel mapping.
fn sys_socket_impl() -> SyscallResult {
    let pid_dbg = crate::process::scheduler::current_pid().unwrap_or(0);
    serial_println!("[DBG] sys_socket PID {}", pid_dbg);
    let id = match CHANNELS.lock().alloc() {
        Some(id) => id,
        None => return errno::ENOMEM,
    };

    let handle = alloc::boxed::Box::new(SocketHandle { channel_id: id });

    unsafe { core::arch::asm!("cli"); }
    let result = {
        let mut sched = super::scheduler::local_scheduler();
        match sched.running_mut() {
            Some(proc) => {
                let pid = proc.pid.0;
                // `.lock()`'s guard must not outlive this statement — it
                // borrows (transitively) from `sched`, which the Ok arm
                // below drops, so the Result is computed and the guard
                // dropped first via a `let`, not a bare match scrutinee
                // (which would extend the guard's lifetime across all arms).
                let alloc_result = proc.files.lock().allocate(handle);
                match alloc_result {
                    Ok(fd) => {
                        drop(sched);
                        set_fd_channel(pid, fd, id);
                        fd as i64
                    }
                    Err(_) => {
                        CHANNELS.lock().free(id);
                        errno::EINVAL
                    }
                }
            }
            None => {
                CHANNELS.lock().free(id);
                errno::ESRCH
            }
        }
    };
    unsafe { core::arch::asm!("sti"); }
    result
}

// ——— sys_bind (proper implementation) ————————————————————————————————————

fn sys_bind_impl(fd: i32, path_ptr: usize, _addrlen: usize) -> SyscallResult {
    if let Err(e) = validate_user_buffer(path_ptr as u64, 64) {
        return e;
    }

    let mut path_buf = [0u8; 64];
    let path_len = unsafe {
        let ptr = path_ptr as *const u8;
        let mut len = 0usize;
        while len < 63 && *ptr.add(len) != 0 {
            path_buf[len] = *ptr.add(len);
            len += 1;
        }
        len
    };
    if path_len == 0 { return errno::EINVAL; }

    let pid = crate::process::scheduler::current_pid().unwrap_or(0);

    let channel_id = match get_fd_channel(pid, fd as usize) {
        Some(id) => id,
        None => return ENOTSOCK,
    };

    let mut tbl = CHANNELS.lock();
    let ch = match tbl.get_mut(channel_id) {
        Some(c) => c,
        None => return errno::EBADF,
    };

    ch.bound_path = Some(path_buf);
    ch.server_state = Some(ServerState::Listening);
    0
}

// ——— sys_connect ——————————————————————————————————————————————————————————

/// sys_connect (#42) — connect a socket fd to a named server endpoint.
///
/// If no server is listening yet: returns -ENOENT.
/// If a server is listening: creates a channel pair and returns 0.
///
/// The server must subsequently call accept() to get the peer fd.
fn sys_connect(fd: i32, path_ptr: usize, _addrlen: usize) -> SyscallResult {
    let pid_dbg = crate::process::scheduler::current_pid().unwrap_or(0);
    serial_println!("[DBG] sys_connect PID {} fd={}", pid_dbg, fd);
    if let Err(e) = validate_user_buffer(path_ptr as u64, 64) {
        return e;
    }

    let path_bytes = unsafe {
        let ptr = path_ptr as *const u8;
        let mut len = 0usize;
        while len < 63 && *ptr.add(len) != 0 { len += 1; }
        core::slice::from_raw_parts(ptr, len)
    };

    let pid = crate::process::scheduler::current_pid().unwrap_or(0);

    let client_channel_id = match get_fd_channel(pid, fd as usize) {
        Some(id) => id,
        None => return ENOTSOCK,
    };

    let mut tbl = CHANNELS.lock();

    // Find the server channel bound to this path
    let server_channel_id = match tbl.find_by_path(path_bytes) {
        Some(id) => id,
        None => return errno::ENOENT,
    };

    // Check it is actually listening
    let is_listening = tbl.get(server_channel_id)
        .map(|ch| ch.server_state == Some(ServerState::Listening))
        .unwrap_or(false);
    if !is_listening {
        return ECONNREFUSED;
    }

    // Allocate a server-side peer channel for this connection
    let server_peer_id = match tbl.alloc() {
        Some(id) => id,
        None => return errno::ENOMEM,
    };

    // Wire up the bidirectional pair:
    //   client_channel ↔ server_peer
    if let Some(ch) = tbl.get_mut(client_channel_id) {
        ch.peer = Some(server_peer_id);
    }
    if let Some(ch) = tbl.get_mut(server_peer_id) {
        ch.peer = Some(client_channel_id);
    }

    // Set server channel to PendingConnect; clear any stale accept waiter list
    if let Some(ch) = tbl.get_mut(server_channel_id) {
        ch.server_state = Some(ServerState::PendingConnect(server_peer_id));
    }

    drop(tbl);

    // If a process is blocked in sys_accept() for this server channel, wake it
    // and allocate the peer fd directly in its file table.
    let accept_waiter = {
        let mut aw = ACCEPT_WAITER.lock();
        if aw.as_ref().map(|w| w.server_channel_id == server_channel_id).unwrap_or(false) {
            aw.take()
        } else {
            None
        }
    };

    if let Some(waiter) = accept_waiter {
        serial_println!("[DBG] connect: waking accept waiter PID {}", waiter.pid);
        // Allocate the peer fd inside the blocked process.
        // cli prevents the timer ISR from preempting while we hold SCHEDULER.
        unsafe { core::arch::asm!("cli"); }

        let handle = alloc::boxed::Box::new(SocketHandle { channel_id: server_peer_id });
        let mut new_fd: i64 = errno::EINVAL;

        {
            let mut sched = super::scheduler::local_scheduler();
            // Reset PendingConnect now that accept() is being satisfied
            CHANNELS.lock().get_mut(server_channel_id)
                .map(|ch| ch.server_state = Some(ServerState::Listening));

            for proc in sched.wait_queue.iter_mut() {
                if proc.pid.0 == waiter.pid
                    && matches!(proc.state, super::ProcessState::Blocked)
                {
                    match proc.files.lock().allocate(handle) {
                        Ok(fd) => {
                            new_fd = fd as i64;
                            proc.trapframe.rax = fd as u64;
                        }
                        Err(_) => {
                            proc.trapframe.rax = (-22i64) as u64; // EINVAL
                        }
                    }
                    break;
                }
            }
            // Set the fd→channel mapping BEFORE wake() so the process
            // can never call recvmsg() with an unmapped fd.
            if new_fd >= 0 {
                set_fd_channel(waiter.pid, new_fd as usize, server_peer_id);
            }
            sched.wake(waiter.pid);
        }

        unsafe { core::arch::asm!("sti"); }
    }

    0
}

// ——— sys_accept ——————————————————————————————————————————————————————————

/// sys_accept (#43) — accept the next incoming connection on a server socket.
///
/// Blocks until a client calls connect().
/// Returns a new fd for the server-side peer channel.
fn sys_accept(fd: i32) -> SyscallResult {
    let tf_ptr = CURRENT_SYSCALL_TF.load(Ordering::Relaxed) as *const TrapFrame;
    let pid = crate::process::scheduler::current_pid().unwrap_or(0);

    let server_channel_id = match get_fd_channel(pid, fd as usize) {
        Some(id) => id,
        None => return ENOTSOCK,
    };

    unsafe { core::arch::asm!("cli"); }

    // Fast path: connection already pending from a previous connect().
    let pending = {
        let mut tbl = CHANNELS.lock();
        match tbl.get_mut(server_channel_id) {
            Some(ch) => {
                if let Some(ServerState::PendingConnect(peer_id)) = ch.server_state {
                    ch.server_state = Some(ServerState::Listening);
                    Some(peer_id)
                } else {
                    None
                }
            }
            None => {
                unsafe { core::arch::asm!("sti"); }
                return errno::EBADF;
            }
        }
    };

    if let Some(peer_channel_id) = pending {
        let handle = alloc::boxed::Box::new(SocketHandle { channel_id: peer_channel_id });
        let new_fd = {
            let mut sched = super::scheduler::local_scheduler();
            match sched.running_mut() {
                Some(proc) => match proc.files.lock().allocate(handle) {
                    Ok(fd) => fd as i64,
                    Err(_) => errno::EINVAL,
                },
                None => errno::ESRCH,
            }
        };
        if new_fd >= 0 { set_fd_channel(pid, new_fd as usize, peer_channel_id); }
        unsafe { core::arch::asm!("sti"); }
        return new_fd;
    }

    // Slow path: register as waiter, block.
    // sys_connect() will allocate the fd for us and set trapframe.rax before
    // calling sched.wake().  We return from the syscall via iretq with rax
    // already set to the correct fd number — no code here runs after blocking.
    *ACCEPT_WAITER.lock() = Some(AcceptWaiter { pid, server_channel_id });

    let next_tf = {
        let mut sched = super::scheduler::local_scheduler();
        sched.block_current(tf_ptr)
    };
    unsafe { super::trapframe::jump_to_user(next_tf) }
}

// ——— sys_sendmsg ——————————————————————————————————————————————————————————

/// sys_sendmsg (#46) — send a message on a connected socket.
///
/// `msg_ptr` points to a user `IpcUserMsg { tag: u32, len: u32, data: [u8; 56] }`.
/// `tag`   — application-defined message type.
/// `len`   — how many bytes of `data` are valid (0..=56).
fn sys_sendmsg(fd: i32, msg_ptr: u64, _flags: u32) -> SyscallResult {
    if let Err(e) = validate_user_buffer(msg_ptr, 64) {
        return e;
    }

    // Read the IpcUserMsg from user memory (user page table active)
    let (tag, len, data) = unsafe {
        let ptr = msg_ptr as *const u8;
        let tag  = u32::from_le_bytes([*ptr, *ptr.add(1), *ptr.add(2), *ptr.add(3)]);
        let len  = u32::from_le_bytes([*ptr.add(4), *ptr.add(5), *ptr.add(6), *ptr.add(7)]);
        let len  = core::cmp::min(len, 56) as usize;
        let mut data = [0u8; 56];
        core::ptr::copy_nonoverlapping(ptr.add(8), data.as_mut_ptr(), len);
        (tag, len as u32, data)
    };

    let msg = IpcMessage { tag, len, data };

    let pid = crate::process::scheduler::current_pid().unwrap_or(0);

    let channel_id = match get_fd_channel(pid, fd as usize) {
        Some(id) => id,
        None => return ENOTSOCK,
    };

    // Check if a process is blocked in recvmsg() on the peer channel.
    // If so, deliver the message directly to its user buffer (zero-copy from
    // the kernel's point of view) and wake it — no need to enqueue.
    let peer_id = {
        let tbl = CHANNELS.lock();
        match tbl.get(channel_id).and_then(|ch| ch.peer) {
            Some(id) => id,
            None => return ENOTCONN,
        }
    };

    let recv_waiter = {
        let mut rw = RECV_WAITER.lock();
        if rw.as_ref().map(|w| w.channel_id == peer_id).unwrap_or(false) {
            rw.take()
        } else {
            None
        }
    };

    if let Some(waiter) = recv_waiter {
        // Fast delivery: write directly to the pre-translated physical address
        // stored in the waiter — no page-table walk needed.
        let phys_offset = crate::memory::physical_memory_offset().as_u64();
        if waiter.phys_buf != 0 {
            let dst = (phys_offset + waiter.phys_buf) as *mut u8;
            unsafe {
                core::ptr::write_bytes(dst, 0, 64);
                core::ptr::copy_nonoverlapping(msg.tag.to_le_bytes().as_ptr(), dst,       4);
                core::ptr::copy_nonoverlapping(msg.len.to_le_bytes().as_ptr(), dst.add(4), 4);
                core::ptr::copy_nonoverlapping(msg.data.as_ptr(),              dst.add(8), msg.len as usize);
            }
        }

        // Wake the receiver, setting its syscall return value — single scan
        // of the wait_queue (previously: separate write-rax loop + wake scan).
        unsafe { core::arch::asm!("cli"); }
        {
            let mut sched = super::scheduler::local_scheduler();
            sched.wake_with_retval(waiter.pid, 64);
        }
        unsafe { core::arch::asm!("sti"); }
        return 64;
    }

    // No waiter — enqueue for future recvmsg().
    let enqueued = {
        let mut tbl = CHANNELS.lock();
        match tbl.get_mut(peer_id) {
            Some(ch) => ch.enqueue(msg),
            None => return EPIPE,
        }
    };
    if !enqueued { return EAGAIN; }

    // Wake any poll/epoll waiter watching peer_id for POLLIN.
    // Called after CHANNELS lock is released.
    poll_wakeup_for_channel(peer_id);

    64
}

/// Write a Message into a user buffer using physical address translation.
///
/// Used by sys_sendmsg to deliver a message to a blocked sys_recvmsg without
/// going through the queue (same technique as stdin_wakeup).
fn write_msg_to_user(
    addr_space: &crate::memory::address_space::AddressSpace,
    user_buf: u64,
    msg: &IpcMessage,
    phys_offset: x86_64::VirtAddr,
) {
    use x86_64::{VirtAddr, structures::paging::{Page, Size4KiB}};

    // The Message is 64 bytes; it might straddle a page boundary (unlikely for
    // aligned allocations, but we handle it field-by-field for safety).
    // For simplicity, assume the 64-byte Message is within a single 4K page
    // (the compiler aligns Message to 64 bytes, so it never crosses a page).
    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(user_buf));
    let offset = user_buf & 0xFFF;

    if let Some(frame) = unsafe { addr_space.translate_page(page) } {
        let dst_va = phys_offset + frame.start_address().as_u64() + offset;
        let dst = dst_va.as_mut_ptr::<u8>();
        unsafe {
            // Zero the 64-byte slot
            core::ptr::write_bytes(dst, 0, 64);
            // tag (4 bytes)
            core::ptr::copy_nonoverlapping(msg.tag.to_le_bytes().as_ptr(), dst, 4);
            // len (4 bytes)
            core::ptr::copy_nonoverlapping(msg.len.to_le_bytes().as_ptr(), dst.add(4), 4);
            // data
            core::ptr::copy_nonoverlapping(msg.data.as_ptr(), dst.add(8), msg.len as usize);
        }
    }
}

// (sys_recvmsg follows)

// ——— sys_recvmsg ——————————————————————————————————————————————————————————

/// sys_recvmsg (#47) — receive a message from a connected socket.
///
/// `msg_ptr` points to a user buffer (64 bytes) that will receive the message.
/// Blocks if no message is available.
fn sys_recvmsg(fd: i32, msg_ptr: u64, _flags: u32) -> SyscallResult {
    if let Err(e) = validate_user_buffer(msg_ptr, 64) {
        return e;
    }

    let tf_ptr = CURRENT_SYSCALL_TF.load(Ordering::Relaxed) as *const TrapFrame;
    let pid = crate::process::scheduler::current_pid().unwrap_or(0);

    let channel_id = match get_fd_channel(pid, fd as usize) {
        Some(id) => id,
        None => return ENOTSOCK,
    };

    unsafe { core::arch::asm!("cli"); }

    // Fast path: message already queued.
    let queued = CHANNELS.lock().get_mut(channel_id).and_then(|ch| ch.dequeue());

    if let Some(m) = queued {
        unsafe { core::arch::asm!("sti"); }
        unsafe {
            let ptr = msg_ptr as *mut u8;
            ptr.write_bytes(0, 64);
            core::ptr::copy_nonoverlapping(m.tag.to_le_bytes().as_ptr(), ptr,       4);
            core::ptr::copy_nonoverlapping(m.len.to_le_bytes().as_ptr(), ptr.add(4), 4);
            core::ptr::copy_nonoverlapping(m.data.as_ptr(),              ptr.add(8), m.len as usize);
        }
        return 64;
    }

    // Slow path: block.
    // Pre-translate the user buffer VA → physical address so that the sender
    // (sys_sendmsg) can write directly to physical memory without a page-table
    // walk on the delivery fast path.
    let phys_buf = {
        use x86_64::{VirtAddr, structures::paging::{Page, Size4KiB}};
        let page   = Page::<Size4KiB>::containing_address(VirtAddr::new(msg_ptr));
        let offset = msg_ptr & 0xFFF;
        // cli is already set; safe to acquire scheduler read-only.
        let sched = super::scheduler::local_scheduler();
        sched.running_ref()
            .and_then(|proc| unsafe { proc.address_space.translate_page(page) })
            .map(|frame| frame.start_address().as_u64() + offset)
            .unwrap_or(0)
        // sched guard dropped here
    };

    *RECV_WAITER.lock() = Some(RecvWaiter { pid, channel_id, phys_buf });

    let next_tf = {
        let mut sched = super::scheduler::local_scheduler();
        sched.block_current(tf_ptr)
    };
    unsafe { super::trapframe::jump_to_user(next_tf) }
}

use errno::*;

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

fn clear_epoll_fd_all(pid: usize) {
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

    let mut sched = super::scheduler::local_scheduler();
    sched.wake_with_retval(waiter.pid, count as u64);
    // sched guard dropped; caller (keyboard ISR) still holds IF=0
}

/// Called from sys_sendmsg after enqueuing a message (CHANNELS released).
///
/// Wakes any process blocked in poll/epoll_wait watching `channel_id` for POLLIN.
pub(crate) fn poll_wakeup_for_channel(channel_id: ChannelId) {
    unsafe { core::arch::asm!("cli"); }

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

    let Some(waiter) = waiter else {
        unsafe { core::arch::asm!("sti"); }
        return;
    };

    if let Some(tid) = waiter.timer_id {
        crate::time::hrtimer::cancel(tid);
    }

    let count = deliver_poll_result_phys(&waiter, phys_offset);
    {
        let mut sched = super::scheduler::local_scheduler();
        sched.wake_with_retval(waiter.pid, count as u64);
    }
    unsafe { core::arch::asm!("sti"); }
}

/// Cancel a pending poll/epoll waiter for a process (called on exit).
fn poll_cancel_waiter(pid: usize) {
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
    let sched = super::scheduler::local_scheduler();
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
fn sys_poll(fds_ptr: u64, nfds: u32, timeout_ms: i32) -> SyscallResult {
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

    unsafe { core::arch::asm!("cli"); }

    let pid = crate::process::scheduler::current_pid().unwrap_or(0);

    // Fast path: check all fds for immediate readiness
    let mut ready = 0i32;
    for i in 0..nfds as usize {
        let rev = fd_check_ready(pid, fds[i].fd, fds[i].events);
        fds[i].revents = rev;
        if rev != 0 { ready += 1; }
    }

    if ready > 0 || timeout_ms == 0 {
        unsafe { core::arch::asm!("sti"); }
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
        None => {
            unsafe { core::arch::asm!("sti"); }
            return errno::EFAULT;
        }
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
        let mut sched = super::scheduler::local_scheduler();
        sched.block_current(tf_ptr)
    };
    unsafe { super::trapframe::jump_to_user(next_tf) }
}

// ── sys_epoll_create ───────────────────────────────────────────────────────

/// epoll_create(213) — create an epoll instance.
///
/// `size` is ignored (Linux ≥ 2.6.8 ignores it too, kept for ABI).
/// Returns a file descriptor referring to the new epoll instance.
fn sys_epoll_create(_size: i32) -> SyscallResult {
    let epoll_id = {
        let pid = crate::process::scheduler::current_pid().unwrap_or(0);
        let mut instances = EPOLL_INSTANCES.lock();
        match instances.alloc(pid) {
            Some(id) => id,
            None => return errno::ENOMEM,
        }
    };

    let handle = alloc::boxed::Box::new(EpollHandle { epoll_id });

    unsafe { core::arch::asm!("cli"); }
    let result = {
        let mut sched = super::scheduler::local_scheduler();
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
    };
    unsafe { core::arch::asm!("sti"); }
    result
}

// ── sys_epoll_ctl ──────────────────────────────────────────────────────────

/// epoll_ctl(233) — modify an epoll instance's interest list.
fn sys_epoll_ctl(epfd: i32, op: i32, fd: i32, event_ptr: u64) -> SyscallResult {
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
fn sys_epoll_wait(epfd: i32, events_ptr: u64, maxevents: i32, timeout_ms: i32) -> SyscallResult {
    if maxevents <= 0 || maxevents > 16 { return errno::EINVAL; }
    let buf_size = maxevents as usize * 12; // sizeof(EpollEvent)
    if let Err(e) = validate_user_buffer(events_ptr, buf_size) { return e; }

    let pid = crate::process::scheduler::current_pid().unwrap_or(0);
    if pid >= MAX_PROCS { return errno::ESRCH; }
    if epfd < 0 || (epfd as usize) >= MAX_FILES_PER_PROC { return errno::EBADF; }

    let epoll_id = get_epoll_fd(pid, epfd as usize);
    if epoll_id == 0 { return errno::EBADF; }

    unsafe { core::arch::asm!("cli"); }

    // Fast path: check readiness now
    let ready = check_epoll_ready_uva(epoll_id, pid, events_ptr, maxevents as usize);

    if ready > 0 || timeout_ms == 0 {
        unsafe { core::arch::asm!("sti"); }
        return ready as SyscallResult;
    }

    // ── Slow path: block ──────────────────────────────────────────────────
    let tf_ptr = CURRENT_SYSCALL_TF.load(Ordering::Relaxed) as *const TrapFrame;

    let phys_buf = match translate_user_buf_phys(events_ptr, buf_size) {
        Some(pa) => pa,
        None => {
            unsafe { core::arch::asm!("sti"); }
            return errno::EFAULT;
        }
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
        let mut sched = super::scheduler::local_scheduler();
        sched.block_current(tf_ptr)
    };
    unsafe { super::trapframe::jump_to_user(next_tf) }
}

