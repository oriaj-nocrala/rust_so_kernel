// kernel/src/process/scheduler.rs

use alloc::collections::VecDeque;
use alloc::boxed::Box;
use spin::Mutex;
use x86_64::VirtAddr;
use crate::process::PrivilegeLevel;

use super::{Process, Pid, ProcessState};
use super::context::Context;

pub static SCHEDULER: Mutex<Scheduler> = Mutex::new(Scheduler::new());

pub struct Scheduler {
    pub processes: VecDeque<Box<Process>>,
    pub current: Option<Pid>,
    next_pid: usize,
}

impl Scheduler {
    pub const fn new() -> Self {
        Self {
            processes: VecDeque::new(),
            current: None,
            next_pid: 1,
        }
    }

    /// Crea un nuevo PID
    pub fn allocate_pid(&mut self) -> Pid {
        let pid = Pid(self.next_pid);
        self.next_pid += 1;
        pid
    }

    /// Agrega un proceso a la cola de ready
    pub fn add_process(&mut self, mut process: Box<Process>) {
        process.state = ProcessState::Ready;
        crate::serial_println!("Scheduler: Added process PID {}", process.pid.0);
        self.processes.push_back(process);
    }

    /// Obtiene el proceso actual
    pub fn current_pid(&self) -> Option<Pid> {
        self.current
    }

    /// Scheduler round-robin: elige el siguiente proceso
    pub fn schedule(&mut self) -> Option<&mut Process> {
        if self.processes.is_empty() {
            return None;
        }

        // Mover el proceso actual al final (si existe)
        if let Some(current_pid) = self.current {
            if let Some(idx) = self.processes.iter().position(|p| p.pid == current_pid) {
                if let Some(mut proc) = self.processes.remove(idx) {
                    if proc.state == ProcessState::Running {
                        proc.state = ProcessState::Ready;
                    }
                    self.processes.push_back(proc);
                }
            }
        }

        // Tomar el siguiente proceso ready
        while let Some(mut proc) = self.processes.pop_front() {
            if proc.state == ProcessState::Ready {
                proc.state = ProcessState::Running;
                self.current = Some(proc.pid);
                
                let _pid = proc.pid;
                self.processes.push_back(proc);
                
                // Retornar referencia mutable
                return self.processes.back_mut().map(|b| &mut **b);
            } else {
                self.processes.push_back(proc);
            }
        }

        None
    }

    /// ✅ Hace context switch y retorna (proceso_anterior, proceso_siguiente)
    /// Retorna None si no hay cambio de contexto necesario
    pub fn switch_to_next(&mut self) -> Option<(*mut Context, *const Context)> {
        if self.processes.is_empty() {
            return None;
        }

        let old_pid = self.current;

        // Mover proceso actual al final si está running
        if let Some(current_pid) = self.current {
            if let Some(idx) = self.processes.iter().position(|p| p.pid == current_pid) {
                if let Some(mut proc) = self.processes.remove(idx) {
                    if proc.state == ProcessState::Running {
                        proc.state = ProcessState::Ready;
                    }
                    self.processes.push_back(proc);
                }
            }
        }

        // Buscar siguiente proceso ready
        let mut rotations = 0;
        let len = self.processes.len();
        
        while rotations < len {
            if let Some(proc) = self.processes.front_mut() {
                if proc.state == ProcessState::Ready {
                    // Encontramos uno ready
                    proc.state = ProcessState::Running;
                    let next_pid = proc.pid;
                    self.current = Some(next_pid);

                    // Si es el mismo proceso, no hacer switch
                    if old_pid == Some(next_pid) {
                        return None;
                    }

                    // Obtener puntero al nuevo contexto
                    let new_ctx = &proc.context as *const Context;

                    // Buscar el proceso anterior
                    if let Some(old_pid) = old_pid {
                        // Buscar en el resto de la cola
                        if let Some(old_proc) = self.processes.iter_mut()
                            .find(|p| p.pid == old_pid) 
                        {
                            let old_ctx = &mut old_proc.context as *mut Context;
                            
                            crate::serial_println!(
                                "Context switch: {} -> {}",
                                old_pid.0,
                                next_pid.0
                            );
                            
                            return Some((old_ctx, new_ctx));
                        }
                    }

                    // Primera vez o no encontramos el anterior
                    return None;
                }
            }
            
            // Rotar y seguir buscando
            if let Some(proc) = self.processes.pop_front() {
                self.processes.push_back(proc);
            }
            rotations += 1;
        }

        // No hay procesos ready
        None
    }

    /// Marca el proceso actual como bloqueado (sleeping)
    pub fn block_current(&mut self) {
        if let Some(current_pid) = self.current {
            if let Some(proc) = self.processes.iter_mut().find(|p| p.pid == current_pid) {
                proc.state = ProcessState::Sleeping;  // ✅ FIX: Blocked -> Sleeping
            }
        }
    }

    /// Desbloquea un proceso
    pub fn unblock(&mut self, pid: Pid) {
        if let Some(proc) = self.processes.iter_mut().find(|p| p.pid == pid) {
            if proc.state == ProcessState::Sleeping {  // ✅ FIX: Blocked -> Sleeping
                proc.state = ProcessState::Ready;
            }
        }
    }

    // ❌ REMOVIDO: run_process() - Ya no se usa con la nueva arquitectura
    // El flow correcto es:
    // 1. switch_context() salta a forkret (primera vez)
    // 2. forkret llama a trapret
    // 3. trapret hace IRETQ a user mode
}