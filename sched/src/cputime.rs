//! CPU time accounting: what a timer tick is charged to, per CPU and per
//! process, and `/proc/stat`'s `cpu` lines in both directions.
//!
//! Tick-based, exactly as Linux does it without `CONFIG_VIRT_CPU_ACCOUNTING`
//! (`account_process_tick`): every tick, on every CPU, looks at what it
//! interrupted and charges one whole tick to one bucket. The interrupted
//! code was in ring 3 → user; in ring 0 on the CPU's idle process → idle;
//! in ring 0 on anything else (a syscall, a fault, the kernel on a process's
//! behalf) → system. That is a sample, not a measurement — a process that
//! always sleeps just before the tick is never charged — and it is the same
//! sample `top`, `ps` and `times(2)` read on Linux.
//!
//! The unit is the tick, and a tick is `1 / USER_HZ` s: the timer runs at
//! 100 Hz, which is also the `USER_HZ` Linux exports to userspace
//! (`sysconf(_SC_CLK_TCK)`), so `/proc` values need no conversion.
//!
//! Buckets this kernel has nothing to put in (`nice` — there is no nice
//! value —, `iowait`, `irq`, `softirq`, `steal`, `guest`) exist so the
//! lines have Linux's ten columns, and read 0.
//!
//! The same module parses the lines back ([`parse_stat_line`]) for the CPU
//! monitor: one definition of the format, with its round trip tested here.

use ::core::fmt::{self, Write};

/// Ticks per second as userspace sees them (`sysconf(_SC_CLK_TCK)`). Equal
/// to the kernel's own tick rate, so no scaling happens anywhere.
pub const USER_HZ: u64 = 100;

/// Where one tick goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TickKind {
    User,
    System,
    Idle,
}

/// Classify a tick from what it interrupted: `user_mode` is the interrupted
/// frame's privilege (CS RPL 3), `idle_task` whether the CPU was running its
/// idle process. User first: an idle process never runs in ring 3.
pub fn classify(user_mode: bool, idle_task: bool) -> TickKind {
    if user_mode {
        TickKind::User
    } else if idle_task {
        TickKind::Idle
    } else {
        TickKind::System
    }
}

/// One CPU's (or the sum of all CPUs') time, in ticks, in the column order
/// of a Linux `/proc/stat` `cpu` line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuTimes {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub steal: u64,
    pub guest: u64,
    pub guest_nice: u64,
}

impl CpuTimes {
    pub fn charge(&mut self, kind: TickKind) {
        match kind {
            TickKind::User => self.user += 1,
            TickKind::System => self.system += 1,
            TickKind::Idle => self.idle += 1,
        }
    }

    /// Every tick accounted. `guest`/`guest_nice` are left out: Linux
    /// already counts guest time inside `user`/`nice`, and `top` sums the
    /// first eight columns the same way.
    pub fn total(&self) -> u64 {
        self.user + self.nice + self.system + self.idle + self.iowait + self.irq + self.softirq + self.steal
    }

    /// Ticks the CPU spent doing something (`total` minus idle and iowait).
    pub fn busy(&self) -> u64 {
        self.total() - self.idle - self.iowait
    }

    /// Column-wise sum, for the aggregate `cpu` line.
    pub fn add(&mut self, o: &CpuTimes) {
        self.user += o.user;
        self.nice += o.nice;
        self.system += o.system;
        self.idle += o.idle;
        self.iowait += o.iowait;
        self.irq += o.irq;
        self.softirq += o.softirq;
        self.steal += o.steal;
        self.guest += o.guest;
        self.guest_nice += o.guest_nice;
    }

    /// What happened between `earlier` and `self`. Saturating: counters are
    /// monotonic, but a reader that pairs samples from two different CPUs
    /// (or two boots) must get zeros, not a wrapped `u64`.
    pub fn since(&self, earlier: &CpuTimes) -> CpuTimes {
        CpuTimes {
            user: self.user.saturating_sub(earlier.user),
            nice: self.nice.saturating_sub(earlier.nice),
            system: self.system.saturating_sub(earlier.system),
            idle: self.idle.saturating_sub(earlier.idle),
            iowait: self.iowait.saturating_sub(earlier.iowait),
            irq: self.irq.saturating_sub(earlier.irq),
            softirq: self.softirq.saturating_sub(earlier.softirq),
            steal: self.steal.saturating_sub(earlier.steal),
            guest: self.guest.saturating_sub(earlier.guest),
            guest_nice: self.guest_nice.saturating_sub(earlier.guest_nice),
        }
    }

