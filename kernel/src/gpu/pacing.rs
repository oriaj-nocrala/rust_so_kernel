// kernel/src/gpu/pacing.rs
//
// The static behind `nvgpu::pacing`: where a compositor frame's time goes (submissions to the GPU's channels and when they are seen done, `PRESENT`
// entering and leaving and how long after the previous vblank it came, vblanks). An instrument: `/proc/kdebug` `gpu_pacing:` (statistics) and
// `gpu_pacing_trace:` (the latest events), `/dev/dispctl` `trace reset` to start a measurement from zero. Taken with interrupts off (`IrqMutex`);
// the vblank handler uses `try_with` and loses an event rather than wait. Lock order: `HW` before this (it takes nothing else).

use alloc::string::String;

use diag::IrqMutex;
use nvgpu::pacing::Pacing;

use crate::allocator::KernelIrq;

static PACING: IrqMutex<Pacing, KernelIrq> = IrqMutex::new(Pacing::new());

/// Events shown by `gpu_pacing_trace:`.
const TAIL: usize = 60;

fn now() -> u64 {
    crate::time::ktime_get()
}

/// Fence `seq` was queued on channel `chan`.
pub fn submit(chan: usize, seq: u64) {
    PACING.with(|p| p.submit(now(), chan, seq));
}

/// A look at `chan`'s fence found everything up to `upto` done.
pub fn done(chan: usize, upto: u64) {
    PACING.with(|p| p.done(now(), chan, upto));
}

pub fn present_begin() {
    let last = super::vblank::last_ns();
    PACING.with(|p| p.present_begin(now(), last));
}

pub fn present_end() {
    PACING.with(|p| p.present_end(now()));
}

/// From the vblank handler (ISR).
pub fn vblank(seq: u64) {
    let t = now();
    let _ = PACING.try_with(|p| p.vblank(t, seq));
}

pub fn reset() {
    PACING.with(|p| p.reset());
}

/// `/proc/kdebug` lines (empty until something was recorded).
pub fn render_kdebug() -> String {
    PACING.with(|p| if p.events().is_empty() { String::new() } else { p.render(TAIL) })
}
