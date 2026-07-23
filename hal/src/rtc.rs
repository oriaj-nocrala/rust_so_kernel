//! CMOS real-time clock (MC146818-compatible, present on every PC/QEMU
//! target this kernel runs on) — pure protocol + decode, generic over the
//! `PortIo` seam so it's host-testable with `cargo test`.
//!
//! Sixth (and, alongside `pit`, last of the originally-planned) driver
//! migrated onto the `hal` pattern — see `hal/src/acpi.rs` / `hal/src/ac97.rs`
//! / `hal/src/keyboard.rs` / `hal/src/mouse.rs` and
//! `.claude/skills/kernel-drivers/SKILL.md` for the general playbook. Read
//! once at boot to get a real wall-clock Unix epoch, so time-since-boot (the
//! only thing `time::clocksource` tracks — there's no hardware tick source
//! for wall-clock time itself) can be turned into a real calendar time.
//! Read-once-at-boot is deliberate: this kernel has no periodic RTC
//! alarm/update IRQ wired up, and doesn't need one —
//! `time::now_unix_secs()` just adds elapsed monotonic uptime on top of the
//! single boot-time reading.
//!
//! [`days_from_civil`] needs no seam at all — it's pure integer date math —
//! so it's tested directly, no mock involved, exactly like
//! `hal::ac97::plan_fill`.

use crate::PortIo;

const CMOS_INDEX: u16 = 0x70;
const CMOS_DATA: u16 = 0x71;

const REG_SECONDS: u8 = 0x00;
const REG_MINUTES: u8 = 0x02;
const REG_HOURS: u8 = 0x04;
const REG_DAY: u8 = 0x07;
const REG_MONTH: u8 = 0x08;
const REG_YEAR: u8 = 0x09;
const REG_STATUS_A: u8 = 0x0A;
const REG_STATUS_B: u8 = 0x0B;

const STATUS_A_UPDATE_IN_PROGRESS: u8 = 1 << 7;
const STATUS_B_BINARY: u8 = 1 << 2; // set = values already binary, clear = BCD
const STATUS_B_24H: u8 = 1 << 1; // set = 24-hour mode, clear = 12-hour + PM bit in hour

/// Bounded polling for the update-in-progress flag and for a stable
/// snapshot, same "never hang boot" convention as every other optional
/// hardware probe in this kernel (mouse, ac97, acpi). Two separate bounds,
/// matching the original nesting exactly: up to `MAX_ATTEMPTS` polls of the
/// UIP flag before giving up and reading anyway, and up to `MAX_ATTEMPTS`
/// snapshot-pairs before giving up on ever seeing two agree.
const MAX_ATTEMPTS: u32 = 1000;

