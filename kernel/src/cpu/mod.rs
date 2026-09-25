// kernel/src/cpu/mod.rs
// CPU topology — today single-CPU, tomorrow SMP.

mod init;
pub mod percpu;
pub mod tsc;

pub use init::{init_this_cpu, render as render_init};
#[cfg(test)]
pub use init::verify as verify_this_cpu;

/// Maximum number of CPUs this kernel supports.
pub const MAX_CPUS: usize = 8;

/// Returns the current CPU's ID (0-based), from the task register — see
/// `percpu`'s module comment for why not `gs:`. Each CPU loads its own TSS
/// slot in `init_this_cpu`; before that (early BSP boot) it reads 0.
pub use percpu::cpu_id;
