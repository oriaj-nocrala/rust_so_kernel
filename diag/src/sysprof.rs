//! Per-process syscall profile: how many calls of each syscall, the wall
//! time they took (time blocked included) and how many were restarted. Built so a
//! test run can say where its time went without another debugging round.
//!
//! The kernel adapter (`kernel/src/sysprof.rs`) owns one [`Profile`] per
//! pid and calls [`Profile::enter`] / [`Profile::exit`] around each
//! syscall. A call that blocks never reaches `exit` on its first pass: it
//! rewinds `rip` over the `syscall` instruction, parks, and enters again
//! when woken. So a call entering with the same number from the same user
//! address as the one in flight is that call's continuation, not a new one.
//! A call in flight that is followed by a *different* one ended without
//! passing `exit` (a signal handler ran, EINTR): it is recorded then, with
//! the time up to that point.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

include!("sysprof_names.rs");

/// Syscall numbers counted; larger ones go to the last slot.
pub const MAX_NR: usize = 512;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stat {
    pub calls: u64,
    pub total_ns: u64,
    pub max_ns: u64,
    /// Calls that blocked by rewinding (`rip -= 2`, re-executed when woken:
    /// pipes, sockets, futex...). A call that sleeps inside the kernel and
    /// returns (nanosleep, wait4) is not counted here; its wait is in its time.
    pub restarted: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Inflight {
    nr: u64,
    rip: u64,
    t0: u64,
    passes: u32,
}

pub struct Profile {
    pub name: [u8; 16],
    /// How many processes (threads count) were merged into this one.
    pub procs: u32,
    stats: [Stat; MAX_NR],
    inflight: Option<Inflight>,
}

impl Profile {
    pub fn new(name: [u8; 16]) -> Profile {
        Profile { name, procs: 1, stats: [Stat::default(); MAX_NR], inflight: None }
    }

    /// A new profile built in place on the heap. What the kernel uses: at
    /// opt-level 0, `Box::new(Profile::new(..))` puts the 16 KiB on the
    /// kernel stack first, maybe more than once.
    pub fn new_boxed(name: [u8; 16]) -> alloc::boxed::Box<Profile> {
        let mut b = alloc::boxed::Box::<Profile>::new_uninit();
        let p = b.as_mut_ptr();
        // SAFETY: every field is written before `assume_init`, each through
        // a raw pointer into the allocation (no reference to uninit memory).
        unsafe {
            core::ptr::addr_of_mut!((*p).name).write(name);
            core::ptr::addr_of_mut!((*p).procs).write(1);
            core::ptr::addr_of_mut!((*p).inflight).write(None);
            let stats = core::ptr::addr_of_mut!((*p).stats) as *mut Stat;
            for i in 0..MAX_NR {
                stats.add(i).write(Stat::default());
            }
            b.assume_init()
        }
    }

    /// A syscall entry: `nr` called from user address `rip` at `now` ns.
    pub fn enter(&mut self, nr: u64, rip: u64, now: u64) {
        if let Some(f) = &mut self.inflight {
            if f.nr == nr && f.rip == rip {
                f.passes += 1;
                return;
            }
            self.finish(now);
        }
        self.inflight = Some(Inflight { nr, rip, t0: now, passes: 1 });
    }

    /// A syscall returning to user mode at `now` ns.
    pub fn exit(&mut self, now: u64) {
        self.finish(now);
    }

    fn finish(&mut self, now: u64) {
        let Some(f) = self.inflight.take() else { return };
        let ns = now.saturating_sub(f.t0);
        let s = &mut self.stats[(f.nr as usize).min(MAX_NR - 1)];
        s.calls += 1;
        s.total_ns += ns;
        s.max_ns = s.max_ns.max(ns);
        if f.passes > 1 {
            s.restarted += 1;
        }
    }

    /// Adds `other`'s counts to this one (a call still in flight in
    /// `other` is not counted).
    pub fn merge(&mut self, other: &Profile) {
        self.procs += other.procs;
        for (a, b) in self.stats.iter_mut().zip(other.stats.iter()) {
            a.calls += b.calls;
            a.total_ns += b.total_ns;
            a.max_ns = a.max_ns.max(b.max_ns);
            a.restarted += b.restarted;
        }
    }

    /// The call in progress: `(nr, ns so far, passes)` (passes > 1: it was
    /// restarted after blocking). What a hung process is stuck in.
    pub fn in_flight(&self, now: u64) -> Option<(u64, u64, u32)> {
        self.inflight.map(|f| (f.nr, now.saturating_sub(f.t0), f.passes))
    }

    pub fn stat(&self, nr: u64) -> Stat {
        self.stats[(nr as usize).min(MAX_NR - 1)]
    }

    pub fn calls(&self) -> u64 {
        self.stats.iter().map(|s| s.calls).sum()
    }

    pub fn total_ns(&self) -> u64 {
        self.stats.iter().map(|s| s.total_ns).sum()
    }

    /// The `n` syscalls with the most total time, most first.
    pub fn top(&self, n: usize) -> Vec<(u64, Stat)> {
        let mut v: Vec<(u64, Stat)> =
            self.stats.iter().enumerate().filter(|(_, s)| s.calls > 0).map(|(i, s)| (i as u64, *s)).collect();
        v.sort_by(|a, b| b.1.total_ns.cmp(&a.1.total_ns).then(a.0.cmp(&b.0)));
        v.truncate(n);
        v
    }

    pub fn name_str(&self) -> &str {
        let end = self.name.iter().position(|&b| b == 0).unwrap_or(16);
        core::str::from_utf8(&self.name[..end]).unwrap_or("?")
    }
}

pub fn syscall_name(nr: u64) -> &'static str {
    match SYSCALL_NAMES.get(nr as usize) {
        Some(n) if !n.is_empty() => n,
        _ => "?",
    }
}

