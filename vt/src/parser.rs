//! Bytes from the pty master → operations on a [`Grid`].
//!
//! The state machine is the usual VT500 one, trimmed: `ESC`, `CSI` with
//! parameters, a private marker (`?`, `>`, `=`, `<`) and intermediates,
//! and control strings (`OSC`, `DCS`, `APC`, `PM`, `SOS`) that are read to
//! their terminator and dropped. **Anything not recognised is consumed and
//! ignored without disturbing the state**; C0 controls inside a sequence
//! are executed, as a VT100 does, and `CAN`/`SUB` abort it.
//!
//! Recognised: everything the kernel console handles (`CUP`, `CUU`…,
//! `ED`, `EL`, `SGR` with 16/256/RGB colour, bold, reverse) plus what
//! `vi`, `less` and `top` use — `DECSTBM`, `IL`/`DL`/`ICH`/`DCH`/`ECH`,
//! `SU`/`SD`, `REP`, `CHA`/`VPA`/`HPA`/`CNL`/`CPL`, `IND`/`NEL`/`RI`, `DECSC`/
//! `DECRC` (`ESC 7`/`8`, `CSI s`/`u`), `RIS`, and the private modes 1
//! (`DECCKM`), 7 (autowrap), 25 (cursor), 47/1047/1048/1049 (alternate
//! screen). `DSR 5n`/`6n` and `DA` are answered through
//! [`Parser::take_replies`].
//!
//! UTF-8 is decoded, so a multi-byte character takes one cell; the font
//! has basic Latin only, and the renderer draws the rest as `?`.

use alloc::vec::Vec;
use core::fmt::Write;

use crate::grid::Grid;
use crate::palette;

const MAX_PARAMS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ground,
    Escape,
    /// `ESC (` and friends: one more byte (a charset) and done.
    EscapeCharset,
    Csi,
    /// A control string, until `BEL` or `ESC \`.
    String,
    /// `ESC` inside a control string: `\` ends it, anything else starts a
    /// new escape sequence.
    StringEscape,
}

pub struct Parser {
    state: State,
    params: [u32; MAX_PARAMS],
    nparams: usize,
    /// A digit or separator has been seen for the parameter being read.
    param_started: bool,
    private: u8,
    intermediate: u8,
    /// UTF-8: the code point so far and the continuation bytes still due.
    utf8: u32,
    utf8_need: u8,
    /// The last character printed, for `REP`.
    last: Option<char>,
    replies: Vec<u8>,
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser {
    pub const fn new() -> Self {
        Parser {
            state: State::Ground,
            params: [0; MAX_PARAMS],
            nparams: 0,
            param_started: false,
            private: 0,
            intermediate: 0,
            utf8: 0,
            utf8_need: 0,
            last: None,
            replies: Vec::new(),
        }
    }

    /// Bytes the terminal owes the program (answers to `DSR`/`DA`), to be
    /// written to the pty master. Empties the queue.
    pub fn take_replies(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.replies)
    }

    pub fn feed(&mut self, grid: &mut Grid, bytes: &[u8]) {
        for &b in bytes {
            self.byte(grid, b);
        }
    }

    fn byte(&mut self, grid: &mut Grid, b: u8) {
        // An unfinished UTF-8 character ends at the first byte that is not
        // a continuation; it becomes U+FFFD and the byte is read afresh.
        if self.utf8_need > 0 {
            if b & 0xC0 == 0x80 {
                self.utf8 = self.utf8 << 6 | (b & 0x3F) as u32;
                self.utf8_need -= 1;
                if self.utf8_need == 0 {
                    self.print(grid, char::from_u32(self.utf8).unwrap_or('\u{FFFD}'));
                }
                return;
            }
            self.utf8_need = 0;
            self.print(grid, '\u{FFFD}');
        }

        match b {
            0x18 | 0x1A => {
                // CAN/SUB: abort whatever sequence is open.
                self.state = State::Ground;
                return;
            }
            0x1B => {
                self.state = if matches!(self.state, State::String) { State::StringEscape } else { State::Escape };
                self.clear_sequence();
                return;
            }
            _ => {}
        }

        match self.state {
            State::Ground => self.ground(grid, b),
            State::Escape => self.escape(grid, b),
            State::EscapeCharset => {
                if b >= 0x20 {
                    self.state = State::Ground;
                } else {
                    self.control(grid, b);
                }
            }
            State::Csi => self.csi(grid, b),
            State::String => {
                if b == 0x07 {
                    self.state = State::Ground;
                }
            }
            State::StringEscape => {
                if b == b'\\' {
                    self.state = State::Ground;
                } else {
                    self.state = State::Escape;
                    self.escape(grid, b);
                }
            }
        }
    }

