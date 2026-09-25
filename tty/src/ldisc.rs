//! The line discipline: what happens to a byte between the keyboard side of
//! a terminal and the program reading it, and on the way back out.
//!
//! Linux's `n_tty`, reduced to what this port's `termios` can express (no
//! `VWERASE`, `VLNEXT`, `ECHOCTL`, `IUCLC`). Nothing here blocks: `read`
//! says [`Read::Wait`] and the caller parks the process; input that does not
//! fit is left unconsumed and the caller parks the writer. That is the whole
//! reason this is a crate of its own — every rule can be tested with
//! `cargo test`, no kernel involved.
//!
//! Not implemented, on purpose: `IXON`/`IXOFF` flow control (`^S`/`^Q` reach
//! the reader as ordinary bytes), parity (`INPCK`/`PARMRK`: a pty has none),
//! and column-exact erasure of a tab (it is erased as one column).

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::termios::*;
use crate::Signal;

/// Bytes the input side holds, completed lines and the line being edited
/// together — Linux's `N_TTY_BUF_SIZE`.
pub const INPUT_CAPACITY: usize = 4096;

/// What [`LineDiscipline::receive`] did with a batch of input.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Received {
    /// Bytes of the input it took. Fewer than offered only when the input
    /// side is full: the rest must be offered again once a reader has made
    /// room (a pty master's writer blocks until then).
    pub consumed: usize,
    /// Echo, already output-processed, for the terminal's output side.
    pub echo: Vec<u8>,
    /// Signals for the foreground process group, in the order typed.
    pub signals: Vec<Signal>,
    /// Something became readable that was not before.
    pub readable: bool,
}

/// What [`LineDiscipline::read`] can return.
#[derive(Debug, PartialEq, Eq)]
pub enum Read {
    /// Bytes copied into the buffer. `Data(0)` is a legitimate answer in
    /// raw mode with `VMIN = 0`.
    Data(usize),
    /// End of file: `VEOF` typed at the start of a line.
    Eof,
    /// Nothing to return yet.
    Wait(Wait),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Wait {
    /// Until input arrives.
    Forever,
    /// Until input arrives or this many tenths of a second pass (`VTIME`);
    /// then `read` is called again with `expired = true`.
    Timeout(u32),
}

pub struct LineDiscipline {
    termios: Termios,
    /// Input a reader may take.
    ready: VecDeque<u8>,
    /// Canonical mode: the length of every completed line in `ready`,
    /// oldest first. 0 is an end of file (`VEOF` on an empty line), and is
    /// the only way a 0 gets here: a partly read line is popped the moment
    /// it is used up.
    lines: VecDeque<usize>,
    /// Canonical mode: the line being edited, not readable yet.
    edit: Vec<u8>,
    /// Output column, for `ONOCR` and the tab stops.
    column: usize,
}

impl Default for LineDiscipline {
    fn default() -> Self {
        Self::new(Termios::sane())
    }
}

impl LineDiscipline {
    pub fn new(termios: Termios) -> Self {
        Self {
            termios,
            ready: VecDeque::new(),
            lines: VecDeque::new(),
            edit: Vec::new(),
            column: 0,
        }
    }

    pub fn termios(&self) -> &Termios {
        &self.termios
    }

    fn canonical(&self) -> bool {
        self.termios.lflag(ICANON)
    }

    /// Bytes the input side holds.
    pub fn input_len(&self) -> usize {
        self.ready.len() + self.edit.len()
    }

    /// Bytes a reader could take in total (`FIONREAD`): completed lines
    /// in canonical mode, everything in raw mode.
    pub fn ready_len(&self) -> usize {
        self.ready.len()
    }

    /// Whether a `read` would return without waiting (`poll`'s `POLLIN`).
    /// In raw mode, any byte; `VMIN > 1` does not hold `poll` back.
    pub fn readable(&self) -> bool {
        if self.canonical() {
            !self.lines.is_empty()
        } else {
            !self.ready.is_empty()
        }
    }

    /// Whether `receive` would take at least one more byte. In canonical
    /// mode a full line still takes the characters that end or edit it,
    /// so this is about completed-but-unread input only.
    pub fn accepts_input(&self) -> bool {
        self.ready.len() < INPUT_CAPACITY
    }

