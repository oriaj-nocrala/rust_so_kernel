// kernel/src/process/context.rs

use x86_64::VirtAddr;

/// Contexto del CPU (registros guardados durante context switch)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Context {
    // Registros de propósito general (callee-saved en System V ABI)
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub rbx: u64,
    pub rbp: u64,

    // Instruction pointer (donde reanudar)
    pub rip: u64,
}

impl Context {
    /// Crea un contexto nuevo apuntando a entry_point
    pub fn new(entry_point: VirtAddr, stack: VirtAddr) -> Self {
        Self {
            r15: 0,
            r14: 0,
            r13: 0,
            r12: 0,
            rbx: 0,
            rbp: stack.as_u64(),  // Stack pointer inicial
            rip: entry_point.as_u64(),
        }
    }

    /// Crea un contexto vacío (para el proceso idle)
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

    /// ✅ NUEVO: Contexto para proceso de usuario con trampolín
    pub fn new_user(entry_point: VirtAddr, kernel_stack: VirtAddr, user_stack: VirtAddr) -> Self {
        // En lugar de ir directo al entry_point, vamos al trampolín
        let mut ctx = Self::new(VirtAddr::new(user_trampoline as u64), kernel_stack);
        
        // Usamos registros callee-saved para pasar datos al trampolín
        // switch_context restaurará estos valores antes de que corra el trampolín
        ctx.r12 = entry_point.as_u64(); // R12 = Dónde saltar en Ring 3
        ctx.r13 = user_stack.as_u64();  // R13 = Stack de Ring 3
        ctx
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
        "mov rax, [rsp]",        // Leer return address del stack
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
        "mov [rsp], rax",        // Poner nuevo rip en el stack
        
        "ret",                   // Saltar al nuevo contexto
        // options(noreturn)
    );
}

/// ✅ NUEVO: Trampolín que lleva de Kernel -> User
#[unsafe(naked)]
unsafe extern "C" fn user_trampoline() {
    core::arch::naked_asm!(
        // Al llegar aquí, switch_context ya restauró R12 y R13
        // R12 contiene el entry_point de usuario
        // R13 contiene el user_stack
        
        "mov rdi, r12", // Primer argumento para jump_to_userspace
        "mov rsi, r13", // Segundo argumento para jump_to_userspace
        
        // Llamar a la función que hace la magia (IRETQ)
        // Esta función NO retorna
        "call jump_to_userspace",
        
        "ud2" // Trap por si acaso retorna (no debería)
    );
}