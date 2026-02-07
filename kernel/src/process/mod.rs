// kernel/src/process/mod.rs

use alloc::boxed::Box;
use x86_64::{
    VirtAddr,
    structures::paging::{FrameAllocator, Mapper, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB},
};
use crate::memory::user_pages::map_user_pages;

pub mod context;
pub mod syscall;
pub mod tss;
pub mod userspace;
pub mod scheduler;

use context::Context;

/// Process ID
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pid(pub usize);

/// Estado del proceso
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    Ready,      // Listo para ejecutar
    Running,    // Ejecutándose actualmente
    Blocked,    // Esperando I/O
    Zombie,     // Terminado pero no recolectado
}

/// Privilege level del proceso
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivilegeLevel {
    Kernel,  // Ring 0
    User,    // Ring 3
}

/// Process Control Block (PCB)
pub struct Process {
    pub pid: Pid,
    pub state: ProcessState,
    pub context: Context,
    pub kernel_stack: VirtAddr,
    pub user_stack: Option<VirtAddr>,
    pub privilege: PrivilegeLevel, 
    pub page_table: PhysFrame,
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
            VirtAddr::new(ptr as u64 + 8192)
        };

        Self {
            pid,
            state: ProcessState::Ready,
            context: Context::new(entry_point, kernel_stack),
            kernel_stack,
            user_stack: None,
            privilege: PrivilegeLevel::Kernel,
            page_table,
            name: [0; 32],
        }
    }

    /// Crea un proceso de user space (Ring 3)
    pub fn new_user(pid: Pid, entry_point: VirtAddr, page_table: PhysFrame) -> Self {
        // Kernel stack
        let kernel_stack = unsafe {
            let layout = core::alloc::Layout::from_size_align(8192, 4096).unwrap();
            let ptr = alloc::alloc::alloc(layout);
            if ptr.is_null() {
                panic!("Failed to allocate kernel stack");
            }
            VirtAddr::new(ptr as u64 + 8192)
        };

        // ✅ User stack: Usar la dirección que pre-mapeamos en main.rs
        const USER_STACK_TOP: u64 = 0x0000_7000_0000_2000;
        let user_stack = VirtAddr::new(USER_STACK_TOP);

        Self {
            pid,
            state: ProcessState::Ready,
            context: Context::new_user(entry_point, kernel_stack, user_stack),
            kernel_stack,
            user_stack: Some(user_stack),
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

/// Yield CPU para permitir context switch
pub fn yield_cpu() {
    use context::switch_context;
    
    let switch_info = {
        let mut scheduler = scheduler::SCHEDULER.lock();
        scheduler.switch_to_next()
    };
    
    if let Some((old_ctx, new_ctx)) = switch_info {
        unsafe {
            switch_context(old_ctx, new_ctx);
        }
    }
}

/// Función de prueba que ejecuta en Ring 3
#[no_mangle]
pub extern "C" fn user_test_function() -> ! {
    // Obtener PID
    let pid = userspace::sys_getpid();
    
    // Mensaje de prueba
    let msg = b"Hello from userspace! PID=";
    userspace::sys_write(1, msg.as_ptr(), msg.len());
    
    // ✅ FIX: Usar array estático o escribir char por char
    if pid < 10 {
        let c = b'0' + pid as u8;
        userspace::sys_write(1, &c as *const u8, 1);
    } else {
        let tens = b'0' + (pid / 10) as u8;
        let ones = b'0' + (pid % 10) as u8;
        userspace::sys_write(1, &tens as *const u8, 1);
        userspace::sys_write(1, &ones as *const u8, 1);
    }
    
    let newline = b"\n";
    userspace::sys_write(1, newline.as_ptr(), newline.len());
    
    // Salir con status 0
    userspace::sys_exit(0);
}