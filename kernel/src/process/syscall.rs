// kernel/src/process/syscall.rs
//
// All syscalls use `with_current_process` or `with_scheduler` helpers
// that guarantee cli before lock, lock dropped before sti.
//
// with_current_process uses scheduler.running_mut() for O(1) access.

use core::arch::global_asm;
use crate::serial_println;

global_asm!(
    ".global syscall_entry",
    "syscall_entry:",
    
    "push rax",
    "push rbx",
    "push rcx",
    "push rdx",
    "push rsi",
    "push rdi",
    "push rbp",
    "push r8",
    "push r9",
    "push r10",
    "push r11",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    
    "mov rdi, rsp",
    "call syscall_handler_asm",
    
    "mov [rsp], rax",
    
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop r11",
    "pop r10",
    "pop r9",
    "pop r8",
    "pop rbp",
    "pop rdi",
    "pop rsi",
    "pop rdx",
    "pop rcx",
    "pop rbx",
    "pop rax",
    
    "iretq",
);

#[repr(C)]
struct SavedRegisters {
    r15: u64, r14: u64, r13: u64, r12: u64,
    r11: u64, r10: u64, r9: u64, r8: u64,
    rbp: u64, rdi: u64, rsi: u64, rdx: u64,
    rcx: u64, rbx: u64, rax: u64,
}

#[no_mangle]
extern "C" fn syscall_handler_asm(regs: &SavedRegisters) -> i64 {
    syscall_handler(regs.rax, regs.rdi, regs.rsi, regs.rdx, regs.r10, regs.r8, regs.r9)
}

#[derive(Debug, Clone, Copy)]
#[repr(u64)]
pub enum SyscallNumber {
    Read = 0,
    Write = 1,
    Open = 2,
    Close = 3,
    Yield = 24,
    GetPid = 39,
    Exit = 60,
}

impl SyscallNumber {
    pub fn from_u64(n: u64) -> Option<Self> {
        match n {
            0 => Some(Self::Read),
            1 => Some(Self::Write),
            2 => Some(Self::Open),
            3 => Some(Self::Close),
            24 => Some(Self::Yield),
            39 => Some(Self::GetPid),
            60 => Some(Self::Exit),
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
    pub const EINTR: i64 = -4;
    pub const EIO: i64 = -5;
    pub const ENXIO: i64 = -6;
    pub const EBADF: i64 = -9;
    pub const ENOMEM: i64 = -12;
    pub const EACCES: i64 = -13;
    pub const EFAULT: i64 = -14;
    pub const ENOTBLK: i64 = -15;
    pub const EBUSY: i64 = -16;
    pub const EEXIST: i64 = -17;
    pub const EINVAL: i64 = -22;
    pub const ENOSYS: i64 = -38;
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
        let mut scheduler = super::scheduler::SCHEDULER.lock();
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
        let mut scheduler = super::scheduler::SCHEDULER.lock();
        f(&mut scheduler)
    };

    unsafe { core::arch::asm!("sti"); }
    result
}

// ============================================================================
// MEMORY VALIDATION
// ============================================================================

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
    _arg4: u64,
    _arg5: u64,
    _arg6: u64,
) -> SyscallResult {
    let syscall = match SyscallNumber::from_u64(syscall_num) {
        Some(s) => s,
        None => return errno::ENOSYS,
    };

    match syscall {
        SyscallNumber::Read => sys_read(arg1 as i32, arg2 as usize, arg3 as usize),
        SyscallNumber::Write => sys_write(arg1 as i32, arg2 as usize, arg3 as usize),
        SyscallNumber::Open => sys_open(arg1 as usize, arg2 as i32),
        SyscallNumber::Close => sys_close(arg1 as i32),
        SyscallNumber::Yield => sys_yield(),
        SyscallNumber::GetPid => sys_getpid(),
        SyscallNumber::Exit => sys_exit(arg1 as i32),
    }
}

// ============================================================================
// SYSCALL IMPLEMENTATIONS
// ============================================================================

fn sys_read(fd: i32, buf: usize, count: usize) -> SyscallResult {
    if let Err(e) = validate_user_buffer(buf as u64, count) {
        return e;
    }

    with_current_process(|proc| {
        let file = match proc.files.get_mut(fd as usize) {
            Ok(f) => f,
            Err(_) => return errno::EBADF,
        };

        let buffer = unsafe {
            core::slice::from_raw_parts_mut(buf as *mut u8, count)
        };

        match file.read(buffer) {
            Ok(n) => n as i64,
            Err(_) => errno::EIO,
        }
    })
}

fn sys_write(fd: i32, buf: usize, count: usize) -> SyscallResult {
    if let Err(e) = validate_user_buffer(buf as u64, count) {
        return e;
    }

    with_current_process(|proc| {
        let file = match proc.files.get_mut(fd as usize) {
            Ok(f) => f,
            Err(_) => return errno::EBADF,
        };

        let buffer = unsafe {
            core::slice::from_raw_parts(buf as *const u8, count)
        };

        match file.write(buffer) {
            Ok(n) => n as i64,
            Err(_) => errno::EIO,
        }
    })
}

fn sys_open(path_ptr: usize, _flags: i32) -> SyscallResult {
    // Validation BEFORE cli — no lock needed
    if let Err(e) = validate_user_buffer(path_ptr as u64, 256) {
        return e;
    }

    let path_bytes = unsafe {
        let mut len = 0;
        let ptr = path_ptr as *const u8;
        while len < 256 && *ptr.add(len) != 0 {
            len += 1;
        }
        core::slice::from_raw_parts(ptr, len)
    };

    let path = match core::str::from_utf8(path_bytes) {
        Ok(s) => s,
        Err(_) => return errno::EINVAL,
    };

    // Ask the driver registry for a handle.
    // Box allocation uses Slab (different lock from SCHEDULER).
    let handle = match crate::drivers::open_device(path) {
        Some(h) => h,
        None => return errno::ENOENT,
    };

    // Only take scheduler lock for the FD table insertion
    with_current_process(|proc| {
        match proc.files.allocate(handle) {
            Ok(fd) => fd as i64,
            Err(_) => errno::EINVAL,
        }
    })
}

fn sys_close(fd: i32) -> SyscallResult {
    with_current_process(|proc| {
        match proc.files.close(fd as usize) {
            Ok(_) => 0,
            Err(_) => errno::EBADF,
        }
    })
}

fn sys_yield() -> SyscallResult {
    // TODO: Trigger voluntary context switch
    0
}

fn sys_getpid() -> SyscallResult {
    with_scheduler(|scheduler| {
        scheduler.current_pid().map(|pid| pid.0 as SyscallResult).unwrap_or(0)
    })
}

fn sys_exit(status: i32) -> SyscallResult {
    use alloc::format;

    let reason = format!("exit({})", status);

    with_scheduler(|scheduler| {
        scheduler.kill_current(&reason);
        0
    });

    // Process is now Zombie in wait_queue.
    // Halt until the timer preempts and switches to another process.
    loop {
        unsafe { core::arch::asm!("hlt"); }
    }
}