// exception.rs o idt.rs

/// Representa el stack frame que el CPU pushea automáticamente cuando ocurre una interrupción
#[repr(C)]
pub struct ExceptionStackFrame {
    /// Instruction pointer (dirección de la siguiente instrucción a ejecutar)
    pub instruction_pointer: u64,
    
    /// Registro de segmento de código
    pub code_segment: u64,
    
    /// CPU flags register (RFLAGS)
    pub cpu_flags: u64,
    
    /// Stack pointer antes de la interrupción
    pub stack_pointer: u64,
    
    /// Registro de segmento de stack
    pub stack_segment: u64,
}

impl ExceptionStackFrame {
    /// Crea un nuevo stack frame (útil para testing)
    pub const fn new() -> Self {
        Self {
            instruction_pointer: 0,
            code_segment: 0,
            cpu_flags: 0,
            stack_pointer: 0,
            stack_segment: 0,
        }
    }
}

impl core::fmt::Debug for ExceptionStackFrame {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        f.debug_struct("ExceptionStackFrame")
            .field("instruction_pointer", &format_args!("{:#x}", self.instruction_pointer))
            .field("code_segment", &format_args!("{:#x}", self.code_segment))
            .field("cpu_flags", &format_args!("{:#x}", self.cpu_flags))
            .field("stack_pointer", &format_args!("{:#x}", self.stack_pointer))
            .field("stack_segment", &format_args!("{:#x}", self.stack_segment))
            .finish()
    }
}