//! Where a compositor frame's time goes: a ring of the latest events and a few statistics, fed by the kernel at the points that see them
//! (a submission to a channel, the first look that finds it done, a `PRESENT` entering and leaving, a vblank). Pure, no clock of its own:
//! the caller passes nanoseconds, so it is tested here and the kernel adapter (`kernel/src/gpu/pacing.rs`) only holds the static.
//!
//! Why: Ryzen #189/#190 found a compositor locked at 30 fps with `present` a constant ~9 ms while the GPU sat in its lowest P-state, and could not
//! say whether that was the GPU executing slowly, the work waiting behind another channel's, or the wait for the previous flip. This separates
//! them: per channel, how long from a submission to the first look that saw it done (queueing plus execution, observed no sooner than someone asks);
//! and **how long after the previous vblank each `PRESENT` arrives**, the number the ~9 ms deadline of `docs/gpu/g5-layer4-handoff.md` is about.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

/// Events kept.
pub const RING: usize = 192;
/// Channels with their own statistics (`chan` ids are the kernel's `ChanId`: the boot GR and copy channels and the run-time slots).
pub const CHANS: usize = 12;
/// Submissions not yet seen done, per channel (the oldest is dropped when more are in flight).
const PENDING: usize = 8;
/// `PRESENT` arrival histogram: buckets of 2 ms after the previous vblank, the last one open (18 ms and more).
pub const PRESENT_BUCKETS: usize = 10;
pub const BUCKET_NS: u64 = 2_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `a` = channel, `b` = fence sequence.
    Submit,
    /// The first look that saw the fence of `b` (and every earlier one) on channel `a` done.
    Done,
    /// A `PRESENT` entered; `b` = microseconds since the previous vblank.
    Present,
    /// That `PRESENT` returned.
    PresentEnd,
    /// A vblank of the primary head; `b` = its sequence number.
    Vblank,
}

#[derive(Clone, Copy, Debug)]
pub struct Event {
    pub t_ns: u64,
    pub kind: Kind,
    pub a: u32,
    pub b: u64,
}

pub struct Pacing {
    ring: Vec<Event>,
    head: usize,
    pending: [[(u64, u64); PENDING]; CHANS],
    pending_len: [usize; CHANS],
    exec_n: [u64; CHANS],
    exec_sum_us: [u64; CHANS],
    exec_max_us: [u64; CHANS],
    present_hist: [u64; PRESENT_BUCKETS],
    present_n: u64,
    present_started: u64,
    ioctl_n: u64,
    ioctl_sum_us: u64,
    ioctl_max_us: u64,
    vblanks: u64,
}

/// The bucket of a `PRESENT` that arrived `ns` after the previous vblank.
pub fn bucket(ns: u64) -> usize {
    ((ns / BUCKET_NS) as usize).min(PRESENT_BUCKETS - 1)
}

impl Default for Pacing {
    fn default() -> Self {
        Self::new()
    }
}

impl Pacing {
    pub const fn new() -> Self {
        Pacing {
            ring: Vec::new(),
            head: 0,
            pending: [[(0, 0); PENDING]; CHANS],
            pending_len: [0; CHANS],
            exec_n: [0; CHANS],
            exec_sum_us: [0; CHANS],
            exec_max_us: [0; CHANS],
            present_hist: [0; PRESENT_BUCKETS],
            present_n: 0,
            present_started: 0,
            ioctl_n: 0,
            ioctl_sum_us: 0,
            ioctl_max_us: 0,
            vblanks: 0,
        }
    }

    pub fn reset(&mut self) {
        *self = Pacing::new();
    }

    fn push(&mut self, e: Event) {
        if self.ring.len() < RING {
            self.ring.push(e);
        } else {
            self.ring[self.head] = e;
            self.head = (self.head + 1) % RING;
        }
    }

    /// Submission of fence `seq` to channel `chan` at `t_ns`.
    pub fn submit(&mut self, t_ns: u64, chan: usize, seq: u64) {
        self.push(Event { t_ns, kind: Kind::Submit, a: chan as u32, b: seq });
        if chan >= CHANS {
            return;
        }
        let n = self.pending_len[chan];
        if n == PENDING {
            self.pending[chan].copy_within(1.., 0); // the oldest goes: its look would be too late to mean anything
            self.pending[chan][PENDING - 1] = (seq, t_ns);
        } else {
            self.pending[chan][n] = (seq, t_ns);
            self.pending_len[chan] = n + 1;
        }
    }