    fn fields(&self) -> [u64; 10] {
        [
            self.user, self.nice, self.system, self.idle, self.iowait,
            self.irq, self.softirq, self.steal, self.guest, self.guest_nice,
        ]
    }
}

/// `part` as thousandths of `whole`, rounded; 0 when `whole` is 0.
pub fn permille(part: u64, whole: u64) -> u32 {
    if whole == 0 {
        return 0;
    }
    ((part.min(whole) * 1000 + whole / 2) / whole) as u32
}

/// Write one `/proc/stat` line: `cpu  …` for the aggregate (`cpu: None`,
/// two spaces, as Linux prints it), `cpuN …` for CPU `N`.
pub fn write_stat_line(out: &mut impl Write, cpu: Option<usize>, t: &CpuTimes) -> fmt::Result {
    match cpu {
        None => out.write_str("cpu ")?,
        Some(n) => write!(out, "cpu{}", n)?,
    }
    for v in t.fields() {
        write!(out, " {}", v)?;
    }
    out.write_char('\n')
}

/// Parse one `/proc/stat` line. `Some((None, t))` for the aggregate `cpu`
/// line, `Some((Some(n), t))` for `cpuN`, `None` for any other line
/// (`intr`, `ctxt`, …) or a malformed one. Accepts from 4 columns (Linux
/// before 2.6) to 10; missing columns read 0, extra ones are ignored.
pub fn parse_stat_line(line: &str) -> Option<(Option<usize>, CpuTimes)> {
    let mut words = line.split_ascii_whitespace();
    let label = words.next()?;
    let rest = label.strip_prefix("cpu")?;
    let cpu = if rest.is_empty() { None } else { Some(rest.parse::<usize>().ok()?) };

    let mut f = [0u64; 10];
    let mut n = 0;
    for w in words.take(10) {
        f[n] = w.parse().ok()?;
        n += 1;
    }
    if n < 4 {
        return None;
    }
    Some((
        cpu,
        CpuTimes {
            user: f[0], nice: f[1], system: f[2], idle: f[3], iowait: f[4],
            irq: f[5], softirq: f[6], steal: f[7], guest: f[8], guest_nice: f[9],
        },
    ))
}

/// One process's CPU time, in ticks: `utime`/`stime` its own, `cutime`/
/// `cstime` its waited-for descendants' — fields 14-17 of `/proc/<pid>/stat`
/// and the four members of `struct tms`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProcTimes {
    pub utime: u64,
    pub stime: u64,
    pub cutime: u64,
    pub cstime: u64,
}

impl ProcTimes {
    /// Charge a tick this process was running for. Idle ticks never land
    /// on a real process (only the idle process is classified idle).
    pub fn charge(&mut self, kind: TickKind) {
        match kind {
            TickKind::User => self.utime += 1,
            TickKind::System => self.stime += 1,
            TickKind::Idle => {}
        }
    }

    /// A child has been waited for (POSIX `times()`: "the times of
    /// terminated children … for which wait() has returned"): its own time
    /// and everything *it* collected from its children move to `c*time`.
    /// A child that was never waited for contributes nothing, as on Linux.
    pub fn reap(&mut self, child: &ProcTimes) {
        self.cutime += child.utime + child.cutime;
        self.cstime += child.stime + child.cstime;
    }

