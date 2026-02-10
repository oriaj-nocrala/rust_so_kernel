// kernel/src/process/user_test_fileio.rs
// ✅ Tests de usuario que usan file descriptors

use core::arch::global_asm;

// ============================================================================
// SYSCALL WRAPPERS
// ============================================================================

global_asm!(
    ".section .text.user",
    
    // sys_write(fd, buf, count) -> ssize_t
    ".global user_sys_write",
    "user_sys_write:",
    "    mov rax, 1",          // SYS_WRITE
    "    int 0x80",
    "    ret",
    
    // sys_open(path, flags) -> fd
    ".global user_sys_open",
    "user_sys_open:",
    "    mov rax, 2",          // SYS_OPEN
    "    int 0x80",
    "    ret",
    
    // sys_close(fd) -> int
    ".global user_sys_close",
    "user_sys_close:",
    "    mov rax, 3",          // SYS_CLOSE
    "    int 0x80",
    "    ret",
    
    // sys_exit(status) -> !
    ".global user_sys_exit",
    "user_sys_exit:",
    "    mov rax, 60",         // SYS_EXIT
    "    int 0x80",
    "    jmp user_sys_exit",   // Nunca debería llegar aquí
);

// ============================================================================
// TEST 1: Write to stdout
// ============================================================================

global_asm!(
    ".section .text.user",
    ".global user_test_write",
    "user_test_write:",
    
    // Preparar mensaje en el stack
    "    sub rsp, 32",
    
    // Escribir "Hello from user!\n" en el stack
    "    mov rax, 0x6c6c6548",           // "Hell"
    "    mov [rsp], rax",
    "    mov rax, 0x7266206f",           // "o fr"
    "    mov [rsp+4], rax",
    "    mov rax, 0x75206d6f",           // "om u"
    "    mov [rsp+8], rax",
    "    mov rax, 0x21726573",           // "ser!"
    "    mov [rsp+12], rax",
    "    mov rax, 0x0a",                 // "\n"
    "    mov [rsp+16], rax",
    
    // sys_write(1, mensaje, 17)
    "    mov rdi, 1",           // FD 1 (stdout)
    "    mov rsi, rsp",         // Puntero al mensaje
    "    mov rdx, 17",          // Longitud
    "    mov rax, 1",           // SYS_WRITE
    "    int 0x80",
    
    // Busy-wait
    "    mov rcx, 1000000",
    "1:  dec rcx",
    "    jnz 1b",
    
    // Limpiar stack y repetir
    "    add rsp, 32",
    "    jmp user_test_write",
);

// ============================================================================
// TEST 2: Open /dev/null y escribir en él
// ============================================================================

global_asm!(
    ".section .text.user",
    ".global user_test_devnull",
    "user_test_devnull:",
    
    // Path en el stack: "/dev/null\0"
    "    sub rsp, 16",
    "    mov rax, 0x65642f",           // "/de" (solo 32-bit)
    "    mov [rsp], rax",
    "    mov rax, 0x756e2f76",         // "v/nu"
    "    mov [rsp+4], rax",
    "    mov rax, 0x00006c6c",         // "ll\0"
    "    mov [rsp+8], rax",
    
    // sys_open("/dev/null", 0)
    "    mov rdi, rsp",         // Path
    "    mov rsi, 0",           // Flags
    "    mov rax, 2",           // SYS_OPEN
    "    int 0x80",
    
    // Guardar FD
    "    mov r15, rax",         // FD en r15
    
    // Mensaje de prueba
    "    mov rax, 0x7473657454",       // "Test"
    "    mov [rsp], rax",
    
    // sys_write(fd, mensaje, 4)
    "    mov rdi, r15",         // FD
    "    mov rsi, rsp",         // Buffer
    "    mov rdx, 4",           // Count
    "    mov rax, 1",           // SYS_WRITE
    "    int 0x80",
    
    // sys_close(fd)
    "    mov rdi, r15",
    "    mov rax, 3",           // SYS_CLOSE
    "    int 0x80",
    
    // Busy-wait
    "    mov rcx, 5000000",
    "1:  dec rcx",
    "    jnz 1b",
    
    // Repetir
    "    add rsp, 16",
    "    jmp user_test_devnull",
);

// ============================================================================
// TEST 3: Open /dev/fb y escribir en framebuffer
// ============================================================================

global_asm!(
    ".section .text.user",
    ".global user_test_fb",
    "user_test_fb:",
    
    // Path: "/dev/fb\0"
    "    sub rsp, 32",
    "    mov rax, 0x65642f",           // "/de"
    "    mov [rsp], rax",
    "    mov rax, 0x0062662f76",       // "v/fb\0"
    "    mov [rsp+4], rax",
    
    // sys_open("/dev/fb", 0)
    "    mov rdi, rsp",
    "    mov rsi, 0",
    "    mov rax, 2",           // SYS_OPEN
    "    int 0x80",
    
    // Guardar FD
    "    mov r15, rax",
    
    // Mensaje: "User says hi!\n"
    "    mov rax, 0x72657355",         // "User"
    "    mov [rsp+8], rax",
    "    mov rax, 0x79617320",         // " say"
    "    mov [rsp+12], rax",
    "    mov rax, 0x69682073",         // "s hi"
    "    mov [rsp+16], rax",
    "    mov rax, 0x00000a21",         // "!\n\0"
    "    mov [rsp+20], rax",
    
    // sys_write(fd, mensaje, 14)
    "    mov rdi, r15",
    "    lea rsi, [rsp+8]",
    "    mov rdx, 14",
    "    mov rax, 1",
    "    int 0x80",
    
    // sys_close(fd)
    "    mov rdi, r15",
    "    mov rax, 3",
    "    int 0x80",
    
    // Busy-wait largo
    "    mov rcx, 10000000",
    "1:  dec rcx",
    "    jnz 1b",
    
    "    add rsp, 32",
    "    jmp user_test_fb",
);

// ============================================================================
// EXPORTS
// ============================================================================

extern "C" {
    pub fn user_test_write() -> !;
    pub fn user_test_devnull() -> !;
    pub fn user_test_fb() -> !;
}

pub fn get_test_ptr(test_name: &str) -> *const u8 {
    match test_name {
        "write" => user_test_write as *const u8,
        "devnull" => user_test_devnull as *const u8,
        "fb" => user_test_fb as *const u8,
        _ => user_test_write as *const u8,
    }
}

pub fn print_available_tests() {
    crate::serial_println!("📋 File I/O tests disponibles:");
    crate::serial_println!("  'write'    - Write to stdout");
    crate::serial_println!("  'devnull'  - Open /dev/null y escribir");
    crate::serial_println!("  'fb'       - Open /dev/fb y escribir en pantalla");
}