// kernel/src/process/scheduler.rs
//
// Run-queue scheduler with time slices, priority aging, and wait queue.
//
// STRUCTURE:
//   run_queues[0..=10]  — ONLY Ready processes, indexed by effective_priority
//   wait_queue           — Blocked and Zombie processes (not scanned by scheduler)
//   running              — the single currently executing process
//
// A process moves between these containers:
//   add_process()   → run_queues[eff_pri]
//   switch_to_next  → running ↔ run_queues  (Ready processes only)
//   block_current() → running → wait_queue  (future: I/O wait)
//   wake(pid)       → wait_queue → run_queues[eff_pri]  (future: I/O complete)
//   kill_current()  → running → wait_queue as Zombie  (segfault, sys_exit)
//
// TIME SLICES + AGING:
//   Each process gets quantum = BASE_QUANTUM + eff_pri * BONUS ticks.
//   When exhausted: preempt, decay eff_pri by 1.
//   Every AGING_EPOCH ticks: boost waiting processes' eff_pri toward base.
//
// HISTORY:
//   - Removed IretFrame and kill_and_switch().  Replaced with
//     kill_and_switch_tf() which returns a *const TrapFrame, enabling
//     a FULL context switch (all GPRs restored) via jump_to_trapframe.
//     The old approach only overwrote the 5-field exception stack frame,
//     leaking RAX..R15 from the killed process into the next one.

use alloc::{boxed::Box, collections::VecDeque, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};

// ── FS.base save / restore helpers ──────────────────────────────────────────
// FS.base (MSR 0xC000_0100) is used by mlibc for TLS.  We must save it
// when context-switching away from a process and restore it for the next one.

const IA32_FS_BASE: u32 = 0xC000_0100;

#[inline(always)]
fn read_fs_base() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") IA32_FS_BASE,
            out("eax") lo,
            out("edx") hi,
            options(nostack, preserves_flags),
        );
    }
    (hi as u64) << 32 | lo as u64
}

#[inline(always)]
fn write_fs_base(val: u64) {
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") IA32_FS_BASE,
            in("eax") (val & 0xFFFF_FFFF) as u32,
            in("edx") (val >> 32) as u32,
            options(nostack, preserves_flags),
        );
    }
}
use spin::Mutex;
use x86_64::VirtAddr;
use super::{Process, Pid, ProcessState, TrapFrame};
use crate::memory::address_space::AddressSpace;
use crate::memory::vma::Vma;

// ============================================================================
// Per-CPU fast-path pointers (updated on every context switch, IF=0)
// ============================================================================
//
// These let the page fault handler look up the running process's AddressSpace
// and PID without acquiring the SCHEDULERS Mutex.
//
// Safety invariant: a fault handler always runs with IF=0 on a single CPU.
// Between a context switch updating these atomics and the next switch, no
// other context can run on the same CPU, so the pointer is always valid.

static CURRENT_AS_PTR: [AtomicUsize; crate::cpu::MAX_CPUS] =
    [const { AtomicUsize::new(0) }; crate::cpu::MAX_CPUS];
static CURRENT_PID_FAST: [AtomicUsize; crate::cpu::MAX_CPUS] =
    [const { AtomicUsize::new(0) }; crate::cpu::MAX_CPUS];

/// Re-sync the per-CPU fast-path pointers for the already-running process.
///
/// Needed after anything replaces `proc.address_space` with a new `Arc`
/// in place (e.g. `sys_exec`'s image swap) — the page fault handler's
/// `find_vma_fast`/`current_as_fast` read a cached `Arc::as_ptr` that a plain
/// field assignment does not update, so without this call every fault in the
/// new address space would look up VMAs in the old (dropped or otherwise
/// unrelated) one and spuriously report "no VMA".
pub fn refresh_current_fast(proc: &Process) {
    update_current_fast(proc);
}

/// Update the per-CPU fast-path pointers to reflect `proc` as the running process.
/// Called with interrupts disabled, just before storing into `self.running`.
#[inline]
fn update_current_fast(proc: &Process) {
    let cpu = crate::cpu::cpu_id();
    // Arc::as_ptr gives a stable pointer to the shared AddressSpace's heap
    // allocation — valid as long as *any* Arc reference is alive, which
    // `proc.address_space` itself guarantees for as long as `proc` is the
    // running process on this CPU.
    CURRENT_AS_PTR[cpu].store(
        alloc::sync::Arc::as_ptr(&proc.address_space) as usize,
        Ordering::Release,
    );
    CURRENT_PID_FAST[cpu].store(proc.pid.0, Ordering::Release);
}

/// Clear the per-CPU fast-path pointers (no process running on this CPU).
#[inline]
fn clear_current_fast() {
    let cpu = crate::cpu::cpu_id();
    CURRENT_AS_PTR[cpu].store(0, Ordering::Release);
    CURRENT_PID_FAST[cpu].store(0, Ordering::Release);
}

const NUM_PRIORITIES: usize = 11;