    /// A thread of this process has exited: its time is the process's own
    /// from now on (Linux folds it into the thread group's `signal->utime`).
    pub fn absorb_thread(&mut self, thread: &ProcTimes) {
        self.utime += thread.utime;
        self.stime += thread.stime;
        self.cutime += thread.cutime;
        self.cstime += thread.cstime;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::String;

    #[test]
    fn classify_follows_linux_order() {
        assert_eq!(classify(true, false), TickKind::User);
        assert_eq!(classify(false, true), TickKind::Idle);
        assert_eq!(classify(false, false), TickKind::System);
        // Ring 3 wins even if the caller also says "idle".
        assert_eq!(classify(true, true), TickKind::User);
    }

    #[test]
    fn charging_fills_the_right_buckets() {
        let mut c = CpuTimes::default();
        let mut p = ProcTimes::default();
        for k in [TickKind::User, TickKind::User, TickKind::System, TickKind::Idle] {
            c.charge(k);
            p.charge(k);
        }
        assert_eq!((c.user, c.system, c.idle, c.total(), c.busy()), (2, 1, 1, 4, 3));
        assert_eq!((p.utime, p.stime), (2, 1));
    }

    #[test]
    fn line_round_trips_and_matches_linux_spacing() {
        let t = CpuTimes { user: 10, system: 3, idle: 987, steal: 1, guest: 2, ..Default::default() };
        let mut s = String::new();
        write_stat_line(&mut s, None, &t).unwrap();
        assert_eq!(s, "cpu  10 0 3 987 0 0 0 1 2 0\n");
        assert_eq!(parse_stat_line(&s), Some((None, t)));

        let mut s = String::new();
        write_stat_line(&mut s, Some(23), &t).unwrap();
        assert_eq!(s, "cpu23 10 0 3 987 0 0 0 1 2 0\n");
        assert_eq!(parse_stat_line(&s), Some((Some(23), t)));
    }

    #[test]
    fn parses_a_real_linux_line_and_an_old_four_column_one() {
        let (cpu, t) = parse_stat_line("cpu3 38529 12 9180 6011720 1830 0 316 0 0 0").unwrap();
        assert_eq!(cpu, Some(3));
        assert_eq!((t.user, t.nice, t.system, t.idle, t.iowait, t.softirq), (38529, 12, 9180, 6011720, 1830, 316));

        let (cpu, t) = parse_stat_line("cpu 1 2 3 4").unwrap();
        assert_eq!(cpu, None);
        assert_eq!(t.total(), 10);
    }

    #[test]
    fn rejects_what_is_not_a_cpu_line() {
        for l in ["intr 12 0 1", "ctxt 5", "cpuX 1 2 3 4", "cpu0 1 2 3", "cpu0 1 2 x 4", "", "cpu"] {
            assert_eq!(parse_stat_line(l), None, "{l:?}");
        }
    }

    #[test]
    fn since_saturates_and_permille_rounds() {
        let a = CpuTimes { user: 5, idle: 100, ..Default::default() };
        let b = CpuTimes { user: 8, idle: 197, ..Default::default() };
        let d = b.since(&a);
        assert_eq!((d.user, d.idle, d.total()), (3, 97, 100));
        assert_eq!(a.since(&b), CpuTimes::default());
        assert_eq!(permille(d.busy(), d.total()), 30);
        assert_eq!(permille(1, 3), 333);
        assert_eq!(permille(2, 3), 667);
        assert_eq!(permille(5, 0), 0);
        assert_eq!(permille(9, 4), 1000);
    }

    #[test]
    fn reap_collects_grandchildren_through_the_child() {
        let grandchild = ProcTimes { utime: 7, stime: 1, ..Default::default() };
        let mut child = ProcTimes { utime: 3, stime: 2, ..Default::default() };
        child.reap(&grandchild);
        let mut parent = ProcTimes { utime: 1, ..Default::default() };
        parent.reap(&child);
        assert_eq!(parent, ProcTimes { utime: 1, stime: 0, cutime: 10, cstime: 3 });
    }

    #[test]
    fn a_dead_thread_becomes_the_process_own_time() {
        let mut p = ProcTimes { utime: 4, stime: 1, ..Default::default() };
        p.absorb_thread(&ProcTimes { utime: 6, stime: 2, ..Default::default() });
        assert_eq!((p.utime, p.stime, p.cutime), (10, 3, 0));
    }
}
