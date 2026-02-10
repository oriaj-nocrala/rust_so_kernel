// kernel/src/process/syscall.rs
// ✅ VERSIÓN MEJORADA: Con File Descriptors y validación de memoria

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
// VALIDACIÓN DE MEMORIA
// ============================================================================

/// Valida que un buffer de usuario esté en espacio de usuario
/// 
/// En x86_64 canonical addresses:
/// - User space: 0x0000_0000_0000_0000 - 0x0000_7FFF_FFFF_FFFF
/// - Kernel space: 0xFFFF_8000_0000_0000 - 0xFFFF_FFFF_FFFF_FFFF
fn validate_user_buffer(addr: u64, size: usize) -> Result<(), i64> {
    // 1. Verificar que no es null
    if addr == 0 {
        return Err(errno::EFAULT);
    }
    
    // 2. Verificar que no hay overflow
    let end = addr.checked_add(size as u64)
        .ok_or(errno::EFAULT)?;
    
    // 3. Verificar que está en user space (< 0x0000_8000_0000_0000)
    const USER_SPACE_MAX: u64 = 0x0000_8000_0000_0000;
    if addr >= USER_SPACE_MAX || end > USER_SPACE_MAX {
        return Err(errno::EFAULT);
    }
    
    // TODO: Verificar que las páginas tienen el bit USER_ACCESSIBLE
    // Por ahora, solo verificamos el rango de direcciones
    
    Ok(())
}

// ============================================================================
// SYSCALL HANDLER
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

/// sys_read: Lee de un file descriptor
fn sys_read(fd: i32, buf: usize, count: usize) -> SyscallResult {
    // Validar buffer
    if let Err(e) = validate_user_buffer(buf as u64, count) {
        return e;
    }
    
    // Deshabilitar interrupciones para acceder al scheduler
    unsafe { core::arch::asm!("cli"); }
    
    let result = {
        let mut scheduler = super::scheduler::SCHEDULER.lock();
        
        // Obtener proceso actual
        let proc = match scheduler.current {
            Some(pid) => {
                match scheduler.processes.iter_mut().find(|p| p.pid == pid) {
                    Some(p) => p,
                    None => {
                        unsafe { core::arch::asm!("sti"); }
                        return errno::ESRCH;
                    }
                }
            }
            None => {
                unsafe { core::arch::asm!("sti"); }
                return errno::ESRCH;
            }
        };
        
        // Obtener file handle
        let file = match proc.files.get_mut(fd as usize) {
            Ok(f) => f,
            Err(_) => {
                unsafe { core::arch::asm!("sti"); }
                return errno::EBADF;
            }
        };
        
        // Crear slice mutable del buffer de usuario
        let buffer = unsafe {
            core::slice::from_raw_parts_mut(buf as *mut u8, count)
        };
        
        // Leer del archivo
        match file.read(buffer) {
            Ok(n) => n as i64,
            Err(_) => {
                unsafe { core::arch::asm!("sti"); }
                return errno::EIO;
            }
        }
    };
    
    unsafe { core::arch::asm!("sti"); }
    result
}

/// sys_write: Escribe a un file descriptor
fn sys_write(fd: i32, buf: usize, count: usize) -> SyscallResult {
    // Validar buffer
    serial_println!("👀 Sys write llamado!");
    if let Err(e) = validate_user_buffer(buf as u64, count) {
        return e;
    }
    
    unsafe { core::arch::asm!("cli"); }
    
    let result = {
        let mut scheduler = super::scheduler::SCHEDULER.lock();
        
        // Obtener proceso actual
        let proc = match scheduler.current {
            Some(pid) => {
                match scheduler.processes.iter_mut().find(|p| p.pid == pid) {
                    Some(p) => p,
                    None => {
                        unsafe { core::arch::asm!("sti"); }
                        return errno::ESRCH;
                    }
                }
            }
            None => {
                unsafe { core::arch::asm!("sti"); }
                return errno::ESRCH;
            }
        };
        
        // Obtener file handle
        let file = match proc.files.get_mut(fd as usize) {
            Ok(f) => f,
            Err(_) => {
                unsafe { core::arch::asm!("sti"); }
                return errno::EBADF;
            }
        };
        
        // Crear slice del buffer de usuario
        let buffer = unsafe {
            core::slice::from_raw_parts(buf as *const u8, count)
        };
        
        // Escribir al archivo
        match file.write(buffer) {
            Ok(n) => n as i64,
            Err(_) => {
                unsafe { core::arch::asm!("sti"); }
                return errno::EIO;
            }
        }
    };
    
    unsafe { core::arch::asm!("sti"); }
    result
}