    /// Replace the settings. `flush` is `TCSETSF`'s "discard pending
    /// input first". Returns whether input became readable, which happens
    /// when leaving canonical mode with a line half typed (it is handed
    /// over as it stands, as Linux does).
    pub fn set_termios(&mut self, new: Termios, flush: bool) -> bool {
        let was_readable = self.readable();
        let was_canon = self.canonical();
        self.termios = new;
        if flush {
            self.flush_input();
        }
        let canon = self.canonical();
        if was_canon && !canon {
            let edit = core::mem::take(&mut self.edit);
            self.ready.extend(edit);
            self.lines.clear();
        } else if !was_canon && canon && !self.ready.is_empty() {
            // Raw input already typed becomes one line, readable as is:
            // what Linux's `n_tty_set_termios` does with its `push`.
            self.lines.clear();
            self.lines.push_back(self.ready.len());
        }
        !was_readable && self.readable()
    }

    /// Discard all input, typed and completed (`TCIFLUSH`, and what an
    /// `ISIG` character does without `NOFLSH`).
    pub fn flush_input(&mut self) {
        self.ready.clear();
        self.lines.clear();
        self.edit.clear();
    }

    /// Feed bytes typed at the terminal.
    pub fn receive(&mut self, input: &[u8]) -> Received {
        let was_readable = self.readable();
        let mut r = Received::default();
        let mut echo = Vec::new();

        for &b in input {
            if self.ready.len() >= INPUT_CAPACITY {
                break;
            }
            r.consumed += 1;
            let t = self.termios;

            let mut c = if t.iflag(ISTRIP) { b & 0x7f } else { b };
            if c == b'\r' {
                if t.iflag(IGNCR) {
                    continue;
                }
                if t.iflag(ICRNL) {
                    c = b'\n';
                }
            } else if c == b'\n' && t.iflag(INLCR) {
                c = b'\r';
            }

            if t.lflag(ISIG) {
                let sig = if t.is_cc(VINTR, c) {
                    Some(Signal::Int)
                } else if t.is_cc(VQUIT, c) {
                    Some(Signal::Quit)
                } else if t.is_cc(VSUSP, c) {
                    Some(Signal::Tstp)
                } else {
                    None
                };
                if let Some(sig) = sig {
                    if !t.lflag(NOFLSH) {
                        self.flush_input();
                    }
                    r.signals.push(sig);
                    continue;
                }
            }

            if !t.lflag(ICANON) {
                self.ready.push_back(c);
                if t.lflag(ECHO) {
                    echo.push(c);
                }
                continue;
            }

            if t.is_cc(VERASE, c) {
                if self.edit.pop().is_some() && t.lflag(ECHO) {
                    if t.lflag(ECHOE) {
                        echo.extend_from_slice(b"\x08 \x08");
                    } else {
                        echo.push(c);
                    }
                }
            } else if t.is_cc(VKILL, c) {
                if !self.edit.is_empty() && t.lflag(ECHO) {
                    if t.lflag(ECHOE) {
                        for _ in 0..self.edit.len() {
                            echo.extend_from_slice(b"\x08 \x08");
                        }
                    } else {
                        echo.push(c);
                        if t.lflag(ECHOK) {
                            echo.push(b'\n');
                        }
                    }
                }
                self.edit.clear();
            } else if t.is_cc(VEOF, c) {
                self.complete_line();
            } else if c == b'\n' || t.is_cc(VEOL, c) {
                self.edit.push(c);
                self.complete_line();
                if t.lflag(ECHO) || (c == b'\n' && t.lflag(ECHONL)) {
                    echo.push(c);
                }
            } else if self.input_len() < INPUT_CAPACITY - 1 {
                // The last slot stays free for the character that ends
                // the line, so a full line can still be sent.
                self.edit.push(c);
                if t.lflag(ECHO) {
                    echo.push(c);
                }
            }
        }

        if !echo.is_empty() {
            self.output(&echo, &mut r.echo, usize::MAX);
        }
        r.readable = !was_readable && self.readable();
        r
    }

    fn complete_line(&mut self) {
        let len = self.edit.len();
        let edit = core::mem::take(&mut self.edit);
        self.ready.extend(edit);
        self.lines.push_back(len);
    }

