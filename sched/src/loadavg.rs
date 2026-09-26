//! The load average: Linux's `kernel/sched/loadavg.c` arithmetic, verbatim.
//!
//! Every [`LOAD_FREQ`] ticks (5 s) the number of runnable processes — running
//! or ready; Linux also counts uninterruptible sleepers, which this kernel
//! does not have — is folded into three exponentially-decaying averages
//! with 1, 5 and 15 minute time constants. Fixed point with [`FSHIFT`] = 11
//! fraction bits, so `/proc/loadavg` and `sysinfo(2)` print what a Linux
//! box with the same run queue would.

/// Fraction bits of the fixed-point averages.
pub const FSHIFT: u32 = 11;
/// 1.0 in fixed point.
pub const FIXED_1: u64 = 1 << FSHIFT;
/// Ticks between samples: 5 s at 100 Hz, plus one (Linux's `5*HZ+1`,
/// chosen so the sample does not beat against other 5 s periodic work).
pub const LOAD_FREQ: u64 = 5 * super::cputime::USER_HZ + 1;

/// `FIXED_1 / exp(5 s / 1 min)`, `/ exp(5 s / 5 min)`, `/ exp(5 s / 15 min)`.
const EXP: [u64; 3] = [1884, 2014, 2037];

/// The three averages, in fixed point.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoadAvg {
    pub avg: [u64; 3],
}

impl LoadAvg {
    /// Fold one sample of `runnable` processes in.
    pub fn sample(&mut self, runnable: u64) {
        let active = runnable * FIXED_1;
        for (a, e) in self.avg.iter_mut().zip(EXP) {
            *a = calc_load(*a, e, active);
        }
    }
}

/// Linux's `calc_load`: `load * e + active * (1 - e)`, rounded up when the
/// load is rising so that a constant load is eventually reached exactly.
fn calc_load(load: u64, exp: u64, active: u64) -> u64 {
    let mut newload = load * exp + active * (FIXED_1 - exp);
    if active >= load {
        newload += FIXED_1 - 1;
    }
    newload / FIXED_1
}

/// A fixed-point average as `(integer, hundredths)` — Linux's
/// `LOAD_INT`/`LOAD_FRAC`, for `"%lu.%02lu"`.
pub fn split(avg: u64) -> (u64, u64) {
    let frac = avg & (FIXED_1 - 1);
    (avg >> FSHIFT, (frac * 100) >> FSHIFT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_stays_zero() {
        let mut l = LoadAvg::default();
        for _ in 0..1000 {
            l.sample(0);
        }
        assert_eq!(l.avg, [0, 0, 0]);
    }

    #[test]
    fn a_constant_load_is_reached_exactly_and_the_one_minute_average_leads() {
        let mut l = LoadAvg::default();
        l.sample(2);
        assert!(l.avg[0] > l.avg[1] && l.avg[1] > l.avg[2]);
        // One minute of samples: the 1-min average is ~63% of the way.
        for _ in 1..12 {
            l.sample(2);
        }
        let (i, f) = split(l.avg[0]);
        assert_eq!(i, 1);
        assert!((20..=35).contains(&f), "1-min after 1 min = {i}.{f:02}");
        // Long enough and every average sits on 2.00 exactly (rounding up
        // while rising is what lets it arrive rather than approach).
        for _ in 0..5000 {
            l.sample(2);
        }
        assert_eq!(l.avg, [2 * FIXED_1; 3]);
        assert_eq!(split(l.avg[2]), (2, 0));
    }

    #[test]
    fn it_decays_back_towards_zero() {
        let mut l = LoadAvg { avg: [4 * FIXED_1; 3] };
        for _ in 0..12 {
            l.sample(0);
        }
        let (i, _) = split(l.avg[0]);
        assert_eq!(i, 1, "1-min average one minute after load stopped");
        assert!(l.avg[2] > l.avg[1] && l.avg[1] > l.avg[0]);
    }

    #[test]
    fn split_prints_like_linux() {
        assert_eq!(split(FIXED_1 + FIXED_1 / 2), (1, 50));
        assert_eq!(split(FIXED_1 / 4), (0, 25));
        assert_eq!(split(0), (0, 0));
    }
}
