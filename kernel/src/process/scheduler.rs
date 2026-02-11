// kernel/src/process/scheduler.rs
// ✅ SCHEDULER CON CONTEXT SWITCH DE PAGE TABLES

use alloc::{boxed::Box, collections::VecDeque};
use spin::Mutex;
use super::{Process, Pid, ProcessState, TrapFrame};

pub static SCHEDULER: Mutex<Scheduler> = Mutex::new(Scheduler::new());

pub struct Scheduler {
    pub processes: VecDeque<Box<Process>>,
    pub current: Option<Pid>,
    next_pid: usize,
}

impl Scheduler {
    pub const fn new() -> Self {
        Scheduler {
            processes: VecDeque::new(),
            current: None,
            next_pid: 1,
        }
    }
    
    pub fn allocate_pid(&mut self) -> Pid {
        let pid = Pid(self.next_pid);
        self.next_pid += 1;
        pid
    }
    
    pub fn add_process(&mut self, process: Box<Process>) {
        crate::serial_println!("Scheduler: Added process PID {} (priority {})", 
            process.pid.0, process.priority);
        self.processes.push_back(process);
    }
    
    /// Context switch with page table (CR3) switching.
    ///
    /// 1. Save current process state
    /// 2. Find highest-priority Ready process
    /// 3. Switch page table (CR3) — skipped if already using the right one
    /// 4. Update TSS kernel stack
    /// 5. Return new trapframe pointer
    pub fn switch_to_next(&mut self, current_tf: *const TrapFrame) -> *const TrapFrame {
        // ============ 1. Save current process state ============
        if let Some(current_pid) = self.current {
            if let Some(proc) = self.processes.iter_mut().find(|p| p.pid == current_pid) {
                unsafe {
                    // Copy trapframe from the interrupt stack (always accessible
                    // via kernel entries 256-511, regardless of which CR3 is active)
                    *proc.trapframe = *current_tf;
                }
                if proc.pid.0 != 0 {
                    proc.state = ProcessState::Ready;
                }
            }
        }
        
        // ============ 2. Find highest-priority Ready process ============
        let mut best_priority = 0;
        let mut found_any = false;
        
        for proc in self.processes.iter() {
            if proc.pid.0 != 0 && proc.state == ProcessState::Ready {
                if proc.priority > best_priority {
                    best_priority = proc.priority;
                }
                found_any = true;
            }
        }
        
        // ============ 3. Select and switch to next process ============
        if found_any {
            let len = self.processes.len();
            for _ in 0..len {
                if let Some(mut proc) = self.processes.pop_front() {
                    if proc.pid.0 != 0 
                       && proc.state == ProcessState::Ready 
                       && proc.priority == best_priority {
                        proc.state = ProcessState::Running;
                        let pid = proc.pid;
                        
                        // ✅ Switch page table (CR3).
                        // activate() is a no-op if CR3 already matches,
                        // so switching between kernel processes (which all
                        // share the kernel page table) is free.
                        unsafe {
                            proc.page_table.activate();
                        }
                        
                        // Update TSS kernel stack for Ring 3 → Ring 0 transitions
                        super::tss::set_kernel_stack(proc.kernel_stack);
                        
                        let tf_ptr = &*proc.trapframe as *const TrapFrame;
                        
                        self.current = Some(pid);
                        self.processes.push_back(proc);
                        
                        return tf_ptr;
                    } else {
                        self.processes.push_back(proc);
                    }
                }
            }
        }
        
        // ============ 4. No ready processes → run IDLE ============
        if let Some(idle) = self.processes.iter_mut().find(|p| p.pid.0 == 0) {
            idle.state = ProcessState::Running;
            self.current = Some(idle.pid);
            
            // Activate idle's page table (= kernel page table, likely no-op)
            unsafe {
                idle.page_table.activate();
            }
            
            let tf_ptr = &*idle.trapframe as *const TrapFrame;
            return tf_ptr;
        }
        
        // Fallback: stay on current process
        current_tf
    }
}

// ============================================================================
// Public API for demand paging
// ============================================================================

/// Returns the PID of the currently running process as a usize.
/// Called by the page fault handler to look up VMAs.
pub fn current_pid() -> Option<usize> {
    let scheduler = SCHEDULER.lock();
    scheduler.current.map(|pid| pid.0)
}