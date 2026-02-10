// kernel/src/process/user_test_minimal.rs
// ✅ VERSIÓN CORREGIDA: Con busy-wait entre syscalls

use core::arch::global_asm;

/// TEST 1: Loop infinito
global_asm!(
    ".global user_infinite_loop",
    ".section .text.user",
    "user_infinite_loop:",
    "1:",
    "    jmp 1b",
);

/// TEST 2: HLT instruction
global_asm!(
    ".global user_hlt_test",
    ".section .text.user",
    "user_hlt_test:",
    "    hlt",
    "    jmp user_hlt_test",
);

/// TEST 3: Syscall con BUSY WAIT
/// 
/// ✅ FIX CRÍTICO: Añade busy-wait entre syscalls
/// Esto da tiempo al timer para hacer preemption
global_asm!(
    ".global user_syscall_test",
    ".section .text.user",
    "user_syscall_test:",
    "1:",
    "    mov rax, 39",      // sys_getpid
    "    int 0x80",         // Syscall
    
    // ✅ AÑADIR: Busy-wait para dar tiempo al timer
    "    mov rcx, 1000000", // ~1M iteraciones
    "2:",
    "    dec rcx",
    "    jnz 2b",
    
    "    jmp 1b",           // Repetir
);

/// TEST 4: Write to stack
global_asm!(
    ".global user_stack_test",
    ".section .text.user",
    "user_stack_test:",
    "    push rbp",
    "    mov rbp, rsp",
    "    sub rsp, 16",
    "    mov rax, 0xDEADBEEF",
    "    mov [rbp-8], rax",
    "1:",
    "    jmp 1b",
);

/// TEST 5: NOP sled
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

/// TEST 6: sys_yield test
global_asm!(
    ".global user_yield_test",
    ".section .text.user",
    "user_yield_test:",
    "1:",
    "    mov rax, 24",      // sys_yield
    "    int 0x80",
    "    jmp 1b",
);

extern "C" {
    pub fn user_infinite_loop() -> !;
    pub fn user_hlt_test() -> !;
    pub fn user_syscall_test() -> !;
    pub fn user_stack_test() -> !;
    pub fn user_nop_test() -> !;
    pub fn user_yield_test() -> !;
}

pub fn get_test_ptr(test_name: &str) -> *const u8 {
    match test_name {
        "loop" => user_infinite_loop as *const u8,
        "hlt" => user_hlt_test as *const u8,
        "syscall" => user_syscall_test as *const u8,
        "stack" => user_stack_test as *const u8,
        "nop" => user_nop_test as *const u8,
        "yield" => user_yield_test as *const u8,
        _ => user_infinite_loop as *const u8,
    }
}

pub fn print_available_tests() {
    crate::serial_println!("📋 Tests de usuario disponibles:");
    crate::serial_println!("  'loop'    - Loop infinito (más simple)");
    crate::serial_println!("  'hlt'     - HLT (debería #GP)");
    crate::serial_println!("  'syscall' - sys_getpid con busy-wait");
    crate::serial_println!("  'stack'   - Write to stack");
    crate::serial_println!("  'nop'     - NOP sled");
    crate::serial_println!("  'yield'   - sys_yield en loop");
}