const BASE_QUANTUM: u32 = 2;
const PRIORITY_QUANTUM_BONUS: u32 = 1;
const AGING_EPOCH: u32 = 50;
const MIN_EFFECTIVE_PRIORITY: u8 = 1;

static SCHEDULERS: [Mutex<Scheduler>; crate::cpu::MAX_CPUS] = [
    Mutex::new(Scheduler::new()),
    Mutex::new(Scheduler::new()),
    Mutex::new(Scheduler::new()),
    Mutex::new(Scheduler::new()),
    Mutex::new(Scheduler::new()),
    Mutex::new(Scheduler::new()),
    Mutex::new(Scheduler::new()),
    Mutex::new(Scheduler::new()),
];

/// Acquires the current CPU's scheduler lock.
/// CALLER must disable interrupts before calling (cli) and
/// re-enable after dropping the guard (sti).
#[inline]
pub fn local_scheduler() -> spin::MutexGuard<'static, Scheduler> {
    SCHEDULERS[crate::cpu::cpu_id()].lock()
}

pub struct Scheduler {
    /// Per-priority run queues — ONLY Ready processes.
    run_queues: [VecDeque<Box<Process>>; NUM_PRIORITIES],

    /// Blocked and Zombie processes.  Not scanned during scheduling.
    pub wait_queue: VecDeque<Box<Process>>,

    /// Currently executing process.
    running: Option<Box<Process>>,

    /// Remaining ticks for the running process.
    remaining_ticks: u32,

    /// Global tick counter for aging epochs.
    global_ticks: u32,

    /// Monotonic PID counter (0 is reserved for idle).
    next_pid: usize,

    /// Kernel stacks awaiting `phys_free` — populated by `kill_current`'s
    /// thread-reap path, which runs *on the dying thread's own kernel
    /// stack* (called mid-syscall/exception, before the switch-away has
    /// actually happened via `jump_to_trapframe`/`iretq`). Freeing those
    /// physical frames immediately would let the Buddy allocator hand them
    /// out to something else while this CPU is still executing on them.
    /// Drained by `tick()` instead, which only ever runs once we're
    /// guaranteed to be on a different process's stack (interrupts stay
    /// off, hence no nested `tick()`, from the moment `kill_current` runs
    /// until the new process's `iretq` re-enables them).
    pending_stack_frees: Vec<VirtAddr>,

    /// Same deferral, for a dying thread's `owned_stack_vma` (its mlibc
    /// `mmap()`-allocated user-mode stack — see `Process::owned_stack_vma`).
    /// The `AddressSpace` is kept alive via this `Arc` for as long as the
    /// entry is queued, even if the `Process` that referenced it has
    /// already been dropped — it may otherwise be the last reference if the
    /// thread's parent process has also exited.
    pending_vma_frees: Vec<(alloc::sync::Arc<AddressSpace>, u64, usize)>,
}

impl Scheduler {
    pub const fn new() -> Self {
        Self {
            run_queues: [
                VecDeque::new(), VecDeque::new(), VecDeque::new(),
                VecDeque::new(), VecDeque::new(), VecDeque::new(),
                VecDeque::new(), VecDeque::new(), VecDeque::new(),
                VecDeque::new(), VecDeque::new(),
            ],
            wait_queue: VecDeque::new(),
            running: None,
            remaining_ticks: 0,
            global_ticks: 0,
            next_pid: 1,
            pending_stack_frees: Vec::new(),
            pending_vma_frees: Vec::new(),
        }
    }

    // ====================================================================
    // Time slice
    // ====================================================================

    fn quantum_for(effective_priority: u8) -> u32 {
        BASE_QUANTUM + (effective_priority as u32) * PRIORITY_QUANTUM_BONUS
    }

    // ====================================================================
    // PID management
    // ====================================================================

    pub fn allocate_pid(&mut self) -> Pid {
        let pid = Pid(self.next_pid);
        self.next_pid += 1;
        pid
    }

    // ====================================================================
    // Process insertion
    // ====================================================================

    pub fn add_process(&mut self, mut process: Box<Process>) {
        process.effective_priority = process.priority;
        let pri = (process.effective_priority as usize).min(NUM_PRIORITIES - 1);
        crate::serial_println!(
            "Scheduler: Added PID {} (base pri {}, effective {}) to queue[{}]",
            process.pid.0, process.priority, process.effective_priority, pri
        );
        self.run_queues[pri].push_back(process);
    }

    // ====================================================================
    // Current process access — O(1)
    // ====================================================================

    pub fn current_pid(&self) -> Option<Pid> {
        self.running.as_ref().map(|p| p.pid)
    }

    pub fn running_ref(&self) -> Option<&Process> {
        self.running.as_deref()
    }

    pub fn running_mut(&mut self) -> Option<&mut Process> {
        self.running.as_deref_mut()
    }

    // ====================================================================
    // Iteration (debug / introspection)
    // ====================================================================

