// kernel/src/process/mod.rs
// ✅ IMPLEMENTACIÓN CON PAGE TABLES AISLADAS

use alloc::boxed::Box;
use x86_64::VirtAddr;
use crate::memory::page_table_manager::OwnedPageTable;

pub mod scheduler;
pub mod trapframe;
pub mod timer_preempt;
pub mod tss;
pub mod syscall;
pub mod user_test_minimal;
pub mod file;
pub mod user_test_fileio;

pub use trapframe::TrapFrame;
pub use file::{FileDescriptorTable, FileHandle};

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
    pub priority: u8,
    pub name: [u8; 16],
    pub trapframe: Box<TrapFrame>,
    pub kernel_stack: VirtAddr,
    pub page_table: OwnedPageTable,
    pub files: FileDescriptorTable,
}

impl Process {
    /// Crear proceso de KERNEL
    ///
    /// Kernel processes share the kernel page table (OwnedPageTable::from_current).
    pub fn new_kernel(
        pid: Pid,
        entry: VirtAddr,
        kernel_stack: VirtAddr,
        page_table: OwnedPageTable,
    ) -> Self {
        let mut trapframe = Box::new(TrapFrame::default());
        
        trapframe.rip = entry.as_u64();
        trapframe.cs = 0x08;
        trapframe.rflags = 0x200;
        trapframe.rsp = kernel_stack.as_u64() - 8;
        trapframe.ss = 0x10;
        
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
            priority: 5,
            name: [0; 16],
            trapframe,
            kernel_stack,
            page_table,
            files: FileDescriptorTable::new_with_stdio(),
        }
    }
    
    /// Crear proceso de USER
    ///
    /// Each user process has its OWN page table (OwnedPageTable::new_user).
    pub fn new_user(
        pid: Pid,
        entry: VirtAddr,
        user_stack: VirtAddr,
        kernel_stack: VirtAddr,
        page_table: OwnedPageTable,
    ) -> Self {
        let mut trapframe = Box::new(TrapFrame::default());
        
        trapframe.rip = entry.as_u64();
        trapframe.cs = 0x23;
        trapframe.rflags = 0x200;
        trapframe.rsp = user_stack.as_u64();
        trapframe.ss = 0x1b;
        
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
            priority: 5,
            name: [0; 16],
            trapframe,
            kernel_stack,
            page_table,
            files: FileDescriptorTable::new_with_stdio(),
        }
    }
    
    pub fn set_name(&mut self, name: &str) {
        let bytes = name.as_bytes();
        let len = core::cmp::min(bytes.len(), 15);
        self.name[..len].copy_from_slice(&bytes[..len]);
    }

    pub fn set_priority(&mut self, priority: u8) {
        self.priority = core::cmp::min(priority, 10);
    }
}

/// Iniciar primer proceso
pub fn start_first_process() -> ! {
    let tf_ptr = {
        let mut scheduler = scheduler::SCHEDULER.lock();
        
        crate::serial_println!("Available processes:");

        // Phase 1: Find first non-idle Ready process (read-only scan)
        let target_pid = scheduler.processes.iter()
            .inspect(|proc| {
                crate::serial_println!("  PID {}: {:?} - {:?}", 
                    proc.pid.0, 
                    core::str::from_utf8(&proc.name).unwrap_or("<?>").trim_end_matches('\0'),
                    proc.privilege
                );
            })
            .find(|proc| proc.state == ProcessState::Ready && proc.pid.0 != 0)
            .map(|proc| proc.pid)
            .expect("No process to start!");
        
        // Phase 2: Modify the found process
        let tf_ptr = scheduler.processes.iter_mut()
            .find(|proc| proc.pid == target_pid)
            .map(|proc| {
                proc.state = ProcessState::Running;
                
                let pid = proc.pid;
                let kernel_stack = proc.kernel_stack;
                let tf_ptr = &*proc.trapframe as *const TrapFrame;
                let name = proc.name;
                
                tss::set_kernel_stack(kernel_stack);
                
                // ✅ Activate the process's page table
                unsafe {
                    proc.page_table.activate();
                }
                
                crate::serial_println!(
                    "\n🚀 Starting first process: PID {} ({})",
                    pid.0,
                    core::str::from_utf8(&name).unwrap_or("<invalid>").trim_end_matches('\0')
                );
                
                tf_ptr
            })
            .expect("Process disappeared!");
        
        scheduler.current = Some(target_pid);
        
        tf_ptr
    };

    // Enable interrupts right before jumping
    unsafe { 
        core::arch::asm!("sti");
    }
    
    unsafe { trapframe::jump_to_trapframe(tf_ptr) }
}