/// Merges profiles that share a name (a program's processes and threads).
pub fn merge_by_name(profiles: Vec<alloc::boxed::Box<Profile>>) -> Vec<alloc::boxed::Box<Profile>> {
    let mut out: Vec<alloc::boxed::Box<Profile>> = Vec::new();
    for p in profiles {
        match out.iter_mut().find(|q| q.name == p.name) {
            Some(q) => q.merge(&p),
            None => out.push(p),
        }
    }
    out
}

/// One line for a process with a call in progress (`None` without one).
pub fn render_in_flight(pid: usize, p: &Profile, now: u64) -> Option<String> {
    let (nr, ns, passes) = p.in_flight(now)?;
    Some(alloc::format!(
        "sysprof in-flight pid {} ({}): {} for {} (passes {})\n",
        pid, p.name_str(), syscall_name(nr), ms(ns), passes
    ))
}

fn ms(ns: u64) -> String {
    alloc::format!("{}.{:01}ms", ns / 1_000_000, ns / 100_000 % 10)
}

/// The report: one block per program, most syscall time first, each with
/// its `top` syscalls.
pub fn render(profiles: &[alloc::boxed::Box<Profile>], top: usize) -> String {
    let mut order: Vec<&Profile> = profiles.iter().map(|b| &**b).collect();
    order.sort_by(|a, b| b.total_ns().cmp(&a.total_ns()));
    let mut out = String::new();
    for p in order {
        let _ = writeln!(
            out,
            "sysprof {} procs={} calls={} time={}",
            p.name_str(), p.procs, p.calls(), ms(p.total_ns())
        );
        for (nr, s) in p.top(top) {
            let _ = writeln!(
                out,
                "  {:<16} n={:<6} total={:<10} max={:<10} restarted={}",
                syscall_name(nr), s.calls, ms(s.total_ns), ms(s.max_ns), s.restarted
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::boxed::Box;

    fn name(s: &str) -> [u8; 16] {
        let mut n = [0u8; 16];
        n[..s.len()].copy_from_slice(s.as_bytes());
        n
    }

    #[test]
    fn a_plain_call_is_its_wall_time() {
        let mut p = Profile::new(name("t"));
        p.enter(0, 0x1000, 100);
        p.exit(350);
        assert_eq!(p.stat(0), Stat { calls: 1, total_ns: 250, max_ns: 250, restarted: 0 });
    }

    #[test]
    fn a_blocked_call_reenters_and_counts_once_with_its_whole_wait() {
        let mut p = Profile::new(name("t"));
        p.enter(35, 0x2000, 1_000);
        // Parked, woken, the rewound `syscall` enters again from the same place.
        p.enter(35, 0x2000, 5_000);
        p.exit(9_000);
        assert_eq!(p.stat(35), Stat { calls: 1, total_ns: 8_000, max_ns: 8_000, restarted: 1 });
    }

    #[test]
    fn the_same_syscall_from_elsewhere_is_a_new_call() {
        let mut p = Profile::new(name("t"));
        p.enter(0, 0x1000, 0);
        p.enter(0, 0x1100, 40); // the first never returned (a handler ran)
        p.exit(100);
        assert_eq!(p.stat(0), Stat { calls: 2, total_ns: 100, max_ns: 60, restarted: 0 });
    }

    #[test]
    fn an_interrupted_call_is_recorded_when_the_next_one_starts() {
        let mut p = Profile::new(name("t"));
        p.enter(0, 0x1000, 0);
        p.enter(15, 0x3000, 70); // rt_sigreturn after the handler
        p.exit(80);
        assert_eq!(p.stat(0).calls, 1);
        assert_eq!(p.stat(0).total_ns, 70);
        assert_eq!(p.stat(15).total_ns, 10);
        assert_eq!(p.calls(), 2);
    }

    #[test]
    fn exit_without_enter_and_huge_numbers_are_harmless() {
        let mut p = Profile::new(name("t"));
        p.exit(10);
        p.enter(100_000, 0, 0);
        p.exit(5);
        assert_eq!(p.stat(MAX_NR as u64 - 1).calls, 1);
        assert_eq!(p.calls(), 1);
    }

    #[test]
    fn new_boxed_is_new() {
        let b = Profile::new_boxed(name("z"));
        let n = Profile::new(name("z"));
        assert_eq!((b.name, b.procs, b.inflight, b.calls()), (n.name, n.procs, n.inflight, n.calls()));
        assert!(b.stats.iter().all(|s| *s == Stat::default()));
    }

    #[test]
    fn merge_and_top() {
        let mut a = Box::new(Profile::new(name("x")));
        a.enter(1, 0, 0);
        a.exit(10);
        let mut b = Box::new(Profile::new(name("x")));
        b.enter(1, 0, 0);
        b.exit(30);
        b.enter(0, 0, 0);
        b.exit(100);
        let mut c = Box::new(Profile::new(name("y")));
        c.enter(2, 0, 0);
        c.exit(1);
        let m = merge_by_name(alloc::vec![a, b, c]);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].procs, 2);
        assert_eq!(m[0].stat(1), Stat { calls: 2, total_ns: 40, max_ns: 30, restarted: 0 });
        assert_eq!(m[0].top(1), alloc::vec![(0, Stat { calls: 1, total_ns: 100, max_ns: 100, restarted: 0 })]);
    }

    #[test]
    fn in_flight_names_the_call_in_progress() {
        let mut p = Profile::new(name("hang"));
        assert_eq!(render_in_flight(7, &p, 0), None);
        p.enter(0, 0x10, 1_000_000);
        p.enter(0, 0x10, 2_000_000);
        assert_eq!(p.in_flight(5_000_000), Some((0, 4_000_000, 2)));
        assert_eq!(
            render_in_flight(7, &p, 5_000_000).unwrap(),
            "sysprof in-flight pid 7 (hang): read for 4.0ms (passes 2)\n"
        );
        p.exit(6_000_000);
        assert_eq!(p.in_flight(7_000_000), None);
    }

    #[test]
    fn names_and_render() {
        assert_eq!(syscall_name(0), "read");
        assert_eq!(syscall_name(35), "nanosleep");
        assert_eq!(syscall_name(403), "kdebug_ctl");
        assert_eq!(syscall_name(434), "pidfd_open");
        assert_eq!(syscall_name(350), "?");
        assert_eq!(syscall_name(9999), "?");
        let mut p = Box::new(Profile::new(name("prog")));
        p.enter(35, 0, 0);
        p.enter(35, 0, 1);
        p.exit(2_500_000);
        let r = render(&[p], 3);
        assert!(r.starts_with("sysprof prog procs=1 calls=1 time=2.5ms\n"), "{}", r);
        assert!(r.contains("  nanosleep        n=1      total=2.5ms"), "{}", r);
        assert!(r.contains("restarted=1"), "{}", r);
    }
}
