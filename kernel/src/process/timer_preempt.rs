// kernel/src/process/timer_preempt.rs

use core::arch::global_asm;
use super::trapframe::TrapFrame;

global_asm!(
    ".global timer_interrupt_entry",
    "timer_interrupt_entry:",
    
    // ============ Guardar TODOS los registros ============
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
    
    // Ahora el stack tiene un TrapFrame completo:
    // [registros] + [RIP, CS, RFLAGS, RSP, SS] (del hardware)
    
    // Pasar puntero al TrapFrame como argumento
    "mov rdi, rsp",
    "call timer_preempt_handler",
    
    // RAX contiene el puntero al TrapFrame del SIGUIENTE proceso
    // (o el mismo si no hay cambio)
    
    // Restaurar desde el TrapFrame retornado
    "mov rsp, rax",
    
    // ============ Restaurar registros ============
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
    
    // IRETQ restaura: RIP, CS, RFLAGS, RSP, SS
    "iretq",
);

extern "C" {
    pub fn timer_interrupt_entry();
}

/// Handler de preemption - llamado desde assembly
#[no_mangle]
pub extern "C" fn timer_preempt_handler(current_tf: *mut TrapFrame) -> *const TrapFrame {
    // EOI
    unsafe {
        use x86_64::instructions::port::PortWriteOnly;
        PortWriteOnly::<u8>::new(0x20).write(0x20);
    }
    
    static mut TICK: usize = 0;
    unsafe {
        TICK += 1;
        if TICK < 10 { return current_tf; }
        TICK = 0;
    }
    
    let mut scheduler = super::scheduler::SCHEDULER.lock();
    
    // Guardar estado del proceso actual
    if let Some(current_pid) = scheduler.current {
        if let Some(proc) = scheduler.processes.iter_mut().find(|p| p.pid == current_pid) {
            if proc.privilege == super::PrivilegeLevel::User {
                if let Some(ref mut tf) = proc.trapframe {
                    unsafe { **tf = *current_tf; }
                }
            }
            proc.state = super::ProcessState::Ready;
        }
    }
    
    // Buscar siguiente proceso (round-robin manual)
    let len = scheduler.processes.len();
    let mut found = None;
    
    // En el loop, cambiar:
    for _ in 0..len {
        if let Some(mut proc) = scheduler.processes.pop_front() {
            if proc.state == super::ProcessState::Ready {
                proc.state = super::ProcessState::Running;
                let pid = proc.pid;
                
                super::tss::set_kernel_stack(proc.kernel_stack);
                
                let result = if proc.privilege == super::PrivilegeLevel::User {
                    proc.trapframe.as_ref().map(|tf| &**tf as *const TrapFrame)
                } else {
                    None
                };
                
                scheduler.current = Some(pid);
                scheduler.processes.push_back(proc);  // ← Mover primero
                
                if let Some(tf) = result {
                    found = Some(tf);
                    break;
                }
            } else {
                scheduler.processes.push_back(proc);  // ← También aquí
            }
        }
    }
    
    found.unwrap_or(current_tf)
}