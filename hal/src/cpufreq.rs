//! Effective CPU frequency from `IA32_APERF`/`IA32_MPERF`.
//!
//! Both count only while the core is in C0 (running, not halted). MPERF
//! ticks at a fixed rate — the TSC's on every CPU with an invariant TSC,
//! AMD's P0 and Intel's maximum non-turbo frequency — and APERF at the
//! frequency the core actually runs at. So over any interval
//!
//! ```text
//!   running frequency = base * ΔAPERF / ΔMPERF
//! ```
//!
//! which is what Linux's `arch_freq_get_on_cpu` reports as `/proc/cpuinfo`'s
//! `cpu MHz`. An idle core adds almost nothing to either counter, so the
//! kernel accumulates deltas until they represent enough running time
//! ([`MIN_ACTIVE_US`]) to mean something, and until then keeps reporting
//! the last result — the frequency the core last ran at.
//!
//! Pure: the kernel reads the MSRs on each CPU and keeps a [`Window`] per
//! CPU; everything here is arithmetic on the values it hands in.

/// CPUID leaf 6, ECX bit 0: the APERF/MPERF pair exists. Without it,
/// reading either MSR raises #GP.
pub fn has_aperfmperf(leaf6_ecx: u32) -> bool {
    leaf6_ecx & 1 != 0
}

/// Running time a result must cover, in microseconds at the MPERF rate.
/// A core that ran for less since the last result keeps accumulating.
pub const MIN_ACTIVE_US: u64 = 10_000;

/// A result above this multiple of the base is taken as a counter that
/// was reset or written (firmware, a hypervisor), not a frequency: no CPU
/// boosts to 4x its base.
const MAX_RATIO: u64 = 4;

/// One CPU's sampling state: the counters at the previous sample, and what
/// has accumulated since the last result.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Window {
    pub last_aperf: u64,
    pub last_mperf: u64,
    pub acc_aperf: u64,
    pub acc_mperf: u64,
    /// Whether `last_*` hold a real reading (the first sample only primes).
    pub primed: bool,
}