    /// Iterate over ALL processes: running + run queues + wait queue.
    pub fn iter_all(&self) -> impl Iterator<Item = &Process> + '_ {
        self.running.as_deref().into_iter()
            .chain(
                self.run_queues.iter()
                    .flat_map(|q| q.iter())
                    .map(|b| b.as_ref())
            )
            .chain(
                self.wait_queue.iter().map(|b| b.as_ref())
            )
    }

    /// Check the currently-`running` process's pending signals against `tf`
    /// (must point at that same process's live TrapFrame — see callers)
    /// and act on the outcome: a caught signal redirects `tf` in place and
    /// is returned unchanged; an uncaught default-terminate signal kills
    /// the process via `kill_and_switch_tf` and repeats against whatever
    /// gets scheduled next, so the final returned pointer always belongs to
    /// a process that's either signal-clean or non-existent-and-replaced.
    ///
    /// Centralizes the same three-line loop that both `trapframe::
    /// jump_to_user` and `syscall_handler_asm`'s tail need — the latter
    /// has no `jump_to_trapframe` call of its own to hang the check off of,
    /// so it calls this directly instead of going through `jump_to_user`.
    pub fn resolve_signals(&mut self, mut tf: *const TrapFrame) -> *const TrapFrame {
        // Ring-3 code segment selector (see Process::new_user's trapframe.cs).
        const USER_CS: u64 = 0x23;
        loop {
            // Only attempt delivery when `tf` genuinely represents a
            // user-mode return point. A kernel-mode-interrupted trapframe's
            // `rsp` is a *kernel* stack address, not a user one — treating
            // it as user (as signal delivery must, to push a handler frame)
            // would corrupt whatever that kernel rsp actually pointed at.
            // Pending signals just stay pending and get retried the next
            // time this process genuinely returns to user mode. Found via
            // `sys_close` briefly running with interrupts enabled and no
            // held lock, letting a timer tick preempt mid-syscall and save
            // exactly this kind of kernel-mode trapframe (now fixed there
            // too, but this check is what makes the class of mistake safe
            // wherever else it might still be lurking).
            if unsafe { (*tf).cs } != USER_CS {
                return tf;
            }

            let outcome = match self.running_mut() {
                Some(proc) if proc.privilege == crate::process::PrivilegeLevel::User => {
                    super::signal::deliver_pending(proc, tf as *mut TrapFrame)
                }
                _ => super::signal::SignalOutcome::None,
            };

            match outcome {
                super::signal::SignalOutcome::Terminate(sig) => {
                    // Tag the about-to-die process with the signal that
                    // killed it (read back by `Process::wait_status_word()`)
                    // and capture what its parent needs to be told, before
                    // `kill_and_switch_tf` below takes it out of `self.running`.
                    let (dead_pid, parent_pid) = match self.running_mut() {
                        Some(proc) => {
                            proc.killed_by_signal = Some(sig);
                            let parent = if proc.is_thread { None } else { proc.parent_pid };
                            (proc.pid.0, parent)
                        }
                        None => (0, None),
                    };

                    tf = self.kill_and_switch_tf("uncaught signal");
                    self.notify_child_death(dead_pid, parent_pid);
                    // Same side-table cleanup `sys_exit` does for a normal
                    // exit — see `syscall::cancel_all_waiters`'s doc comment
                    // for why skipping this here specifically caused a
                    // stale poll waiter to leak and later spuriously affect
                    // an unrelated process.
                    super::syscall::cancel_all_waiters(dead_pid);
                }
                super::signal::SignalOutcome::Stop(sig) => {
                    // Same shape as Terminate above, but parks the process
                    // as Stopped instead of discarding it — see
                    // `stop_and_switch_tf`/`notify_child_stopped`.
                    let (stopped_pid, parent_pid) = match self.running_mut() {
                        Some(proc) => {
                            proc.stopped_by_signal = Some(sig);
                            proc.stop_reported = false;
                            (proc.pid.0, proc.parent_pid)
                        }
                        None => (0, None),
                    };

                    tf = self.stop_and_switch_tf(tf);
                    self.notify_child_stopped(stopped_pid, parent_pid);
                }
                _ => return tf,
            }
        }
    }

    /// Find a Ready (run_queues) or Blocked/Zombie (wait_queue) process by
    /// pid — i.e. everything *except* the currently running one, which
    /// callers (e.g. `sys_kill`) handle separately via `running_mut()`.
    /// Used to deliver a signal to a process other than the caller itself.
    pub fn find_process_mut(&mut self, pid: usize) -> Option<&mut Process> {
        for queue in self.run_queues.iter_mut() {
            if let Some(proc) = queue.iter_mut().find(|p| p.pid.0 == pid) {
                return Some(proc.as_mut());
            }
        }
        self.wait_queue.iter_mut().find(|p| p.pid.0 == pid).map(|p| p.as_mut())
    }

    // ====================================================================
    // Kill current process (user segfault, sys_exit)
    // ====================================================================

    /// Mark the running process as Zombie and move it to the wait queue —
    /// unless it's a thread (`is_thread`), in which case it's reaped
    /// immediately instead (dropped here and now).
    ///
    /// Threads never get an explicit `waitpid()` call collecting them:
    /// mlibc's `pthread_join()` (upstream, shared by every sysdeps port —
    /// see `Process::is_thread`'s doc comment) is purely futex-based and
    /// never issues one. Zombie-parking a thread the normal way would leak
    /// its `Process` struct (and kernel stack) forever, since nothing will
    /// ever remove it from `wait_queue`. So this is the thread-exit
    /// equivalent of an implicit, always-successful `waitpid()`.
    ///
    /// Returns true if a process was killed, false if nothing was running.
    /// After calling this, the caller must trigger a context switch
    /// (the running slot is now empty).
    pub fn kill_current(&mut self, reason: &str) -> bool {
        if let Some(mut proc) = self.running.take() {
            crate::serial_println!(
                "💀 Killed PID {} ({}): {}",
                proc.pid.0,
                core::str::from_utf8(&proc.name)
                    .unwrap_or("<?>")
                    .trim_end_matches('\0'),
                reason,
            );
            if proc.is_thread {
                crate::serial_println!("  → thread, reaped immediately (no waitpid() will ever collect it)");
                // Defer the kernel stack's phys_free — see pending_stack_frees'
                // doc comment for why it can't happen right here.
                self.pending_stack_frees.push(proc.kernel_stack);
                // Same deferral for the thread's own mmap'd user stack, if
                // sys_clone found one — see pending_vma_frees' doc comment.
                if let Some((start, size_pages)) = proc.owned_stack_vma {
                    self.pending_vma_frees.push((proc.address_space.clone(), start, size_pages));
                }
                // `proc` drops here: releases the Process struct itself and its
                // Arc references to the shared AddressSpace/FileDescriptorTable
                // (safe immediately — unlike the kernel stack, that's ordinary
                // kernel-heap memory, not the stack this code is executing on).
            } else {
                proc.state = ProcessState::Zombie;
                self.wait_queue.push_back(proc);
            }
            true
        } else {
            false
        }
    }

    /// Kill the running process and schedule the next one.
    ///
    /// Returns a pointer to the next process's FULL TrapFrame (all GPRs
    /// + iret fields).  The caller must use `jump_to_trapframe` to load
    /// all registers and iretq into the new process.
    ///
    /// This replaces the old `kill_and_switch` which returned only the 5
    /// iret-frame fields, leaking GPR values from the killed process.
    ///
    /// Also activates the new address space and updates TSS.
    ///
    /// Panics if no Ready process exists (shouldn't happen with idle).
    pub fn kill_and_switch_tf(&mut self, reason: &str) -> *const TrapFrame {
        self.kill_current(reason);

        // Find and schedule next Ready process
        for priority in (0..NUM_PRIORITIES).rev() {
            if let Some(mut proc) = self.run_queues[priority].pop_front() {
                proc.state = ProcessState::Running;

                unsafe {
                    proc.address_space.activate();
                }
                super::tss::set_kernel_stack(proc.kernel_stack);

                self.remaining_ticks = Self::quantum_for(proc.effective_priority);

                let tf_ptr = &*proc.trapframe as *const TrapFrame;
                update_current_fast(&proc);
                self.running = Some(proc);
                return tf_ptr;
            }
        }

        panic!("No process to switch to after killing user process");
    }

    /// Stop the running process (job control: SIGSTOP/SIGTSTP) and schedule
    /// the next one. Mirrors `kill_and_switch_tf`, except the process is
    /// parked as `ProcessState::Stopped` in `wait_queue` instead of being
    /// discarded — `sys_kill`'s SIGCONT handling (`wake_stopped`) is the
    /// only thing that ever resumes it.
    ///
    /// Unlike `kill_and_switch_tf` (which never needs the outgoing process's
    /// register state, since it's being thrown away), this *does* need to
    /// save `tf` into `proc.trapframe` first — `tf` may be the live syscall-
    /// entry stack frame rather than `proc.trapframe` itself (see
    /// `resolve_signals`'s call sites), and a stopped process must resume
    /// later exactly where it left off.
    pub fn stop_and_switch_tf(&mut self, tf: *const TrapFrame) -> *const TrapFrame {
        if let Some(mut proc) = self.running.take() {
            unsafe { *proc.trapframe = *tf; }
            proc.fs_base = read_fs_base();
            crate::serial_println!(
                "⏸ Stopped PID {} ({})",
                proc.pid.0,
                core::str::from_utf8(&proc.name).unwrap_or("<?>").trim_end_matches('\0'),
            );
            proc.state = ProcessState::Stopped;
            self.wait_queue.push_back(proc);
        }
        clear_current_fast();

        for priority in (0..NUM_PRIORITIES).rev() {
            if let Some(mut proc) = self.run_queues[priority].pop_front() {
                proc.state = ProcessState::Running;
                unsafe { proc.address_space.activate(); }
                super::tss::set_kernel_stack(proc.kernel_stack);
                write_fs_base(proc.fs_base);
                self.remaining_ticks = Self::quantum_for(proc.effective_priority);
                let tf_ptr = &*proc.trapframe as *const TrapFrame;
                update_current_fast(&proc);
                self.running = Some(proc);
                return tf_ptr;
            }
        }

        panic!("No process to switch to after stopping process");
    }

    /// Queue `sig` on every process whose `pgid` matches — used for
    /// job-control signals (Ctrl-C/Ctrl-Z at the tty, see `tty::feed_input`)
    /// and `sys_kill`'s process-group target forms (`pid == 0` / negative).
    /// Caller must already hold the scheduler lock (this takes `&mut self`,
    /// not a fresh lock) — see `syscall::send_to_group` for the ISR-context
    /// wrapper that acquires one.
    pub fn queue_signal_to_group(&mut self, pgid: u32, sig: u32) {
        if let Some(proc) = self.running.as_deref_mut() {
            if proc.pgid == pgid {
                super::signal::queue_signal(proc, sig);
            }
        }
        for queue in self.run_queues.iter_mut() {
            for proc in queue.iter_mut() {
                if proc.pgid == pgid {
                    super::signal::queue_signal(proc, sig);
                }
            }
        }
        for proc in self.wait_queue.iter_mut() {
            if proc.pgid == pgid {
                super::signal::queue_signal(proc, sig);
            }
        }
    }

    /// Wake a Stopped process (SIGCONT): move it from `wait_queue` back to
    /// its run queue, exactly like `wake()` does for a Blocked one. Unlike
    /// `wake()`, this is the *only* wakeup path a Stopped process ever has
    /// — it can't wake itself the way a Blocked process does when its I/O
    /// completes, since being stopped isn't waiting on anything.
    pub fn wake_stopped(&mut self, pid: usize) -> bool {
        if let Some(pos) = self.wait_queue.iter().position(|p| {
            p.pid.0 == pid && matches!(p.state, ProcessState::Stopped)
        }) {
            if let Some(mut proc) = self.wait_queue.remove(pos) {
                proc.state = ProcessState::Ready;
                proc.stopped_by_signal = None;
                let pri = (proc.effective_priority as usize).min(NUM_PRIORITIES - 1);
                self.run_queues[pri].push_back(proc);
            }
            true
        } else {
            false
        }
    }

    // ====================================================================
    // Blocking / wakeup (I/O wait)
    // ====================================================================

    /// Block the running process (copy TF into Box, move to wait_queue).
    ///
    /// Returns the next Ready process's TrapFrame pointer.
    /// Panics if no Ready process exists (idle must always be ready).
    pub fn block_current(&mut self, current_tf: *const TrapFrame) -> *const TrapFrame {
        if let Some(mut proc) = self.running.take() {
            unsafe { *proc.trapframe = *current_tf; }
            proc.fs_base = read_fs_base();
            proc.state = ProcessState::Blocked;
            self.wait_queue.push_back(proc);
        }
        // No process running on this CPU until we schedule the next one.
        clear_current_fast();

        for priority in (0..NUM_PRIORITIES).rev() {
            if let Some(mut proc) = self.run_queues[priority].pop_front() {
                proc.state = ProcessState::Running;
                unsafe { proc.address_space.activate(); }
                super::tss::set_kernel_stack(proc.kernel_stack);
                write_fs_base(proc.fs_base);
                self.remaining_ticks = Self::quantum_for(proc.effective_priority);
                let tf_ptr = &*proc.trapframe as *const TrapFrame;
                update_current_fast(&proc);
                self.running = Some(proc);
                return tf_ptr;
            }
        }

        panic!("No process to switch to after blocking");
    }

    /// Wake a Blocked process: move it from wait_queue to its run_queue.
    pub fn wake(&mut self, pid: usize) {
        if let Some(pos) = self.wait_queue.iter().position(|p| {
            p.pid.0 == pid && matches!(p.state, ProcessState::Blocked)
        }) {
            if let Some(mut proc) = self.wait_queue.remove(pos) {
                proc.state = ProcessState::Ready;
                let pri = (proc.effective_priority as usize).min(NUM_PRIORITIES - 1);
                self.run_queues[pri].push_back(proc);
            }
        }
    }

    /// Wake a Blocked process and set its syscall return value in one scan.
    ///
    /// Combines what was previously two separate operations in the IPC delivery
    /// path (set trapframe.rax then call wake()) into a single wait_queue scan,
    /// halving the linear-search overhead for IPC hot paths.
    pub fn wake_with_retval(&mut self, pid: usize, rax: u64) {
        if let Some(pos) = self.wait_queue.iter().position(|p| {
            p.pid.0 == pid && matches!(p.state, ProcessState::Blocked)
        }) {
            if let Some(mut proc) = self.wait_queue.remove(pos) {
                proc.trapframe.rax = rax;
                proc.state = ProcessState::Ready;
                let pri = (proc.effective_priority as usize).min(NUM_PRIORITIES - 1);
                self.run_queues[pri].push_back(proc);
            }
        }
    }

    /// Called once `dead_pid` is fully dead (either already zombie-parked
    /// in `wait_queue`, or reaped immediately if it was a thread) — queues
    /// `SIGCHLD` on the parent (if there is one to notify — threads never
    /// get one, see `Process::is_thread`'s doc comment) and wakes it if
    /// it's blocked in `waitpid()` for exactly this child.
    ///
    /// Must be called with the scheduler lock already held (`&mut self`,
    /// i.e. from inside a method on `Scheduler`) and interrupts already
    /// disabled — every call site satisfies both by the time a process is
    /// fully dead. This replaces what used to be two separate operations
    /// (a manual SIGCHLD-queue block plus a freestanding `waitpid_wakeup`
    /// function that re-acquired the scheduler lock itself) so both
    /// `sys_exit` and the uncaught-signal/hardware-fault kill paths can
    /// share one correctly-locked implementation instead of each growing
    /// their own copy.
    pub fn notify_child_death(&mut self, dead_pid: usize, parent_pid: Option<Pid>) {
        if let Some(parent_pid) = parent_pid {
            if self.current_pid() == Some(parent_pid) {
                if let Some(parent) = self.running_mut() {
                    super::signal::queue_signal(parent, super::signal::SIGCHLD);
                }
            } else if let Some(parent) = self.find_process_mut(parent_pid.0) {
                super::signal::queue_signal(parent, super::signal::SIGCHLD);
            }
        }

        // Real exit status, if `dead_pid` is parked as a zombie. Threads
        // aren't (reaped immediately in `kill_current`), so this stays at
        // the "exited(0)" default for them — matches this kernel's existing
        // stance that nothing meaningful ever `waitpid()`s a thread's tid.
        let dead = self.wait_queue.iter()
            .find(|p| p.pid.0 == dead_pid && matches!(p.state, ProcessState::Zombie));
        let status_word = dead.map(|p| p.wait_status_word()).unwrap_or(0x200);
        let dead_pgid = dead.map(|p| p.pgid).unwrap_or(0);

        // Only the real parent can be woken — `WaitTarget::AnyChild`/`Pgid`
        // still must not wake an unrelated process just because its own
        // `waitpid()` target happens to match by pid/pgid coincidence.
        let mut waker_pid: Option<usize> = None;
        for proc in self.wait_queue.iter_mut() {
            if Some(proc.pid) == parent_pid
                && matches!(proc.state, ProcessState::Blocked)
                && proc.waiting_for.map(|t| t.matches(dead_pid, dead_pgid)).unwrap_or(false)
            {
                proc.trapframe.rax = dead_pid as u64;
                proc.waiting_for = None;
                proc.pending_wait_status = Some(status_word);
                waker_pid = Some(proc.pid.0);
                break;
            }
        }
        if let Some(pid) = waker_pid {
            self.wake(pid);
        }
    }

    /// Called once a child transitions to `ProcessState::Stopped` (SIGSTOP/
    /// SIGTSTP) — queues `SIGCHLD` on the parent (matches real POSIX: a
    /// child stopping is also a `SIGCHLD`-worthy event, not just exiting)
    /// and wakes the parent if it's blocked in a `WUNTRACED` `waitpid()`
    /// matching this pid/pgid. Unlike `notify_child_death`, the stopped
    /// process is NOT removed from `wait_queue` — it stays there so a later
    /// real exit, or another stop/continue cycle, can still be observed.
    pub fn notify_child_stopped(&mut self, stopped_pid: usize, parent_pid: Option<Pid>) {
        if let Some(parent_pid) = parent_pid {
            if self.current_pid() == Some(parent_pid) {
                if let Some(parent) = self.running_mut() {
                    super::signal::queue_signal(parent, super::signal::SIGCHLD);
                }
            } else if let Some(parent) = self.find_process_mut(parent_pid.0) {
                super::signal::queue_signal(parent, super::signal::SIGCHLD);
            }
        }

        let Some((stopped_pgid, status_word)) = self.wait_queue.iter()
            .find(|p| p.pid.0 == stopped_pid && matches!(p.state, ProcessState::Stopped))
            .map(|p| (p.pgid, p.stop_status_word()))
        else {
            return;
        };

        const WUNTRACED: i32 = 4;
        let mut waker_pid: Option<usize> = None;
        for proc in self.wait_queue.iter_mut() {
            if Some(proc.pid) == parent_pid
                && matches!(proc.state, ProcessState::Blocked)
                && proc.waiting_options & WUNTRACED != 0
                && proc.waiting_for.map(|t| t.matches(stopped_pid, stopped_pgid)).unwrap_or(false)
            {
                proc.trapframe.rax = stopped_pid as u64;
                proc.waiting_for = None;
                proc.pending_wait_status = Some(status_word);
                waker_pid = Some(proc.pid.0);
                break;
            }
        }
        if let Some(pid) = waker_pid {
            // One-shot: don't let a future waitpid() scan re-report the
            // same stop event (see `Process::stop_reported`'s doc comment).
            if let Some(p) = self.wait_queue.iter_mut().find(|p| p.pid.0 == stopped_pid) {
                p.stop_reported = true;
            }
            self.wake(pid);
        }
    }

    /// If the process about to resume (`self.running`) has a pending
    /// reaped-child wait status (stashed by `notify_child_death`, possibly
    /// while a completely different process's page table was active, since
    /// a dying child can't safely write into its blocked parent's user
    /// memory directly), write it into the user pointer that process
    /// originally passed to `waitpid()`, now that its own address space is
    /// active again.
    ///
    /// Called from every "about to return to user mode" site (mirrors
    /// `resolve_signals`, see its call sites) — cheap no-op check when
    /// there's nothing pending, which is the common case.
    pub fn resolve_wait_status(&mut self) {
        let Some(proc) = self.running_mut() else { return; };
        let Some(status) = proc.pending_wait_status.take() else { return; };
        if proc.waiting_status_ptr != 0 {
            unsafe {
                core::ptr::write(proc.waiting_status_ptr as *mut i32, status);
            }
        }
        proc.waiting_status_ptr = 0;
    }

    // ====================================================================
    // Timer tick
    // ====================================================================

    /// Called on every timer tick.  Returns true if a context switch
    /// should happen (time slice exhausted).
    pub fn tick(&mut self) -> bool {
        self.global_ticks = self.global_ticks.wrapping_add(1);

        // Safe w.r.t. *which* stacks these are: reaching a new timer tick
        // means the CPU already executed some process's iretq since any
        // pending_stack_frees entry was queued (interrupts are off from
        // kill_current through that iretq, so no tick can land in between)
        // — so none of these can be the stack we're currently running on.
        //
        // Still must use try_free (non-blocking): this runs inside the
        // timer ISR, which can interrupt code that already holds the
        // Buddy lock without having disabled interrupts (nothing before
        // this ever called into Buddy from an ISR). Entries that lose the
        // race just stay queued for the next tick.
        self.pending_stack_frees.retain(|&stack_top| {
            !crate::init::processes::try_free_kernel_stack(stack_top)
        });
        // Same reasoning as pending_stack_frees above — see try_free_huge_vma's
        // doc comment for why this specific free needs the try_lock treatment.
        self.pending_vma_frees.retain(|(address_space, start, size_pages)| {
            !unsafe { address_space.try_free_huge_vma(*start, *size_pages) }
        });

        if self.global_ticks % AGING_EPOCH == 0 {
            self.age_processes();
        }

        if self.remaining_ticks > 0 {
            self.remaining_ticks -= 1;
        }

        self.remaining_ticks == 0
    }

    // ====================================================================
    // Priority aging
    // ====================================================================

    /// Boost effective_priority of all Ready processes in run queues
    /// toward their base_priority.
    fn age_processes(&mut self) {
        for pri in 0..NUM_PRIORITIES {
            let mut i = 0;
            while i < self.run_queues[pri].len() {
                let proc = &self.run_queues[pri][i];

                if proc.pid.0 == 0 {
                    i += 1;
                    continue;
                }

                if proc.effective_priority < proc.priority {
                    let mut proc = self.run_queues[pri].remove(i).unwrap();
                    proc.effective_priority = (proc.effective_priority + 1).min(proc.priority);
                    let new_pri = (proc.effective_priority as usize).min(NUM_PRIORITIES - 1);
                    self.run_queues[new_pri].push_back(proc);
                    // Don't increment i — next element shifted into position i
                } else {
                    i += 1;
                }
            }
        }
    }

    // ====================================================================
    // Context switch
    // ====================================================================

    /// Save current process, find next Ready, activate, return new TrapFrame.
    pub fn switch_to_next(&mut self, current_tf: *const TrapFrame) -> *const TrapFrame {
        // ── 1. Save current process back to its run queue ─────────────

        if let Some(mut proc) = self.running.take() {
            unsafe {
                *proc.trapframe = *current_tf;
            }
            proc.fs_base = read_fs_base();

            match proc.state {
                ProcessState::Running => {
                    // Normal preemption — put back in run queue as Ready
                    proc.state = ProcessState::Ready;

                    // Decay effective priority (not idle)
                    if proc.pid.0 != 0 && proc.effective_priority > MIN_EFFECTIVE_PRIORITY {
                        proc.effective_priority -= 1;
                    }

                    let pri = (proc.effective_priority as usize).min(NUM_PRIORITIES - 1);
                    self.run_queues[pri].push_back(proc);
                }
                ProcessState::Zombie | ProcessState::Blocked | ProcessState::Stopped => {
                    // Process was killed, blocked, or stopped (job control)
                    // during its slice.
                    self.wait_queue.push_back(proc);
                }
                ProcessState::Ready => {
                    let pri = (proc.effective_priority as usize).min(NUM_PRIORITIES - 1);
                    self.run_queues[pri].push_back(proc);
                }
            }
        }

        // ── 2. Find highest effective-priority Ready process ──────────
        //
        // Run queues contain ONLY Ready processes, so no need to skip
        // Blocked/Zombie.  Just pop from front.

        for priority in (0..NUM_PRIORITIES).rev() {
            if let Some(mut proc) = self.run_queues[priority].pop_front() {
                proc.state = ProcessState::Running;

                unsafe {
                    proc.address_space.activate();
                }
                super::tss::set_kernel_stack(proc.kernel_stack);
                write_fs_base(proc.fs_base);

                self.remaining_ticks = Self::quantum_for(proc.effective_priority);

                let tf_ptr = &*proc.trapframe as *const TrapFrame;
                update_current_fast(&proc);
                self.running = Some(proc);
                return tf_ptr;
            }
        }

        // ── 3. Nothing Ready (shouldn't happen if idle exists) ────────
        current_tf
    }

    // ====================================================================
    // Boot: start first process
    // ====================================================================

    pub fn start_first(&mut self) -> *const TrapFrame {
        crate::serial_println!("Available processes:");
        for pri in (0..NUM_PRIORITIES).rev() {
            for proc in self.run_queues[pri].iter() {
                crate::serial_println!(
                    "  PID {} (base pri {}, eff {}): {:?} - {:?}",
                    proc.pid.0,
                    proc.priority,
                    proc.effective_priority,
                    core::str::from_utf8(&proc.name)
                        .unwrap_or("<?>")
                        .trim_end_matches('\0'),
                    proc.privilege,
                );
            }
        }

        for priority in (1..NUM_PRIORITIES).rev() {
            let queue = &mut self.run_queues[priority];

            for i in 0..queue.len() {
                if queue[i].state == ProcessState::Ready && queue[i].pid.0 != 0 {
                    let mut proc = queue.remove(i).unwrap();
                    proc.state = ProcessState::Running;

                    crate::serial_println!(
                        "\n🚀 Starting first process: PID {} ({})",
                        proc.pid.0,
                        core::str::from_utf8(&proc.name)
                            .unwrap_or("<invalid>")
                            .trim_end_matches('\0'),
                    );

                    super::tss::set_kernel_stack(proc.kernel_stack);
                    unsafe {
                        proc.address_space.activate();
                    }

                    self.remaining_ticks = Self::quantum_for(proc.effective_priority);

                    let tf_ptr = &*proc.trapframe as *const TrapFrame;
                    update_current_fast(&proc);
                    self.running = Some(proc);
                    return tf_ptr;
                }
            }
        }

        panic!("No process to start!");
    }
}

