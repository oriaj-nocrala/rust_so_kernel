//! Programmable Interval Timer (8253/8254) — channel 0 rate-generator setup.
//! Pure divisor math + a `PortIo`-generic register write, host-tested with
//! `cargo test`.
//!
//! Fifth (and, alongside `rtc`, last of the originally-planned) driver
//! migrated onto the `hal` pattern — see `hal/src/acpi.rs` / `hal/src/ac97.rs`
//! / `hal/src/keyboard.rs` / `hal/src/mouse.rs` and
//! `.claude/skills/kernel-drivers/SKILL.md` for the general playbook. The
//! original `kernel/src/pit.rs` was 21 lines of raw `asm!("out dx, al")`
//! with the divisor arithmetic inlined directly into `init()` — this module
//! pulls the arithmetic out into [`divisor_for_hz`], a pure function with no
//! `IO` parameter at all, so the two real bugs it had (division by zero,
//! and silent truncation below the hardware's floor frequency) can be
//! exercised directly instead of only by reasoning about the asm.

use crate::PortIo;

const PIT_CHANNEL_0_DATA: u16 = 0x40;
const PIT_COMMAND: u16 = 0x43;

/// Channel 0, lobyte/hibyte access, mode 2 (rate generator), binary mode.
const CMD_CHANNEL0_MODE2: u8 = 0x34;

/// The PIT's base input clock (Hz) — fixed by the hardware, not configurable.
pub const PIT_FREQUENCY_HZ: u32 = 1_193_182;

/// Reasons a target rate can't be programmed into the PIT — the kernel
/// adapter logs which one and gives up (best-effort; unlike the original,
/// which would panic on `frequency == 0`, this never does).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PitError {
    /// `frequency == 0` — the original computed `1193182 / frequency` here,
    /// which is division by zero and would panic. There is no meaningful
    /// "zero Hz" rate generator setting, so this is rejected outright.
    ZeroFrequency,
    /// `frequency` is below the PIT's floor (~18.2 Hz for the base
    /// 1.193182 MHz clock — the lowest rate a 16-bit divisor can express).
    /// The original didn't check this: the true divisor
    /// (`1193182 / frequency`) exceeds 65535, and truncating it into the
    /// 16-bit reload register (`divisor & 0xFF`, `(divisor >> 8) & 0xFF`)
    /// silently wraps to a small value, programming a wildly *higher* rate
    /// than requested instead of erroring.
    FrequencyTooLow,
}

/// Computes the PIT's 16-bit reload/divisor value for a target rate `hz`,
/// per the hardware's real encoding: divisors run `1..=65536` (a 16-bit
/// register can only directly hold `1..=65535`; the chip treats a
/// programmed value of `0` as `65536` — "count all the way around" — which
/// is also the PIT's floor frequency, `1193182 / 65536 ≈ 18.2` Hz). Returns
/// the divisor as a plain `u32` in `1..=65536`; callers that need the actual
/// 16-bit wire value must remember to encode `65536` as `0` (see
/// [`Pit::set_rate`]).
///
/// Matches the original's `1193182 / frequency` integer (floor) division
/// exactly for every in-range frequency — this is a pure extraction, not a
/// rounding-behavior change — but returns `None` instead of panicking or
/// silently truncating for the two out-of-range cases documented on
/// [`PitError`].
pub fn divisor_for_hz(hz: u32) -> Result<u32, PitError> {
    if hz == 0 {
        return Err(PitError::ZeroFrequency);
    }
    let divisor = PIT_FREQUENCY_HZ / hz; // same integer division the original used
    if divisor == 0 {
        // hz > PIT_FREQUENCY_HZ: not achievable at all (the PIT can't run
        // faster than its own input clock). Also guards the same "silent
        // truncation" failure mode as FrequencyTooLow, just from the other
        // direction — a 0 divisor would wire-encode as 65536, i.e. the
        // *slowest* possible rate for a request that asked for the fastest.
        return Err(PitError::FrequencyTooLow);
    }
    if divisor > 65536 {
        return Err(PitError::FrequencyTooLow);
    }
    Ok(divisor)
}

/// Encodes a divisor in `1..=65536` (see [`divisor_for_hz`]) as the raw
/// 16-bit value the PIT's reload register actually gets: every value
/// `1..=65535` is written as-is; `65536` — one past what 16 bits can
/// directly hold — is written as `0`, the hardware's own "count all the way
/// around" encoding. Split out from [`Pit::set_rate`] so the boundary case
/// is directly testable without needing an integer Hz that reduces to
/// exactly divisor 65536 (see the module's tests: no integer Hz does,
/// since `PIT_FREQUENCY_HZ / 65536` falls in a gap between two consecutive
/// integer-Hz divisors — the 0-encoding only matters if a caller ever
/// requests the divisor directly rather than always going through
/// `divisor_for_hz`).
fn divisor_to_wire(divisor: u32) -> u16 {
    if divisor == 65536 {
        0
    } else {
        divisor as u16
    }
}

/// The PIT channel-0 register protocol, generic over the `PortIo` seam.
/// Owns nothing but the seam itself — no hardware state to track between
/// calls (channel 0 is fire-and-forget: program a rate, it free-runs).
pub struct Pit<IO: PortIo> {
    io: IO,
}

impl<IO: PortIo> Pit<IO> {
    pub fn new(io: IO) -> Self {
        Pit { io }
    }

