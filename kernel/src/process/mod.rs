// kernel/src/process/mod.rs
// ✅ IMPLEMENTACIÓN CORRECTA: Solo TrapFrame, sin Context

use alloc::boxed::Box;
use x86_64::{VirtAddr, structures::paging::PhysFrame};

pub mod scheduler;
pub mod trapframe;
pub mod timer_preempt;
pub mod tss;
pub mod syscall;
pub mod user_test_minimal;

pub use trapframe::TrapFrame;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pid(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    Ready,
    Running,
    Blocked,
    Zombie,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivilegeLevel {
    Kernel,
    User,
}

pub struct Process {
    pub pid: Pid,
    pub state: ProcessState,
    pub privilege: PrivilegeLevel,
    pub priority: u8,  // ← AÑADIR ESTO (0 = lowest, 10 = highest)
    pub name: [u8; 16],
    pub trapframe: Box<TrapFrame>,
    pub kernel_stack: VirtAddr,
    pub page_table: PhysFrame,
}

impl Process {
    /// Crear proceso de KERNEL
    /// 
    /// El proceso ejecuta en kernel mode (Ring 0).
    /// El timer lo puede preemptar como cualquier otro proceso.
    pub fn new_kernel(
        pid: Pid,
        entry: VirtAddr,
        kernel_stack: VirtAddr,
        page_table: PhysFrame,
    ) -> Self {
        let mut trapframe = Box::new(TrapFrame::default());
        
        // ✅ Configurar TrapFrame para kernel mode
        trapframe.rip = entry.as_u64();
        trapframe.cs = 0x08;       // Kernel code segment (RPL=0)
        trapframe.rflags = 0x200;  // IF=1 (interrupts enabled)
        trapframe.rsp = kernel_stack.as_u64() - 8;
        trapframe.ss = 0x10;       // Kernel data segment (RPL=0)
        
        // Registros en cero
        trapframe.rax = 0;
        trapframe.rbx = 0;
        trapframe.rcx = 0;
        trapframe.rdx = 0;
        trapframe.rsi = 0;
        trapframe.rdi = 0;
        trapframe.rbp = 0;
        trapframe.r8 = 0;
        trapframe.r9 = 0;
        trapframe.r10 = 0;
        trapframe.r11 = 0;
        trapframe.r12 = 0;
        trapframe.r13 = 0;
        trapframe.r14 = 0;
        trapframe.r15 = 0;
        
        crate::serial_println!(
            "Creating KERNEL process PID {}: entry={:#x} stack={:#x}",
            pid.0, entry.as_u64(), kernel_stack.as_u64()
        );
        
        Process {
            pid,
            state: ProcessState::Ready,
            privilege: PrivilegeLevel::Kernel,
            priority: 5,  // ← Prioridad normal por defecto
            name: [0; 16],
            trapframe,
            kernel_stack,
            page_table,
        }
    }
    
    /// Crear proceso de USER
    /// 
    /// El proceso ejecuta en user mode (Ring 3).
    pub fn new_user(
        pid: Pid,
        entry: VirtAddr,
        user_stack: VirtAddr,
        kernel_stack: VirtAddr,
        page_table: PhysFrame,
    ) -> Self {
        let mut trapframe = Box::new(TrapFrame::default());
        
        // ✅ Configurar TrapFrame para user mode
        trapframe.rip = entry.as_u64();
        trapframe.cs = 0x23;       // User code segment (RPL=3)
        trapframe.rflags = 0x200;  // IF=1
        trapframe.rsp = user_stack.as_u64();
        trapframe.ss = 0x1b;       // User data segment (RPL=3)
        
        trapframe.rax = 0;
        trapframe.rbx = 0;
        trapframe.rcx = 0;
        trapframe.rdx = 0;
        trapframe.rsi = 0;
        trapframe.rdi = 0;
        trapframe.rbp = 0;
        trapframe.r8 = 0;
        trapframe.r9 = 0;
        trapframe.r10 = 0;
        trapframe.r11 = 0;
        trapframe.r12 = 0;
        trapframe.r13 = 0;
        trapframe.r14 = 0;
        trapframe.r15 = 0;
        
        crate::serial_println!(
            "Creating USER process PID {}: entry={:#x} user_stack={:#x} kernel_stack={:#x}",
            pid.0, entry.as_u64(), user_stack.as_u64(), kernel_stack.as_u64()
        );
        
        Process {
            pid,
            state: ProcessState::Ready,
            privilege: PrivilegeLevel::User,
            priority: 5,  // ← Prioridad normal por defecto
            name: [0; 16],
            trapframe,
            kernel_stack,
            page_table,
        }
    }
    
    pub fn set_name(&mut self, name: &str) {
        let bytes = name.as_bytes();
        let len = core::cmp::min(bytes.len(), 15);
        self.name[..len].copy_from_slice(&bytes[..len]);
    }

    pub fn set_priority(&mut self, priority: u8) {
        self.priority = core::cmp::min(priority, 10);  // Max 10
    }
}

/// ✅ Iniciar primer proceso
/// 
/// Esta función se llama UNA VEZ al inicio para arrancar el primer proceso.
/// Después de esto, SOLO el timer hace scheduling.
pub fn start_first_process() -> ! {
    let tf_ptr = {
        let mut scheduler = scheduler::SCHEDULER.lock();
        
        // Buscar primer proceso Ready
        let mut found = None;
        
        for proc in scheduler.processes.iter_mut() {
            if proc.state == ProcessState::Ready {
                proc.state = ProcessState::Running;
                
                // ✅ Guardar valores ANTES de la segunda mutación
                let pid = proc.pid;
                let kernel_stack = proc.kernel_stack;
                let tf_ptr = &*proc.trapframe as *const TrapFrame;
                let name = proc.name;
                
                // ✅ AHORA sí podemos mutar scheduler.current
                scheduler.current = Some(pid);
                
                // Actualizar TSS
                tss::set_kernel_stack(kernel_stack);
                
                crate::serial_println!(
                    "\n🚀 Starting first process: PID {} ({})",
                    pid.0,
                    core::str::from_utf8(&name).unwrap_or("<invalid>").trim_end_matches('\0')
                );
                
                found = Some(tf_ptr);
                break;
            }
        }
        
        found.expect("No process to start!")
    }; // ← Lock se libera aquí
    
    // Saltar al primer proceso (lock ya liberado)
    unsafe { trapframe::jump_to_trapframe(tf_ptr) }
}