fn bcd_to_bin(v: u8) -> u8 {
    (v & 0x0F) + (v >> 4) * 10
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct RawTime {
    second: u8,
    minute: u8,
    hour: u8,
    day: u8,
    month: u8,
    year: u8,
}

/// The CMOS RTC protocol, generic over the `PortIo` seam. Owns nothing but
/// the seam itself — the chip has no software-side state to track between
/// reads (unlike ac97's ring cursor or the keyboard's modifier latch).
pub struct Rtc<IO: PortIo> {
    io: IO,
}

impl<IO: PortIo> Rtc<IO> {
    pub fn new(io: IO) -> Self {
        Rtc { io }
    }

    fn cmos_read(&self, reg: u8) -> u8 {
        self.io.outb(CMOS_INDEX, reg);
        self.io.inb(CMOS_DATA)
    }

    fn update_in_progress(&self) -> bool {
        self.cmos_read(REG_STATUS_A) & STATUS_A_UPDATE_IN_PROGRESS != 0
    }

    /// One raw register snapshot. Doesn't wait for the update-in-progress
    /// flag itself — callers loop this until two consecutive reads agree,
    /// which is what actually rules out a read torn by the RTC's
    /// once-a-second update.
    fn read_raw_snapshot(&self) -> RawTime {
        RawTime {
            second: self.cmos_read(REG_SECONDS),
            minute: self.cmos_read(REG_MINUTES),
            hour: self.cmos_read(REG_HOURS),
            day: self.cmos_read(REG_DAY),
            month: self.cmos_read(REG_MONTH),
            year: self.cmos_read(REG_YEAR),
        }
    }

    /// Read a stable register snapshot: wait out any in-progress update,
    /// read, then confirm a second read (after waiting out the update flag
    /// again) agrees — the standard CMOS RTC technique for avoiding a
    /// snapshot torn across the ~244us window where the chip is updating
    /// its registers. Bounded (not an infinite loop): a machine whose CMOS
    /// is stuck in "update in progress" would otherwise hang boot forever
    /// over a best-effort wall-clock reading — see `read_unix_time`'s
    /// fallback.
    fn read_stable_snapshot(&self) -> Option<RawTime> {
        for _ in 0..MAX_ATTEMPTS {
            for _ in 0..MAX_ATTEMPTS {
                if !self.update_in_progress() {
                    break;
                }
            }
            let first = self.read_raw_snapshot();
            for _ in 0..MAX_ATTEMPTS {
                if !self.update_in_progress() {
                    break;
                }
            }
            let second = self.read_raw_snapshot();
            if first == second {
                return Some(first);
            }
        }
        None
    }

    /// Read the CMOS RTC and convert to a Unix epoch timestamp (seconds).
    /// Best-effort: returns `None` rather than hanging or panicking if the
    /// hardware never settles (see `read_stable_snapshot`) — same "best
    /// effort, log and move on" contract as `mouse::init`/`ac97::init`.
    ///
    /// Century: CMOS has no standardized register for it (the ACPI FADT
    /// century-register field is often absent/unreliable, including under
    /// QEMU's default RTC), so this assumes 2000-2099 — fine for any
    /// machine actually running this kernel today, same "single-user,
    /// don't over-engineer for a case nothing here will ever hit"
    /// pragmatism as the hostname/uid stubs in the mlibc port.
    pub fn read_unix_time(&self) -> Option<u64> {
        let raw = self.read_stable_snapshot()?;
        let status_b = self.cmos_read(REG_STATUS_B);

        // The PM flag lives in bit 7 of the *raw* hour register, and is
        // independent of the BCD-vs-binary encoding of the digits below it.
        // It therefore has to be captured here, before decoding: the BCD
        // path has to mask it off (`& 0x7F`) to decode the remaining digits
        // at all, and `bcd_to_bin` of anything `<= 0x7F` tops out at 85, so
        // the flag cannot survive into the decoded value. Reading it off the
        // decoded hour instead — which is what the original
        // `kernel/src/rtc.rs` did — meant `pm` was unconditionally false in
        // BCD mode, silently reporting every PM time as AM. See
        // `bcd_mode_twelve_hour_pm_flag_is_not_lost`.
        let pm = raw.hour & 0x80 != 0;

        let (second, minute, mut hour, day, month, year_2digit) = if status_b & STATUS_B_BINARY != 0 {
            (raw.second, raw.minute, raw.hour & 0x7F, raw.day, raw.month, raw.year)
        } else {
            (
                bcd_to_bin(raw.second),
                bcd_to_bin(raw.minute),
                bcd_to_bin(raw.hour & 0x7F),
                bcd_to_bin(raw.day),
                bcd_to_bin(raw.month),
                bcd_to_bin(raw.year),
            )
        };

        if status_b & STATUS_B_24H == 0 {
            if pm && hour != 12 {
                hour += 12;
            } else if !pm && hour == 12 {
                hour = 0;
            }
        }

        let year = 2000i64 + year_2digit as i64;
        let days = days_from_civil(year, month as u32, day as u32);
        let secs_of_day = hour as i64 * 3600 + minute as i64 * 60 + second as i64;
        Some((days * 86_400 + secs_of_day) as u64)
    }
}

/// Days since the Unix epoch (1970-01-01) for a given proleptic Gregorian
/// civil date — Howard Hinnant's `days_from_civil` algorithm: exact,
/// integer-only (no floating point, no external crate — this is a
/// `#![no_std]` bare-metal kernel), correct across the full Gregorian
/// leap-year rule (divisible by 4, not by 100, unless by 400). Needs no
/// seam at all — pure date math — so it's tested directly.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let m = m as i64;
    let d = d as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ScriptedIo;

    // ── days_from_civil (no seam) ────────────────────────────────────────

    #[test]
    fn epoch_itself_is_day_zero() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
    }

    #[test]
    fn day_before_epoch_is_negative_one() {
        assert_eq!(days_from_civil(1969, 12, 31), -1);
    }

    #[test]
    fn leap_day_2024_02_29() {
        // 2024 is a leap year (divisible by 4, not by 100).
        // Known: 2024-02-29 is day 19782 since epoch.
        assert_eq!(days_from_civil(2024, 2, 29), 19782);
        // The very next day rolls into March, one day later.
        assert_eq!(days_from_civil(2024, 3, 1), 19783);
    }

    #[test]
    fn century_boundary_2000_02_29_divisible_by_400_is_a_leap_day() {
        // 2000 is divisible by 400, so unlike a typical century year
        // (divisible by 100 but not 400) it IS a leap year and Feb 29
        // exists. Known: 2000-02-29 is day 11016 since epoch.
        assert_eq!(days_from_civil(2000, 2, 29), 11016);
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
    }

    #[test]
    fn ordinary_year_day_count_matches_known_value() {
        // 2023-01-01, a non-leap year immediately after 2022 — known day
        // count since epoch is 19358.
        assert_eq!(days_from_civil(2023, 1, 1), 19358);
    }

    // ── ScriptedIo-backed RTC decode ─────────────────────────────────────

    /// `ScriptedIo` is keyed by port, not by (port, register-index) — but
    /// `cmos_read` always does outb(CMOS_INDEX, reg) then inb(CMOS_DATA), so
    /// every register read pulls from the *same* CMOS_DATA FIFO in the
    /// exact order the driver issues them. This helper queues one full
    /// snapshot (STATUS_A "not updating" + the 6 time registers, twice,
    /// since read_stable_snapshot reads them twice to compare) followed by
    /// one STATUS_B read for read_unix_time.
    fn queue_stable_snapshot(io: &ScriptedIo, regs: &[u8; 6], status_b: u8) {
        let regs32: alloc::vec::Vec<u32> = regs.iter().map(|&b| b as u32).collect();
        // First pass: wait_uip's single STATUS_A read (not updating) + 6 regs.
        io.queue_read(CMOS_DATA, 0); // STATUS_A: not updating
        io.queue_reads(CMOS_DATA, &regs32);
        // Second pass: same again (identical snapshot -> stable).
        io.queue_read(CMOS_DATA, 0); // STATUS_A: not updating
        io.queue_reads(CMOS_DATA, &regs32);
        // Final STATUS_B read in read_unix_time.
        io.queue_read(CMOS_DATA, status_b as u32);
    }

    #[test]
    fn binary_mode_decodes_registers_directly() {
        let io = ScriptedIo::new();
        // second=30, minute=15, hour=14, day=23, month=7, year=26 (2026).
        let regs = [30u8, 15, 14, 23, 7, 26];
        queue_stable_snapshot(&io, &regs, STATUS_B_BINARY | STATUS_B_24H);

        let rtc = Rtc::new(&io);
        let ts = rtc.read_unix_time().expect("stable read");

        let expected_days = days_from_civil(2026, 7, 23);
        let expected = (expected_days * 86_400 + 14 * 3600 + 15 * 60 + 30) as u64;
        assert_eq!(ts, expected);
    }

    #[test]
    fn bcd_mode_decodes_registers_via_bcd_to_bin() {
        let io = ScriptedIo::new();
        // BCD-encoded: second=0x30 (30), minute=0x15 (15), hour=0x14 (14),
        // day=0x23 (23), month=0x07 (7), year=0x26 (26).
        let regs = [0x30u8, 0x15, 0x14, 0x23, 0x07, 0x26];
        // STATUS_B_BINARY clear -> BCD; 24h mode set.
        queue_stable_snapshot(&io, &regs, STATUS_B_24H);

        let rtc = Rtc::new(&io);
        let ts = rtc.read_unix_time().expect("stable read");

        let expected_days = days_from_civil(2026, 7, 23);
        let expected = (expected_days * 86_400 + 14 * 3600 + 15 * 60 + 30) as u64;
        assert_eq!(ts, expected);
    }

    #[test]
    fn twelve_hour_mode_am_hour_unchanged() {
        let io = ScriptedIo::new();
        // hour=9, PM bit (0x80) clear -> stays 9 AM.
        let regs = [0u8, 0, 9, 1, 1, 26];
        queue_stable_snapshot(&io, &regs, STATUS_B_BINARY); // 12h mode (24H bit clear)

        let rtc = Rtc::new(&io);
        let ts = rtc.read_unix_time().expect("stable read");
        let expected_days = days_from_civil(2026, 1, 1);
        assert_eq!(ts, (expected_days * 86_400 + 9 * 3600) as u64);
    }

    #[test]
    fn twelve_hour_mode_pm_hour_adds_twelve() {
        let io = ScriptedIo::new();
        // hour = 9 | 0x80 (PM) -> 21:00.
        let regs = [0u8, 0, 9 | 0x80, 1, 1, 26];
        queue_stable_snapshot(&io, &regs, STATUS_B_BINARY);

        let rtc = Rtc::new(&io);
        let ts = rtc.read_unix_time().expect("stable read");
        let expected_days = days_from_civil(2026, 1, 1);
        assert_eq!(ts, (expected_days * 86_400 + 21 * 3600) as u64);
    }

    /// Regression test for a real bug carried over from the original
    /// `kernel/src/rtc.rs`: in BCD mode the hour was decoded as
    /// `bcd_to_bin(raw.hour & 0x7F)`, which clears the PM flag (bit 7)
    /// *before* the 12-hour block below ever tests it — and `bcd_to_bin`
    /// of any value `<= 0x7F` maxes out at 85 (`0x55`), so the decoded
    /// hour could never have bit 7 set either. Every PM time therefore
    /// read back as AM: a silent 12-hour error on any machine whose CMOS
    /// is in the (extremely common) BCD + 12-hour configuration.
    ///
    /// Never bit under QEMU, whose RTC defaults to BCD + *24*-hour, so the
    /// 12-hour path never ran there at all — which is exactly why it
    /// survived: the only 12-hour coverage this driver had used binary
    /// mode, where the raw hour passes through unmasked and the flag
    /// happens to survive.
    #[test]
    fn bcd_mode_twelve_hour_pm_flag_is_not_lost() {
        let io = ScriptedIo::new();
        // 9 PM in BCD (0x09) with the PM flag set -> 21:00.
        let regs = [0u8, 0, 0x09 | 0x80, 0x01, 0x01, 0x26];
        queue_stable_snapshot(&io, &regs, 0); // BCD (bit 2 clear) + 12h (bit 1 clear)

        let rtc = Rtc::new(&io);
        let ts = rtc.read_unix_time().expect("stable read");
        let expected_days = days_from_civil(2026, 1, 1);
        assert_eq!(ts, (expected_days * 86_400 + 21 * 3600) as u64);
    }

    /// The 12 AM / 12 PM edge cases again, but in BCD mode — the pairing
    /// that the binary-mode-only tests above left uncovered.
    #[test]
    fn bcd_mode_twelve_pm_stays_noon_and_twelve_am_is_midnight() {
        for (hour_reg, expected_hour) in [(0x12u8 | 0x80, 12i64), (0x12u8, 0i64)] {
            let io = ScriptedIo::new();
            let regs = [0u8, 0, hour_reg, 0x01, 0x01, 0x26];
            queue_stable_snapshot(&io, &regs, 0);

            let rtc = Rtc::new(&io);
            let ts = rtc.read_unix_time().expect("stable read");
            let expected_days = days_from_civil(2026, 1, 1);
            assert_eq!(ts, (expected_days * 86_400 + expected_hour * 3600) as u64);
        }
    }

    #[test]
    fn twelve_am_edge_case_hour_becomes_zero() {
        let io = ScriptedIo::new();
        // "12 AM" on a 12-hour CMOS clock is encoded as hour=12, PM clear.
        let regs = [0u8, 0, 12, 1, 1, 26];
        queue_stable_snapshot(&io, &regs, STATUS_B_BINARY);

        let rtc = Rtc::new(&io);
        let ts = rtc.read_unix_time().expect("stable read");
        let expected_days = days_from_civil(2026, 1, 1);
        assert_eq!(ts, (expected_days * 86_400) as u64); // hour 0
    }

    #[test]
    fn twelve_pm_edge_case_hour_stays_twelve() {
        let io = ScriptedIo::new();
        // "12 PM" (noon) is encoded as hour=12, PM set.
        let regs = [0u8, 0, 12 | 0x80, 1, 1, 26];
        queue_stable_snapshot(&io, &regs, STATUS_B_BINARY);

        let rtc = Rtc::new(&io);
        let ts = rtc.read_unix_time().expect("stable read");
        let expected_days = days_from_civil(2026, 1, 1);
        assert_eq!(ts, (expected_days * 86_400 + 12 * 3600) as u64);
    }

    #[test]
    fn torn_read_first_snapshot_mismatches_second_retries_until_stable() {
        let io = ScriptedIo::new();
        // First pass: STATUS_A not updating, then a snapshot with second=1.
        io.queue_read(CMOS_DATA, 0);
        io.queue_reads(CMOS_DATA, &[1u32, 0, 12, 1, 1, 26]);
        // Second pass (same outer loop iteration): STATUS_A not updating,
        // snapshot with second=2 -> mismatches the first -> torn, retry.
        io.queue_read(CMOS_DATA, 0);
        io.queue_reads(CMOS_DATA, &[2u32, 0, 12, 1, 1, 26]);
        // Next outer iteration: two matching reads of second=3 -> stable.
        io.queue_read(CMOS_DATA, 0);
        io.queue_reads(CMOS_DATA, &[3u32, 0, 12, 1, 1, 26]);
        io.queue_read(CMOS_DATA, 0);
        io.queue_reads(CMOS_DATA, &[3u32, 0, 12, 1, 1, 26]);
        // STATUS_B for the final decode (24h mode, so hour=12 stays noon
        // instead of being reinterpreted as a 12-hour-clock "12 AM").
        io.queue_read(CMOS_DATA, (STATUS_B_BINARY | STATUS_B_24H) as u32);

        let rtc = Rtc::new(&io);
        let ts = rtc.read_unix_time().expect("eventually stabilizes");
        let expected_days = days_from_civil(2026, 1, 1);
        // second=3 is the stable value that won, not 1 or 2.
        assert_eq!(ts, (expected_days * 86_400 + 12 * 3600 + 3) as u64);
    }

    #[test]
    fn chip_that_never_settles_returns_none_within_the_bound_not_a_hang() {
        // A genuinely broken/never-stabilizing CMOS: every full snapshot
        // read differs from the one right before it, so `first == second`
        // never holds across all MAX_ATTEMPTS outer retries. This is what
        // actually drives read_stable_snapshot to None — the bounded
        // wait_uip loops merely give up and read anyway (they don't by
        // themselves force a None; see the note below), so the property
        // that actually needs covering is "reads that never agree", not
        // "UIP asserted" per se.
        //
        // Surprising property found while writing this test: a CMOS whose
        // status register reports "update in progress" *forever* but whose
        // six time registers happen to stay perfectly static would NOT hit
        // this path — read_stable_snapshot's inner wait loops are bounded
        // and, on timeout, proceed to read the registers anyway; two
        // back-to-back reads of an unchanging value are trivially equal,
        // so it would return `Some(stale snapshot)`, not `None`. The
        // "never hang" contract is unconditional; the "None on failure"
        // contract only holds when the underlying registers are actually
        // torn/inconsistent across the retry window, which is what this
        // test models instead.
        let io = ScriptedIo::new();
        // Every queued byte here is < 0x80 (bit7/UPDATE_IN_PROGRESS clear),
        // so every wait_uip call resolves in exactly one read regardless of
        // which "logical" slot (status vs. register) it lands on — the
        // fixture doesn't need to track that distinction, just the total
        // read count: 1 (uip) + 6 (snapshot) + 1 (uip) + 6 (snapshot) = 14
        // reads per outer iteration.
        let mut seq = alloc::vec::Vec::new();
        for i in 0..MAX_ATTEMPTS {
            let sec_a = i % 100;
            let sec_b = (i % 100) + 1; // always different from sec_a, still < 0x80
            seq.push(0); // wait_uip #1
            seq.extend_from_slice(&[sec_a, 0, 12, 1, 1, 26]); // first snapshot
            seq.push(0); // wait_uip #2
            seq.extend_from_slice(&[sec_b, 0, 12, 1, 1, 26]); // second snapshot, mismatches
        }
        io.queue_reads(CMOS_DATA, &seq);

        let rtc = Rtc::new(&io);
        assert_eq!(rtc.read_unix_time(), None);
    }

    #[test]
    fn end_to_end_known_real_timestamp_via_read_unix_time() {
        // 2026-07-23 12:34:56 UTC, binary + 24h mode, matches the
        // "currentDate" this session actually runs under.
        let io = ScriptedIo::new();
        let regs = [56u8, 34, 12, 23, 7, 26];
        queue_stable_snapshot(&io, &regs, STATUS_B_BINARY | STATUS_B_24H);

        let rtc = Rtc::new(&io);
        let ts = rtc.read_unix_time().expect("stable read");

        // Independently computed expected epoch seconds for
        // 2026-07-23T12:34:56Z (cross-checked externally, not just derived
        // from the same days_from_civil under test): 1784810096.
        let expected_days = days_from_civil(2026, 7, 23);
        let expected = (expected_days * 86_400 + 12 * 3600 + 34 * 60 + 56) as u64;
        assert_eq!(ts, expected);
        assert_eq!(ts, 1_784_810_096);
    }
}
