//! A pseudo-terminal pair: what a master and its slaves see of each other.
//!
//! The master is the terminal emulator's end: what it writes is "typed" (it
//! goes through the line discipline to the slave's readers), and it reads
//! what programs on the slave print (output-processed). The slave is an
//! ordinary terminal to whoever has it open — `ash`, and its jobs.
//!
//! Like [`crate::ldisc`], nothing blocks and every consequence comes back
//! as data ([`Effects`]): who became readable or writable, and which
//! signals go where. The kernel adapter parks and wakes processes and sends
//! the signals; this module decides.
//!
//! Semantics follow Linux's `pty.c`:
//!
//! - The slave cannot be opened until the master unlocks it (`TIOCSPTLCK`),
//!   nor after the master is gone (`EIO` in both cases).
//! - Master closed ("hung up"): slave reads return end of file, slave
//!   writes `EIO`, and the session leader and the foreground group get
//!   `SIGHUP` (and `SIGCONT`, so a stopped one sees it). The session loses
//!   its controlling terminal.
//! - Every slave closed, once one had been opened: master reads return
//!   `EIO` and `poll` reports `POLLHUP` — how a terminal emulator learns its
//!   shell is gone.
//!
//! The pair itself is gone when the master and every slave are closed.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::ldisc::{LineDiscipline, Read};
use crate::termios::{Termios, Winsize};
use crate::{Signal, TtyError};

/// Slave → master bytes held for the master's reader.
pub const OUTPUT_CAPACITY: usize = 4096;

/// Who a signal is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// Every process in this process group.
    Group(u32),
    /// One process (the session leader: its pid is the session id).
    Process(u32),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Wakes {
    pub master_readable: bool,
    pub master_writable: bool,
    pub slave_readable: bool,
    pub slave_writable: bool,
}

