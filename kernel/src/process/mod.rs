// kernel/src/process/mod.rs
// ✅ IMPLEMENTACIÓN CON ADDRESS SPACES AISLADOS

use alloc::boxed::Box;
use alloc::sync::Arc;
use crate::sync::Mutex;
use x86_64::VirtAddr;
use crate::memory::address_space::AddressSpace;

pub mod scheduler;
pub mod trapframe;
pub mod timer_preempt;
pub mod tss;
pub mod syscall;
pub(crate) mod irq_guard;
pub mod file;
pub mod dead_files;
pub mod fpu;
pub mod pipe;
pub mod signal;
pub mod wait;
pub mod user_test_fileio;
pub mod user_programs;

pub use signal::SignalAction;

pub use trapframe::TrapFrame;
pub use file::{FileDescriptorTable, FileHandle};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pid(pub usize);

/// What `block_current` does with a wakeup that beat the block — see
/// `Process::wake_pending`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakePending {
    /// Resume at the saved frame as it is: a socket wait, whose frame is
    /// already rewound onto the `syscall` instruction.
    Restart,
    /// The waker completed the operation: return this from the syscall.
    Return(u64),
}

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
    /// default behavior.
    pub pgid: u32,

    /// Session id (phase 3.2 of `docs/gui/gui-plan.md`). The pid of the
    /// session's leader; inherited by `fork()`/`clone()`, changed only by
    /// `setsid()`. PID 1 leads session 1, and every process descends from
    /// it. Until 2026-09-25 there was none: `setsid()` only made the caller
    /// a group leader, and `setpgid()` could move a process into any group.
    pub sid: u32,

    /// Controlling terminal: the pty number (`/dev/pts/<n>`), or `None`
    /// (the console is nobody's — its job control stays global, see
    /// `crate::tty`). Inherited by `fork()`/`clone()`, cleared by
    /// `setsid()`, acquired with `TIOCSCTTY` or by a session leader's
    /// first open of a slave (`crate::pty`).
    pub ctty: Option<usize>,

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
    /// simplification vs. real Linux `CLONE_FS`.
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
    /// The mask `rt_sigsuspend` replaced, to be put back once it returns —
    /// Linux's `saved_sigmask` + `TIF_RESTORE_SIGMASK`. A handler frame
    /// pushed while this is set saves *this* mask rather than the temporary
    /// one, so the handler's `sigreturn` restores what the caller had; if no
    /// handler runs, `signal::deliver_pending` restores it directly.
    pub saved_sigmask: Option<u64>,
    /// Blocked in `rt_sigsuspend`: the one wait a signal ends. Signal
    /// senders call `Scheduler::interrupt_blocked`, which wakes a process
    /// with this set once a signal it would act on is pending and unblocked.
    pub in_sigsuspend: bool,
    /// Per-signal disposition; index = signal number. Inherited by `fork()`
    /// (with `sig_restart` and the mask) and reset to `Default` for caught
    /// signals by `exec()`, as POSIX says. A `clone()`d thread gets a copy
    /// at creation rather than a shared table (Linux's `CLONE_SIGHAND`).
    pub signal_handlers: [SignalAction; signal::NUM_SIGNALS],

    // ── TrapFrame SAVE/RESUME sequence tracking ───────────────────────────
    // See `process::scheduler::tf_note_save`/`tf_note_resume` and
    // `debug::TfRewindDiag`. These three fields exist only to answer one
    // question: does this process ever get resumed from a `TrapFrame` that
    // isn't the last one actually saved for it? (It did, for months — see
    // docs/hang-hunt-bug2-findings.md.) Everything here runs under the
    // single-core `SCHEDULER` lock (never touched from two contexts at
    // once), so plain fields suffice — no atomics needed.
    /// Bumped by 1 every time `trapframe` is overwritten wholesale from a
    /// live register snapshot (a "SAVE" — `switch_to_next`/`block_current`/
    /// `stop_and_switch_tf`/`sys_exec`'s direct rewrite). Never touched by
    /// process construction (`new_user`/`new_user_from_fork`/`new_thread`
    /// all start it at 0) — a fresh process has never been "saved" yet, it
    /// starts pre-loaded with its initial trapframe.
    pub tf_seq: u64,
    /// True from the moment a SAVE bumps `tf_seq` until the matching RESUME
    /// consumes it (starts `false`: the initial trapframe from construction
    /// counts as pre-consumed, needing no RESUME to "unlock" it). Finding
    /// this already `true` at the start of a *new* SAVE means the previous
    /// SAVE's content was never resumed — one of the two rewind signatures.
    pub tf_awaiting_resume: bool,
    /// `tf_seq` as of this process's last RESUME, or `None` before its
    /// first one (`start_first`, or a freshly `fork()`/`clone()`d process's
    /// first ever scheduling). A RESUME whose *current* `tf_seq` isn't
    /// strictly greater than this is resuming stale/already-consumed
    /// content — the other rewind signature.
    pub tf_last_resumed_seq: Option<u64>,

    /// A wakeup that arrived while this process was still running — on
    /// another CPU, between registering as a waiter and blocking (stage 7
    /// of `docs/smp/smp-plan.md`). `block_current` consumes it instead of
    /// blocking. Only set by `Scheduler::wake_or_defer`/`deliver_to_waiter`,
    /// whose callers (pipes, sockets) register waiters only on the way to
    /// blocking, so it can never outlive the syscall that registered.
    pub wake_pending: Option<WakePending>,

    /// The cell of the wait this process is registering for, between
    /// `Scheduler::begin_wait`/`arm_wait` and the `block_current` that
    /// takes it into `wait` (see `process::wait`).
    pub armed_wait: Option<Arc<wait::WaitCell>>,
    /// The wait this process is Blocked in, if it named one — what a
    /// signal interrupts (`Scheduler::interrupt_blocked`).
    pub wait: Option<wait::ActiveWait>,
    /// Set when a signal ended the wait; `signal::deliver_pending` turns
    /// it into `EINTR` or a re-executed call.
    pub interrupted: Option<wait::Interrupted>,
    /// `SA_RESTART`, one bit per signal (bit N = signal N), from
    /// `sigaction`. Reset with the handlers on `exec`.
    pub sig_restart: u64,

    /// CPU time in ticks (`sched::cputime`): user/system sampled by the
    /// timer, one tick at a time, to whatever each CPU interrupted; plus
    /// the children this process has waited for. Fields 14-17 of `/proc/<pid>/stat`,
    /// `times(2)`, `getrusage(2)`.
    pub times: sched::cputime::ProcTimes,
    /// The time of this process's threads that have exited — part of the
    /// thread group's time (`times`, `CLOCK_PROCESS_CPUTIME_ID`,
    /// `/proc/<pid>/stat`), never of this thread's own
    /// (`CLOCK_THREAD_CPUTIME_ID`, `RUSAGE_THREAD`). Only ever nonzero on a
    /// group's leader.
    pub dead_threads: sched::cputime::ProcTimes,
    pub dead_threads_ns: u64,
    /// Time actually run, in nanoseconds, measured at every switch rather
    /// than sampled (Linux's `sum_exec_runtime`): what
    /// `CLOCK_PROCESS_CPUTIME_ID`/`CLOCK_THREAD_CPUTIME_ID` report. Does
    /// not include the current run until the process is switched out —
    /// `Scheduler::exec_ns_now` adds it.
    pub exec_ns: u64,
    /// `ktime_get()` when this process last started running (`switch_in`).
    pub run_since_ns: u64,
    /// When it was created, in ticks since boot (field 22 of
    /// `/proc/<pid>/stat`, `ps`'s elapsed time).
    pub start_ticks: u64,
    /// The CPU it last ran on (field 39 of `/proc/<pid>/stat`).
    pub last_cpu: usize,
    /// argv as `exec` received it, each argument NUL-terminated —
    /// `/proc/<pid>/cmdline`. A copy taken at exec (Linux reads the live
    /// argument area of the process's memory, so a program rewriting its
    /// own argv is not reflected here). Shared with `fork` children and
    /// threads until they exec; empty for kernel processes.
    pub cmdline: alloc::sync::Arc<[u8]>,
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
        trapframe.rflags = 0x202; // IF, plus bit 1 (reserved, always 1)
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
            sid: pid.0 as u32,
            ctty: None,
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
            saved_sigmask: None,
            in_sigsuspend: false,
            pending_signals: 0,
            tf_seq: 0,
            tf_awaiting_resume: false,
            tf_last_resumed_seq: None,
            wake_pending: None,
            armed_wait: None,
            wait: None,
            interrupted: None,
            sig_restart: 0,
            times: sched::cputime::ProcTimes::default(),
            dead_threads: sched::cputime::ProcTimes::default(),
            dead_threads_ns: 0,
            exec_ns: 0,
            last_cpu: 0,
            cmdline: alloc::sync::Arc::from(&[][..]),
            run_since_ns: 0,
            start_ticks: crate::time::ktime_get() / (1_000_000_000 / sched::cputime::USER_HZ),
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
        trapframe.rflags = 0x202; // IF, plus bit 1 (reserved, always 1)
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
            sid: pid.0 as u32,
            ctty: None,
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
            saved_sigmask: None,
            in_sigsuspend: false,
            pending_signals: 0,
            tf_seq: 0,
            tf_awaiting_resume: false,
            tf_last_resumed_seq: None,
            wake_pending: None,
            armed_wait: None,
            wait: None,
            interrupted: None,
            sig_restart: 0,
            times: sched::cputime::ProcTimes::default(),
            dead_threads: sched::cputime::ProcTimes::default(),
            dead_threads_ns: 0,
            exec_ns: 0,
            last_cpu: 0,
            cmdline: alloc::sync::Arc::from(&[][..]),
            run_since_ns: 0,
            start_ticks: crate::time::ktime_get() / (1_000_000_000 / sched::cputime::USER_HZ),
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
        parent_sid: u32,
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
            sid: parent_sid,
            ctty: None,
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
            saved_sigmask: None,
            in_sigsuspend: false,
            pending_signals: 0,
            tf_seq: 0,
            tf_awaiting_resume: false,
            tf_last_resumed_seq: None,
            wake_pending: None,
            armed_wait: None,
            wait: None,
            interrupted: None,
            sig_restart: 0,
            times: sched::cputime::ProcTimes::default(),
            dead_threads: sched::cputime::ProcTimes::default(),
            dead_threads_ns: 0,
            exec_ns: 0,
            last_cpu: 0,
            cmdline: alloc::sync::Arc::from(&[][..]),
            run_since_ns: 0,
            start_ticks: crate::time::ktime_get() / (1_000_000_000 / sched::cputime::USER_HZ),
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
        parent_sid: u32,
        exe_name: alloc::string::String,
    ) -> Self {
        let mut trapframe = Box::new(TrapFrame::default());

        trapframe.rip = entry.as_u64();
        trapframe.cs = 0x23;
        trapframe.rflags = 0x202; // IF, plus bit 1 (reserved, always 1)
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
            sid: parent_sid,
            ctty: None,
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
            saved_sigmask: None,
            in_sigsuspend: false,
            pending_signals: 0,
            tf_seq: 0,
            tf_awaiting_resume: false,
            tf_last_resumed_seq: None,
            wake_pending: None,
            armed_wait: None,
            wait: None,
            interrupted: None,
            sig_restart: 0,
            times: sched::cputime::ProcTimes::default(),
            dead_threads: sched::cputime::ProcTimes::default(),
            dead_threads_ns: 0,
            exec_ns: 0,
            last_cpu: 0,
            cmdline: alloc::sync::Arc::from(&[][..]),
            run_since_ns: 0,
            start_ticks: crate::time::ktime_get() / (1_000_000_000 / sched::cputime::USER_HZ),
        }
    }

    /// Set the process's display name (Linux's `comm`) — what
    /// `/proc/<pid>/stat` reports, and so what BusyBox `ps`/`top` show.
    /// Truncated to 15 bytes plus a NUL, matching Linux's
    /// `TASK_COMM_LEN`.
    ///
    /// Zeroes the rest of the field. That used to be unnecessary — every
    /// name was set exactly once, onto a freshly zeroed `[0; 16]` — but
    /// `sys_exec` now renames a live process, and readers stop at the
    /// first NUL (`fs::procfs`'s `render_proc_stat`), so a shorter name
    /// written over a longer one would otherwise leave the old tail
    /// visible: "child" renamed to "ls" would read as "lsild".
    pub fn set_name(&mut self, name: &str) {
        let bytes = name.as_bytes();
        let len = core::cmp::min(bytes.len(), 15);
        self.name = [0; 16];
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

/// Start the first user process — and, with it, the APs (stage 7 of
/// `docs/smp/smp-plan.md`): from here on every scheduling CPU takes work.
pub fn start_first_process() -> ! {
    let tf_ptr = {
        let mut scheduler = scheduler::local_scheduler();
        scheduler.start_first()
    };
    crate::smp::release_aps();

    unsafe {
        core::arch::asm!("sti");
    }

    unsafe { trapframe::jump_to_trapframe(tf_ptr) }
}
/// An AP enters the scheduler, from its boot loop (`smp::release_aps`),
/// IF=0: onto its idle process, with its tick running.
pub fn start_ap_scheduling() -> ! {
    let tf_ptr = scheduler::local_scheduler().start_ap();
    crate::interrupts::apic::start_timer_on_ap();
    unsafe { trapframe::jump_to_trapframe(tf_ptr) }
}
