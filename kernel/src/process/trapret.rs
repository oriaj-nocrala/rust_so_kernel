// kernel/src/process/trapret.rs
// Basado en xv6's trapret

use super::trapframe::TrapFrame;

/// Retorna de una trap/syscall a user mode
/// 
/// # Safety
/// - `tf` debe apuntar a un TrapFrame válido en el kernel stack
/// - Los valores en el TrapFrame deben ser válidos para user mode
/// 
/// Esta función NUNCA retorna - hace IRETQ a user space
#[unsafe(naked)]
pub unsafe extern "C" fn trapret(tf: *const TrapFrame) -> ! {
    core::arch::naked_asm!(
        // El argumento tf está en RDI (System V ABI)
        // Mover el stack pointer al trapframe
        "mov rsp, rdi",
        
        // ============ Restaurar registros de propósito general ============
        "pop rax",
        "pop rbx",
        "pop rcx",
        "pop rdx",
        "pop rsi",
        "pop rdi",
        "pop rbp",
        "pop r8",
        "pop r9",
        "pop r10",
        "pop r11",
        "pop r12",
        "pop r13",
        "pop r14",
        "pop r15",
        
        // ============ IRETQ restaura automáticamente ============
        // En este punto el stack tiene:
        // [rsp + 0]  = RIP
        // [rsp + 8]  = CS
        // [rsp + 16] = RFLAGS
        // [rsp + 24] = RSP (user)
        // [rsp + 32] = SS
        
        // ✅ NO tocamos DS/ES/FS/GS - en x86-64 long mode son ignorados
        // ✅ IRETQ carga CS y SS automáticamente desde el stack
        
        "iretq",
    );
}

/// Versión con DEBUGGING - llama a debug_print antes de saltar
/// 
/// Usa esta versión para debuggear el TrapFrame
pub unsafe extern "C" fn trapret_debug(tf: *const TrapFrame) -> ! {
    // Debug print ANTES de hacer IRETQ
    crate::serial_println!("\n🔍 DEBUG: Antes de IRETQ");
    
    if !tf.is_null() {
        (*tf).debug_print();
        
        // Verificar que las páginas están mapeadas
        check_user_pages_mapped(&*tf);
    }
    
    crate::serial_println!("🚀 Ejecutando IRETQ...\n");
    
    // Ahora sí, hacer el salto
    trapret(tf)
}

/// Verifica que las páginas de código y stack estén mapeadas
unsafe fn check_user_pages_mapped(tf: &TrapFrame) {
    use crate::memory::paging::ActivePageTable;
    use x86_64::VirtAddr;
    
    let phys_offset = crate::memory::physical_memory_offset();
    let page_table = ActivePageTable::new(phys_offset);
    
    crate::serial_println!("🗺️  Verificando page mappings:");
    
    // Check RIP
    let rip_addr = VirtAddr::new(tf.rip);
    match page_table.translate(rip_addr) {
        Some(phys) => {
            crate::serial_println!("  ✅ RIP {:#x} → Phys {:#x}", tf.rip, phys.as_u64());
        }
        None => {
            crate::serial_println!("  ❌ RIP {:#x} → NOT MAPPED!", tf.rip);
        }
    }
    
    // Check RSP
    let rsp_addr = VirtAddr::new(tf.rsp);
    match page_table.translate(rsp_addr) {
        Some(phys) => {
            crate::serial_println!("  ✅ RSP {:#x} → Phys {:#x}", tf.rsp, phys.as_u64());
        }
        None => {
            crate::serial_println!("  ❌ RSP {:#x} → NOT MAPPED!", tf.rsp);
        }
    }
    
    // Check RSP-8 (primera push location)
    let rsp_minus_8 = VirtAddr::new(tf.rsp - 8);
    match page_table.translate(rsp_minus_8) {
        Some(phys) => {
            crate::serial_println!("  ✅ RSP-8 {:#x} → Phys {:#x}", tf.rsp - 8, phys.as_u64());
        }
        None => {
            crate::serial_println!("  ❌ RSP-8 {:#x} → NOT MAPPED!", tf.rsp - 8);
        }
    }
}

/// Versión alternativa: Construye el trapframe en el stack actual y salta
/// 
/// Útil para la primera ejecución de un proceso
#[unsafe(naked)]
pub unsafe extern "C" fn enter_userspace(
    entry_point: u64,
    user_stack: u64,
    user_cs: u64,
    user_ss: u64,
) -> ! {
    core::arch::naked_asm!(
        // Argumentos en: RDI (entry), RSI (stack), RDX (cs), RCX (ss)
        
        // Limpiar todos los registros de propósito general
        "xor rax, rax",
        "xor rbx, rbx",
        "xor r8, r8",
        "xor r9, r9",
        "xor r10, r10",
        "xor r11, r11",
        "xor r12, r12",
        "xor r13, r13",
        "xor r14, r14",
        "xor r15, r15",
        // RDI, RSI, RDX, RCX contienen los argumentos, los limpiaremos después
        
        // Configurar segmentos de datos (user)
        "or rcx, 3",         // SS con RPL=3
        "mov ds, cx",
        "mov es, cx",
        "mov fs, cx",
        "mov gs, cx",
        
        // Preparar IRETQ frame en el stack
        "push rcx",          // SS (user_ss | 3)
        "push rsi",          // RSP (user_stack)
        "push 0x200",        // RFLAGS (interrupts enabled)
        "or rdx, 3",         // CS con RPL=3
        "push rdx",          // CS (user_cs | 3)
        "push rdi",          // RIP (entry_point)
        
        // Limpiar los últimos registros
        "xor rdi, rdi",
        "xor rsi, rsi",
        "xor rdx, rdx",
        "xor rcx, rcx",
        "xor rbp, rbp",
        
        // ¡Salto a Ring 3!
        "iretq",
    );
}