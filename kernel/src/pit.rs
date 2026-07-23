// kernel/src/pit.rs
//
// Programmable Interval Timer (channel 0, mode 2 rate generator) — thin
// adapter around `hal::pit`'s `PortIo`-generic register protocol. The
// divisor arithmetic and its two real guard cases (zero frequency, a
// frequency below the hardware's ~18.2 Hz floor) now live in `hal::pit`,
// unit tested on the host — see `hal/src/pit.rs`.
//
// Not folded into the `crate::hal::Driver` registry (`crate::hal::run_all`):
// `Driver::init()` takes no arguments, but the PIT's rate is caller-supplied
// (`init_hardware_interrupts` always passes 100 Hz today, matching
// `cpu::tsc`'s `PIT_HZ` assumption) — forcing this through a zero-arg
// lifecycle would mean hardcoding 100 Hz *inside* this adapter, hiding the
// real call site's intent instead of just keeping the direct call.

use crate::hal::X86PortIo;

/// Programs the PIT's channel 0 to `frequency` Hz (mode 2, rate generator).
/// Best-effort: an invalid frequency (`0`, or below the hardware's ~18.2 Hz
/// floor) is logged and left un-programmed rather than panicking — the
/// original computed `1193182 / frequency` inline and would panic (division
/// by zero) on `frequency == 0`, and would silently truncate into a wildly
/// wrong rate below the floor. See `hal::pit::PitError`.
pub fn init(frequency: u32) {
    let pit = hal::pit::Pit::new(X86PortIo);
    if let Err(e) = pit.set_rate(frequency) {
        crate::serial_println!("pit: failed to program {} Hz: {:?}", frequency, e);
    }
}