    fn clear_sequence(&mut self) {
        self.params = [0; MAX_PARAMS];
        self.nparams = 0;
        self.param_started = false;
        self.private = 0;
        self.intermediate = 0;
    }

    fn ground(&mut self, grid: &mut Grid, b: u8) {
        match b {
            0x00..=0x1F | 0x7F => self.control(grid, b),
            0x20..=0x7E => self.print(grid, b as char),
            0xC2..=0xDF => self.start_utf8(b & 0x1F, 1),
            0xE0..=0xEF => self.start_utf8(b & 0x0F, 2),
            0xF0..=0xF4 => self.start_utf8(b & 0x07, 3),
            // A stray continuation byte or an invalid lead.
            _ => self.print(grid, '\u{FFFD}'),
        }
    }

    fn print(&mut self, grid: &mut Grid, c: char) {
        grid.print(c);
        self.last = Some(c);
    }

    fn start_utf8(&mut self, bits: u8, need: u8) {
        self.utf8 = bits as u32;
        self.utf8_need = need;
    }

    /// C0 controls. `DEL` and the rest are ignored.
    fn control(&mut self, grid: &mut Grid, b: u8) {
        match b {
            0x08 => grid.backspace(),
            0x09 => grid.tab(),
            0x0A..=0x0C => grid.index(),
            0x0D => grid.carriage_return(),
            _ => {}
        }
    }

