// kernel/src/sysprof.rs
//
// Syscall profile per program: `kdebug sysprof on`, run something, `cat
// /proc/sysprof`. Says how many calls of each syscall a program made, the
// wall time they took (time blocked included), the slowest one and how many
// were restarted after blocking. `scripts/run-abi-suite.sh` turns it on and
// prints it after every test, so a slow or stuck test names its syscall
// without another round.
//
// The counting is `diag::sysprof` (host-tested); this file keeps one
// profile per pid, hooks syscall entry and return (`syscall_handler_asm`)
// and exec (a new image is a new program), and renders. Off by default: one
// relaxed load per syscall. When on, every syscall takes `LIVE` (an
// `IrqMutex`: inserting allocates). A pid's 16 KiB profile is allocated
// outside it.
// Lock order: `LIVE`/`DONE` are leaves; the scheduler lock (for a name) is
// never taken while holding them.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use diag::sysprof::{self, Profile};
use diag::IrqMutex;

use crate::allocator::KernelIrq;

static ENABLED: AtomicBool = AtomicBool::new(false);
/// Profiles of processes that may still be running, by pid.
static LIVE: IrqMutex<BTreeMap<usize, Box<Profile>>, KernelIrq> = IrqMutex::new(BTreeMap::new());
/// Finished ones (the process exited or exec'd), merged by name.
static DONE: IrqMutex<Vec<Box<Profile>>, KernelIrq> = IrqMutex::new(Vec::new());

/// How many syscalls per program the report lists.
const TOP: usize = 6;

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// Forgets everything counted so far.
pub fn reset() {
    let live = LIVE.with(core::mem::take);
    let done = DONE.with(core::mem::take);
    drop((live, done));
}

fn profile_for(pid: usize) -> bool {
    if LIVE.with(|m| m.contains_key(&pid)) {
        return true;
    }
    // Outside `LIVE`: the name comes from under the scheduler lock.
    let name = crate::process::scheduler::process_name(pid).unwrap_or(*b"?\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0");
    let p = Profile::new_boxed(name);
    // Another CPU cannot have added this pid meanwhile (a pid runs on one CPU at a time); `or_insert` drops a loser anyway.
    let loser = LIVE.with(|m| match m.entry(pid) {
        alloc::collections::btree_map::Entry::Vacant(v) => {
            v.insert(p);
            None
        }
        alloc::collections::btree_map::Entry::Occupied(_) => Some(p),
    });
    drop(loser);
    true
}

/// Syscall entry: `nr` from user address `rip` (the instruction after the
/// `syscall`).
#[inline]
pub fn enter(nr: u64, rip: u64) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let pid = crate::process::scheduler::current_pid_fast();
    let now = crate::time::ktime_get();
    if profile_for(pid) {
        LIVE.with(|m| {
            if let Some(p) = m.get_mut(&pid) {
                p.enter(nr, rip, now);
            }
        });
    }
}

/// Syscall return to user mode.
#[inline]
pub fn exit() {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let pid = crate::process::scheduler::current_pid_fast();
    let now = crate::time::ktime_get();
    LIVE.with(|m| {
        if let Some(p) = m.get_mut(&pid) {
            p.exit(now);
        }
    });
}

/// `pid` replaced its image: what it did so far belongs to the old name.
pub fn exec(pid: usize) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    if let Some(p) = LIVE.with(|m| m.remove(&pid)) {
        retire(p);
    }
}

fn retire(p: Box<Profile>) {
    let leftover = DONE.with(|done| match done.iter_mut().find(|q| q.name == p.name) {
        Some(q) => {
            q.merge(&p);
            Some(p)
        }
        None => {
            done.push(p);
            None
        }
    });
    // A 16 KiB free, outside the lock.
    drop(leftover);
}

/// `/proc/sysprof`: every program seen since the last reset, most syscall
/// time first. Processes that are gone move to the finished list here.
pub fn render() -> String {
    let pids: Vec<usize> = LIVE.with(|m| m.keys().copied().collect());
    for pid in pids {
        if crate::process::scheduler::process_name(pid).is_none() {
            if let Some(p) = LIVE.with(|m| m.remove(&pid)) {
                retire(p);
            }
        }
    }
    let mut acc: Vec<Box<Profile>> = Vec::new();
    let mut add = |p: &Profile| match acc.iter_mut().find(|q| q.name == p.name) {
        Some(q) => q.merge(p),
        None => {
            let mut q = Profile::new_boxed(p.name);
            q.procs = 0;
            q.merge(p);
            acc.push(q);
        }
    };
    DONE.with(|done| done.iter().for_each(|p| add(p)));
    LIVE.with(|live| live.values().for_each(|p| add(p)));
    let mut out = String::new();
    if !ENABLED.load(Ordering::Relaxed) {
        out.push_str("sysprof: off (kdebug sysprof on)\n");
    }
    out.push_str(&sysprof::render(&acc, TOP));
    // Who is waiting in what right now (the `cat` reading this included).
    let now = crate::time::ktime_get();
    LIVE.with(|live| {
        for (pid, p) in live.iter() {
            if let Some(line) = sysprof::render_in_flight(*pid, p, now) {
                out.push_str(&line);
            }
        }
    });
    out
}
