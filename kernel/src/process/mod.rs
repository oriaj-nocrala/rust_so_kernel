// kernel/src/process/mod.rs
// ✅ IMPLEMENTACIÓN CON ADDRESS SPACES AISLADOS

use alloc::boxed::Box;
use alloc::sync::Arc;
use spin::Mutex;
use x86_64::VirtAddr;
use crate::memory::address_space::AddressSpace;

pub mod scheduler;
pub mod trapframe;
pub mod timer_preempt;
pub mod tss;
pub mod syscall;
mod irq_guard;
pub mod file;
pub mod fpu;
pub mod pipe;
pub mod signal;
pub mod user_test_fileio;
pub mod user_programs;

pub use signal::SignalAction;

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
    /// Stopped by SIGSTOP/SIGTSTP (job control). Parked in `wait_queue` like
    /// Blocked/Zombie (excluded from the scheduler's run queues), but unlike
    /// Blocked it never wakes itself — only an explicit SIGCONT (`sys_kill`)
    /// moves it back to Ready. See `Scheduler::stop_and_switch_tf`/`wake_stopped`.
    Stopped,
}

/// What a process blocked in `waitpid()` is waiting for — mirrors the pid
/// argument's POSIX overload (specific pid / process group / any child).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitTarget {
    Pid(usize),
    Pgid(u32),
    AnyChild,
}

impl WaitTarget {
    pub fn matches(&self, pid: usize, pgid: u32) -> bool {
        match *self {
            WaitTarget::Pid(p) => p == pid,
            WaitTarget::Pgid(g) => g == pgid,
            WaitTarget::AnyChild => true,
        }
    }
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
    /// `Arc<Mutex<..>>` for the same reason as `address_space`: threads
    /// created via `clone()` (see `syscall::sys_clone`) share one fd table
    /// with the process that spawned them, matching POSIX thread semantics
    /// (a file one thread opens is visible to its siblings). `fork()` still
    /// gets its own independent table (a fresh `Arc` around a cloned copy).
    pub files: Arc<Mutex<FileDescriptorTable>>,

    /// Set while this process is blocked in waitpid(), waiting for a child.
    /// Stored here (not in a global) so multiple processes can wait concurrently.
    pub waiting_for: Option<WaitTarget>,
    /// The `options` (WNOHANG/WUNTRACED) passed to the `waitpid()` call that
    /// set `waiting_for`. Only meaningful while `waiting_for` is `Some`.
    pub waiting_options: i32,

    /// User pointer `waitpid()`'s caller wants the reaped child's wait
    /// status written to (0 = none requested, i.e. a NULL status pointer).
    /// Only meaningful while `waiting_for` is `Some`. Not usable directly
    /// from `Scheduler::notify_child_death` (which can run in the *dying
    /// child's* address space, not this process's) — that instead stashes
    /// the value in `pending_wait_status`, consumed by
    /// `Scheduler::resolve_wait_status` the next time this process actually
    /// resumes in user mode, once its own page table is active again.
    pub waiting_status_ptr: usize,
    /// See `waiting_status_ptr`: the actual status word, waiting to be
    /// written into user memory once it's safe to.
    pub pending_wait_status: Option<i32>,

    /// Set just before this process is killed by an uncaught signal or a
    /// hardware fault (segfault, GPF, divide-by-zero, ...) — `None` for a
    /// normal `exit()`. Hardware faults are all reported as `SIGSEGV` for
    /// wait-status purposes (this kernel doesn't distinguish fault kinds
    /// at the signal level). Read by `wait_status_word()`.
    pub killed_by_signal: Option<u32>,

    /// Process group id (job control). Defaults to this process's own pid
    /// (group leader) at creation; `fork()`/`clone()` inherit the parent's
    /// pgid unless `setpgid()` later changes it — matches real POSIX
    /// default behavior. There is no separate session-id concept tracked —
    /// `setsid()` is approximated as "become your own group leader".
    pub pgid: u32,