    fn escape(&mut self, grid: &mut Grid, b: u8) {
        self.state = State::Ground;
        match b {
            0x00..=0x1F => {
                self.control(grid, b);
                self.state = State::Escape;
            }
            b'[' => {
                self.clear_sequence();
                self.state = State::Csi;
            }
            b']' | b'P' | b'_' | b'^' | b'X' => self.state = State::String,
            b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/' | b'#' | b'%' | b' ' => {
                self.state = State::EscapeCharset
            }
            b'7' => grid.save_cursor(),
            b'8' => grid.restore_cursor(),
            b'D' => grid.index(),
            b'E' => {
                grid.carriage_return();
                grid.index();
            }
            b'M' => grid.reverse_index(),
            b'c' => {
                grid.reset();
                self.last = None;
            }
            // `=`/`>` (keypad modes) and everything else: ignored.
            _ => {}
        }
    }

    fn csi(&mut self, grid: &mut Grid, b: u8) {
        match b {
            0x00..=0x1F => self.control(grid, b),
            b'0'..=b'9' => {
                if self.nparams < MAX_PARAMS {
                    let p = &mut self.params[self.nparams];
                    *p = p.saturating_mul(10).saturating_add((b - b'0') as u32);
                }
                self.param_started = true;
            }
            b';' | b':' => {
                self.nparams = (self.nparams + 1).min(MAX_PARAMS);
                self.param_started = true;
            }
            b'<'..=b'?' => {
                // A private marker is only valid first; later it spoils
                // the sequence, which is then read to its end and dropped.
                if self.private == 0 && !self.param_started && self.nparams == 0 {
                    self.private = b;
                } else {
                    self.intermediate = 0xFF;
                }
            }
            0x20..=0x2F => self.intermediate = b,
            0x40..=0x7E => {
                if self.param_started {
                    self.nparams = (self.nparams + 1).min(MAX_PARAMS);
                }
                self.state = State::Ground;
                self.dispatch(grid, b);
            }
            _ => {}
        }
    }

    /// Parameter `i`, or `default` when absent or zero.
    fn arg(&self, i: usize, default: u32) -> u32 {
        match self.params.get(i).copied() {
            Some(0) | None => default,
            Some(v) if i < self.nparams => v,
            Some(_) => default,
        }
    }

    fn n(&self, i: usize) -> usize {
        self.arg(i, 1) as usize
    }

    fn dispatch(&mut self, grid: &mut Grid, fin: u8) {
        if self.intermediate != 0 {
            return;
        }
        match self.private {
            0 => {}
            b'?' => return self.private_mode(grid, fin),
            _ => return,
        }
        let (row, col) = grid.cursor();
        match fin {
            b'@' => grid.insert_chars(self.n(0)),
            b'A' => grid.up(self.n(0)),
            b'B' | b'e' => grid.down(self.n(0)),
            b'C' | b'a' => grid.right(self.n(0)),
            b'D' => grid.left(self.n(0)),
            b'E' => {
                grid.down(self.n(0));
                grid.carriage_return();
            }
            b'F' => {
                grid.up(self.n(0));
                grid.carriage_return();
            }
            b'G' | b'`' => grid.move_to(row, self.n(0) - 1),
            b'd' => grid.move_to(self.n(0) - 1, col),
            b'H' | b'f' => grid.move_to(self.n(0) - 1, self.n(1) - 1),
            b'J' => grid.erase_display(self.params[0]),
            b'K' => grid.erase_line(self.params[0]),
            b'L' => grid.insert_lines(self.n(0)),
            b'M' => grid.delete_lines(self.n(0)),
            b'P' => grid.delete_chars(self.n(0)),
            b'S' => grid.scroll_up(self.n(0)),
            b'T' => grid.scroll_down(self.n(0)),
            b'X' => grid.erase_chars(self.n(0)),
            // `REP`: xterm-256color's terminfo advertises it, so ncurses
            // uses it. Capped at a screenful: the count is the program's.
            b'b' => {
                if let Some(c) = self.last {
                    for _ in 0..self.n(0).min(grid.cols() * grid.rows()) {
                        grid.print(c);
                    }
                }
            }
            b'm' => self.sgr(grid),
            b'r' => {
                let bottom = self.arg(1, grid.rows() as u32) as usize;
                grid.set_scroll_region(self.n(0) - 1, bottom);
            }
            b's' => grid.save_cursor(),
            b'u' => grid.restore_cursor(),
            b'n' => match self.params[0] {
                5 => self.replies.extend_from_slice(b"\x1b[0n"),
                6 => {
                    let _ = write!(Replies(&mut self.replies), "\x1b[{};{}R", row + 1, col + 1);
                }
                _ => {}
            },
            // Primary device attributes: a VT102.
            b'c' if self.params[0] == 0 => self.replies.extend_from_slice(b"\x1b[?6c"),
            _ => {}
        }
    }

    fn private_mode(&mut self, grid: &mut Grid, fin: u8) {
        let on = match fin {
            b'h' => true,
            b'l' => false,
            _ => return,
        };
        for i in 0..self.nparams.max(1) {
            match self.params[i] {
                1 => grid.set_app_cursor(on),
                7 => grid.set_autowrap(on),
                25 => grid.set_cursor_visible(on),
                47 | 1047 => {
                    if on { grid.enter_alt_screen(false) } else { grid.leave_alt_screen(false) }
                }
                1048 => {
                    if on { grid.save_cursor() } else { grid.restore_cursor() }
                }
                1049 => {
                    if on { grid.enter_alt_screen(true) } else { grid.leave_alt_screen(true) }
                }
                _ => {}
            }
        }
    }

    /// `SGR`, with the kernel console's rules (`apply_sgr`).
    fn sgr(&mut self, grid: &mut Grid) {
        let n = self.nparams.max(1);
        let p = &self.params[..n];
        let pen = &mut grid.pen;
        let mut i = 0;
        while i < n {
            match p[i] {
                0 => *pen = crate::grid::Pen::DEFAULT,
                1 => pen.attrs.bold = true,
                7 => pen.attrs.reverse = true,
                22 => pen.attrs.bold = false,
                27 => pen.attrs.reverse = false,
                30..=37 => pen.fg = palette::fg((p[i] - 30) as u8, false),
                39 => pen.fg = palette::DEFAULT_FG,
                40..=47 => pen.bg = palette::bg((p[i] - 40) as u8),
                49 => pen.bg = palette::DEFAULT_BG,
                90..=97 => pen.fg = palette::fg((p[i] - 90) as u8, true),
                100..=107 => pen.bg = palette::fg((p[i] - 100) as u8, true),
                38 | 48 => {
                    let fg = p[i] == 38;
                    let colour = match p.get(i + 1) {
                        Some(5) if i + 2 < n => {
                            let c = p[i + 2] as u8;
                            i += 2;
                            Some(if fg { palette::color256(c) } else { palette::bg256(c) })
                        }
                        Some(2) if i + 4 < n => {
                            let c = (p[i + 2] as u8 as u32) << 16 | (p[i + 3] as u8 as u32) << 8 | p[i + 4] as u8 as u32;
                            i += 4;
                            Some(c)
                        }
                        _ => None,
                    };
                    if let Some(c) = colour {
                        if fg { pen.fg = c } else { pen.bg = c }
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }
}

/// `core::fmt::Write` into the reply queue.
struct Replies<'a>(&'a mut Vec<u8>);

impl Write for Replies<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::Attrs;
    use crate::palette::{DEFAULT_BG, DEFAULT_FG};
    use alloc::string::String;

    fn term(cols: usize, rows: usize) -> (Grid, Parser) {
        (Grid::new(cols, rows), Parser::new())
    }

    fn feed(g: &mut Grid, p: &mut Parser, s: &str) {
        p.feed(g, s.as_bytes());
    }

    fn screen(g: &Grid) -> Vec<String> {
        (0..g.rows()).map(|r| g.row_text(r)).collect()
    }

    #[test]
    fn plain_text_and_crlf() {
        let (mut g, mut p) = term(10, 3);
        feed(&mut g, &mut p, "ab\r\ncd");
        assert_eq!(screen(&g), ["ab", "cd", ""]);
        assert_eq!(g.cursor(), (1, 2));
    }

    #[test]
    fn a_bare_line_feed_keeps_the_column() {
        let (mut g, mut p) = term(10, 3);
        feed(&mut g, &mut p, "ab\ncd");
        assert_eq!(screen(&g), ["ab", "  cd", ""]);
    }

    #[test]
    fn cursor_position_and_erase_like_busybox_vi() {
        let (mut g, mut p) = term(6, 3);
        feed(&mut g, &mut p, "junk\r\njunk\r\njunk");
        // vi's redraw(): home, clear to end of screen, then its rows.
        feed(&mut g, &mut p, "\x1b[H\x1b[J~\r\n~\x1b[3;1Hfile");
        assert_eq!(screen(&g), ["~", "~", "file"]);
        feed(&mut g, &mut p, "\x1b[2;3Hxy\x1b[K");
        assert_eq!(g.row_text(1), "~ xy");
        assert_eq!(g.cursor(), (1, 4));
    }

    #[test]
    fn relative_motion_defaults_to_one_and_clamps() {
        let (mut g, mut p) = term(5, 5);
        feed(&mut g, &mut p, "\x1b[3;3H\x1b[A\x1b[2D\x1b[0B\x1b[99C");
        assert_eq!(g.cursor(), (2, 4));
        feed(&mut g, &mut p, "\x1b[G\x1b[4d");
        assert_eq!(g.cursor(), (3, 0));
        feed(&mut g, &mut p, "\x1b[2F");
        assert_eq!(g.cursor(), (1, 0));
    }

    #[test]
    fn sgr_colours_bold_reverse_and_reset() {
        let (mut g, mut p) = term(10, 1);
        feed(&mut g, &mut p, "\x1b[1;31mA\x1b[7;44mB\x1b[22;27;39;49mC\x1b[38;5;196;48;2;1;2;3mD\x1b[0mE\x1b[91;101mF");
        let a = g.cell(0, 0);
        assert!(a.attrs.bold && !a.attrs.reverse);
        assert_eq!(a.fg, palette::fg(1, false));
        let b = g.cell(0, 1);
        assert!(b.attrs.reverse);
        assert_eq!(b.bg, palette::bg(4));
        let c = g.cell(0, 2);
        assert_eq!((c.fg, c.bg, c.attrs), (DEFAULT_FG, DEFAULT_BG, Attrs::default()));
        let d = g.cell(0, 3);
        assert_eq!((d.fg, d.bg), (0xFF0000, 0x010203));
        let e = g.cell(0, 4);
        assert_eq!((e.fg, e.bg), (DEFAULT_FG, DEFAULT_BG));
        let f = g.cell(0, 5);
        assert_eq!((f.fg, f.bg), (palette::fg(1, true), palette::fg(1, true)));
    }

    #[test]
    fn empty_sgr_is_a_reset() {
        let (mut g, mut p) = term(4, 1);
        feed(&mut g, &mut p, "\x1b[1;32m\x1b[mX");
        assert_eq!(g.cell(0, 0).fg, DEFAULT_FG);
        assert!(!g.cell(0, 0).attrs.bold);
    }

    #[test]
    fn scroll_region_with_insert_and_delete_line_like_less() {
        let (mut g, mut p) = term(4, 5);
        feed(&mut g, &mut p, "a\r\nb\r\nc\r\nd\r\n:");
        // Region rows 1-4 (1-based), scroll it up with a line feed at its
        // bottom, then insert a line at its top.
        feed(&mut g, &mut p, "\x1b[1;4r");
        assert_eq!(g.cursor(), (0, 0), "DECSTBM homes the cursor");
        feed(&mut g, &mut p, "\x1b[4;1H\ne\x1b[1;1H\x1b[Lz");
        assert_eq!(screen(&g), ["z", "b", "c", "d", ":"], "e fell off the region");
        feed(&mut g, &mut p, "\x1b[2;1H\x1b[2M");
        assert_eq!(screen(&g), ["z", "d", "", "", ":"]);
        feed(&mut g, &mut p, "\x1b[r");
        assert_eq!(g.scroll_region(), (0, 5));
    }

    #[test]
    fn reverse_index_and_nel() {
        let (mut g, mut p) = term(3, 3);
        feed(&mut g, &mut p, "a\x1bMb");
        assert_eq!(screen(&g), [" b", "a", ""]);
        feed(&mut g, &mut p, "\x1bEc");
        assert_eq!(g.row_text(1), "c");
    }

    #[test]
    fn alt_screen_1049_like_vi_and_top() {
        let (mut g, mut p) = term(8, 3);
        feed(&mut g, &mut p, "$ vi f");
        feed(&mut g, &mut p, "\x1b[?1049h\x1b[H\x1b[2J~\r\n~");
        assert_eq!(screen(&g), ["~", "~", ""]);
        feed(&mut g, &mut p, "\x1b[?1049l");
        assert_eq!(screen(&g), ["$ vi f", "", ""]);
        assert_eq!(g.cursor(), (0, 6));
    }

    #[test]
    fn save_restore_cursor_both_spellings() {
        let (mut g, mut p) = term(8, 4);
        feed(&mut g, &mut p, "\x1b[2;3H\x1b7\x1b[H\x1b8");
        assert_eq!(g.cursor(), (1, 2));
        feed(&mut g, &mut p, "\x1b[4;4H\x1b[s\x1b[H\x1b[u");
        assert_eq!(g.cursor(), (3, 3));
    }

    #[test]
    fn modes_cursor_autowrap_app_cursor() {
        let (mut g, mut p) = term(3, 2);
        feed(&mut g, &mut p, "\x1b[?25l\x1b[?1h");
        assert!(!g.cursor_visible());
        assert!(g.app_cursor());
        feed(&mut g, &mut p, "\x1b[?25;1l");
        assert!(!g.app_cursor(), "several modes in one sequence");
        feed(&mut g, &mut p, "\x1b[?25h\x1b[?7labcd");
        assert!(g.cursor_visible());
        assert_eq!(screen(&g), ["abd", ""]);
    }

    #[test]
    fn dsr_and_da_are_answered_as_data() {
        let (mut g, mut p) = term(80, 25);
        feed(&mut g, &mut p, "\x1b[12;34H\x1b[6n\x1b[5n\x1b[c");
        assert_eq!(p.take_replies(), b"\x1b[12;34R\x1b[0n\x1b[?6c");
        assert!(p.take_replies().is_empty());
        feed(&mut g, &mut p, "\x1b[>c\x1b[?6n");
        assert!(p.take_replies().is_empty(), "secondary DA and DECXCPR are not answered");
    }

    #[test]
    fn unknown_sequences_are_swallowed_whole() {
        let (mut g, mut p) = term(20, 2);
        feed(
            &mut g,
            &mut p,
            "a\x1b[?2004hb\x1b[>4;1mc\x1b[2 qd\x1b]0;title\x07e\x1b]2;t\x1b\\f\x1b(Bg\x1b=h\x1b[!pi\x1bPdcs\x1b\\j",
        );
        assert_eq!(g.row_text(0), "abcdefghij");
        // SGR state untouched by the private-marker `m`.
        assert_eq!(g.cell(0, 2).attrs, Attrs::default());
    }

    #[test]
    fn controls_inside_a_sequence_are_executed_and_can_aborts() {
        let (mut g, mut p) = term(10, 3);
        feed(&mut g, &mut p, "ab\x1b[\r2Cx");
        assert_eq!(g.row_text(0), "abx");
        feed(&mut g, &mut p, "\x1b[3\x18;4Hy");
        assert_eq!(g.row_text(0), "abx;4Hy");
    }

    #[test]
    fn escape_restarts_an_open_sequence() {
        let (mut g, mut p) = term(10, 3);
        feed(&mut g, &mut p, "\x1b[12\x1b[2;2Hz");
        assert_eq!(g.cursor(), (1, 2));
        assert_eq!(g.row_text(1), " z");
    }

    #[test]
    fn utf8_takes_one_cell_and_bad_bytes_become_replacement() {
        let (mut g, mut p) = term(10, 1);
        p.feed(&mut g, "añ€😀b".as_bytes());
        assert_eq!(g.row_text(0), "añ€😀b");
        assert_eq!(g.cursor(), (0, 5));
        let (mut g, mut p) = term(10, 1);
        p.feed(&mut g, b"\xC3x\x80\xFFy");
        assert_eq!(g.row_text(0), "\u{FFFD}x\u{FFFD}\u{FFFD}y");
    }

    #[test]
    fn a_sequence_split_across_feeds_is_the_same_sequence() {
        let (mut g, mut p) = term(10, 5);
        for b in b"\x1b[3;4H\x1b[1;31mZ" {
            p.feed(&mut g, &[*b]);
        }
        assert_eq!(g.cursor(), (2, 4));
        assert_eq!(g.cell(2, 3).fg, palette::fg(1, false));
        p.feed(&mut g, b"\xE2\x82");
        p.feed(&mut g, b"\xAC");
        assert_eq!(g.cell(2, 4).ch, '€');
    }

    #[test]
    fn rep_repeats_the_last_character_and_is_capped() {
        let (mut g, mut p) = term(10, 2);
        feed(&mut g, &mut p, "\x1b[3bab\x1b[3b");
        assert_eq!(g.row_text(0), "abbbb", "nothing to repeat at first");
        feed(&mut g, &mut p, "\x1b[2;1H\x1b[4294967295b");
        assert_eq!(g.row_text(1), "bbbbbbbbbb");
    }

    #[test]
    fn ris_resets_everything() {
        let (mut g, mut p) = term(5, 3);
        feed(&mut g, &mut p, "abc\x1b[31m\x1b[2;3r\x1b[?25l\x1b[?1049h\x1bc");
        assert_eq!(screen(&g), ["", "", ""]);
        assert_eq!(g.scroll_region(), (0, 3));
        assert!(g.cursor_visible() && !g.alt_screen());
        assert_eq!(g.pen.fg, DEFAULT_FG);
    }

    #[test]
    fn huge_parameters_do_not_overflow() {
        let (mut g, mut p) = term(5, 5);
        feed(&mut g, &mut p, "\x1b[99999999999999999999;4294967296H\x1b[4294967295@\x1b[4294967295L");
        assert_eq!(g.cursor(), (4, 0));
        let many = String::from("\x1b[") + &"1;".repeat(100) + "m";
        feed(&mut g, &mut p, &many);
    }

    /// Random bytes, biased towards escape-sequence material: nothing
    /// panics, the cursor stays on the screen and the region stays valid.
    #[test]
    fn random_input_keeps_the_invariants() {
        let alphabet: &[u8] = b"\x1b[;?0123456789HJKLMPSTX@mrhlsuABCDEFGdf\r\n\x08\t\x07\x18]\\7 8cM\xC3\xA9\xE2";
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for round in 0..200 {
            let cols = 1 + (next() % 12) as usize;
            let rows = 1 + (next() % 8) as usize;
            let (mut g, mut p) = term(cols, rows);
            let bytes: Vec<u8> = (0..2000)
                .map(|_| {
                    let r = next();
                    if r % 4 == 0 { (r >> 8) as u8 } else { alphabet[(r >> 8) as usize % alphabet.len()] }
                })
                .collect();
            p.feed(&mut g, &bytes);
            let (r, c) = g.cursor();
            assert!(r < rows && c < cols, "round {round}: cursor ({r},{c}) off a {cols}x{rows} grid");
            let (t, b) = g.scroll_region();
            assert!(t < b && b <= rows, "round {round}: region ({t},{b})");
            let _ = g.take_damage();
        }
    }
}
