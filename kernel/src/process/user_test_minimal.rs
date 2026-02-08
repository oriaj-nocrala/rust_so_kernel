// kernel/src/process/user_test_minimal.rs
// Código de usuario MINIMAL para testing

use core::arch::global_asm;

/// ✅ TEST 1: Loop infinito (lo más simple posible)
/// 
/// Esto solo hace un salto a sí mismo infinitamente.
/// Si esto funciona, sabemos que:
/// - IRETQ funcionó
/// - El proceso está en Ring 3
/// - Las páginas están mapeadas correctamente
global_asm!(
    ".global user_infinite_loop",
    ".section .text.user",
    "user_infinite_loop:",
    "1:",
    "    jmp 1b",  // Loop infinito
);

/// ✅ TEST 2: HLT instruction
/// 
/// Esto debería causar #GP inmediatamente porque HLT es privilegiada.
/// Si ves #GP con error code específico, sabes que:
/// - Sí está en Ring 3 (porque HLT falló)
/// - IRETQ funcionó
global_asm!(
    ".global user_hlt_test",
    ".section .text.user",
    "user_hlt_test:",
    "    hlt",     // Esto debería fallar con #GP
    "    jmp user_hlt_test",
);

/// ✅ TEST 3: Syscall simple
/// 
/// Intenta hacer syscall inmediatamente.
/// Si funciona, sabes que:
/// - IRETQ funcionó
/// - El proceso está en Ring 3
/// - Las syscalls funcionan
global_asm!(
    ".global user_syscall_test",
    ".section .text.user",
    "user_syscall_test:",
    "    mov rax, 39",     // sys_getpid
    "    int 0x80",
    "1:",
    "    jmp 1b",          // Loop después del syscall
);

/// ✅ TEST 4: Write to stack
/// 
/// Intenta escribir en el stack.
/// Si falla, el stack no está mapeado correctamente.
global_asm!(
    ".global user_stack_test",
    ".section .text.user",
    "user_stack_test:",
    "    push rbp",        // Esto accede a [RSP-8]
    "    mov rbp, rsp",
    "    sub rsp, 16",     // Allocar espacio local
    "    mov rax, 0xDEADBEEF",
    "    mov [rbp-8], rax",
    "1:",
    "    jmp 1b",
);

/// ✅ TEST 5: NOP sled
/// 
/// Solo ejecuta NOPs. Útil para ver si el fetch funciona.
global_asm!(
    ".global user_nop_test",
    ".section .text.user",
    "user_nop_test:",
    "    nop",
    "    nop",
    "    nop",
    "    nop",
    "    nop",
    "1:",
    "    jmp 1b",
);

// Declarar los símbolos como externos para que Rust los vea
extern "C" {
    pub fn user_infinite_loop() -> !;
    pub fn user_hlt_test() -> !;
    pub fn user_syscall_test() -> !;
    pub fn user_stack_test() -> !;
    pub fn user_nop_test() -> !;
}

/// Helper para obtener el puntero a cualquier test
pub fn get_test_ptr(test_name: &str) -> *const u8 {
    match test_name {
        "loop" => user_infinite_loop as *const u8,
        "hlt" => user_hlt_test as *const u8,
        "syscall" => user_syscall_test as *const u8,
        "stack" => user_stack_test as *const u8,
        "nop" => user_nop_test as *const u8,
        _ => user_infinite_loop as *const u8,
    }
}

/// Imprime la lista de tests disponibles
pub fn print_available_tests() {
    crate::serial_println!("📋 Tests de usuario disponibles:");
    crate::serial_println!("  'loop'    - Loop infinito (más simple)");
    crate::serial_println!("  'hlt'     - HLT (debería #GP)");
    crate::serial_println!("  'syscall' - Syscall inmediato");
    crate::serial_println!("  'stack'   - Write to stack");
    crate::serial_println!("  'nop'     - NOP sled");
}