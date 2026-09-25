//! Job-control rules: who may read, write and reconfigure a terminal.
//!
//! Linux's `__tty_check_change` and `job_control`, as pure functions. The
//! kernel gathers the facts (the caller's group and session, whether the
//! terminal is its controlling one, whether the signal would be ignored or
//! blocked, whether its group is orphaned) and acts on the verdict; nothing
//! here knows what a process is.

use crate::{Signal, TtyError};

/// The facts about one access to a terminal.
#[derive(Clone, Copy, Debug)]
pub struct Access {
    /// The terminal is the caller's controlling terminal. Only then does
    /// job control apply at all: any other process that has it open reads
    /// and writes freely.
    pub is_ctty: bool,
    /// The caller's process group.
    pub pgid: u32,
    /// The terminal's foreground process group, if one was ever set.
    pub foreground: Option<u32>,
    /// The signal this check would send (`SIGTTIN` for a read, `SIGTTOU`
    /// otherwise) is ignored or blocked by the caller.
    pub signal_ignored_or_blocked: bool,
    /// The caller's process group is orphaned: nobody left to continue it
    /// if it stopped.
    pub orphaned: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    /// Send this signal to the caller's process group and restart the
    /// call: after `SIGCONT` it runs again and checks again.
    Stop(Signal),
    Error(TtyError),
}

impl Access {
    fn in_background(&self) -> bool {
        self.is_ctty && self.foreground.is_some_and(|fg| fg != self.pgid)
    }
}

/// A `read` of the terminal.
pub fn check_read(a: &Access) -> Verdict {
    if !a.in_background() {
        return Verdict::Allow;
    }
    if a.signal_ignored_or_blocked || a.orphaned {
        return Verdict::Error(TtyError::Io);
    }
    Verdict::Stop(Signal::Ttin)
}

/// A `write` with `TOSTOP` set, or a change to the terminal (`TCSETS*`,
/// `TIOCSPGRP`, `TIOCSWINSZ`) — these apply whether or not `TOSTOP` is.
/// Unlike a read, an ignored or blocked `SIGTTOU` lets it through.
pub fn check_change(a: &Access) -> Verdict {
    if !a.in_background() {
        return Verdict::Allow;
    }
    if a.signal_ignored_or_blocked {
        return Verdict::Allow;
    }
    if a.orphaned {
        return Verdict::Error(TtyError::Io);
    }
    Verdict::Stop(Signal::Ttou)
}

/// `tcsetpgrp`: after [`check_change`], whether the new group may become
/// the foreground one. `target_in_session`: a process group with that id
/// exists in the caller's session.
pub fn check_set_foreground(is_ctty: bool, target_in_session: bool) -> Result<(), TtyError> {
    if !is_ctty {
        return Err(TtyError::NotTty);
    }
    if !target_in_session {
        return Err(TtyError::Perm);
    }
    Ok(())
}

/// `TIOCSCTTY`: whether a caller may make this terminal its controlling
/// one. `tty_session` is the session the terminal already controls, if
/// any; `steal` is the ioctl's argument being 1, which Linux honours for
/// `CAP_SYS_ADMIN` — every process here, having no uid model. Returns
/// `Ok(true)` when it is already the caller's (nothing to do).
pub fn check_set_ctty(
    caller_pid: u32,
    caller_sid: u32,
    caller_has_ctty: bool,
    tty_session: Option<u32>,
    steal: bool,
) -> Result<bool, TtyError> {
    let leader = caller_pid == caller_sid;
    if leader && tty_session == Some(caller_sid) {
        return Ok(true);
    }
    if !leader || caller_has_ctty {
        return Err(TtyError::Perm);
    }
    if tty_session.is_some() && !steal {
        return Err(TtyError::Perm);
    }
    Ok(false)
}

