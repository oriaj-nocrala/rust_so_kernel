// kernel/src/cpu/mod.rs
// CPU topology — today single-CPU, tomorrow SMP.

pub mod freq;
mod init;
pub mod percpu;
pub mod tsc;

pub use init::{init_this_cpu, render as render_init};
#[cfg(test)]
pub use init::verify as verify_this_cpu;

/// Maximum number of CPUs this kernel supports. The target machine (a
/// 5900X) has 24 logical CPUs; each costs ~40 KiB of static stacks
/// (`process::tss`) and two GDT entries.
pub const MAX_CPUS: usize = 32;

/// Returns the current CPU's ID (0-based), from the task register — see
/// `percpu`'s module comment for why not `gs:`. Each CPU loads its own TSS
/// slot in `init_this_cpu`; before that (early BSP boot) it reads 0.
pub use percpu::cpu_id;