    /// Take input for a reader. `expired` says the `Wait::Timeout` this
    /// returned last time has run out.
    pub fn read(&mut self, buf: &mut [u8], expired: bool) -> Read {
        if buf.is_empty() {
            return Read::Data(0);
        }
        if self.canonical() {
            let Some(len) = self.lines.front_mut() else {
                return Read::Wait(Wait::Forever);
            };
            if *len == 0 {
                self.lines.pop_front();
                return Read::Eof;
            }
            let n = (*len).min(buf.len());
            *len -= n;
            if *len == 0 {
                self.lines.pop_front();
            }
            self.take(&mut buf[..n]);
            return Read::Data(n);
        }

        let vmin = self.termios.c_cc[VMIN] as usize;
        let vtime = self.termios.c_cc[VTIME];
        let avail = self.ready.len();
        let give = |this: &mut Self, buf: &mut [u8]| {
            let n = avail.min(buf.len());
            this.take(&mut buf[..n]);
            Read::Data(n)
        };
        let need = vmin.min(buf.len());
        match (vmin, vtime) {
            (0, 0) => give(self, buf),
            (_, 0) => {
                if avail >= need {
                    give(self, buf)
                } else {
                    Read::Wait(Wait::Forever)
                }
            }
            (0, t) => {
                if avail > 0 || expired {
                    give(self, buf)
                } else {
                    Read::Wait(Wait::Timeout(t))
                }
            }
            // POSIX starts this timer at each byte; here it runs from the
            // first byte on, which is the same for a reader that is
            // already waiting when input starts.
            (_, t) => {
                if avail >= need || (avail > 0 && expired) {
                    give(self, buf)
                } else if avail > 0 {
                    Read::Wait(Wait::Timeout(t))
                } else {
                    Read::Wait(Wait::Forever)
                }
            }
        }
    }

    fn take(&mut self, dst: &mut [u8]) {
        for d in dst.iter_mut() {
            *d = self.ready.pop_front().expect("take beyond ready");
        }
    }