    /// Set when this process is currently `ProcessState::Stopped`, to the
    /// signal that stopped it (SIGSTOP or SIGTSTP) — read by
    /// `stop_status_word()` for a `WUNTRACED` `waitpid()` report.
    pub stopped_by_signal: Option<u32>,
    /// Whether the *current* stop (see `stopped_by_signal`) has already
    /// been reported to a `waitpid(WUNTRACED)` caller. Reset to `false`
    /// every time this process is freshly stopped, so a stop is reported
    /// exactly once — matching real POSIX "each stop/continue transition
    /// is reported once" semantics (this kernel doesn't track WCONTINUED).
    pub stop_reported: bool,

    /// FS segment base (used for TLS via arch_prctl ARCH_SET_FS).
    /// Saved/restored on every context switch so mlibc's TLS works correctly.
    pub fs_base: u64,

    /// FPU/SSE register state (x87, XMM0-15, MXCSR) — saved/restored on
    /// every context switch (see `process::fpu`) so a preemption mid
    /// floating-point computation doesn't corrupt it. Boxed: 512 bytes,
    /// 16-byte aligned, no reason to carry that inline in every `Process`
    /// when it's only ever touched at switch time.
    pub fpu_state: Box<fpu::FpuState>,

    /// True for a `Process` created by `new_thread` (i.e. `clone()`, POSIX
    /// thread), false for a normal process (fork/exec).
    ///
    /// mlibc's `pthread_join()` (`mlibc/options/internal/generic/threads.cpp`
    /// — upstream, shared by every sysdeps port, not something this port can
    /// override) is entirely futex-based: it waits on the TCB's `didExit`
    /// flag and never calls `waitpid()` on the tid. So unlike a fork()ed
    /// child, nothing will ever collect a thread's zombie from the
    /// scheduler's `wait_queue`. The scheduler uses this flag to reap a
    /// thread's `Process` immediately on exit instead of zombie-parking it
    /// forever — see `Scheduler::kill_current`.
    pub is_thread: bool,

    /// For a thread (`is_thread == true`) whose stack `sys_clone` found to
    /// be a private `mmap()`-backed VMA (as opposed to a caller-supplied
    /// one via `pthread_attr_setstack`): `(vma_start, size_pages)`, freed
    /// automatically when this thread dies. `None` for every other process
    /// and for threads given an explicit stack. See `Scheduler::kill_current`
    /// and `pending_vma_frees` for why the actual free is deferred rather
    /// than happening inline.
    ///
    /// Exists because upstream mlibc never frees a thread's stack itself —
    /// `pthread_exit()`/`thread_join()` both have explicit TODO/FIXME
    /// comments admitting the leak (see `mlibc/options/posix/generic/
    /// pthread.cpp` and `mlibc/options/internal/generic/threads.cpp`). The
    /// kernel doing it is the only fix that doesn't require patching mlibc
    /// itself, and reuses the exact same "runs on the exiting thread's own
    /// stack, can't free anything inline" logic as `kernel_stack`.
    pub owned_stack_vma: Option<(u64, usize)>,

    /// Current working directory, always a clean absolute path (see
    /// `fs::vfs::normalize_path`). Survives `exec()` (same `Process`, never
    /// reset) like real POSIX cwd; NOT shared between `clone()`-created
    /// threads (each gets its own `String` copy at creation time) — a
    /// simplification vs. real Linux `CLONE_FS`, same spirit as
    /// `signal_handlers` not being inherited across `fork()`.
    pub cwd: alloc::string::String,

    /// The `PROGRAMS` registry name (see `user_programs.rs`) that resolved
    /// the ELF currently running in this process — set on every successful
    /// `exec()`, inherited across `fork()`/`clone()` like `cwd`. Exists so
    /// `execve("/proc/self/exe", ...)` (BusyBox's `FEATURE_SH_STANDALONE`
    /// re-exec trick for any applet that isn't `NOFORK`/`NOEXEC`, e.g.
    /// `cat`) can resolve to "whatever ELF this process is currently
    /// running", the same thing a real `/proc/self/exe` symlink would
    /// point at — see `syscall::find_program_elf`.
    pub exe_name: alloc::string::String,

