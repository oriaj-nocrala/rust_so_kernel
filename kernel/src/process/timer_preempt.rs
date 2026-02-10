// kernel/src/process/timer_preempt.rs
// ✅ TIMER HANDLER CORRECTO: Preempta a TODOS los procesos (kernel y user)

use core::arch::global_asm;
use super::trapframe::TrapFrame;

global_asm!(
    ".global timer_interrupt_entry",
    "timer_interrupt_entry:",
    
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
    
    // Llamar al handler con puntero al TrapFrame actual
    "mov rdi, rsp",
    "call timer_preempt_handler",
    
    // El handler retorna el nuevo TrapFrame en RAX
    // Cambiar RSP al nuevo TrapFrame
    "mov rsp, rax",
    
    // Restaurar registros del NUEVO proceso
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
    
    // IRETQ al NUEVO proceso (puede ser kernel o user)
    "iretq",
);

extern "C" {
    pub fn timer_interrupt_entry();
}

#[no_mangle]
pub extern "C" fn timer_preempt_handler(current_tf: *const TrapFrame) -> *const TrapFrame {
    // ============ 1. EOI ============
    unsafe {
        use x86_64::instructions::port::PortWriteOnly;
        PortWriteOnly::<u8>::new(0x20).write(0x20);
    }
    
    // ============ 2. THROTTLE ============
    // No hacer context switch en cada tick, solo cada 10
    static mut TICK: usize = 0;
    unsafe {
        TICK += 1;
        if TICK < 2 {
            return current_tf;
        }
        TICK = 0;
    }
    
    // ============ 3. SCHEDULER ============
    // El scheduler maneja el context switch
    let mut scheduler = super::scheduler::SCHEDULER.lock();
    scheduler.switch_to_next(current_tf)
}