// kernel/src/process/mod.rs
// ✅ IMPLEMENTACIÓN CON ADDRESS SPACES AISLADOS

use alloc::boxed::Box;
use alloc::sync::Arc;
use x86_64::VirtAddr;
use crate::memory::address_space::AddressSpace;

pub mod scheduler;
pub mod trapframe;
pub mod timer_preempt;
pub mod tss;
pub mod syscall;
pub mod file;
pub mod user_test_fileio;
pub mod user_programs;

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
    pub parent_pid: Option<Pid>,
    pub exit_status: i32,
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
    ///
    /// `Arc`-wrapped so real threads (created via `clone()`, see
    /// `syscall::sys_clone`) can share one address space across multiple
    /// `Process`es (one per thread). For a normal fork'd/exec'd process
    /// this `Arc` simply has a single owner, behaving exactly as before —
    /// `AddressSpace`'s `Drop` (which frees the page table and all mapped
    /// pages) only runs once the last thread sharing it exits.
    pub address_space: Arc<AddressSpace>,
    pub files: FileDescriptorTable,

    /// Set while this process is blocked in waitpid(), waiting for a child.
    /// Stored here (not in a global) so multiple processes can wait concurrently.
    pub waiting_for: Option<usize>,

    /// FS segment base (used for TLS via arch_prctl ARCH_SET_FS).
    /// Saved/restored on every context switch so mlibc's TLS works correctly.
    pub fs_base: u64,
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
            parent_pid: None,
            exit_status: 0,
            state: ProcessState::Ready,
            privilege: PrivilegeLevel::Kernel,
            priority: 5,
            effective_priority: 5,
            name: [0; 16],
            trapframe,
            kernel_stack,
            address_space: Arc::new(address_space),
            files: FileDescriptorTable::new_with_stdio(),
            waiting_for: None,
            fs_base: 0,
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
            parent_pid: None,
            exit_status: 0,
            state: ProcessState::Ready,
            privilege: PrivilegeLevel::User,
            priority: 5,
            effective_priority: 5,
            name: [0; 16],
            trapframe,
            kernel_stack,
            address_space: Arc::new(address_space),
            files: FileDescriptorTable::new_with_stdio(),
            waiting_for: None,
            fs_base: 0,
        }
    }

    /// Create a forked child process.
    ///
    /// The child gets the parent's TrapFrame (with rax=0 so fork() returns 0
    /// in the child), a copy of the address space, and cloned file descriptors.
    pub fn new_user_from_fork(
        pid: Pid,
        parent_pid: Pid,
        trapframe: Box<TrapFrame>,
        kernel_stack: VirtAddr,
        address_space: AddressSpace,
        files: FileDescriptorTable,
    ) -> Self {
        crate::serial_println!(
            "Creating FORKED process PID {} (parent PID {})",
            pid.0, parent_pid.0,
        );
        Process {
            pid,
            parent_pid: Some(parent_pid),
            exit_status: 0,
            state: ProcessState::Ready,
            privilege: PrivilegeLevel::User,
            priority: 5,
            effective_priority: 5,
            name: [0; 16],
            trapframe,
            kernel_stack,
            address_space: Arc::new(address_space),
            files,
            waiting_for: None,
            fs_base: 0,
        }
    }

    /// Create a new thread: a schedulable context that SHARES the caller's
    /// address space (via the `Arc` already held by the caller) instead of
    /// getting a fresh COW-forked one. Used by `syscall::sys_clone`.
    ///
    /// `entry`/`stack` become the new thread's initial RIP/RSP — for the
    /// mlibc port, `entry` is `__mlibc_start_thread` and `stack` is the
    /// pre-built stack `sys_prepare_stack` set up in userspace (already
    /// carrying the real entry/arg/tcb the assembly trampoline expects).
    pub fn new_thread(
        pid: Pid,
        parent_pid: Pid,
        entry: VirtAddr,
        stack: VirtAddr,
        kernel_stack: VirtAddr,
        address_space: Arc<AddressSpace>,
    ) -> Self {
        let mut trapframe = Box::new(TrapFrame::default());

        trapframe.rip = entry.as_u64();
        trapframe.cs = 0x23;
        trapframe.rflags = 0x200;
        trapframe.rsp = stack.as_u64();
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
            "Creating THREAD PID {} (parent PID {}): entry={:#x} stack={:#x}, sharing address space",
            pid.0, parent_pid.0, entry.as_u64(), stack.as_u64(),
        );

        Process {
            pid,
            parent_pid: Some(parent_pid),
            exit_status: 0,
            state: ProcessState::Ready,
            privilege: PrivilegeLevel::User,
            priority: 5,
            effective_priority: 5,
            name: [0; 16],
            trapframe,
            kernel_stack,
            address_space,
            // A real POSIX thread should share its FD table with siblings
            // too (files opened by one are visible to all). We don't do
            // that yet — each thread gets its own table pre-opened to the
            // same stdio devices, which is enough for stdout/stderr-only
            // programs but not for e.g. a thread opening a file that a
            // sibling then reads.
            files: FileDescriptorTable::new_with_stdio(),
            waiting_for: None,
            fs_base: 0,
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
        let mut scheduler = scheduler::local_scheduler();
        scheduler.start_first()
    };

    unsafe {
        core::arch::asm!("sti");
    }

    unsafe { trapframe::jump_to_trapframe(tf_ptr) }
}