    /// Bitmask of pending (not yet delivered) signals — bit N = signal N.
    pub pending_signals: u64,
    /// Bitmask of currently blocked signals (`sigprocmask`).
    pub blocked_signals: u64,
    /// Per-signal disposition; index = signal number. Not inherited across
    /// `fork()` in this implementation (every new `Process` starts with all
    /// `Default` — a simplification vs. real POSIX, which does inherit).
    pub signal_handlers: [SignalAction; signal::NUM_SIGNALS],
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
            files: Arc::new(Mutex::new(FileDescriptorTable::new_with_stdio())),
            waiting_for: None,
            waiting_options: 0,
            waiting_status_ptr: 0,
            pending_wait_status: None,
            killed_by_signal: None,
            pgid: pid.0 as u32,
            stopped_by_signal: None,
            stop_reported: false,
            fs_base: 0,
            fpu_state: Box::new(fpu::default_state()),
            is_thread: false,
            owned_stack_vma: None,
            cwd: alloc::string::String::from("/"),
            exe_name: alloc::string::String::new(),
            signal_handlers: [SignalAction::Default; signal::NUM_SIGNALS],
            blocked_signals: 0,
            pending_signals: 0,
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
            files: Arc::new(Mutex::new(FileDescriptorTable::new_with_stdio())),
            waiting_for: None,
            waiting_options: 0,
            waiting_status_ptr: 0,
            pending_wait_status: None,
            killed_by_signal: None,
            pgid: pid.0 as u32,
            stopped_by_signal: None,
            stop_reported: false,
            fs_base: 0,
            fpu_state: Box::new(fpu::default_state()),
            is_thread: false,
            owned_stack_vma: None,
            cwd: alloc::string::String::from("/"),
            exe_name: alloc::string::String::new(),
            signal_handlers: [SignalAction::Default; signal::NUM_SIGNALS],
            blocked_signals: 0,
            pending_signals: 0,
        }
    }

    /// Create a forked child process.
    ///
    /// The child gets the parent's TrapFrame (with rax=0 so fork() returns 0
    /// in the child), a copy of the address space, and cloned file descriptors.
    /// `fpu_state` is a copy of the parent's *live* FPU/SSE registers at the
    /// moment of `fork()` (real `fork()` semantics — a child starts with the
    /// same register contents, not a reset default) — see `syscall::sys_fork`,
    /// which captures it with a fresh `fpu::save()` rather than reusing
    /// whatever was last stashed in the parent's own `Process::fpu_state`
    /// (stale as of its last preemption, not necessarily its current state).
    pub fn new_user_from_fork(
        pid: Pid,
        parent_pid: Pid,
        trapframe: Box<TrapFrame>,
        kernel_stack: VirtAddr,
        address_space: AddressSpace,
        files: FileDescriptorTable,
        cwd: alloc::string::String,
        parent_pgid: u32,
        exe_name: alloc::string::String,
        fpu_state: Box<fpu::FpuState>,
    ) -> Self {
        crate::serial_println!(
            "Creating FORKED process PID {} (parent PID {})",
            pid.0, parent_pid.0,
        );
        crate::debug::inc_forks();
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
            files: Arc::new(Mutex::new(files)),
            waiting_for: None,
            waiting_options: 0,
            waiting_status_ptr: 0,
            pending_wait_status: None,
            killed_by_signal: None,
            pgid: parent_pgid,
            stopped_by_signal: None,
            stop_reported: false,
            fs_base: 0,
            fpu_state,
            is_thread: false,
            owned_stack_vma: None,
            cwd,
            exe_name,
            signal_handlers: [SignalAction::Default; signal::NUM_SIGNALS],
            blocked_signals: 0,
            pending_signals: 0,
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
    ///
    /// `files` is the caller's own `Arc<Mutex<FileDescriptorTable>>`, passed
    /// in (not built fresh) so the new thread shares fd space with its
    /// siblings — POSIX threads see each other's open files.
    pub fn new_thread(
        pid: Pid,
        parent_pid: Pid,
        entry: VirtAddr,
        stack: VirtAddr,
        kernel_stack: VirtAddr,
        address_space: Arc<AddressSpace>,
        files: Arc<Mutex<FileDescriptorTable>>,
        owned_stack_vma: Option<(u64, usize)>,
        cwd: alloc::string::String,
        parent_pgid: u32,
        exe_name: alloc::string::String,
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
            files,
            waiting_for: None,
            waiting_options: 0,
            waiting_status_ptr: 0,
            pending_wait_status: None,
            killed_by_signal: None,
            pgid: parent_pgid,
            stopped_by_signal: None,
            stop_reported: false,
            fs_base: 0,
            fpu_state: Box::new(fpu::default_state()),
            is_thread: true,
            owned_stack_vma,
            cwd,
            exe_name,
            signal_handlers: [SignalAction::Default; signal::NUM_SIGNALS],
            blocked_signals: 0,
            pending_signals: 0,
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

    /// Encodes this (dead) process's exit condition into this kernel's
    /// wait(2)-ABI status word.
    ///
    /// `mlibc-port/constanos-sysdeps/include/abi-bits/wait.h` uses the
    /// dripos-style encoding (`WIFEXITED` = bit `0x200`, `WIFSIGNALED` =
    /// bit `0x400` with the signal number in bits 24-31) rather than
    /// Linux's `WTERMSIG(x) == 0` trick — see the mlibc-port ABI-bug
    /// history for why this port follows that header instead of assuming
    /// Linux's layout here.
    pub fn wait_status_word(&self) -> i32 {
        match self.killed_by_signal {
            Some(sig) => (((sig as i32) & 0xFF) << 24) | 0x400,
            None => 0x200 | (self.exit_status & 0xFF),
        }
    }

    /// Encodes this (currently `Stopped`) process's condition into a
    /// `WUNTRACED` wait status: `WIFSTOPPED` = bit `0x800`, stop signal in
    /// bits 16-23 (`WSTOPSIG`) — see `abi-bits/wait.h`. Returns a plain
    /// exited(0) word if called on a process that isn't actually stopped
    /// (shouldn't happen — callers only reach this via a `Stopped`-state
    /// match — but this avoids a bogus status word if that invariant is
    /// ever violated).
    pub fn stop_status_word(&self) -> i32 {
        match self.stopped_by_signal {
            Some(sig) => 0x800 | (((sig as i32) & 0xFF) << 16),
            None => 0x200,
        }
    }
}

/// Ensure every page in `[addr, addr+len)` is mapped in `proc`'s address
/// space, demand-paging any that aren't yet.
///
/// Needed before any *kernel-mode* code writes directly to a user address
/// (signal frame construction in `signal.rs`, cross-process pipe delivery in
/// `pipe.rs`) — a page fault on a kernel-mode instruction is never
/// demand-paged by this kernel's fault handler (`init/devices.rs` panics on
/// it instead; only user-mode faults get mapped on the fly), so a write to
/// a legitimately-valid-but-never-yet-touched user page (e.g. a deeper
/// stack slot than anything the process itself has used) would otherwise
/// crash the kernel instead of just transparently mapping it the way the
/// same write *would* have if the user process had issued it itself.
pub fn ensure_user_pages_mapped(proc: &Process, addr: u64, len: u64) {
    let first_page = addr & !0xFFF;
    let last_page = addr.saturating_add(len.saturating_sub(1)) & !0xFFF;
    let mut page_addr = first_page;
    while page_addr <= last_page {
        let page = x86_64::structures::paging::Page::<x86_64::structures::paging::Size4KiB>::containing_address(
            VirtAddr::new(page_addr),
        );
        let mapped = unsafe { proc.address_space.translate_page(page).is_some() };
        if !mapped {
            if let Some(vma) = proc.address_space.find_vma(page_addr) {
                let _ = crate::memory::demand_paging::map_demand_page(page_addr, &vma, proc.pid.0, true);
            }
        }
        page_addr += 0x1000;
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