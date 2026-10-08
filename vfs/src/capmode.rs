// vfs/src/capmode.rs
//
// Capability mode (Capsicum's `cap_enter`): once a process enters it, it loses every global
// namespace (paths from `/` or the cwd, pids other than its own, socket addresses, ids) and
// can only act through the descriptors it holds, within their rights (`crate::rights`). The
// mode is inherited by `fork`/`clone`/`exec` and never left.
//
// This file is the audit: one rule per Linux syscall number this kernel implements, written
// down in `docs/reference/syscalls.md`. A number with no rule is denied, so a syscall added
// later is closed in capability mode until someone classifies it (a kernel test checks that
// every implemented number has a rule). The kernel's dispatcher applies the rule; `PathAt`
// calls are finished in the path layer, which walks a relative path beneath the dirfd
// (`MountTable::resolve_at` with `RESOLVE_BENEATH`).

use alloc::collections::VecDeque;
use alloc::string::String;

/// What a syscall may do in capability mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// No global namespace involved (or only the caller's own state).
    Allow,
    /// Names something global: a path from `/` or the cwd, another process, an address, an id.
    Deny,
    /// A `*at` call: only a relative path through a real dirfd, walked beneath it. `AT_FDCWD`
    /// and absolute paths are `ECAPMODE`; a `..` or symlink that leaves the dirfd is
    /// `ENOTCAPABLE`.
    PathAt,
    /// Argument `arg` names a process: only the caller's own (thread group id, or thread id for
    /// `tkill`), or 0 when `zero_ok` ("myself" for `setpgid`/`getpgid`/`getsid`).
    OwnPid { arg: usize, zero_ok: bool },
    /// Allowed while argument `arg` is 0 (`sendto` without a destination address).
    ZeroArg { arg: usize },
    /// `sendmsg` without a `msg_name` (no destination address).
    SendmsgNoName,
}

/// The rule for Linux syscall number `nr` (`None`: not classified, which the kernel treats as `Deny`).
pub fn rule(nr: u64) -> Option<Rule> {
    use Rule::*;
    Some(match nr {
        // I/O and descriptors: governed by the descriptor's rights.
        0 | 1 | 3 | 5 | 7 | 8 | 16 | 17 | 18 | 20 | 32 | 33 | 72 | 77 | 91 | 217 | 292 => Allow,
        // Memory.
        9 | 10 | 11 | 12 => Allow,
        // Signals on oneself, sleeping, time.
        13 | 14 | 15 | 34 | 35 | 36 | 37 | 38 | 130 | 131 | 228 | 229 | 230 => Allow,
        // Own identity and accounting.
        24 | 39 | 98 | 99 | 100 | 102 | 104 | 107 | 108 | 110 | 115 | 118 | 120 | 186 | 218 => Allow,
        // New objects that are not in any namespace.
        22 | 41 | 53 | 213 | 284 | 290 | 291 | 293 | 318 | 319 => Allow,
        // Sockets already held: accept, listen, options, names, shutdown, receive.
        43 | 45 | 47 | 48 | 50 | 51 | 52 | 54 | 55 | 288 => Allow,
        // Processes: create (they inherit the mode), wait for own children, exit.
        56 | 57 | 58 | 60 | 61 | 231 => Allow,
        // Process attributes of oneself; futexes; epoll; the cwd only through a held dirfd.
        81 | 112 | 157 | 158 | 202 | 232 | 233 | 281 => Allow,
        // Flushing caches; a signal through a pidfd (CAP_PDKILL).
        162 | 424 => Allow,
        // This kernel's own: uptime, meminfo, rights, the mode itself.
        400 | 401 | 402 | 405 | 406 | 407 | 408 => Allow,
        // Paths from `/` or the cwd.
        2 | 4 | 6 | 21 | 79 | 80 | 82 | 83 | 84 | 86 | 87 | 88 | 89 | 90 | 404 => Deny,
        // exec by path; a program is run from a descriptor instead (`execveat`, below).
        59 => Deny,
        // Addresses: connect and bind by name.
        42 | 49 => Deny,
        // Ids, reboot, the kernel's debug switches, a pidfd by pid.
        105 | 106 | 113 | 114 | 116 | 117 | 119 | 122 | 123 | 169 | 403 | 434 => Deny,
        // The `*at` family.
        // `execveat` too: `AT_EMPTY_PATH` runs the descriptor itself (`fexecve`), a relative path a file beneath it.
        257 | 258 | 262 | 263 | 264 | 265 | 266 | 267 | 268 | 269 | 280 | 316 | 322 | 332 | 437 | 439 | 452 => PathAt,
        // Another process.
        62 | 200 | 234 => OwnPid { arg: 0, zero_ok: false },
        109 | 121 | 124 | 204 => OwnPid { arg: 0, zero_ok: true },
        // Sending to an address.
        44 => ZeroArg { arg: 4 },
        46 => SendmsgNoName,
        _ => return None,
    })
}

