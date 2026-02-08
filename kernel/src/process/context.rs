// kernel/src/process/context.rs
// Basado en xv6's context

use x86_64::VirtAddr;

use crate::serial_println;

/// Contexto del CPU para context switch en KERNEL MODE
/// 
/// Solo contiene registros callee-saved (System V ABI)
/// NO se usa para saltar a user mode - para eso está TrapFrame
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Context {
    // Callee-saved registers (System V ABI)
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub rbx: u64,
    pub rbp: u64,
    
    // Instruction pointer (donde reanudar en kernel mode)
    pub rip: u64,
}

impl Context {
    /// Crea un contexto que apunta a una función de kernel
    pub fn new(entry_point: VirtAddr, stack: VirtAddr) -> Self {
        Self {
            r15: 0,
            r14: 0,
            r13: 0,
            r12: 0,
            rbx: 0,
            rbp: stack.as_u64(),
            rip: entry_point.as_u64(),
        }
    }

    /// Crea un contexto vacío (para proceso idle)
    pub const fn empty() -> Self {
        Self {
            r15: 0,
            r14: 0,
            r13: 0,
            r12: 0,
            rbx: 0,
            rbp: 0,
            rip: 0,
        }
    }

    /// ✅ NUEVO: Crea un contexto que apunta a forkret
    /// (primera función que ejecuta un proceso user cuando se schedulea)
    pub fn new_for_user_process(kernel_stack: VirtAddr) -> Self {
        extern "C" {
            fn forkret();
        }
        
        Self {
            r15: 0,
            r14: 0,
            r13: 0,
            r12: 0,
            rbx: 0,
            rbp: kernel_stack.as_u64(),
            rip: forkret as u64,
        }
    }
}

/// Switch de contexto (en assembly)
/// 
/// Guarda el contexto actual en `old` y carga el contexto de `new`
#[unsafe(naked)]
pub unsafe extern "C" fn switch_context(old: *mut Context, new: *const Context) {
    core::arch::naked_asm!(
        // Guardar contexto actual (callee-saved registers)
        "mov [rdi + 0x00], r15",
        "mov [rdi + 0x08], r14",
        "mov [rdi + 0x10], r13",
        "mov [rdi + 0x18], r12",
        "mov [rdi + 0x20], rbx",
        "mov [rdi + 0x28], rbp",
        
        // Guardar rip (dirección de retorno)
        "mov rax, [rsp]",
        "mov [rdi + 0x30], rax",
        
        // Cargar nuevo contexto
        "mov r15, [rsi + 0x00]",
        "mov r14, [rsi + 0x08]",
        "mov r13, [rsi + 0x10]",
        "mov r12, [rsi + 0x18]",
        "mov rbx, [rsi + 0x20]",
        "mov rbp, [rsi + 0x28]",
        
        // Saltar al nuevo rip
        "mov rax, [rsi + 0x30]",
        "mov [rsp], rax",
        
        "ret",
    );
}

/// forkret: Primera función que ejecuta un proceso user
/// Hace cleanup y llama a trapret
#[no_mangle]
extern "C" fn forkret() {
    // TODO: Aquí podrías hacer cleanup (como en xv6)
    // Por ahora, directamente llamamos a trapret
    
    unsafe {
        // Obtener el proceso actual
        let mut scheduler = crate::process::scheduler::SCHEDULER.lock();
        
        if let Some(pid) = scheduler.current {
            if let Some(proc) = scheduler.processes.iter_mut().find(|p| p.pid == pid) {
                crate::serial_println!("forkret: PID {} entering userspace", pid.0);
                
                // Obtener puntero al trapframe
                if let Some(ref tf) = proc.trapframe {
                    // ✅ FIX: Usar &**tf para obtener &TrapFrame desde &Box<TrapFrame>
                    let tf_ptr = &**tf as *const super::trapframe::TrapFrame;
                    
                    // Liberar el lock antes de IRETQ
                    drop(scheduler);
                    
                    // Nunca retorna
                    super::trapret::trapret_debug(tf_ptr);
                }
            }
        }
        
        panic!("forkret: No process to return to!");
    }
}