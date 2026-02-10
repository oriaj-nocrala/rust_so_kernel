// kernel/src/process/scheduler.rs
// ✅ SCHEDULER COMPLETO: Idle especial + Prioridades (como Linux)

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
    
    /// ✅ Cambiar al siguiente proceso
    /// 
    /// Algoritmo:
    /// 1. Buscar proceso con MAYOR prioridad que esté Ready
    /// 2. Idle (PID 0) solo si NO hay otros procesos
    /// 3. Round-robin entre procesos de misma prioridad
    pub fn switch_to_next(&mut self, current_tf: *const TrapFrame) -> *const TrapFrame {
        // Guardar TrapFrame del proceso actual
        if let Some(current_pid) = self.current {
            if let Some(proc) = self.processes.iter_mut().find(|p| p.pid == current_pid) {
                unsafe {
                    *proc.trapframe = *current_tf;
                }
                if proc.pid.0 != 0 {
                    proc.state = ProcessState::Ready;
                }
            }
        }
        
        // ============ 1. Buscar proceso con MAYOR prioridad ============
        let mut best_priority = 0;
        let mut found_any = false;
        
        // Primera pasada: Encontrar la mayor prioridad disponible
        for proc in self.processes.iter() {
            if proc.pid.0 != 0 && proc.state == ProcessState::Ready {
                if proc.priority > best_priority {
                    best_priority = proc.priority;
                }
                found_any = true;
            }
        }
        
        // Segunda pasada: Seleccionar el PRIMER proceso con esa prioridad
        if found_any {
            let len = self.processes.len();
            for _ in 0..len {
                if let Some(mut proc) = self.processes.pop_front() {
                    if proc.pid.0 != 0 
                       && proc.state == ProcessState::Ready 
                       && proc.priority == best_priority {
                        // ✅ Encontrado!
                        proc.state = ProcessState::Running;
                        let pid = proc.pid;
                        
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
        
        // ============ 2. No hay procesos reales → Ejecutar IDLE ============
        if let Some(idle) = self.processes.iter_mut().find(|p| p.pid.0 == 0) {
            idle.state = ProcessState::Running;
            self.current = Some(idle.pid);
            
            let tf_ptr = &*idle.trapframe as *const TrapFrame;
            return tf_ptr;
        }
        
        current_tf
    }
}