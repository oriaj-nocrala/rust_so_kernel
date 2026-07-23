// kernel/src/rtc.rs
//
// CMOS real-time clock (MC146818-compatible) — thin adapter around
// `hal::rtc`'s `PortIo`-generic protocol. All decode/protocol logic (BCD vs.
// binary, 12- vs. 24-hour, the torn-read retry, `days_from_civil`) now lives
// in `hal::rtc`, unit tested on the host — see `hal/src/rtc.rs`.
//
// Read-once-at-boot, called directly from `time::init()` — not folded into
// the `crate::hal::Driver` registry (`crate::hal::run_all`): there's no
// persistent hardware state to own after the single read (no periodic RTC
// IRQ — see `hal::rtc`'s module doc), so a `Driver`-lifecycle wrapper would
// add structure with nothing left to hold onto afterward.

use crate::hal::X86PortIo;

/// Reads the CMOS RTC and converts it to a Unix epoch timestamp (seconds).
/// Best-effort: `None` if the hardware never settles — see
/// `hal::rtc::Rtc::read_unix_time`.
pub fn read_unix_time() -> Option<u64> {
    let rtc = hal::rtc::Rtc::new(X86PortIo);
    rtc.read_unix_time()
}
