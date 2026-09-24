// kernel/src/autorun.rs
//
// Unattended test runs on bare metal (docs/metal/autonomous-loop-plan.md,
// phase 3). The host drops a job at `/mnt/autorun/job` on the stick's data
// partition and `efibootmgr --bootnext`s into it; PID 1
// (`userspace/src/bin/shell.rs`) runs the job and `reboot(2)`s back to
// Linux, which reads the result off the log partition.
//
// The kernel's half is one decision: with nobody at the keyboard, a panic
// must reset the machine rather than `hlt` forever, or every crashed run
// strands the machine in constanos until someone presses reset. That is
// decided here, once, right after `/mnt` is mounted — early enough to cover
// every panic from process creation on, and without trusting userspace to
// report it (a job that crashes PID 1 must still come back).
//
// The job is never removed from here: `/mnt` is read-only when it comes from
// the stick. Removing it is the host's job, after it has collected the run.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::serial_println;

/// Where the host puts the job. Must match `shell.rs` and
/// `scripts/metal-run.sh`.
pub const JOB_PATH: &str = "/mnt/autorun/job";

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Checks for a job. Call once, after `fs::init()` has mounted `/mnt`.
pub fn detect() {
    if crate::fs::vfs::resolve(JOB_PATH).is_ok() {
        ENABLED.store(true, Ordering::Relaxed);
        serial_println!("autorun: {} present — a kernel panic will reset the machine", JOB_PATH);
    }
}

/// Whether this boot is an unattended run. Lock-free: read from the panic
/// handler.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}
