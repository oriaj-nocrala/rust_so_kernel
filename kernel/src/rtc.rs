// kernel/src/rtc.rs
//
// CMOS real-time clock (MC146818-compatible, present on every PC/QEMU
// target this kernel runs on): read once at boot to get a real wall-clock
// Unix epoch, so time-since-boot (the only thing `time::clocksource`
// tracks — there's no hardware tick source for wall-clock time itself)
// can be turned into a real calendar time. Read-once-at-boot is
// deliberate: this kernel has no periodic RTC alarm/update IRQ wired up,
// and doesn't need one — `time::now_unix_secs()` just adds elapsed
// monotonic uptime on top of the single boot-time reading.

use x86_64::instructions::port::Port;

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

fn cmos_read(reg: u8) -> u8 {
    unsafe {
        Port::<u8>::new(CMOS_INDEX).write(reg);
        Port::<u8>::new(CMOS_DATA).read()
    }
}

fn update_in_progress() -> bool {
    cmos_read(REG_STATUS_A) & STATUS_A_UPDATE_IN_PROGRESS != 0
}

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

/// One raw register snapshot. Doesn't wait for the update-in-progress flag
/// itself — callers loop this until two consecutive reads agree, which is
/// what actually rules out a read torn by the RTC's once-a-second update.
fn read_raw_snapshot() -> RawTime {
    RawTime {
        second: cmos_read(REG_SECONDS),
        minute: cmos_read(REG_MINUTES),
        hour: cmos_read(REG_HOURS),
        day: cmos_read(REG_DAY),
        month: cmos_read(REG_MONTH),
        year: cmos_read(REG_YEAR),
    }
}

/// Read a stable register snapshot: wait out any in-progress update, read,
/// then confirm a second read (after waiting out the update flag again)
/// agrees — the standard CMOS RTC technique for avoiding a snapshot torn
/// across the ~244us window where the chip is updating its registers.
/// Bounded (not an infinite loop): a machine whose CMOS is stuck in
/// "update in progress" would otherwise hang boot forever over a
/// best-effort wall-clock reading — see `read_unix_time`'s fallback.
fn read_stable_snapshot() -> Option<RawTime> {
    const MAX_ATTEMPTS: u32 = 1000;
    for _ in 0..MAX_ATTEMPTS {
        for _ in 0..MAX_ATTEMPTS {
            if !update_in_progress() {
                break;
            }
        }
        let first = read_raw_snapshot();
        for _ in 0..MAX_ATTEMPTS {
            if !update_in_progress() {
                break;
            }
        }
        let second = read_raw_snapshot();
        if first == second {
            return Some(first);
        }
    }
    None
}

/// Days since the Unix epoch (1970-01-01) for a given proleptic Gregorian
/// civil date — Howard Hinnant's `days_from_civil` algorithm: exact,
/// integer-only (no floating point, no external crate — this is a
/// `#![no_std]` bare-metal kernel), correct across the full Gregorian
/// leap-year rule (divisible by 4, not by 100, unless by 400).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let m = m as i64;
    let d = d as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Read the CMOS RTC and convert to a Unix epoch timestamp (seconds).
/// Best-effort: returns `None` rather than hanging or panicking if the
/// hardware never settles (see `read_stable_snapshot`) — same "best
/// effort, log and move on" contract as `mouse::init`/`ac97::init`.
///
/// Century: CMOS has no standardized register for it (the ACPI FADT
/// century-register field is often absent/unreliable, including under
/// QEMU's default RTC), so this assumes 2000-2099 — fine for any machine
/// actually running this kernel today, same "single-user, don't
/// over-engineer for a case nothing here will ever hit" pragmatism as the
/// hostname/uid stubs in the mlibc port.
pub fn read_unix_time() -> Option<u64> {
    let raw = read_stable_snapshot()?;
    let status_b = cmos_read(REG_STATUS_B);

    let (second, minute, mut hour, day, month, year_2digit) = if status_b & STATUS_B_BINARY != 0 {
        (raw.second, raw.minute, raw.hour, raw.day, raw.month, raw.year)
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
        // 12-hour mode: bit 7 of the (already-decoded) hour is the PM flag.
        let pm = hour & 0x80 != 0;
        hour &= 0x7F;
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
