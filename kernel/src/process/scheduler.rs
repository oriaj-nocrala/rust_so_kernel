// kernel/src/process/scheduler.rs
//
// Run-queue scheduler with time slices and priority aging.
//
// DESIGN:
//   - 11 priority levels (0 = idle-only, 1..=10 = normal).
//   - Per-priority run queues.  Running process is in `running`, not in any queue.
//   - Each process has a TIME SLICE (ticks).  Higher base_priority → longer slice.
//   - On each timer tick: decrement running process's remaining ticks.
//   - When slice exhausted: preempt, lower effective_priority by 1 (decay).
//   - Every AGING_EPOCH ticks: boost all waiting processes' effective_priority
//     toward their base_priority (anti-starvation).
//
// RESULT:
//   Shell (pri 8) runs for its slice, then decays to effective pri 7.
//   After enough decays, user processes (pri 5) get a turn.
//   Aging eventually restores the shell's effective priority.
//   Everyone runs.  No starvation.
//
// TIME SLICE FORMULA:
//   ticks = BASE_QUANTUM + (effective_priority * PRIORITY_QUANTUM_BONUS)
//   With BASE_QUANTUM=2, BONUS=1: pri 8 → 10 ticks, pri 5 → 7 ticks, pri 0 → 2 ticks.

use alloc::{boxed::Box, collections::VecDeque};
use spin::Mutex;
use super::{Process, Pid, ProcessState, TrapFrame};
use crate::memory::vma::Vma;

/// Number of priority levels (0..=10).
const NUM_PRIORITIES: usize = 11;

/// Base time slice (ticks) for all processes.
const BASE_QUANTUM: u32 = 2;

/// Extra ticks per effective priority level.
const PRIORITY_QUANTUM_BONUS: u32 = 1;

/// Every this many timer ticks, boost starving processes.
const AGING_EPOCH: u32 = 50;

/// Minimum effective priority (idle excluded — idle is always 0).
const MIN_EFFECTIVE_PRIORITY: u8 = 1;

pub static SCHEDULER: Mutex<Scheduler> = Mutex::new(Scheduler::new());

pub struct Scheduler {
    /// Per-priority run queues indexed by EFFECTIVE priority.
    run_queues: [VecDeque<Box<Process>>; NUM_PRIORITIES],

    /// Currently executing process — not in any queue.
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
            running: None,
            remaining_ticks: 0,
            global_ticks: 0,
            next_pid: 1,
        }
    }

    // ====================================================================
    // Time slice calculation
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

    /// Add a process to its effective priority queue.
    pub fn add_process(&mut self, mut process: Box<Process>) {
        // New processes start with effective = base
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

    pub fn iter_all(&self) -> impl Iterator<Item = &Process> + '_ {
        self.running.as_deref().into_iter().chain(
            self.run_queues
                .iter()
                .flat_map(|q| q.iter())
                .map(|boxed| boxed.as_ref()),
        )
    }

    // ====================================================================
    // Timer tick — called EVERY timer interrupt
    // ====================================================================

    /// Called on every timer tick.  Returns true if the running process's
    /// time slice is exhausted and a context switch should happen.
    pub fn tick(&mut self) -> bool {
        self.global_ticks = self.global_ticks.wrapping_add(1);

        // Periodic aging: boost starving processes
        if self.global_ticks % AGING_EPOCH == 0 {
            self.age_processes();
        }

        // Decrement running process's slice
        if self.remaining_ticks > 0 {
            self.remaining_ticks -= 1;
        }

        // Slice exhausted → need context switch
        self.remaining_ticks == 0
    }

    // ====================================================================
    // Priority aging (anti-starvation)
    // ====================================================================

    /// Boost effective_priority of all waiting processes toward their
    /// base_priority.  This ensures that low-priority processes that have
    /// been waiting a long time eventually get scheduled.
    fn age_processes(&mut self) {
        // We need to move processes between queues if their effective
        // priority changes.  To avoid borrow issues, collect first.
        for pri in 0..NUM_PRIORITIES {
            let mut i = 0;
            while i < self.run_queues[pri].len() {
                let proc = &self.run_queues[pri][i];

                // Skip idle (always stays at 0)
                if proc.pid.0 == 0 {
                    i += 1;
                    continue;
                }

                // Only boost if effective < base (i.e., it was decayed)
                if proc.effective_priority < proc.priority {
                    let mut proc = self.run_queues[pri].remove(i).unwrap();
                    proc.effective_priority = (proc.effective_priority + 1).min(proc.priority);
                    let new_pri = (proc.effective_priority as usize).min(NUM_PRIORITIES - 1);
                    self.run_queues[new_pri].push_back(proc);
                    // Don't increment i — the next element shifted into position i
                } else {
                    i += 1;
                }
            }
        }
    }

    // ====================================================================
    // Context switch
    // ====================================================================

    /// Save current process, find next, activate, return new TrapFrame.
    ///
    /// Called from `timer_preempt_handler` when `tick()` returns true
    /// (slice exhausted).
    pub fn switch_to_next(&mut self, current_tf: *const TrapFrame) -> *const TrapFrame {
        // ── 1. Save current process ───────────────────────────────────

        if let Some(mut proc) = self.running.take() {
            unsafe {
                *proc.trapframe = *current_tf;
            }

            if proc.state == ProcessState::Running {
                proc.state = ProcessState::Ready;

                // Decay effective priority (unless idle or already at minimum)
                if proc.pid.0 != 0 && proc.effective_priority > MIN_EFFECTIVE_PRIORITY {
                    proc.effective_priority -= 1;
                }
            }

            let pri = (proc.effective_priority as usize).min(NUM_PRIORITIES - 1);
            self.run_queues[pri].push_back(proc);
        }

        // ── 2. Find highest effective-priority Ready process ──────────

        for priority in (0..NUM_PRIORITIES).rev() {
            let queue = &mut self.run_queues[priority];
            let len = queue.len();

            for _ in 0..len {
                if let Some(mut proc) = queue.pop_front() {
                    if proc.state == ProcessState::Ready {
                        proc.state = ProcessState::Running;

                        unsafe {
                            proc.address_space.activate();
                        }
                        super::tss::set_kernel_stack(proc.kernel_stack);

                        // Set time slice for this process
                        self.remaining_ticks = Self::quantum_for(proc.effective_priority);

                        let tf_ptr = &*proc.trapframe as *const TrapFrame;
                        self.running = Some(proc);
                        return tf_ptr;
                    }

                    // Not Ready (Zombie/Blocked) — put back at tail
                    queue.push_back(proc);
                }
            }
        }

        // ── 3. Nothing Ready — stay on current_tf (should not happen) ─
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

        // Find highest-priority non-idle Ready process
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
// Public API for demand paging + syscalls
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