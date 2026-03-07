// kernel/src/cpu/mod.rs
// CPU topology — today single-CPU, tomorrow SMP.

pub mod tsc;

/// Maximum number of CPUs this kernel supports.
pub const MAX_CPUS: usize = 8;

/// Returns the current CPU's ID (0-based).
/// Single-CPU: always 0.
/// SMP future: read from GS-base per-CPU variable or LAPIC ID.
#[inline(always)]
pub fn cpu_id() -> usize {
    0
}