/// Whether opening the terminal makes it the opener's controlling one:
/// a session leader without one, a terminal no session has, and no
/// `O_NOCTTY`.
pub fn acquires_on_open(
    caller_pid: u32,
    caller_sid: u32,
    caller_has_ctty: bool,
    tty_session: Option<u32>,
    noctty: bool,
) -> bool {
    !noctty && caller_pid == caller_sid && !caller_has_ctty && tty_session.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bg() -> Access {
        Access {
            is_ctty: true,
            pgid: 20,
            foreground: Some(10),
            signal_ignored_or_blocked: false,
            orphaned: false,
        }
    }

    #[test]
    fn the_foreground_group_reads_and_writes() {
        let a = Access { pgid: 10, ..bg() };
        assert_eq!(check_read(&a), Verdict::Allow);
        assert_eq!(check_change(&a), Verdict::Allow);
    }

    #[test]
    fn a_terminal_that_is_not_the_callers_is_not_job_controlled() {
        let a = Access { is_ctty: false, ..bg() };
        assert_eq!(check_read(&a), Verdict::Allow);
        assert_eq!(check_change(&a), Verdict::Allow);
    }

    #[test]
    fn no_foreground_group_means_no_background() {
        let a = Access { foreground: None, ..bg() };
        assert_eq!(check_read(&a), Verdict::Allow);
    }

    #[test]
    fn background_read_stops_with_sigttin() {
        assert_eq!(check_read(&bg()), Verdict::Stop(Signal::Ttin));
    }

    #[test]
    fn background_read_with_sigttin_ignored_or_orphaned_is_eio() {
        let a = Access { signal_ignored_or_blocked: true, ..bg() };
        assert_eq!(check_read(&a), Verdict::Error(TtyError::Io));
        let a = Access { orphaned: true, ..bg() };
        assert_eq!(check_read(&a), Verdict::Error(TtyError::Io));
    }

    #[test]
    fn background_change_stops_with_sigttou_unless_ignored() {
        assert_eq!(check_change(&bg()), Verdict::Stop(Signal::Ttou));
        // ash ignores SIGTTOU around its own tcsetpgrp: it must go through.
        let a = Access { signal_ignored_or_blocked: true, ..bg() };
        assert_eq!(check_change(&a), Verdict::Allow);
        let a = Access { orphaned: true, ..bg() };
        assert_eq!(check_change(&a), Verdict::Error(TtyError::Io));
    }

    #[test]
    fn set_foreground_rules() {
        assert_eq!(check_set_foreground(false, true), Err(TtyError::NotTty));
        assert_eq!(check_set_foreground(true, false), Err(TtyError::Perm));
        assert_eq!(check_set_foreground(true, true), Ok(()));
    }

    #[test]
    fn set_ctty_rules() {
        // A session leader without a terminal takes a free one.
        assert_eq!(check_set_ctty(5, 5, false, None, false), Ok(false));
        // Not a leader.
        assert_eq!(check_set_ctty(6, 5, false, None, false), Err(TtyError::Perm));
        // Already has another.
        assert_eq!(check_set_ctty(5, 5, true, None, false), Err(TtyError::Perm));
        // Already this one.
        assert_eq!(check_set_ctty(5, 5, true, Some(5), false), Ok(true));
        // Another session's, unless stolen.
        assert_eq!(check_set_ctty(5, 5, false, Some(9), false), Err(TtyError::Perm));
        assert_eq!(check_set_ctty(5, 5, false, Some(9), true), Ok(false));
    }

    #[test]
    fn open_acquires_only_for_a_bare_session_leader() {
        assert!(acquires_on_open(5, 5, false, None, false));
        assert!(!acquires_on_open(5, 5, false, None, true), "O_NOCTTY");
        assert!(!acquires_on_open(6, 5, false, None, false), "not a leader");
        assert!(!acquires_on_open(5, 5, true, None, false), "has one");
        assert!(!acquires_on_open(5, 5, false, Some(9), false), "taken");
    }
}
