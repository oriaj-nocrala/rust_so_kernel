//! `OpStat` — a per-operation cost counter: calls, bytes, TSC cycles.
//!
//! The framebuffer console's instrument. It exists because the console is
//! imperceptibly fast in QEMU (the framebuffer is host RAM) and visibly
//! slow on the physical machine this kernel is brought up on (the
//! framebuffer is across PCIe), and that machine has no serial capture —
//! so "it feels slow" can only become a number if the kernel counts the
//! cost itself and renders it into a file somebody can `cat`.
//!
//! Deliberately shaped so a single line answers the question that matters.
//! Raw totals alone do not: 1.4 million cycles could be one enormous clear
//! or a million cheap ones, and on a machine where the next measurement
//! costs a reboot, an ambiguous counter is worse than no counter (this
//! kernel's `measure-the-instrument-first` rule, learned the hard way).
//! So `render` reports the derived per-call and per-byte costs too, plus
//! an actual throughput in MB/s once the caller supplies the TSC
//! frequency — which is the number that says, with no theory attached,
//! whether the mapping is write-combining or uncacheable.
//!
//! `bytes` is whatever unit of work the caller is measuring, in bytes of
//! framebuffer touched. An operation with no meaningful byte count (a
//! parse, say) can pass 0 and still get calls and cycles.
//!
//! ## The bias in `cycles`, and why `min`/`max` are here
//!
//! Callers measure with a plain TSC delta, which is wall-clock: the
//! console does not run with interrupts disabled, so a timer preemption
//! landing inside a measured operation charges it for everything another
//! process did in the meantime. That inflates the *mean* without bound
//! under load — measured, not feared: `draw_char` averaged ~120k cycles
//! on a quiet boot and ~1.5M printing 400 lines, for identical work.
//!
//! The honest fix is not to hide it. `min` is the closest thing to the
//! uncontaminated cost of one call (some call somewhere got through
//! without being interrupted), `max` shows how bad the contamination
//! gets, and the mean sits between them. A reader who sees min and max
//! three orders of magnitude apart knows to compare totals under a fixed
//! workload rather than trusting the average — which is exactly the
//! judgement an ambiguous single number would have taken away.

use core::sync::atomic::{AtomicU64, Ordering};

pub struct OpStat {
    calls: AtomicU64,
    bytes: AtomicU64,
    cycles: AtomicU64,
    min_cycles: AtomicU64,
    max_cycles: AtomicU64,
}

impl Default for OpStat {
    fn default() -> Self {
        Self::new()
    }
}

impl OpStat {
    pub const fn new() -> Self {
        Self {
            calls: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            cycles: AtomicU64::new(0),
            min_cycles: AtomicU64::new(u64::MAX),
            max_cycles: AtomicU64::new(0),
        }
    }