    /// Output processing (`OPOST`): append `src` to `dst`, transformed,
    /// without making `dst` grow by more than `room` bytes. Returns how
    /// many bytes of `src` it took — never half of one: a `\n` that would
    /// become `\r\n` with room for one byte waits for the next call.
    pub fn output(&mut self, src: &[u8], dst: &mut Vec<u8>, room: usize) -> usize {
        let t = self.termios;
        if !t.oflag(OPOST) {
            let n = src.len().min(room);
            dst.extend_from_slice(&src[..n]);
            return n;
        }
        let mut used = 0usize;
        let mut taken = 0usize;
        for &c in src {
            let mut out = [0u8; 2];
            let (len, col) = match c {
                b'\n' if t.oflag(ONLCR) => {
                    out = [b'\r', b'\n'];
                    (2, 0)
                }
                b'\n' => {
                    out[0] = b'\n';
                    (1, if t.oflag(ONLRET) { 0 } else { self.column })
                }
                b'\r' if t.oflag(ONOCR) && self.column == 0 => (0, 0),
                b'\r' if t.oflag(OCRNL) => {
                    out[0] = b'\n';
                    (1, if t.oflag(ONLRET) { 0 } else { self.column })
                }
                b'\r' => {
                    out[0] = b'\r';
                    (1, 0)
                }
                b'\t' => {
                    out[0] = b'\t';
                    (1, (self.column | 7) + 1)
                }
                0x08 => {
                    out[0] = c;
                    (1, self.column.saturating_sub(1))
                }
                c if c < 0x20 || c == 0x7f => {
                    out[0] = c;
                    (1, self.column)
                }
                _ => {
                    out[0] = c;
                    (1, self.column + 1)
                }
            };
            if used + len > room {
                break;
            }
            dst.extend_from_slice(&out[..len]);
            used += len;
            taken += 1;
            self.column = col;
        }
        taken
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn raw() -> Termios {
        let mut t = Termios::sane();
        t.c_lflag &= !(ICANON | ECHO);
        t
    }

    fn read_all(ld: &mut LineDiscipline) -> Read {
        let mut buf = [0u8; 256];
        ld.read(&mut buf, false)
    }

    fn read_str(ld: &mut LineDiscipline, cap: usize) -> Vec<u8> {
        let mut buf = vec![0u8; cap];
        match ld.read(&mut buf, false) {
            Read::Data(n) => buf[..n].to_vec(),
            other => panic!("expected data, got {other:?}"),
        }
    }

    #[test]
    fn canonical_line_is_not_readable_until_enter() {
        let mut ld = LineDiscipline::default();
        let r = ld.receive(b"echo hola");
        assert_eq!(r.consumed, 9);
        assert!(!r.readable);
        assert!(!ld.readable());
        assert_eq!(read_all(&mut ld), Read::Wait(Wait::Forever));
        let r = ld.receive(b"\r");
        assert!(r.readable);
        assert_eq!(read_str(&mut ld, 64), b"echo hola\n");
        assert!(!ld.readable());
    }

    #[test]
    fn canonical_echo_and_crlf_on_the_way_back() {
        let mut ld = LineDiscipline::default();
        let r = ld.receive(b"ls\r");
        // `\r` became `\n` (ICRNL) and was echoed as `\r\n` (ONLCR).
        assert_eq!(r.echo, b"ls\r\n");
    }

    #[test]
    fn erase_and_kill_edit_the_line_and_the_echo() {
        let mut ld = LineDiscipline::default();
        let r = ld.receive(b"cax\x7f\x7ft\n");
        assert_eq!(r.echo, b"cax\x08 \x08\x08 \x08t\r\n");
        assert_eq!(read_str(&mut ld, 64), b"ct\n");

        let r = ld.receive(b"basura\x15ok\n");
        assert_eq!(&r.echo[..6], b"basura");
        assert_eq!(r.echo.windows(3).filter(|w| w == b"\x08 \x08").count(), 6);
        assert_eq!(read_str(&mut ld, 64), b"ok\n");
    }

    #[test]
    fn erase_on_an_empty_line_echoes_nothing() {
        let mut ld = LineDiscipline::default();
        assert!(ld.receive(b"\x7f\x7f").echo.is_empty());
    }

    #[test]
    fn kill_without_echoe_echoes_the_character_and_a_newline() {
        let mut t = Termios::sane();
        t.c_lflag &= !ECHOE;
        let mut ld = LineDiscipline::new(t);
        let r = ld.receive(b"ab\x15");
        assert_eq!(r.echo, b"ab\x15\r\n");
    }

    #[test]
    fn eof_at_line_start_is_end_of_file_and_mid_line_just_sends() {
        let mut ld = LineDiscipline::default();
        ld.receive(b"abc\x04");
        assert_eq!(read_str(&mut ld, 64), b"abc");
        assert_eq!(read_all(&mut ld), Read::Wait(Wait::Forever));
        ld.receive(b"\x04");
        assert_eq!(read_all(&mut ld), Read::Eof);
        assert_eq!(read_all(&mut ld), Read::Wait(Wait::Forever));
    }

    #[test]
    fn a_read_never_crosses_a_line_and_a_small_buffer_splits_one() {
        let mut ld = LineDiscipline::default();
        ld.receive(b"uno\ndos\n");
        assert_eq!(read_str(&mut ld, 2), b"un");
        assert_eq!(read_str(&mut ld, 64), b"o\n");
        assert_eq!(read_str(&mut ld, 64), b"dos\n");
    }

    #[test]
    fn a_partly_read_line_is_never_taken_for_eof() {
        let mut ld = LineDiscipline::default();
        ld.receive(b"ab\n");
        assert_eq!(read_str(&mut ld, 3), b"ab\n");
        assert_eq!(read_all(&mut ld), Read::Wait(Wait::Forever));
    }

    #[test]
    fn isig_characters_become_signals_and_flush() {
        let mut ld = LineDiscipline::default();
        ld.receive(b"listo\n");
        let r = ld.receive(b"medio\x03");
        assert_eq!(r.signals, vec![Signal::Int]);
        assert!(!ld.readable(), "^C flushed the unread line too");
        assert_eq!(ld.input_len(), 0);
        let r = ld.receive(b"\x1c\x1a");
        assert_eq!(r.signals, vec![Signal::Quit, Signal::Tstp]);
    }

    #[test]
    fn noflsh_keeps_the_input() {
        let mut t = Termios::sane();
        t.c_lflag |= NOFLSH;
        let mut ld = LineDiscipline::new(t);
        ld.receive(b"sigue\n\x03");
        assert_eq!(read_str(&mut ld, 64), b"sigue\n");
    }

    #[test]
    fn without_isig_control_characters_are_data() {
        let mut t = raw();
        t.c_lflag &= !ISIG;
        let mut ld = LineDiscipline::new(t);
        let r = ld.receive(b"\x03\x1a");
        assert!(r.signals.is_empty());
        assert_eq!(read_str(&mut ld, 64), b"\x03\x1a");
    }

    #[test]
    fn raw_mode_as_ash_uses_it() {
        // ash's line editor: ICANON and ECHO off, ISIG on, VMIN 1.
        let mut ld = LineDiscipline::new(raw());
        let r = ld.receive(b"l");
        assert!(r.echo.is_empty());
        assert!(r.readable);
        assert_eq!(read_str(&mut ld, 64), b"l");
        // Arrow keys arrive whole and untouched.
        ld.receive(b"\x1b[A");
        assert_eq!(read_str(&mut ld, 64), b"\x1b[A");
        // Enter still goes through ICRNL.
        ld.receive(b"\r");
        assert_eq!(read_str(&mut ld, 64), b"\n");
    }

    #[test]
    fn vmin_and_vtime() {
        let mut t = raw();
        t.c_cc[VMIN] = 0;
        t.c_cc[VTIME] = 0;
        let mut ld = LineDiscipline::new(t);
        assert_eq!(read_all(&mut ld), Read::Data(0), "polling read");

        t.c_cc[VMIN] = 3;
        ld.set_termios(t, false);
        ld.receive(b"ab");
        assert_eq!(read_all(&mut ld), Read::Wait(Wait::Forever));
        let mut small = [0u8; 2];
        assert_eq!(ld.read(&mut small, false), Read::Data(2), "VMIN is capped by the buffer");
        ld.receive(b"xyz");
        assert_eq!(read_str(&mut ld, 64), b"xyz");

        t.c_cc[VMIN] = 0;
        t.c_cc[VTIME] = 5;
        ld.set_termios(t, false);
        assert_eq!(read_all(&mut ld), Read::Wait(Wait::Timeout(5)));
        assert_eq!(ld.read(&mut [0u8; 8], true), Read::Data(0), "timed out");

        t.c_cc[VMIN] = 4;
        ld.set_termios(t, false);
        assert_eq!(read_all(&mut ld), Read::Wait(Wait::Forever));
        ld.receive(b"q");
        assert_eq!(read_all(&mut ld), Read::Wait(Wait::Timeout(5)));
        assert_eq!(ld.read(&mut [0u8; 8], true), Read::Data(1));
    }

    #[test]
    fn leaving_canonical_hands_over_the_half_typed_line() {
        let mut ld = LineDiscipline::default();
        ld.receive(b"medio");
        assert!(ld.set_termios(raw(), false));
        assert_eq!(read_str(&mut ld, 64), b"medio");
    }

    #[test]
    fn entering_canonical_makes_raw_input_one_line() {
        let mut ld = LineDiscipline::new(raw());
        ld.receive(b"xy");
        ld.set_termios(Termios::sane(), false);
        assert!(ld.readable());
        assert_eq!(read_str(&mut ld, 64), b"xy");
    }

    #[test]
    fn tcsetsf_flushes() {
        let mut ld = LineDiscipline::default();
        ld.receive(b"fuera\n");
        ld.set_termios(Termios::sane(), true);
        assert!(!ld.readable());
    }

    #[test]
    fn input_iflags() {
        let mut t = raw();
        t.c_iflag = IGNCR;
        let mut ld = LineDiscipline::new(t);
        ld.receive(b"a\rb");
        assert_eq!(read_str(&mut ld, 64), b"ab");

        t.c_iflag = INLCR;
        ld.set_termios(t, false);
        ld.receive(b"\n");
        assert_eq!(read_str(&mut ld, 64), b"\r");

        t.c_iflag = ISTRIP;
        ld.set_termios(t, false);
        ld.receive(&[0xE1]);
        assert_eq!(read_str(&mut ld, 64), [0x61]);

        t.c_iflag = 0;
        ld.set_termios(t, false);
        ld.receive(b"\r");
        assert_eq!(read_str(&mut ld, 64), b"\r", "no ICRNL, no translation");
    }

    #[test]
    fn veol_ends_a_line_and_a_disabled_one_does_not() {
        let mut t = Termios::sane();
        t.c_cc[VEOL] = b';' as u32;
        let mut ld = LineDiscipline::new(t);
        ld.receive(b"a;");
        assert_eq!(read_str(&mut ld, 64), b"a;");

        let mut ld = LineDiscipline::default();
        ld.receive(b"a\0");
        assert!(!ld.readable(), "VEOL disabled: NUL is just a character");
    }

    #[test]
    fn echonl_echoes_newline_without_echo() {
        let mut t = Termios::sane();
        t.c_lflag &= !ECHO;
        t.c_lflag |= ECHONL;
        let mut ld = LineDiscipline::new(t);
        let r = ld.receive(b"secreto\n");
        assert_eq!(r.echo, b"\r\n");
    }

    #[test]
    fn raw_input_stops_at_capacity_and_resumes_after_a_read() {
        let mut ld = LineDiscipline::new(raw());
        let big = vec![b'x'; INPUT_CAPACITY + 100];
        let r = ld.receive(&big);
        assert_eq!(r.consumed, INPUT_CAPACITY);
        assert!(!ld.accepts_input());
        let mut buf = [0u8; 100];
        assert_eq!(ld.read(&mut buf, false), Read::Data(100));
        assert_eq!(ld.receive(&big[INPUT_CAPACITY..]).consumed, 100);
        assert_eq!(ld.input_len(), INPUT_CAPACITY);
    }

    #[test]
    fn a_full_canonical_line_still_takes_enter_and_erase() {
        let mut ld = LineDiscipline::default();
        let big = vec![b'x'; INPUT_CAPACITY + 10];
        let r = ld.receive(&big);
        assert_eq!(r.consumed, big.len(), "overflow is discarded, not refused");
        assert_eq!(ld.input_len(), INPUT_CAPACITY - 1);
        ld.receive(b"\x7f");
        assert_eq!(ld.input_len(), INPUT_CAPACITY - 2);
        ld.receive(b"\n");
        assert!(ld.readable());
        let mut buf = vec![0u8; INPUT_CAPACITY];
        assert_eq!(ld.read(&mut buf, false), Read::Data(INPUT_CAPACITY - 1));
        assert_eq!(buf[INPUT_CAPACITY - 2], b'\n');
    }

    #[test]
    fn output_processing() {
        let mut ld = LineDiscipline::default();
        let mut out = Vec::new();
        assert_eq!(ld.output(b"a\nb", &mut out, usize::MAX), 3);
        assert_eq!(out, b"a\r\nb");

        // Never half of an expansion.
        let mut out = Vec::new();
        assert_eq!(ld.output(b"a\n", &mut out, 2), 1);
        assert_eq!(out, b"a");

        let mut t = Termios::sane();
        t.c_oflag = 0;
        ld.set_termios(t, false);
        let mut out = Vec::new();
        ld.output(b"a\n", &mut out, usize::MAX);
        assert_eq!(out, b"a\n", "no OPOST, bytes as written");

        t.c_oflag = OPOST | OCRNL;
        ld.set_termios(t, false);
        let mut out = Vec::new();
        ld.output(b"\r", &mut out, usize::MAX);
        assert_eq!(out, b"\n");
    }

    #[test]
    fn onocr_drops_a_carriage_return_at_column_zero() {
        let mut t = Termios::sane();
        t.c_oflag = OPOST | ONOCR;
        let mut ld = LineDiscipline::new(t);
        let mut out = Vec::new();
        ld.output(b"\rab\r\r", &mut out, usize::MAX);
        assert_eq!(out, b"ab\r");
        // A tab moves to the next stop, a backspace one back.
        let mut out = Vec::new();
        ld.output(b"\tx\x08\x08\x08\x08\x08\x08\x08\x08\x08\r", &mut out, usize::MAX);
        assert!(out.ends_with(b"\x08"), "back at column 0, the CR is dropped");
    }

    /// Property: whatever arrives, input never grows past its capacity,
    /// and in canonical mode `lines` accounts for every ready byte.
    #[test]
    fn invariants_hold_under_arbitrary_input() {
        let mut seed: u32 = 0x1234_5678;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for round in 0..200 {
            let mut t = Termios::sane();
            if round % 2 == 1 {
                t.c_lflag &= !ICANON;
            }
            let mut ld = LineDiscipline::new(t);
            for _ in 0..300 {
                let n = (rnd() % 64) as usize;
                let bytes: Vec<u8> = (0..n)
                    .map(|_| match rnd() % 8 {
                        0 => b'\n',
                        1 => 0x7f,
                        2 => 0x04,
                        3 => 0x15,
                        _ => b'a' + (rnd() % 26) as u8,
                    })
                    .collect();
                ld.receive(&bytes);
                assert!(ld.input_len() <= INPUT_CAPACITY);
                if ld.canonical() {
                    assert_eq!(ld.lines.iter().sum::<usize>(), ld.ready.len());
                }
                if rnd() % 3 == 0 {
                    let mut buf = vec![0u8; (rnd() % 32) as usize + 1];
                    let first_line = ld.lines.front().copied();
                    if let Read::Data(n) = ld.read(&mut buf, false) {
                        if let Some(len) = first_line {
                            assert_eq!(n, len.min(buf.len()), "one line at most");
                        }
                    }
                }
            }
        }
    }
}
