// kernel/src/process/mod.rs
// Arquitectura basada en xv6

use alloc::boxed::Box;
use x86_64::{VirtAddr, structures::paging::PhysFrame};

pub mod context;
pub mod syscall;
pub mod tss;
pub mod trapframe;
pub mod trapret;
pub mod scheduler;
pub mod timer_preempt;
pub mod userspace;
pub mod user_test_minimal;

use context::Context;
use trapframe::TrapFrame;

/// Process ID
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pid(pub usize);

/// Estado del proceso
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    Embryo,     // En creación
    Ready,      // Listo para ejecutar
    Running,    // Ejecutándose actualmente
    Sleeping,   // Esperando I/O
    Zombie,     // Terminado pero no recolectado
}

/// Privilege level del proceso
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivilegeLevel {
    Kernel,  // Ring 0
    User,    // Ring 3
}

/// Process Control Block (PCB)
/// 
/// Arquitectura similar a xv6:
/// - `context`: Estado del kernel (para context switch)
/// - `trapframe`: Estado de usuario (guardado en trap/syscall)
/// - `kernel_stack`: Stack que usa cuando está en kernel mode
pub struct Process {
    pub pid: Pid,
    pub state: ProcessState,
    
    // ============ Context Switching (Kernel Mode) ============
    pub context: Context,
    pub kernel_stack: VirtAddr,
    
    // ============ User Mode State ============
    pub trapframe: Option<Box<TrapFrame>>,  // Solo para procesos user
    pub privilege: PrivilegeLevel,
    
    // ============ Memory Management ============
    pub page_table: PhysFrame,
    
    // ============ Metadata ============
    pub name: [u8; 32],
}

impl Process {
    /// Crea un nuevo proceso de kernel (Ring 0)
    pub fn new(pid: Pid, entry_point: VirtAddr, page_table: PhysFrame) -> Self {
        let kernel_stack = unsafe {
            let layout = core::alloc::Layout::from_size_align(8192, 4096).unwrap();
            let ptr = alloc::alloc::alloc(layout);
            if ptr.is_null() {
                panic!("Failed to allocate kernel stack");
            }
            VirtAddr::new(ptr as u64 + 8192)  // Top of stack
        };

        Self {
            pid,
            state: ProcessState::Embryo,
            context: Context::new(entry_point, kernel_stack),
            kernel_stack,
            trapframe: None,  // Kernel processes don't need trapframe
            privilege: PrivilegeLevel::Kernel,
            page_table,
            name: [0; 32],
        }
    }

    /// Crea un proceso de user space (Ring 3)
    /// 
    /// # Arguments
    /// * `test_name` - Nombre del test: "loop", "hlt", "syscall", "stack", "nop"
    pub fn new_user(
        pid: Pid,
        entry_point: VirtAddr,
        page_table: PhysFrame,
        test_name: Option<&str>
    ) -> Self {
        // Allocar kernel stack
        let kernel_stack = unsafe {
            let layout = core::alloc::Layout::from_size_align(8192, 4096).unwrap();
            let ptr = alloc::alloc::alloc(layout);
            if ptr.is_null() {
                panic!("Failed to allocate kernel stack");
            }
            VirtAddr::new(ptr as u64 + 8192)
        };

        // ✅ FIX: User stack - RSP debe apuntar DENTRO de la región mapeada
        const USER_STACK_TOP: u64 = 0x0000_7000_0000_2000;
        const USER_STACK_SIZE: u64 = 8192;
        
        // ⚠️ IMPORTANTE: Stack crece hacia abajo, RSP debe estar dentro
        // Si mapeamos [BASE, TOP), RSP debe estar en TOP - 8 (no en TOP)
        let user_rsp = USER_STACK_TOP - 8;
        
        // Obtener selectores
        let (user_cs, user_ss) = tss::get_user_selectors();
        
        // Crear trapframe
        let trapframe = Box::new(TrapFrame::new_user(
            entry_point.as_u64(),
            user_rsp,  // ✅ Ahora apunta a memoria válida
            user_cs.0 as u64,
            user_ss.0 as u64,
        ));

        // Context apunta a forkret
        let context = Context::new_for_user_process(kernel_stack);

        // Log detallado
        crate::serial_println!("╔════════════════════════════════════════════════════════╗");
        crate::serial_println!("║ CREATING USER PROCESS                                  ║");
        crate::serial_println!("╠════════════════════════════════════════════════════════╣");
        crate::serial_println!("║ PID:         {}                                         ║", pid.0);
        crate::serial_println!("║ Entry:       {:#018x}                        ║", entry_point.as_u64());
        crate::serial_println!("║ User RSP:    {:#018x}                        ║", user_rsp);
        crate::serial_println!("║ Kernel RSP:  {:#018x}                        ║", kernel_stack.as_u64());
        crate::serial_println!("║ CS selector: {:#018x} (RPL={})                    ║", user_cs.0, user_cs.0 & 3);
        crate::serial_println!("║ SS selector: {:#018x} (RPL={})                    ║", user_ss.0, user_ss.0 & 3);
        if let Some(test) = test_name {
            crate::serial_println!("║ Test:        {}                                         ║", test);
        }
        crate::serial_println!("╚════════════════════════════════════════════════════════╝");

        Self {
            pid,
            state: ProcessState::Embryo,
            context,
            kernel_stack,
            trapframe: Some(trapframe),
            privilege: PrivilegeLevel::User,
            page_table,
            name: [0; 32],
        }
    }

    pub fn set_name(&mut self, name: &str) {
        let bytes = name.as_bytes();
        let len = bytes.len().min(31);
        self.name[..len].copy_from_slice(&bytes[..len]);
    }
}