/// Feed one reading of this CPU's counters. Returns the new window and,
/// when it has covered [`MIN_ACTIVE_US`] of running time, the frequency in
/// kHz over it (the window then starts over).
///
/// `base_khz` is the MPERF rate — the TSC frequency. Deltas wrap as the
/// 64-bit counters do.
pub fn sample(w: Window, aperf: u64, mperf: u64, base_khz: u64) -> (Window, Option<u64>) {
    if !w.primed || base_khz == 0 {
        return (Window { last_aperf: aperf, last_mperf: mperf, primed: true, ..Window::default() }, None);
    }
    let da = aperf.wrapping_sub(w.last_aperf);
    let dm = mperf.wrapping_sub(w.last_mperf);
    let mut next = Window {
        last_aperf: aperf,
        last_mperf: mperf,
        acc_aperf: w.acc_aperf.saturating_add(da),
        acc_mperf: w.acc_mperf.saturating_add(dm),
        primed: true,
    };
    // MPERF ticks base_khz * 1000 times a second: MIN_ACTIVE_US of it is
    // base_khz * MIN_ACTIVE_US / 1000 ticks.
    let needed = base_khz.saturating_mul(MIN_ACTIVE_US) / 1000;
    if next.acc_mperf < needed.max(1) {
        return (next, None);
    }
    let khz = (base_khz as u128 * next.acc_aperf as u128 / next.acc_mperf as u128) as u64;
    next.acc_aperf = 0;
    next.acc_mperf = 0;
    if khz == 0 || khz > base_khz.saturating_mul(MAX_RATIO) {
        return (next, None);
    }
    (next, Some(khz))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u64 = 3_700_000; // kHz: a 5900X's P0 / TSC

    /// Run a core for `us` microseconds of C0 at `khz` from `(a, m)`.
    fn run(a: u64, m: u64, us: u64, khz: u64) -> (u64, u64) {
        (a.wrapping_add(khz * us / 1000), m.wrapping_add(BASE * us / 1000))
    }

    #[test]
    fn detection_is_leaf6_ecx_bit0() {
        assert!(has_aperfmperf(1));
        assert!(has_aperfmperf(0b1011));
        assert!(!has_aperfmperf(0b1010));
    }

    #[test]
    fn first_sample_only_primes() {
        let (w, r) = sample(Window::default(), 123, 456, BASE);
        assert_eq!(r, None);
        assert!(w.primed);
        assert_eq!((w.last_aperf, w.last_mperf, w.acc_aperf, w.acc_mperf), (123, 456, 0, 0));
    }

    #[test]
    fn boosted_core_reports_its_real_frequency() {
        let (w, _) = sample(Window::default(), 1000, 2000, BASE);
        let (a, m) = run(1000, 2000, 10_000, 4_950_000);
        let (w, r) = sample(w, a, m, BASE);
        assert_eq!(r, Some(4_950_000));
        assert_eq!((w.acc_aperf, w.acc_mperf), (0, 0), "window starts over");
    }

    #[test]
    fn slow_core_reports_below_base() {
        let (w, _) = sample(Window::default(), 0, 0, BASE);
        let (a, m) = run(0, 0, 20_000, 2_200_000);
        assert_eq!(sample(w, a, m, BASE).1, Some(2_200_000));
    }

    #[test]
    fn short_activity_accumulates_until_it_means_something() {
        // An idle core: 1 ms of C0 per 10 ms tick, at 4.5 GHz.
        let (mut w, _) = sample(Window::default(), 0, 0, BASE);
        let (mut a, mut m) = (0, 0);
        let mut results = 0;
        for _ in 0..9 {
            (a, m) = run(a, m, 1_000, 4_500_000);
            let (nw, r) = sample(w, a, m, BASE);
            w = nw;
            results += r.is_some() as u32;
        }
        assert_eq!(results, 0, "9 ms of running time is not enough yet");
        (a, m) = run(a, m, 1_000, 4_500_000);
        let (w2, r) = sample(w, a, m, BASE);
        assert_eq!(r, Some(4_500_000));
        assert_eq!(w2.acc_mperf, 0);
    }

    #[test]
    fn a_halted_core_changes_nothing() {
        let (w, _) = sample(Window::default(), 500, 700, BASE);
        let (w2, r) = sample(w, 500, 700, BASE);
        assert_eq!(r, None);
        assert_eq!((w2.acc_aperf, w2.acc_mperf), (0, 0));
    }

    #[test]
    fn counters_wrap() {
        let a0 = u64::MAX - 1_000;
        let m0 = u64::MAX - 5_000;
        let (w, _) = sample(Window::default(), a0, m0, BASE);
        let (a, m) = run(a0, m0, 10_000, 4_000_000);
        assert!(a < a0 && m < m0, "both wrapped");
        assert_eq!(sample(w, a, m, BASE).1, Some(4_000_000));
    }

    #[test]
    fn a_reset_counter_is_not_a_frequency() {
        // MPERF written back to near zero while APERF kept going: the
        // wrapped MPERF delta is enormous, the ratio near zero — and the
        // other way round, a reset APERF gives an absurd ratio.
        let (w, _) = sample(Window::default(), 1 << 40, 1 << 40, BASE);
        let (a, m) = run(1 << 40, 0, 10_000, 4_000_000);
        assert_eq!(sample(w, a, m, BASE).1, None);
        let (w, _) = sample(Window::default(), 1 << 40, 1 << 20, BASE);
        let (a, m) = run(0, 1 << 20, 10_000, 4_000_000);
        assert_eq!(sample(w, a, m, BASE).1, None);
    }

    #[test]
    fn no_base_no_result() {
        let (w, _) = sample(Window::default(), 0, 0, 0);
        let (w, r) = sample(w, 1 << 30, 1 << 30, 0);
        assert_eq!(r, None);
        assert!(!w.primed || w.acc_mperf == 0);
    }
}
