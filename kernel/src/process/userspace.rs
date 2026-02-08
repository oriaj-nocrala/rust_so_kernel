// kernel/src/process/userspace.rs

use x86_64::VirtAddr;
use core::arch::asm;

/// Salta a user space (Ring 3)
/// 
/// # Safety
/// - `entry_point` debe ser código ejecutable válido
/// - `user_stack` debe apuntar a memoria válida
#[no_mangle]
pub unsafe extern "C" fn jump_to_userspace(entry_point: VirtAddr, user_stack: VirtAddr) -> ! {
    crate::serial_println!("funcion jump_to_userspace");
    let (user_cs, user_ds) = super::tss::get_user_selectors();
    
    let user_cs_val = user_cs.0 as u64 | 3;
    let user_ds_val = user_ds.0 as u64 | 3;
    let rflags: u64 = 0x200; // Interrupt enable flag
    
    crate::serial_println!("Jumping to userspace:");
    crate::serial_println!("  Entry: {:#x}", entry_point.as_u64());
    crate::serial_println!("  Stack: {:#x}", user_stack.as_u64());
    
    asm!(
        // Configurar segmentos de datos
        "mov ds, {0:x}",
        "mov es, {0:x}",
        "mov fs, {0:x}",
        "mov gs, {0:x}",
        
        // ✅ LIMPIAR TODOS LOS REGISTROS
        "xor rax, rax",
        "xor rbx, rbx",
        "xor rcx, rcx",
        "xor rdx, rdx",
        "xor rsi, rsi",
        "xor rdi, rdi",
        "xor rbp, rbp",
        "xor r8, r8",
        "xor r9, r9",
        "xor r10, r10",
        "xor r11, r11",
        "xor r12, r12",
        "xor r13, r13",
        "xor r14, r14",
        "xor r15, r15",
        
        // Preparar stack para IRETQ
        "push {0:r}",           // SS
        "push {1:r}",           // RSP
        "push {2:r}",           // RFLAGS
        "push {3:r}",           // CS
        "push {4:r}",           // RIP
        
        // 4. ¡Salto de fe!
        "iretq",
        
        in(reg) user_ds_val,
        in(reg) user_stack.as_u64(),
        in(reg) rflags,
        in(reg) user_cs_val,
        in(reg) entry_point.as_u64(),
        options(noreturn)
    );
}

/// Syscall wrappers para user space

#[inline(always)]
pub fn sys_write(fd: i32, buf: *const u8, count: usize) -> isize {
    let result: isize;
    unsafe {
        core::arch::asm!(
            "mov rax, 1",
            "mov rdi, {0}",
            "mov rsi, {1}",
            "mov rdx, {2}",
            "int 0x80",
            "mov {3}, rax",
            in(reg) fd as u64,
            in(reg) buf as u64,
            in(reg) count,
            out(reg) result,
            // ✅ IMPORTANTE: Marcar registros que la syscall puede corromper
            lateout("rax") _,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    result
}

#[inline(always)]
pub fn sys_exit(status: i32) -> ! {
    unsafe {
        core::arch::asm!(
            "mov rax, 60",
            "mov rdi, {0}",
            "int 0x80",
            in(reg) status as u64,
            options(noreturn)
        );
    }
}

#[inline(always)]
pub fn sys_getpid() -> i32 {
    let result: i64;
    unsafe {
        core::arch::asm!(
            "mov rax, 39",
            "int 0x80",
            lateout("rax") result,
            // ✅ Marcar otros registros
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    result as i32
}