    /// The first look that found every fence up to `upto` on channel `chan` done, at `t_ns`.
    pub fn done(&mut self, t_ns: u64, chan: usize, upto: u64) {
        if chan >= CHANS {
            return;
        }
        let mut kept = 0;
        let mut any = false;
        for i in 0..self.pending_len[chan] {
            let (seq, t0) = self.pending[chan][i];
            if seq <= upto {
                let us = t_ns.saturating_sub(t0) / 1000;
                self.exec_n[chan] += 1;
                self.exec_sum_us[chan] += us;
                self.exec_max_us[chan] = self.exec_max_us[chan].max(us);
                any = true;
            } else {
                self.pending[chan][kept] = self.pending[chan][i];
                kept += 1;
            }
        }
        self.pending_len[chan] = kept;
        if any {
            self.push(Event { t_ns, kind: Kind::Done, a: chan as u32, b: upto });
        }
    }

    /// A `PRESENT` entering at `t_ns`; the previous vblank was at `last_vblank_ns` (0: none seen yet).
    pub fn present_begin(&mut self, t_ns: u64, last_vblank_ns: u64) {
        let after = if last_vblank_ns == 0 { 0 } else { t_ns.saturating_sub(last_vblank_ns) };
        self.present_n += 1;
        self.present_hist[bucket(after)] += 1;
        self.present_started = t_ns;
        self.push(Event { t_ns, kind: Kind::Present, a: 0, b: after / 1000 });
    }

    /// That `PRESENT` returning at `t_ns`.
    pub fn present_end(&mut self, t_ns: u64) {
        let us = t_ns.saturating_sub(self.present_started) / 1000;
        self.ioctl_n += 1;
        self.ioctl_sum_us += us;
        self.ioctl_max_us = self.ioctl_max_us.max(us);
        self.push(Event { t_ns, kind: Kind::PresentEnd, a: 0, b: us });
    }

    pub fn vblank(&mut self, t_ns: u64, seq: u64) {
        self.vblanks += 1;
        self.push(Event { t_ns, kind: Kind::Vblank, a: 0, b: seq });
    }

    /// The latest events, oldest first.
    pub fn events(&self) -> Vec<Event> {
        let n = self.ring.len();
        (0..n).map(|i| self.ring[(self.head + i) % n.max(1)]).collect()
    }

