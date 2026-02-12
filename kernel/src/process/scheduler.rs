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

use alloc::{boxed::Box, collections::VecDeque, vec::Vec};
use spin::Mutex;
use super::{Process, Pid, ProcessState, TrapFrame};
use crate::memory::vma::Vma;

const NUM_PRIORITIES: usize = 11;

/// Data needed to overwrite an exception stack frame for process switching.
/// Returned by `kill_and_switch` so the caller (page fault handler) can
/// redirect `iretq` to the next process without touching scheduler internals.
pub struct IretFrame {
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}
const BASE_QUANTUM: u32 = 2;
const PRIORITY_QUANTUM_BONUS: u32 = 1;
const AGING_EPOCH: u32 = 50;
const MIN_EFFECTIVE_PRIORITY: u8 = 1;

pub static SCHEDULER: Mutex<Scheduler> = Mutex::new(Scheduler::new());

pub struct Scheduler {
    /// Per-priority run queues — ONLY Ready processes.
    run_queues: [VecDeque<Box<Process>>; NUM_PRIORITIES],

    /// Blocked and Zombie processes.  Not scanned during scheduling.
    wait_queue: VecDeque<Box<Process>>,

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

    /// Mark the running process as Zombie and move it to the wait queue.
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
            proc.state = ProcessState::Zombie;
            self.wait_queue.push_back(proc);
            true
        } else {
            false
        }
    }

    /// Kill the running process and schedule the next one.
    ///
    /// Returns the iret frame fields (rip, cs, rflags, rsp, ss) of the
    /// next process so the caller can overwrite an exception stack frame.
    /// Also activates the new address space and updates TSS.
    ///
    /// Panics if no Ready process exists (shouldn't happen with idle).
    pub fn kill_and_switch(&mut self, reason: &str) -> IretFrame {
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

                let frame = IretFrame {
                    rip: proc.trapframe.rip,
                    cs: proc.trapframe.cs,
                    rflags: proc.trapframe.rflags,
                    rsp: proc.trapframe.rsp,
                    ss: proc.trapframe.ss,
                };

                self.running = Some(proc);
                return frame;
            }
        }

        panic!("No process to switch to after killing user process");
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
                    // (e.g. kill_current was called but running was already taken)
                    self.wait_queue.push_back(proc);
                }
                ProcessState::Ready => {
                    // Shouldn't happen, but handle gracefully
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

                self.remaining_ticks = Self::quantum_for(proc.effective_priority);

                let tf_ptr = &*proc.trapframe as *const TrapFrame;
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
    let scheduler = SCHEDULER.lock();
    scheduler.current_pid().map(|pid| pid.0)
}

pub fn find_current_vma(addr: u64) -> Option<(usize, Vma)> {
    let scheduler = SCHEDULER.lock();
    let proc = scheduler.running_ref()?;
    let vma = proc.address_space.find_vma(addr)?;
    Some((proc.pid.0, vma))
}