// ============================================================================
// Public API
// ============================================================================

pub fn current_pid() -> Option<usize> {
    local_scheduler().current_pid().map(|pid| pid.0)
}

pub fn find_current_vma(addr: u64) -> Option<(usize, Vma)> {
    let scheduler = local_scheduler();
    let proc = scheduler.running_ref()?;
    let vma = proc.address_space.find_vma(addr)?;
    Some((proc.pid.0, vma))
}

/// Fast VMA lookup without acquiring the Scheduler Mutex.
///
/// Safe because:
/// - The fault handler runs with IF=0 (preemption impossible on a single CPU).
/// - `CURRENT_AS_PTR` always points into the currently-running process's
///   `Box<Process>`, which is kept alive by `self.running`.
/// - The pointer is updated before storing into `self.running`, so it is
///   always consistent with the actual running process.
///
/// # Safety
/// Must be called with interrupts disabled.
pub unsafe fn find_vma_fast(fault_addr: u64) -> Option<(usize, Vma)> {
    let cpu = crate::cpu::cpu_id();
    let as_ptr = CURRENT_AS_PTR[cpu].load(Ordering::Acquire) as *const AddressSpace;
    if as_ptr.is_null() {
        return None;
    }
    let pid = CURRENT_PID_FAST[cpu].load(Ordering::Acquire);
    let vma = (*as_ptr).find_vma(fault_addr)?;
    Some((pid, vma))
}

/// Fast access to the running process's AddressSpace without the Mutex.
///
/// Same safety invariants as `find_vma_fast`.
///
/// # Safety
/// Must be called with interrupts disabled.
pub unsafe fn current_as_fast() -> Option<&'static AddressSpace> {
    let cpu = crate::cpu::cpu_id();
    let as_ptr = CURRENT_AS_PTR[cpu].load(Ordering::Acquire) as *const AddressSpace;
    if as_ptr.is_null() {
        None
    } else {
        Some(&*as_ptr)
    }
}

/// Fast PID read for logging (no Mutex).
pub fn current_pid_fast() -> usize {
    CURRENT_PID_FAST[crate::cpu::cpu_id()].load(Ordering::Relaxed)
}