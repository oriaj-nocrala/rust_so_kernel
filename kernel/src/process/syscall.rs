// kernel/src/process/syscall.rs
// ✅ VERSIÓN LIMPIA: Usar cli/sti para evitar deadlock

use core::arch::global_asm;

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
    
    "pop rax",
    "pop rbx",
    "pop rcx",
    "pop rdx",
    "pop rsi",
    "pop rdi",
    "pop rbp",
    "pop r8",
    "pop r9",
    "pop r10",
    "pop r11",
    "pop r12",
    "pop r13",
    "pop r14",
    "pop r15",
    
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
    Read = 0, Write = 1, Open = 2, Close = 3,
    Yield = 24, GetPid = 39, Exit = 60,
}

impl SyscallNumber {
    pub fn from_u64(n: u64) -> Option<Self> {
        match n {
            0 => Some(Self::Read), 1 => Some(Self::Write),
            2 => Some(Self::Open), 3 => Some(Self::Close),
            24 => Some(Self::Yield), 39 => Some(Self::GetPid),
            60 => Some(Self::Exit), _ => None,
        }
    }
}

pub type SyscallResult = i64;

#[allow(dead_code)]
pub mod errno {
    pub const EPERM: i64 = -1; pub const ENOENT: i64 = -2;
    pub const ESRCH: i64 = -3; pub const EINTR: i64 = -4;
    pub const EIO: i64 = -5; pub const ENXIO: i64 = -6;
    pub const EBADF: i64 = -9; pub const ENOMEM: i64 = -12;
    pub const EACCES: i64 = -13; pub const EFAULT: i64 = -14;
    pub const ENOTBLK: i64 = -15; pub const EBUSY: i64 = -16;
    pub const EEXIST: i64 = -17; pub const EINVAL: i64 = -22;
    pub const ENOSYS: i64 = -38;
}

pub fn syscall_handler(
    syscall_num: u64, arg1: u64, arg2: u64, arg3: u64,
    _arg4: u64, _arg5: u64, _arg6: u64,
) -> SyscallResult {
    let syscall = match SyscallNumber::from_u64(syscall_num) {
        Some(s) => s,
        None => return errno::ENOSYS,
    };

    match syscall {
        SyscallNumber::Write => sys_write(arg1 as i32, arg2 as usize, arg3 as usize),
        SyscallNumber::Read => errno::ENOSYS,
        SyscallNumber::Exit => sys_exit(arg1 as i32),
        SyscallNumber::GetPid => sys_getpid(),
        SyscallNumber::Open | SyscallNumber::Close => errno::ENOSYS,
        SyscallNumber::Yield => 0,
    }
}

fn sys_write(fd: i32, buf: usize, count: usize) -> SyscallResult {
    if fd != 1 && fd != 2 { return errno::EBADF; }
    if buf == 0 { return errno::EFAULT; }
    
    let slice = unsafe { core::slice::from_raw_parts(buf as *const u8, count) };
    for &byte in slice {
        unsafe {
            x86_64::instructions::port::Port::<u8>::new(0x3F8).write(byte);
        }
    }
    count as SyscallResult
}

fn sys_exit(status: i32) -> SyscallResult {
    // ✅ CRÍTICO: cli ANTES de tomar el lock
    unsafe { core::arch::asm!("cli"); }
    
    {
        let mut scheduler = super::scheduler::SCHEDULER.lock();
        for proc in scheduler.processes.iter_mut() {
            if proc.state == super::ProcessState::Running {
                proc.state = super::ProcessState::Zombie;
                break;
            }
        }
    }
    
    unsafe { core::arch::asm!("sti"); }
    loop { unsafe { core::arch::asm!("hlt"); } }
}

/// ✅ CRÍTICO: sys_getpid con cli/sti para evitar deadlock
fn sys_getpid() -> SyscallResult {
    // Deshabilitar interrupciones ANTES de tomar el lock
    // Esto garantiza que el timer interrupt NO puede interrumpirnos
    // mientras tenemos el SCHEDULER lock
    unsafe { core::arch::asm!("cli"); }
    
    let result = {
        let scheduler = super::scheduler::SCHEDULER.lock();
        scheduler.current.map(|pid| pid.0 as SyscallResult).unwrap_or(0)
    };
    
    // Re-habilitar interrupciones DESPUÉS de liberar el lock
    unsafe { core::arch::asm!("sti"); }
    
    result
}