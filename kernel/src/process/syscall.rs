// kernel/src/process/syscall.rs

use core::arch::global_asm;

// ✅ Assembly correcto que preserva TODOS los registros
global_asm!(
    ".global syscall_entry",
    "syscall_entry:",
    
    // Guardar TODOS los registros
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
    
    // Ahora RSP apunta al principio del bloque guardado
    // Pasar RSP como único argumento (puntero a los registros)
    "mov rdi, rsp",
    "call syscall_handler_asm",
    
    // RAX tiene el resultado, lo guardamos en el stack
    "mov [rsp], rax",  // Sobreescribir el RAX guardado con el resultado
    
    // Restaurar registros
    "pop rax",         // Este es el resultado ahora
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

// ✅ Estructura que representa los registros guardados
#[repr(C)]
struct SavedRegisters {
    r15: u64,
    r14: u64,
    r13: u64,
    r12: u64,
    r11: u64,
    r10: u64,
    r9: u64,
    r8: u64,
    rbp: u64,
    rdi: u64,
    rsi: u64,
    rdx: u64,
    rcx: u64,
    rbx: u64,
    rax: u64,
}

// ✅ Wrapper que lee los registros del stack
#[no_mangle]
extern "C" fn syscall_handler_asm(regs: &SavedRegisters) -> i64 {
    syscall_handler(
        regs.rax,  // syscall_num
        regs.rdi,  // arg1
        regs.rsi,  // arg2
        regs.rdx,  // arg3
        regs.r10,  // arg4
        regs.r8,   // arg5
        regs.r9,   // arg6
    )
}

/// Números de syscall compatibles con Linux x86_64
#[derive(Debug, Clone, Copy)]
#[repr(u64)]
pub enum SyscallNumber {
    Read = 0,
    Write = 1,
    Open = 2,
    Close = 3,
    Exit = 60,
    GetPid = 39,
}

impl SyscallNumber {
    pub fn from_u64(n: u64) -> Option<Self> {
        match n {
            0 => Some(Self::Read),
            1 => Some(Self::Write),
            2 => Some(Self::Open),
            3 => Some(Self::Close),
            39 => Some(Self::GetPid),
            60 => Some(Self::Exit),
            _ => None,
        }
    }
}

/// Resultado de una syscall
pub type SyscallResult = i64;

/// Códigos de error compatibles con Linux (negados)
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

/// Handler principal de syscalls
pub fn syscall_handler(
    syscall_num: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    _arg4: u64,
    _arg5: u64,
    _arg6: u64,
) -> SyscallResult {
    crate::serial_println!(
        "SYSCALL: num={}, args=({:#x}, {:#x}, {:#x})",
        syscall_num, arg1, arg2, arg3
    );

    let syscall = match SyscallNumber::from_u64(syscall_num) {
        Some(s) => s,
        None => {
            crate::serial_println!("  Unknown syscall: {}", syscall_num);
            return errno::ENOSYS;
        }
    };

    match syscall {
        SyscallNumber::Write => sys_write(arg1 as i32, arg2 as usize, arg3 as usize),
        SyscallNumber::Read => sys_read(arg1 as i32, arg2 as usize, arg3 as usize),
        SyscallNumber::Exit => sys_exit(arg1 as i32),
        SyscallNumber::GetPid => sys_getpid(),
        SyscallNumber::Open => errno::ENOSYS,
        SyscallNumber::Close => errno::ENOSYS,
    }
}

/// sys_write(fd, buf, count)
fn sys_write(fd: i32, buf: usize, count: usize) -> SyscallResult {
    crate::serial_println!("  sys_write(fd={}, buf={:#x}, count={})", fd, buf, count);

    if fd != 1 && fd != 2 {
        return errno::EBADF;
    }

    if buf == 0 {
        return errno::EFAULT;
    }

    let slice = unsafe {
        core::slice::from_raw_parts(buf as *const u8, count)
    };

    for &byte in slice {
        unsafe {
            let mut port = x86_64::instructions::port::Port::<u8>::new(0x3F8);
            port.write(byte);
        }
    }

    count as SyscallResult
}

/// sys_read(fd, buf, count)
fn sys_read(_fd: i32, _buf: usize, _count: usize) -> SyscallResult {
    errno::ENOSYS
}

/// sys_exit(status)
fn sys_exit(status: i32) -> SyscallResult {
    crate::serial_println!("  sys_exit(status={})", status);
    
    {
        let mut scheduler = super::scheduler::SCHEDULER.lock();
        
        for proc in scheduler.processes.iter_mut() {
            if proc.state == super::ProcessState::Running {
                proc.state = super::ProcessState::Zombie;
                crate::serial_println!("  Process PID {} exited with status {}", proc.pid.0, status);
                break;
            }
        }
    }
    
    // ✅ FIX: Hacer yield manualmente en lugar de llamar a yield_cpu()
    loop {
        use super::context::switch_context;
        
        let switch_info = {
            let mut scheduler = super::scheduler::SCHEDULER.lock();
            scheduler.switch_to_next()
        };
        
        if let Some((old_ctx, new_ctx)) = switch_info {
            unsafe {
                switch_context(old_ctx, new_ctx);
            }
        }
    }
}

/// sys_getpid()
fn sys_getpid() -> SyscallResult {
    let scheduler = super::scheduler::SCHEDULER.lock();
    
    if let Some(pid) = scheduler.current {
        crate::serial_println!("  sys_getpid() -> {}", pid.0);
        pid.0 as SyscallResult
    } else {
        0
    }
}