    /// Record one completed operation. Five relaxed read-modify-writes —
    /// the same "free by comparison" argument `switches_total` already
    /// makes (the cheapest thing measured here writes 288 bytes to a bus
    /// that charges per transaction), and the reason this can stay on
    /// permanently instead of being a debugging build's privilege.
    #[inline]
    pub fn record(&self, bytes: u64, cycles: u64) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
        self.cycles.fetch_add(cycles, Ordering::Relaxed);
        self.min_cycles.fetch_min(cycles, Ordering::Relaxed);
        self.max_cycles.fetch_max(cycles, Ordering::Relaxed);
    }

    /// Cheapest single call observed, or `None` before the first one. The
    /// least preemption-contaminated estimate of what one call really
    /// costs — see this module's doc comment.
    pub fn min_cycles(&self) -> Option<u64> {
        match self.min_cycles.load(Ordering::Relaxed) {
            u64::MAX => None,
            v => Some(v),
        }
    }

    /// Most expensive single call observed, or `None` before the first.
    pub fn max_cycles(&self) -> Option<u64> {
        if self.calls() == 0 {
            return None;
        }
        Some(self.max_cycles.load(Ordering::Relaxed))
    }

    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
    pub fn cycles(&self) -> u64 {
        self.cycles.load(Ordering::Relaxed)
    }

    /// Throughput in MB/s (10^6 bytes/s), or `None` when it cannot be
    /// derived — no calls yet, no TSC frequency, or an operation that
    /// reports no bytes. `None` rather than `0` on purpose: a rate of
    /// zero and "not measurable" are different facts, and printing the
    /// first for the second is exactly the ambiguity this module refuses.
    pub fn mb_per_sec(&self, tsc_hz: u64) -> Option<u64> {
        let bytes = self.bytes();
        let cycles = self.cycles();
        if tsc_hz == 0 || cycles == 0 || bytes == 0 {
            return None;
        }
        // u128 throughout: `bytes * tsc_hz` overflows u64 for a few
        // hundred MB moved on a 4 GHz part, which a full-screen repaint
        // reaches in seconds.
        Some(((bytes as u128 * tsc_hz as u128) / cycles as u128 / 1_000_000) as u64)
    }

    /// Mean cycles per call, or `None` before the first call.
    pub fn cycles_per_call(&self) -> Option<u64> {
        let calls = self.calls();
        if calls == 0 {
            return None;
        }
        Some(self.cycles() / calls)
    }

    /// One `/proc/fbinfo` line. Every counter is snapshotted once and the
    /// derived figures come from that snapshot, so the line can never be
    /// internally inconsistent — same discipline as `LockDiag::render`,
    /// and for the same reason: this is read live, while the PIT ISR
    /// keeps updating the same counters.
    pub fn render(&self, name: &str, tsc_hz: u64) -> alloc::string::String {
        use alloc::format;

        let calls = self.calls();
        let bytes = self.bytes();
        let cycles = self.cycles();

        if calls == 0 {
            return format!("{name}: calls=0\n");
        }

        let per_call = cycles / calls;
        let lo = self.min_cycles.load(Ordering::Relaxed);
        let hi = self.max_cycles.load(Ordering::Relaxed);
        let rate = if tsc_hz == 0 || cycles == 0 || bytes == 0 {
            alloc::string::String::from("n/a")
        } else {
            format!(
                "{} MB/s",
                (bytes as u128 * tsc_hz as u128) / cycles as u128 / 1_000_000
            )
        };

        format!(
            "{name}: calls={} bytes={} cycles={} ({} cyc/call, min {}, max {}, {})\n",
            calls, bytes, cycles, per_call, lo, hi, rate,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_never_called_op_renders_one_short_line_and_no_derived_figures() {
        let s = OpStat::new();
        assert_eq!(s.render("fb_fill_rect", 1_000_000_000), "fb_fill_rect: calls=0\n");
        assert_eq!(s.cycles_per_call(), None);
        assert_eq!(s.mb_per_sec(1_000_000_000), None);
        assert_eq!(s.min_cycles(), None, "no call yet is None, not u64::MAX");
        assert_eq!(s.max_cycles(), None, "no call yet is None, not a plausible-looking 0");
    }

    #[test]
    fn record_accumulates_all_three_counters() {
        let s = OpStat::new();
        s.record(100, 10);
        s.record(200, 30);
        assert_eq!(s.calls(), 2);
        assert_eq!(s.bytes(), 300);
        assert_eq!(s.cycles(), 40);
        assert_eq!(s.cycles_per_call(), Some(20));
        assert_eq!(s.min_cycles(), Some(10));
        assert_eq!(s.max_cycles(), Some(30));
    }

    #[test]
    fn throughput_is_bytes_times_tsc_hz_over_cycles() {
        let s = OpStat::new();
        // 1 GB moved in 1 GHz-worth of cycles == 1000 MB/s.
        s.record(1_000_000_000, 1_000_000_000);
        assert_eq!(s.mb_per_sec(1_000_000_000), Some(1000));
    }

    #[test]
    fn throughput_does_not_overflow_on_a_realistic_repaint_at_four_ghz() {
        let s = OpStat::new();
        // 8 MiB of framebuffer at 1 byte per 20 cycles on a 4 GHz part:
        // `bytes * tsc_hz` is ~3.4e16 here, and a naive u64 product
        // overflows once a boot has repainted a few hundred megabytes.
        for _ in 0..64 {
            s.record(8 * 1024 * 1024, 8 * 1024 * 1024 * 20);
        }
        assert_eq!(s.mb_per_sec(4_000_000_000), Some(200));
    }

    #[test]
    fn an_op_reporting_no_bytes_still_reports_calls_and_cycles_but_no_rate() {
        let s = OpStat::new();
        s.record(0, 500);
        assert_eq!(s.mb_per_sec(3_000_000_000), None);
        assert_eq!(
            s.render("fb_parse", 3_000_000_000),
            "fb_parse: calls=1 bytes=0 cycles=500 (500 cyc/call, min 500, max 500, n/a)\n"
        );
    }

    #[test]
    fn an_unknown_tsc_frequency_reports_n_a_rather_than_a_made_up_rate() {
        let s = OpStat::new();
        s.record(4096, 1000);
        assert_eq!(s.mb_per_sec(0), None);
        assert!(s.render("fb_fill_rect", 0).ends_with("(1000 cyc/call, min 1000, max 1000, n/a)\n"));
    }

    #[test]
    fn a_preempted_outlier_shows_up_as_a_spread_instead_of_vanishing_into_the_mean() {
        // The real shape of the bias: many honest calls plus one that was
        // interrupted. The mean alone would read as "every call costs 10x
        // what it does"; min/max says what actually happened.
        let s = OpStat::new();
        for _ in 0..9 {
            s.record(288, 1_000);
        }
        s.record(288, 900_000);
        assert_eq!(s.cycles_per_call(), Some(90_900));
        assert_eq!(s.min_cycles(), Some(1_000));
        assert_eq!(s.max_cycles(), Some(900_000));
    }

    #[test]
    fn render_full_line_after_activity() {
        let s = OpStat::new();
        s.record(2_000_000, 1_000_000);
        assert_eq!(
            s.render("fb_fill_rect", 1_000_000_000),
            "fb_fill_rect: calls=1 bytes=2000000 cycles=1000000 (1000000 cyc/call, min 1000000, max 1000000, 2000 MB/s)\n"
        );
    }
}
