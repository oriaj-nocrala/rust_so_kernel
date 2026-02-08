// kernel/src/process/trapframe.rs
// Basado en xv6's trapframe

/// TrapFrame: Estado del proceso de usuario guardado en el kernel stack
/// cuando ocurre una interrupción/syscall
/// 
/// Layout compatible con el stack frame que IRETQ espera
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TrapFrame {
    // ============ Guardados por el kernel (pusha/popa) ============
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    
    // ============ Guardados por el HARDWARE (IRETQ frame) ============
    pub rip: u64,      // User instruction pointer
    pub cs: u64,       // User code segment (with RPL=3)
    pub rflags: u64,   // CPU flags
    pub rsp: u64,      // User stack pointer
    pub ss: u64,       // User stack segment (with RPL=3)
}

impl TrapFrame {
    /// Crea un trapframe nuevo para un proceso que nunca ha corrido
    pub fn new_user(entry_point: u64, user_stack: u64, user_cs: u64, user_ss: u64) -> Self {
        Self {
            // Limpiar todos los registros de propósito general
            rax: 0,
            rbx: 0,
            rcx: 0,
            rdx: 0,
            rsi: 0,
            rdi: 0,
            rbp: 0,
            r8: 0,
            r9: 0,
            r10: 0,
            r11: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            
            // IRETQ frame
            rip: entry_point,
            cs: user_cs | 3,  // RPL = 3
            rflags: 0x202,    // Interrupts enabled
            rsp: user_stack,
            ss: user_ss | 3,  // RPL = 3
        }
    }

     /// ✅ NUEVO: Debug detallado del trapframe
    pub fn debug_print(&self) {
        crate::serial_println!("╔════════════════════════════════════════════════════════╗");
        crate::serial_println!("║           TRAPFRAME DEBUG (antes de IRETQ)            ║");
        crate::serial_println!("╠════════════════════════════════════════════════════════╣");
        
        // IRETQ Frame (lo más crítico)
        crate::serial_println!("║ IRETQ FRAME (Hardware):                                ║");
        crate::serial_println!("║   RIP    = {:#018x}  ← User entry point       ║", self.rip);
        crate::serial_println!("║   CS     = {:#018x}  ← Code segment (RPL={})   ║", self.cs, self.cs & 3);
        crate::serial_println!("║   RFLAGS = {:#018x}  ← CPU flags             ║", self.rflags);
        crate::serial_println!("║   RSP    = {:#018x}  ← User stack            ║", self.rsp);
        crate::serial_println!("║   SS     = {:#018x}  ← Stack segment (RPL={}) ║", self.ss, self.ss & 3);
        crate::serial_println!("╠════════════════════════════════════════════════════════╣");
        
        // Validaciones críticas
        let mut errors = 0;
        
        // Check 1: RPL debe ser 3
        if (self.cs & 3) != 3 {
            crate::serial_println!("║ ❌ ERROR: CS RPL = {} (debe ser 3)                      ║", self.cs & 3);
            errors += 1;
        }
        if (self.ss & 3) != 3 {
            crate::serial_println!("║ ❌ ERROR: SS RPL = {} (debe ser 3)                      ║", self.ss & 3);
            errors += 1;
        }
        
        // Check 2: RFLAGS debe tener IF (bit 9)
        if (self.rflags & 0x200) == 0 {
            crate::serial_println!("║ ⚠️  WARNING: Interrupts disabled in RFLAGS             ║");
        }
        
        // Check 3: RIP debe estar en user space (< 0x0000_8000_0000_0000)
        if self.rip >= 0x0000_8000_0000_0000 {
            crate::serial_println!("║ ❌ ERROR: RIP en kernel space!                          ║");
            errors += 1;
        }
        
        // Check 4: RSP debe estar en user space
        if self.rsp >= 0x0000_8000_0000_0000 {
            crate::serial_println!("║ ❌ ERROR: RSP en kernel space!                          ║");
            errors += 1;
        }
        
        // Check 5: RSP debe estar alineado a 8 bytes
        if (self.rsp % 8) != 0 {
            crate::serial_println!("║ ⚠️  WARNING: RSP no alineado a 8 bytes                 ║");
        }
        
        crate::serial_println!("╠════════════════════════════════════════════════════════╣");
        crate::serial_println!("║ Registros generales:                                   ║");
        crate::serial_println!("║   RAX={:#018x}  RBX={:#018x}  ║", self.rax, self.rbx);
        crate::serial_println!("║   RCX={:#018x}  RDX={:#018x}  ║", self.rcx, self.rdx);
        crate::serial_println!("║   RSI={:#018x}  RDI={:#018x}  ║", self.rsi, self.rdi);
        crate::serial_println!("║   RBP={:#018x}                      ║", self.rbp);
        crate::serial_println!("╠════════════════════════════════════════════════════════╣");
        
        if errors == 0 {
            crate::serial_println!("║ ✅ TrapFrame parece válido                              ║");
        } else {
            crate::serial_println!("║ ❌ {} ERRORES DETECTADOS - IRETQ FALLARÁ               ║", errors);
        }
        
        crate::serial_println!("╚════════════════════════════════════════════════════════╝");
    }
}