    /// Two `/proc/kdebug` lines: the statistics, and the tail of the ring (`+µs` from its first event; `S<chan>.<seq>` a submission, `D<chan>.<seq>`
    /// seen done, `P<µs after the vblank>` a PRESENT entering, `E<µs>` it returning, `V<seq>` a vblank).
    pub fn render(&self, tail: usize) -> String {
        let mut s = String::from("gpu_pacing:");
        for c in 0..CHANS {
            if self.exec_n[c] > 0 {
                let _ = write!(s, " ch{}=n{}/avg{}us/max{}us", c, self.exec_n[c], self.exec_sum_us[c] / self.exec_n[c], self.exec_max_us[c]);
            }
        }
        let _ = write!(s, " present_n={} after_vblank_2ms_buckets=", self.present_n);
        for (i, h) in self.present_hist.iter().enumerate() {
            let _ = write!(s, "{}{}", if i > 0 { "," } else { "" }, h);
        }
        let _ = write!(s, " present_ioctl_us=avg{}/max{} vblanks={}", self.ioctl_sum_us / self.ioctl_n.max(1), self.ioctl_max_us, self.vblanks);
        let ev = self.events();
        let from = ev.len().saturating_sub(tail);
        s.push_str("\ngpu_pacing_trace:");
        if let Some(first) = ev.get(from) {
            for e in &ev[from..] {
                let dt = e.t_ns.saturating_sub(first.t_ns) / 1000;
                match e.kind {
                    Kind::Submit => { let _ = write!(s, " +{}S{}.{}", dt, e.a, e.b); }
                    Kind::Done => { let _ = write!(s, " +{}D{}.{}", dt, e.a, e.b); }
                    Kind::Present => { let _ = write!(s, " +{}P{}", dt, e.b); }
                    Kind::PresentEnd => { let _ = write!(s, " +{}E{}", dt, e.b); }
                    Kind::Vblank => { let _ = write!(s, " +{}V{}", dt, e.b); }
                }
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_submission_is_timed_by_the_first_look_that_finds_it_done() {
        let mut p = Pacing::new();
        p.submit(1_000_000, 0, 5);
        p.submit(2_000_000, 0, 6);
        p.done(4_000_000, 0, 5); // sees 5 (3 ms), not 6
        p.done(9_500_000, 0, 6); // 6 after 7.5 ms
        let r = p.render(8);
        assert!(r.contains("ch0=n2/avg5250us/max7500us"), "{r}");
        // a second look at the same fence changes nothing
        p.done(20_000_000, 0, 6);
        assert!(p.render(8).contains("ch0=n2/"));
    }

    #[test]
    fn one_look_can_find_several_done_and_other_channels_are_separate() {
        let mut p = Pacing::new();
        p.submit(0, 1, 1);
        p.submit(1_000_000, 1, 2);
        p.submit(0, 2, 1);
        p.done(5_000_000, 1, 2);
        let r = p.render(8);
        assert!(r.contains("ch1=n2/avg4500us/max5000us"), "{r}");
        assert!(!r.contains("ch2="), "channel 2 has not been seen done: {r}");
    }

    #[test]
    fn more_in_flight_than_kept_drops_the_oldest_not_the_newest() {
        let mut p = Pacing::new();
        for i in 0..(PENDING as u64 + 3) {
            p.submit(i * 1_000_000, 0, i);
        }
        p.done(100_000_000, 0, u64::MAX);
        let r = p.render(4);
        assert!(r.contains(&alloc::format!("ch0=n{}/", PENDING)), "{r}");
        // the newest survived: its latency is 100 ms - 10 ms = 90 ms, the max over the kept ones is the oldest kept (3 ms submitted)
        assert!(r.contains("max97000us"), "{r}");
    }

    #[test]
    fn present_arrival_is_bucketed_by_two_milliseconds_after_the_vblank() {
        assert_eq!(bucket(0), 0);
        assert_eq!(bucket(1_999_999), 0);
        assert_eq!(bucket(2_000_000), 1);
        assert_eq!(bucket(9_100_000), 4);
        assert_eq!(bucket(17_999_999), 8);
        assert_eq!(bucket(18_000_000), 9);
        assert_eq!(bucket(10_000_000_000), PRESENT_BUCKETS - 1, "the last bucket is open");
        let mut p = Pacing::new();
        p.present_begin(1_011_000_000, 1_000_000_000); // 11 ms after
        p.present_end(1_012_500_000);
        p.present_begin(2_000_500_000, 2_000_000_000); // 0.5 ms after
        p.present_end(2_000_900_000);
        let r = p.render(8);
        assert!(r.contains("present_n=2 after_vblank_2ms_buckets=1,0,0,0,0,1,0,0,0,0"), "{r}");
        assert!(r.contains("present_ioctl_us=avg950/max1500"), "{r}");
    }

    #[test]
    fn no_vblank_seen_yet_counts_as_zero_not_as_an_enormous_gap() {
        let mut p = Pacing::new();
        p.present_begin(5_000_000_000, 0);
        assert!(p.render(1).contains("after_vblank_2ms_buckets=1,0,"));
    }

    #[test]
    fn the_ring_keeps_the_latest_events_in_order() {
        let mut p = Pacing::new();
        for i in 0..(RING as u64 + 10) {
            p.vblank(i * 1000, i);
        }
        let ev = p.events();
        assert_eq!(ev.len(), RING);
        assert_eq!(ev.first().unwrap().b, 10);
        assert_eq!(ev.last().unwrap().b, RING as u64 + 9);
        assert!(ev.windows(2).all(|w| w[0].b + 1 == w[1].b));
        let r = p.render(3);
        assert!(r.ends_with("+0V199 +1V200 +2V201"), "{r}");
        assert!(r.contains(&alloc::format!("vblanks={}", RING + 10)));
    }

    #[test]
    fn reset_forgets_everything_and_a_channel_past_the_table_is_ignored() {
        let mut p = Pacing::new();
        p.submit(0, 0, 1);
        p.done(1_000_000, 0, 1);
        p.vblank(5, 1);
        p.submit(0, CHANS + 3, 1);
        p.done(1, CHANS + 3, 1);
        p.submit(0, CHANS, 1); // exactly one past the table: the boundary
        p.done(1, CHANS, 1);
        p.submit(0, CHANS - 1, 1); // the last channel that has a slot
        p.done(2_000, CHANS - 1, 1);
        assert!(p.render(1).contains(&alloc::format!("ch{}=n1/", CHANS - 1)));
        p.reset();
        let r = p.render(8);
        assert!(!r.contains("ch0="), "{r}");
        assert!(r.contains("present_n=0") && r.contains("vblanks=0") && p.events().is_empty());
    }
}