/// Recent capability refusals, readable from outside the process (`/proc/capdenials`,
/// `/proc/<pid>/capdenials`), because the program itself usually only prints "Not permitted"
/// or "unknown error 134" (P1.1: say why). A ring: the oldest entry goes when it is full.
pub struct DenialLog {
    entries: VecDeque<(usize, String)>,
    cap: usize,
}

impl DenialLog {
    pub const fn new(cap: usize) -> Self {
        Self { entries: VecDeque::new(), cap }
    }

    /// Record that `pid` was refused, and why.
    pub fn push(&mut self, pid: usize, why: String) {
        if self.cap == 0 {
            return;
        }
        while self.entries.len() >= self.cap {
            self.entries.pop_front();
        }
        self.entries.push_back((pid, why));
    }

    /// One line per entry, oldest first: `<pid> <why>`. With `pid`, only that process's.
    pub fn render(&self, pid: Option<usize>) -> String {
        let mut out = String::new();
        for (p, why) in &self.entries {
            if pid.is_none_or(|want| want == *p) {
                out.push_str(&alloc::format!("{} {}\n", p, why));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_namespaces_are_denied() {
        for nr in [2, 4, 6, 21, 42, 49, 59, 79, 80, 82, 83, 84, 86, 87, 88, 89, 90, 105, 169, 403, 404, 434] {
            assert_eq!(rule(nr), Some(Rule::Deny), "nr {nr}");
        }
    }

    #[test]
    fn descriptor_calls_are_allowed() {
        for nr in [0, 1, 3, 5, 8, 9, 16, 32, 41, 43, 53, 56, 57, 60, 61, 217, 231, 405, 406, 407, 408] {
            assert_eq!(rule(nr), Some(Rule::Allow), "nr {nr}");
        }
    }

    #[test]
    fn the_at_family_is_path_at() {
        for nr in [257, 258, 262, 263, 264, 265, 266, 267, 268, 269, 280, 316, 322, 332, 437, 439, 452] {
            assert_eq!(rule(nr), Some(Rule::PathAt), "nr {nr}");
        }
    }

    #[test]
    fn other_processes_and_addresses_are_conditional() {
        assert_eq!(rule(62), Some(Rule::OwnPid { arg: 0, zero_ok: false })); // kill
        assert_eq!(rule(109), Some(Rule::OwnPid { arg: 0, zero_ok: true })); // setpgid
        assert_eq!(rule(44), Some(Rule::ZeroArg { arg: 4 })); // sendto's dest_addr
        assert_eq!(rule(46), Some(Rule::SendmsgNoName));
    }

    #[test]
    fn unknown_numbers_have_no_rule() {
        // The kernel denies these: a new syscall stays closed until it is classified.
        for nr in [500, 999, 333, 409, u64::MAX] {
            assert_eq!(rule(nr), None, "nr {nr}");
        }
    }

    #[test]
    fn denial_log_is_a_ring_and_filters_by_pid() {
        let mut log = DenialLog::new(3);
        log.push(10, "open /etc/passwd: ECAPMODE".into());
        log.push(11, "kill 1: ECAPMODE".into());
        log.push(10, "connect: ECAPMODE".into());
        log.push(10, "mkdirat: no CAP_MKDIRAT".into()); // pushes out the first
        assert_eq!(log.render(None), "11 kill 1: ECAPMODE\n10 connect: ECAPMODE\n10 mkdirat: no CAP_MKDIRAT\n");
        assert_eq!(log.render(Some(10)), "10 connect: ECAPMODE\n10 mkdirat: no CAP_MKDIRAT\n");
        assert_eq!(log.render(Some(12)), "");
    }
}
