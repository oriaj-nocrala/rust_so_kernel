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
                // `proc` drops here: releases its kernel stack slot and its
                // Arc references to the shared AddressSpace/FileDescriptorTable.
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

    // ====================================================================
    // Timer tick
    // ====================================================================

    /// Called on every timer tick.  Returns true if a context switch
    /// should happen (time slice exhausted).
    pub fn tick(&mut self) -> bool {
        self.global_ticks = self.global_ticks.wrapping_add(1);

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
                ProcessState::Zombie | ProcessState::Blocked => {
                    // Process was killed or blocked during its slice
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