impl Wakes {
    pub fn any(&self) -> bool {
        self.master_readable || self.master_writable || self.slave_readable || self.slave_writable
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Effects {
    pub wakes: Wakes,
    pub signals: Vec<(Target, Signal)>,
    /// Every process whose controlling terminal this was must lose it
    /// (the session with this id).
    pub detach_session: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PollMask {
    pub readable: bool,
    pub writable: bool,
    pub hup: bool,
}

/// Which queues `TCFLSH` empties.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flush {
    Input,
    Output,
    Both,
}

pub struct Pty {
    ldisc: LineDiscipline,
    output: VecDeque<u8>,
    master_open: bool,
    slaves: u32,
    slave_opened_once: bool,
    locked: bool,
    winsize: Winsize,
    session: Option<u32>,
    foreground: Option<u32>,
}

impl Default for Pty {
    fn default() -> Self {
        Self::new()
    }
}

impl Pty {
    /// A new pair: master open, slave locked and never opened.
    pub fn new() -> Self {
        Self {
            ldisc: LineDiscipline::default(),
            output: VecDeque::new(),
            master_open: true,
            slaves: 0,
            slave_opened_once: false,
            locked: true,
            winsize: Winsize::default(),
            session: None,
            foreground: None,
        }
    }

    // ── Lifetime ────────────────────────────────────────────────────────

    pub fn set_locked(&mut self, locked: bool) {
        self.locked = locked;
    }

    pub fn locked(&self) -> bool {
        self.locked
    }

    pub fn open_slave(&mut self) -> Result<(), TtyError> {
        if !self.master_open || self.locked {
            return Err(TtyError::Io);
        }
        self.slaves += 1;
        self.slave_opened_once = true;
        Ok(())
    }

    /// A new handle on an already-open slave (`dup`, `fork`).
    pub fn dup_slave(&mut self) {
        self.slaves += 1;
    }

    pub fn close_slave(&mut self) -> Effects {
        debug_assert!(self.slaves > 0);
        self.slaves = self.slaves.saturating_sub(1);
        let mut e = Effects::default();
        if self.slaves == 0 {
            // Master readers see EIO, master writers stop waiting for room
            // nobody will make.
            e.wakes.master_readable = true;
            e.wakes.master_writable = true;
        }
        e
    }

    pub fn close_master(&mut self) -> Effects {
        self.master_open = false;
        self.ldisc.flush_input();
        self.output.clear();
        let mut e = Effects::default();
        if let Some(sid) = self.session.take() {
            e.signals.push((Target::Process(sid), Signal::Hup));
            e.signals.push((Target::Process(sid), Signal::Cont));
            e.detach_session = Some(sid);
        }
        if let Some(fg) = self.foreground.take() {
            e.signals.push((Target::Group(fg), Signal::Hup));
            e.signals.push((Target::Group(fg), Signal::Cont));
        }
        e.wakes.slave_readable = true;
        e.wakes.slave_writable = true;
        e
    }

    pub fn master_open(&self) -> bool {
        self.master_open
    }

    pub fn slaves(&self) -> u32 {
        self.slaves
    }

    /// Nothing holds either end: the pair can be freed.
    pub fn is_dead(&self) -> bool {
        !self.master_open && self.slaves == 0
    }

    fn slaves_gone(&self) -> bool {
        self.slave_opened_once && self.slaves == 0
    }

    // ── Master ──────────────────────────────────────────────────────────

    /// Type `bytes` at the terminal. `Again` when the slave's input side
    /// is full: the writer waits for a slave reader.
    pub fn master_write(&mut self, bytes: &[u8]) -> (Result<usize, TtyError>, Effects) {
        let mut e = Effects::default();
        if bytes.is_empty() {
            return (Ok(0), e);
        }
        if !self.ldisc.accepts_input() {
            return (Err(TtyError::Again), e);
        }
        let r = self.ldisc.receive(bytes);
        e.wakes.slave_readable = r.readable;
        if !r.echo.is_empty() {
            let room = OUTPUT_CAPACITY - self.output.len();
            // Echo that does not fit is lost, as in Linux when the output
            // side is full: the terminal is not reading its own screen.
            let n = r.echo.len().min(room);
            if n > 0 {
                e.wakes.master_readable = self.output.is_empty();
                self.output.extend(&r.echo[..n]);
            }
        }
        if let Some(fg) = self.foreground {
            for sig in r.signals {
                e.signals.push((Target::Group(fg), sig));
            }
        }
        (Ok(r.consumed), e)
    }

    /// What programs on the slave printed. `Again` when there is nothing
    /// yet; `Io` once every slave is closed.
    pub fn master_read(&mut self, buf: &mut [u8]) -> (Result<usize, TtyError>, Effects) {
        let mut e = Effects::default();
        if buf.is_empty() {
            return (Ok(0), e);
        }
        if self.output.is_empty() {
            let err = if self.slaves_gone() { TtyError::Io } else { TtyError::Again };
            return (Err(err), e);
        }
        let was_full = self.output.len() == OUTPUT_CAPACITY;
        let n = buf.len().min(self.output.len());
        for b in buf[..n].iter_mut() {
            *b = self.output.pop_front().unwrap();
        }
        e.wakes.slave_writable = was_full || n > 0;
        (Ok(n), e)
    }

    pub fn poll_master(&self) -> PollMask {
        PollMask {
            readable: !self.output.is_empty(),
            writable: self.ldisc.accepts_input(),
            hup: self.slaves_gone(),
        }
    }

    // ── Slave ───────────────────────────────────────────────────────────

    /// `Read::Eof` also after a hangup. Job control is the caller's, before
    /// this (see [`crate::jobctl`]).
    pub fn slave_read(&mut self, buf: &mut [u8], expired: bool) -> (Read, Effects) {
        let mut e = Effects::default();
        if !self.master_open {
            return (Read::Eof, e);
        }
        let was_full = !self.ldisc.accepts_input();
        let r = self.ldisc.read(buf, expired);
        if let Read::Data(n) = r {
            e.wakes.master_writable = was_full && n > 0;
        }
        (r, e)
    }

    /// Print `bytes`. `Again` when the output side is full (the writer
    /// waits for the master's reader), `Io` after a hangup.
    pub fn slave_write(&mut self, bytes: &[u8]) -> (Result<usize, TtyError>, Effects) {
        let mut e = Effects::default();
        if !self.master_open {
            return (Err(TtyError::Io), e);
        }
        if bytes.is_empty() {
            return (Ok(0), e);
        }
        let room = OUTPUT_CAPACITY - self.output.len();
        let mut out = Vec::new();
        let n = self.ldisc.output(bytes, &mut out, room);
        if n == 0 {
            return (Err(TtyError::Again), e);
        }
        e.wakes.master_readable = self.output.is_empty() && !out.is_empty();
        self.output.extend(out);
        (Ok(n), e)
    }

    pub fn poll_slave(&self) -> PollMask {
        PollMask {
            readable: !self.master_open || self.ldisc.readable(),
            writable: self.master_open && self.output.len() < OUTPUT_CAPACITY,
            hup: !self.master_open,
        }
    }

    // ── Settings, either end ────────────────────────────────────────────

    pub fn termios(&self) -> &Termios {
        self.ldisc.termios()
    }

    pub fn set_termios(&mut self, t: Termios, flush: bool) -> Effects {
        let was_full = !self.ldisc.accepts_input();
        let mut e = Effects::default();
        e.wakes.slave_readable = self.ldisc.set_termios(t, flush);
        e.wakes.master_writable = was_full && self.ldisc.accepts_input();
        e
    }

    pub fn flush(&mut self, which: Flush) -> Effects {
        let mut e = Effects::default();
        if matches!(which, Flush::Input | Flush::Both) {
            e.wakes.master_writable = !self.ldisc.accepts_input();
            self.ldisc.flush_input();
        }
        if matches!(which, Flush::Output | Flush::Both) {
            e.wakes.slave_writable = self.output.len() == OUTPUT_CAPACITY;
            self.output.clear();
        }
        e
    }

    pub fn winsize(&self) -> Winsize {
        self.winsize
    }

    /// `TIOCSWINSZ`: a change sends `SIGWINCH` to the foreground group.
    pub fn set_winsize(&mut self, ws: Winsize) -> Effects {
        let mut e = Effects::default();
        if ws != self.winsize {
            self.winsize = ws;
            if let Some(fg) = self.foreground {
                e.signals.push((Target::Group(fg), Signal::Winch));
            }
        }
        e
    }

    /// `FIONREAD` on the slave: bytes a read could take now.
    pub fn slave_pending(&self) -> usize {
        self.ldisc.ready_len()
    }

    /// `FIONREAD` on the master.
    pub fn master_pending(&self) -> usize {
        self.output.len()
    }

    // ── Job control state ───────────────────────────────────────────────

    /// The session this is the controlling terminal of.
    pub fn session(&self) -> Option<u32> {
        self.session
    }

    /// Becomes (or stops being, with `None`) a session's controlling
    /// terminal. Becoming one makes the leader's group the foreground, as
    /// Linux's `TIOCSCTTY`/open do.
    pub fn set_session(&mut self, sid: Option<u32>, leader_pgid: u32) {
        self.session = sid;
        self.foreground = sid.map(|_| leader_pgid);
    }

    pub fn foreground(&self) -> Option<u32> {
        self.foreground
    }

    pub fn set_foreground(&mut self, pgid: u32) {
        self.foreground = Some(pgid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::termios::{ECHO, ICANON};
    use alloc::vec;

    fn open_pair() -> Pty {
        let mut p = Pty::new();
        p.set_locked(false);
        p.open_slave().unwrap();
        p.set_session(Some(7), 7);
        p
    }

    fn drain_master(p: &mut Pty) -> Vec<u8> {
        let mut buf = vec![0u8; OUTPUT_CAPACITY];
        match p.master_read(&mut buf).0 {
            Ok(n) => buf[..n].to_vec(),
            Err(_) => Vec::new(),
        }
    }

    fn slave_read(p: &mut Pty) -> Read {
        let mut buf = [0u8; 64];
        p.slave_read(&mut buf, false).0
    }

    #[test]
    fn the_slave_is_locked_until_unlocked() {
        let mut p = Pty::new();
        assert_eq!(p.open_slave(), Err(TtyError::Io));
        p.set_locked(false);
        assert_eq!(p.open_slave(), Ok(()));
    }

    #[test]
    fn typing_a_line_echoes_and_delivers_it() {
        let mut p = open_pair();
        let (r, e) = p.master_write(b"echo hola\r");
        assert_eq!(r, Ok(10));
        assert!(e.wakes.slave_readable);
        assert!(e.wakes.master_readable, "echo");
        assert_eq!(drain_master(&mut p), b"echo hola\r\n");
        let mut buf = [0u8; 64];
        let (r, _) = p.slave_read(&mut buf, false);
        assert_eq!(r, Read::Data(10));
        assert_eq!(&buf[..10], b"echo hola\n");
    }

    #[test]
    fn slave_output_is_processed_for_the_master() {
        let mut p = open_pair();
        let (r, e) = p.slave_write(b"hola\n");
        assert_eq!(r, Ok(5));
        assert!(e.wakes.master_readable);
        assert_eq!(drain_master(&mut p), b"hola\r\n");
        assert_eq!(p.master_read(&mut [0u8; 8]).0, Err(TtyError::Again));
    }

    #[test]
    fn a_full_output_blocks_the_slave_writer_until_the_master_reads() {
        let mut p = open_pair();
        let big = vec![b'x'; OUTPUT_CAPACITY + 10];
        assert_eq!(p.slave_write(&big).0, Ok(OUTPUT_CAPACITY));
        assert_eq!(p.slave_write(b"y").0, Err(TtyError::Again));
        assert!(!p.poll_slave().writable);
        let (_, e) = p.master_read(&mut [0u8; 16]);
        assert!(e.wakes.slave_writable);
        assert_eq!(p.slave_write(b"y").0, Ok(1));
    }

    #[test]
    fn a_full_input_blocks_the_master_writer_until_the_slave_reads() {
        let mut p = open_pair();
        let mut raw = *p.termios();
        raw.c_lflag &= !(ICANON | ECHO);
        p.set_termios(raw, false);
        let big = vec![b'x'; crate::ldisc::INPUT_CAPACITY + 1];
        assert_eq!(p.master_write(&big).0, Ok(crate::ldisc::INPUT_CAPACITY));
        assert_eq!(p.master_write(b"x").0, Err(TtyError::Again));
        assert!(!p.poll_master().writable);
        let (_, e) = p.slave_read(&mut [0u8; 8], false);
        assert!(e.wakes.master_writable);
        assert_eq!(p.master_write(b"x").0, Ok(1));
    }

    #[test]
    fn control_c_signals_the_foreground_group_not_the_leader() {
        let mut p = open_pair();
        p.set_foreground(42);
        let (_, e) = p.master_write(b"\x03");
        assert_eq!(e.signals, vec![(Target::Group(42), Signal::Int)]);
    }

    #[test]
    fn without_a_session_control_c_signals_nobody() {
        let mut p = Pty::new();
        p.set_locked(false);
        p.open_slave().unwrap();
        let (_, e) = p.master_write(b"\x03");
        assert!(e.signals.is_empty());
    }

    #[test]
    fn closing_the_master_hangs_up_the_slave() {
        let mut p = open_pair();
        p.set_foreground(42);
        p.master_write(b"sin leer\n");
        let e = p.close_master();
        assert_eq!(
            e.signals,
            vec![
                (Target::Process(7), Signal::Hup),
                (Target::Process(7), Signal::Cont),
                (Target::Group(42), Signal::Hup),
                (Target::Group(42), Signal::Cont),
            ]
        );
        assert_eq!(e.detach_session, Some(7));
        assert!(e.wakes.slave_readable && e.wakes.slave_writable);
        assert_eq!(slave_read(&mut p), Read::Eof, "input was flushed");
        assert_eq!(p.slave_write(b"x").0, Err(TtyError::Io));
        assert_eq!(p.open_slave(), Err(TtyError::Io));
        let m = p.poll_slave();
        assert!(m.hup && m.readable);
        assert_eq!(p.session(), None);
        assert!(!p.is_dead(), "a slave is still open");
        p.close_slave();
        assert!(p.is_dead());
    }

    #[test]
    fn closing_every_slave_is_eio_and_pollhup_on_the_master() {
        let mut p = open_pair();
        p.dup_slave();
        p.slave_write(b"adios").0.unwrap();
        assert!(p.close_slave().signals.is_empty());
        assert!(!p.poll_master().hup, "one slave still open");
        let e = p.close_slave();
        assert!(e.wakes.master_readable);
        assert!(p.poll_master().hup);
        // What was printed before is still read first.
        assert_eq!(drain_master(&mut p), b"adios");
        assert_eq!(p.master_read(&mut [0u8; 8]).0, Err(TtyError::Io));
    }

    #[test]
    fn before_any_slave_opens_the_master_just_waits() {
        let mut p = Pty::new();
        assert_eq!(p.master_read(&mut [0u8; 8]).0, Err(TtyError::Again));
        assert!(!p.poll_master().hup);
    }

    #[test]
    fn winsize_change_sends_sigwinch_once() {
        let mut p = open_pair();
        let ws = Winsize { rows: 25, cols: 80, xpixel: 0, ypixel: 0 };
        assert_eq!(p.set_winsize(ws).signals, vec![(Target::Group(7), Signal::Winch)]);
        assert!(p.set_winsize(ws).signals.is_empty(), "no change, no signal");
        assert_eq!(p.winsize(), ws);
    }

    #[test]
    fn flush_and_pending_counts() {
        let mut p = open_pair();
        p.master_write(b"ab\n");
        p.slave_write(b"xyz").0.unwrap();
        assert_eq!(p.slave_pending(), 3);
        assert_eq!(p.master_pending(), 3 + 4, "echo ab\\r\\n + xyz");
        p.flush(Flush::Both);
        assert_eq!(p.slave_pending(), 0);
        assert_eq!(p.master_pending(), 0);
    }

    #[test]
    fn half_typed_line_is_not_pending() {
        let mut p = open_pair();
        p.master_write(b"medio");
        assert_eq!(p.slave_pending(), 0);
        assert!(!p.poll_slave().readable);
    }
}