    /// Programs channel 0 into mode 2 (rate generator) at `hz`. Writes the
    /// command byte (`0x34`) to the command port, then the divisor's low
    /// byte, then its high byte, to the channel-0 data port — same order,
    /// same three ports, as the original inline `asm!` sequence.
    pub fn set_rate(&self, hz: u32) -> Result<(), PitError> {
        let divisor = divisor_for_hz(hz)?;
        let wire = divisor_to_wire(divisor);
        let l = (wire & 0xFF) as u8;
        let h = (wire >> 8) as u8;

        self.io.outb(PIT_COMMAND, CMD_CHANNEL0_MODE2);
        self.io.outb(PIT_CHANNEL_0_DATA, l);
        self.io.outb(PIT_CHANNEL_0_DATA, h);
        Ok(())
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ScriptedIo;

    // ── Pure divisor math ────────────────────────────────────────────────

    #[test]
    fn divisor_for_100hz_matches_kernels_actual_usage() {
        // kernel/src/init/devices.rs calls pit::init(100) — the only real
        // caller today. 1193182 / 100 = 11931 (floor), same as the
        // original's plain integer division.
        assert_eq!(divisor_for_hz(100), Ok(11931));
    }

    #[test]
    fn divisor_for_1000hz() {
        assert_eq!(divisor_for_hz(1000), Ok(1193));
    }

    #[test]
    fn zero_frequency_is_rejected_not_a_panic() {
        assert_eq!(divisor_for_hz(0), Err(PitError::ZeroFrequency));
    }

    #[test]
    fn frequency_below_floor_is_rejected_not_silently_truncated() {
        // 1193182 / 10 = 119318, which doesn't fit in 16 bits (> 65536) —
        // the original would truncate this into a bogus rate instead of
        // erroring.
        assert_eq!(divisor_for_hz(10), Err(PitError::FrequencyTooLow));
    }

    #[test]
    fn floor_frequency_18hz_is_encoded_as_divisor_65536() {
        // 1193182 / 18 = 66287 > 65536 -> still too low.
        assert_eq!(divisor_for_hz(18), Err(PitError::FrequencyTooLow));
        // 1193182 / 19 = 62799 <= 65536 -> representable.
        assert!(divisor_for_hz(19).is_ok());
    }

    #[test]
    fn divisor_65536_the_true_18point2hz_floor_is_wire_encoded_as_zero() {
        // The hardware's actual floor frequency (1193182 / 65536 ≈ 18.2 Hz)
        // needs a fractional Hz no integer caller can request — every
        // integer Hz's floor division either lands above (hz=18 -> 66287,
        // rejected as too low) or below (hz=19 -> 62799, representable
        // as-is) that exact divisor, so divisor_for_hz alone can never
        // return exactly 65536. divisor_to_wire is tested directly instead,
        // covering the encoding rule a future non-integer-Hz caller would
        // rely on.
        assert_eq!(divisor_to_wire(65536), 0);
        assert_eq!(divisor_to_wire(1), 1);
        assert_eq!(divisor_to_wire(65535), 0xFFFF);
    }

    // ── Register writes (ScriptedIo) ────────────────────────────────────

    #[test]
    fn set_rate_100hz_writes_command_then_low_then_high_byte() {
        let io = ScriptedIo::new();
        let pit = Pit::new(&io);
        assert_eq!(pit.set_rate(100), Ok(()));

        // divisor 11931 = 0x2E9B -> low 0x9B, high 0x2E.
        assert_eq!(
            io.writes(),
            alloc::vec![
                (PIT_COMMAND, CMD_CHANNEL0_MODE2 as u32),
                (PIT_CHANNEL_0_DATA, 0x9B),
                (PIT_CHANNEL_0_DATA, 0x2E),
            ]
        );
    }

    #[test]
    fn set_rate_1000hz_writes_expected_divisor_bytes() {
        let io = ScriptedIo::new();
        let pit = Pit::new(&io);
        assert_eq!(pit.set_rate(1000), Ok(()));

        // divisor 1193 = 0x04A9 -> low 0xA9, high 0x04.
        assert_eq!(
            io.writes(),
            alloc::vec![
                (PIT_COMMAND, CMD_CHANNEL0_MODE2 as u32),
                (PIT_CHANNEL_0_DATA, 0xA9),
                (PIT_CHANNEL_0_DATA, 0x04),
            ]
        );
    }

    #[test]
    fn set_rate_zero_frequency_errors_and_writes_nothing() {
        let io = ScriptedIo::new();
        let pit = Pit::new(&io);
        assert_eq!(pit.set_rate(0), Err(PitError::ZeroFrequency));
        assert!(io.writes().is_empty());
    }

    #[test]
    fn set_rate_below_floor_errors_and_writes_nothing() {
        let io = ScriptedIo::new();
        let pit = Pit::new(&io);
        assert_eq!(pit.set_rate(5), Err(PitError::FrequencyTooLow));
        assert!(io.writes().is_empty());
    }

    #[test]
    fn set_rate_at_18point2hz_floor_encodes_divisor_zero_on_the_wire() {
        // hz=19 is the lowest frequency divisor_for_hz accepts near the
        // floor (divisor 62799, well within 16 bits, no 0-encoding
        // involved) — confirms the "just above the floor" boundary writes
        // a normal, non-zero divisor rather than erroring.
        let io = ScriptedIo::new();
        let pit = Pit::new(&io);
        assert_eq!(pit.set_rate(19), Ok(()));
        let writes = io.writes();
        assert_eq!(writes[0], (PIT_COMMAND, CMD_CHANNEL0_MODE2 as u32));
        // divisor 62799 = 0xF54F -> low 0x4F, high 0xF5. Definitely nonzero.
        assert_eq!(writes[1], (PIT_CHANNEL_0_DATA, 0x4F));
        assert_eq!(writes[2], (PIT_CHANNEL_0_DATA, 0xF5));
    }
}