/// sys_open: Abre un "archivo" (de momento solo dispositivos)
/// 
/// arg1: Puntero a string con el path
/// arg2: Flags (ignorados por ahora)
fn sys_open(path_ptr: usize, _flags: i32) -> SyscallResult {
    use alloc::boxed::Box;
    use super::file::*;
    
    // Validar puntero al path
    if let Err(e) = validate_user_buffer(path_ptr as u64, 256) {
        return e;
    }
    
    // Leer el path (limitado a 256 bytes)
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
    
    // Por ahora, solo soportamos algunos dispositivos
    let handle: Box<dyn FileHandle> = match path {
        "/dev/null" => Box::new(DevNull),
        "/dev/zero" => Box::new(DevZero),
        "/dev/console" => Box::new(SerialConsole),
        "/dev/fb" => Box::new(FramebufferConsole::new()),
        _ => return errno::ENOENT,
    };
    
    unsafe { core::arch::asm!("cli"); }
    
    let result = {
        let mut scheduler = super::scheduler::SCHEDULER.lock();
        
        let proc = match scheduler.current {
            Some(pid) => {
                match scheduler.processes.iter_mut().find(|p| p.pid == pid) {
                    Some(p) => p,
                    None => {
                        unsafe { core::arch::asm!("sti"); }
                        return errno::ESRCH;
                    }
                }
            }
            None => {
                unsafe { core::arch::asm!("sti"); }
                return errno::ESRCH;
            }
        };
        
        match proc.files.allocate(handle) {
            Ok(fd) => fd as i64,
            Err(_) => {
                unsafe { core::arch::asm!("sti"); }
                return errno::EINVAL;
            }
        }
    };
    
    unsafe { core::arch::asm!("sti"); }
    result
}

/// sys_close: Cierra un file descriptor
fn sys_close(fd: i32) -> SyscallResult {
    unsafe { core::arch::asm!("cli"); }
    
    let result = {
        let mut scheduler = super::scheduler::SCHEDULER.lock();
        
        let proc = match scheduler.current {
            Some(pid) => {
                match scheduler.processes.iter_mut().find(|p| p.pid == pid) {
                    Some(p) => p,
                    None => {
                        unsafe { core::arch::asm!("sti"); }
                        return errno::ESRCH;
                    }
                }
            }
            None => {
                unsafe { core::arch::asm!("sti"); }
                return errno::ESRCH;
            }
        };
        
        match proc.files.close(fd as usize) {
            Ok(_) => 0,
            Err(_) => {
                unsafe { core::arch::asm!("sti"); }
                return errno::EBADF;
            }
        }
    };
    
    unsafe { core::arch::asm!("sti"); }
    result
}

/// sys_yield: Cede voluntariamente el CPU
fn sys_yield() -> SyscallResult {
    // TODO: Llamar al scheduler para hacer un context switch voluntario
    // Por ahora, simplemente retornamos 0
    0
}

/// sys_getpid: Obtiene el PID del proceso actual
fn sys_getpid() -> SyscallResult {
    unsafe { core::arch::asm!("cli"); }
    
    let result = {
        let scheduler = super::scheduler::SCHEDULER.lock();
        scheduler.current.map(|pid| pid.0 as SyscallResult).unwrap_or(0)
    };
    
    unsafe { core::arch::asm!("sti"); }
    
    result
}

/// sys_exit: Termina el proceso actual
fn sys_exit(status: i32) -> SyscallResult {
    unsafe { core::arch::asm!("cli"); }
    
    {
        let mut scheduler = super::scheduler::SCHEDULER.lock();
        
        // Marcar como zombie
        for proc in scheduler.processes.iter_mut() {
            if proc.state == super::ProcessState::Running {
                proc.state = super::ProcessState::Zombie;
                
                crate::serial_println!(
                    "Process {} exited with status {}",
                    proc.pid.0,
                    status
                );
                
                break;
            }
        }
    }
    
    // Re-habilitar interrupciones
    unsafe { core::arch::asm!("sti"); }
    
    // Dormir hasta que el timer nos saque
    loop {
        unsafe { core::arch::asm!("hlt"); }
    }
}