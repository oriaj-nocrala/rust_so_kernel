// kernel/src/process/mod.rs
// ✅ IMPLEMENTACIÓN CON ADDRESS SPACES AISLADOS

use alloc::boxed::Box;
use x86_64::VirtAddr;
use crate::memory::address_space::AddressSpace;

pub mod scheduler;
pub mod trapframe;
pub mod timer_preempt;
pub mod tss;
pub mod syscall;
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

    /// Base priority (set once at creation, never changes).
    pub priority: u8,

    /// Effective priority (used for scheduling decisions).
    /// Starts equal to `priority`.  Decays when a time slice is consumed.
    /// Restored toward `priority` by periodic aging.
    pub effective_priority: u8,

    pub name: [u8; 16],
    pub trapframe: Box<TrapFrame>,
    pub kernel_stack: VirtAddr,
    /// The process's virtual address space (page table + VMAs).
    pub address_space: AddressSpace,
    pub files: FileDescriptorTable,
}

impl Process {
    /// Crear proceso de KERNEL
    pub fn new_kernel(
        pid: Pid,
        entry: VirtAddr,
        kernel_stack: VirtAddr,
        address_space: AddressSpace,
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
            effective_priority: 5,
            name: [0; 16],
            trapframe,
            kernel_stack,
            address_space,
            files: FileDescriptorTable::new_with_stdio(),
        }
    }
    
    /// Crear proceso de USER
    pub fn new_user(
        pid: Pid,
        entry: VirtAddr,
        user_stack: VirtAddr,
        kernel_stack: VirtAddr,
        address_space: AddressSpace,
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
            effective_priority: 5,
            name: [0; 16],
            trapframe,
            kernel_stack,
            address_space,
            files: FileDescriptorTable::new_with_stdio(),
        }
    }
    
    pub fn set_name(&mut self, name: &str) {
        let bytes = name.as_bytes();
        let len = core::cmp::min(bytes.len(), 15);
        self.name[..len].copy_from_slice(&bytes[..len]);
    }

    pub fn set_priority(&mut self, priority: u8) {
        let p = core::cmp::min(priority, 10);
        self.priority = p;
        self.effective_priority = p;
    }
}

/// Start the first user process.
pub fn start_first_process() -> ! {
    let tf_ptr = {
        let mut scheduler = scheduler::SCHEDULER.lock();
        scheduler.start_first()
    };

    unsafe {
        core::arch::asm!("sti");
    }

    unsafe { trapframe::jump_to_trapframe(tf_ptr) }
}