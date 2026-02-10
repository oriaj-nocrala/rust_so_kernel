// kernel/src/process/trapframe.rs
// TrapFrame con función para saltar al primer proceso

use core::arch::global_asm;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct TrapFrame {
    // Registros de propósito general (pushados por nuestro código)
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    
    // IRETQ frame (pushado por hardware)
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

// Función en assembly para saltar a un TrapFrame
// Esto se usa SOLO para arrancar el primer proceso
global_asm!(
    ".global jump_to_trapframe",
    "jump_to_trapframe:",
    
    // RDI contiene el puntero al TrapFrame
    "mov rsp, rdi",  // Apuntar RSP al TrapFrame
    
    // Restaurar registros generales
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
    
    // IRETQ lee: RIP, CS, RFLAGS, RSP, SS del stack
    "iretq",
);

extern "C" {
    pub fn jump_to_trapframe(tf: *const TrapFrame) -> !;
}