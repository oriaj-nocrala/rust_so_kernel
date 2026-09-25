// kernel/src/cpu/mod.rs
// CPU topology — today single-CPU, tomorrow SMP.

pub mod percpu;
pub mod tsc;

/// Maximum number of CPUs this kernel supports.
pub const MAX_CPUS: usize = 8;

/// Returns the current CPU's ID (0-based), from the task register — see
/// `percpu`'s module comment for why not `gs:`. Always 0 until stage 3 gives
/// each CPU its own TSS slot.
pub use percpu::cpu_id;
