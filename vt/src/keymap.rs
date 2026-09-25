//! A Linux `KEY_*` code and its press or release → the bytes a terminal
//! sends to the pty master.
//!
//! The US layout of `hal::keyboard::KeyDecoder`, read from evdev codes
//! instead of Set-1 scancodes (for the main block they are the same
//! numbers, which is how the kernel derives `KEY_*` from Set-1 in the first
//! place). Letters follow Shift xor Caps Lock, symbols Shift only;
//! Ctrl-letter is 1-26, and Ctrl `[` `\` `]` are `ESC`/`FS`/`GS` as there.
//!
//! Where a pty wants something different from the console, this follows
//! xterm, because the line discipline on the other side is a real one:
//! **Enter sends `\r`** (`ICRNL` makes it `\n`), **Backspace sends `DEL`**
//! (the pty's `VERASE`), and so **Delete sends `ESC [3~`** — the console's
//! Delete-as-`DEL` would erase backwards here. Arrows and Home/End follow
//! `DECCKM` (`ESC O x` when the program asked for application mode), and
//! Alt puts an `ESC` in front of what the key would send.

/// At most 8 bytes: the longest is Alt + a function key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyBytes {
    buf: [u8; 8],
    len: usize,
}

impl KeyBytes {
    const EMPTY: KeyBytes = KeyBytes { buf: [0; 8], len: 0 };

    fn push(&mut self, bytes: &[u8]) {
        self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

pub mod code {
    //! The `linux/input-event-codes.h` values used here.
    pub const ESC: u32 = 1;
    pub const BACKSPACE: u32 = 14;
    pub const TAB: u32 = 15;
    pub const ENTER: u32 = 28;
    pub const LEFTCTRL: u32 = 29;
    pub const LEFTSHIFT: u32 = 42;
    pub const RIGHTSHIFT: u32 = 54;
    pub const KPASTERISK: u32 = 55;
    pub const LEFTALT: u32 = 56;
    pub const SPACE: u32 = 57;
    pub const CAPSLOCK: u32 = 58;
    pub const F1: u32 = 59;
    pub const F10: u32 = 68;
    pub const KPMINUS: u32 = 74;
    pub const KPPLUS: u32 = 78;
    pub const F11: u32 = 87;
    pub const F12: u32 = 88;
    pub const KPENTER: u32 = 96;
    pub const RIGHTCTRL: u32 = 97;
    pub const KPSLASH: u32 = 98;
    pub const RIGHTALT: u32 = 100;
    pub const HOME: u32 = 102;
    pub const UP: u32 = 103;
    pub const PAGEUP: u32 = 104;
    pub const LEFT: u32 = 105;
    pub const RIGHT: u32 = 106;
    pub const END: u32 = 107;
    pub const DOWN: u32 = 108;
    pub const PAGEDOWN: u32 = 109;
    pub const INSERT: u32 = 110;
    pub const DELETE: u32 = 111;
}

/// `(unshifted, shifted)` for the printable keys, indexed by code.
fn printable(c: u32) -> Option<(u8, u8)> {
    const ROW_NUM: &[u8; 12] = b"1234567890-=";
    const ROW_NUM_S: &[u8; 12] = b"!@#$%^&*()_+";
    const ROW_Q: &[u8; 12] = b"qwertyuiop[]";
    const ROW_Q_S: &[u8; 12] = b"QWERTYUIOP{}";
    const ROW_A: &[u8; 12] = b"asdfghjkl;'`";
    const ROW_A_S: &[u8; 12] = b"ASDFGHJKL:\"~";
    const ROW_Z: &[u8; 11] = b"\\zxcvbnm,./";
    const ROW_Z_S: &[u8; 11] = b"|ZXCVBNM<>?";
    let i = c as usize;
    Some(match c {
        2..=13 => (ROW_NUM[i - 2], ROW_NUM_S[i - 2]),
        16..=27 => (ROW_Q[i - 16], ROW_Q_S[i - 16]),
        30..=41 => (ROW_A[i - 30], ROW_A_S[i - 30]),
        43..=53 => (ROW_Z[i - 43], ROW_Z_S[i - 43]),
        code::SPACE => (b' ', b' '),
        code::KPASTERISK => (b'*', b'*'),
        code::KPMINUS => (b'-', b'-'),
        code::KPPLUS => (b'+', b'+'),
        code::KPSLASH => (b'/', b'/'),
        _ => return None,
    })
}

/// Keys that send a sequence of their own, whatever the modifiers.
fn special(c: u32, app_cursor: bool) -> Option<&'static [u8]> {
    let cursor = |normal: &'static [u8], app: &'static [u8]| if app_cursor { app } else { normal };
    Some(match c {
        code::UP => cursor(b"\x1b[A", b"\x1bOA"),
        code::DOWN => cursor(b"\x1b[B", b"\x1bOB"),
        code::RIGHT => cursor(b"\x1b[C", b"\x1bOC"),
        code::LEFT => cursor(b"\x1b[D", b"\x1bOD"),
        code::HOME => cursor(b"\x1b[H", b"\x1bOH"),
        code::END => cursor(b"\x1b[F", b"\x1bOF"),
        code::INSERT => b"\x1b[2~",
        code::DELETE => b"\x1b[3~",
        code::PAGEUP => b"\x1b[5~",
        code::PAGEDOWN => b"\x1b[6~",
        59 => b"\x1bOP",
        60 => b"\x1bOQ",
        61 => b"\x1bOR",
        62 => b"\x1bOS",
        63 => b"\x1b[15~",
        64 => b"\x1b[17~",
        65 => b"\x1b[18~",
        66 => b"\x1b[19~",
        67 => b"\x1b[20~",
        code::F10 => b"\x1b[21~",
        code::F11 => b"\x1b[23~",
        code::F12 => b"\x1b[24~",
        _ => return None,
    })
}

/// Modifier state across key events.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Keyboard {
    lshift: bool,
    rshift: bool,
    lctrl: bool,
    rctrl: bool,
    lalt: bool,
    ralt: bool,
    caps: bool,
}

impl Keyboard {
    pub const fn new() -> Self {
        Keyboard { lshift: false, rshift: false, lctrl: false, rctrl: false, lalt: false, ralt: false, caps: false }
    }

    /// Focus left the window: the releases will go elsewhere, so forget
    /// held modifiers (Caps Lock is a toggle and stays).
    pub fn release_all(&mut self) {
        *self = Keyboard { caps: self.caps, ..Keyboard::new() };
    }

    /// One key event. `app_cursor` is the grid's `DECCKM`.
    pub fn key(&mut self, c: u32, pressed: bool, app_cursor: bool) -> KeyBytes {
        let slot = match c {
            code::LEFTSHIFT => Some(&mut self.lshift),
            code::RIGHTSHIFT => Some(&mut self.rshift),
            code::LEFTCTRL => Some(&mut self.lctrl),
            code::RIGHTCTRL => Some(&mut self.rctrl),
            code::LEFTALT => Some(&mut self.lalt),
            code::RIGHTALT => Some(&mut self.ralt),
            _ => None,
        };
        if let Some(held) = slot {
            *held = pressed;
            return KeyBytes::EMPTY;
        }
        if !pressed {
            return KeyBytes::EMPTY;
        }
        if c == code::CAPSLOCK {
            self.caps = !self.caps;
            return KeyBytes::EMPTY;
        }

        let mut out = KeyBytes::EMPTY;
        if let Some(seq) = special(c, app_cursor) {
            out.push(seq);
            return out;
        }

        let shift = self.lshift || self.rshift;
        let ctrl = self.lctrl || self.rctrl;
        let byte = match c {
            code::ENTER | code::KPENTER => b'\r',
            code::BACKSPACE => if ctrl { 0x08 } else { 0x7F },
            code::TAB => b'\t',
            code::ESC => 0x1B,
            _ => {
                let Some((lower, upper)) = printable(c) else { return out };
                let letter = lower.is_ascii_lowercase();
                let b = if shift ^ (letter && self.caps) { upper } else { lower };
                if ctrl { control(b).unwrap_or(b) } else { b }
            }
        };
        if self.lalt || self.ralt {
            out.push(b"\x1b");
        }
        out.push(&[byte]);
        out
    }
}

/// Ctrl + a printable byte, as `KeyDecoder` does, plus Ctrl-Space (`NUL`).
fn control(b: u8) -> Option<u8> {
    match b {
        b'a'..=b'z' | b'A'..=b'Z' => Some(b.to_ascii_uppercase() - b'A' + 1),
        b'[' => Some(0x1B),
        b'\\' => Some(0x1C),
        b']' => Some(0x1D),
        b' ' => Some(0),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::code::*;
    use super::*;
    use alloc::vec::Vec;

    const A: u32 = 30;
    const C: u32 = 46;
    const D: u32 = 32;
    const ONE: u32 = 2;
    const MINUS: u32 = 12;
    const LBRACE: u32 = 26;
    const BACKSLASH: u32 = 43;

    fn tap(k: &mut Keyboard, c: u32) -> Vec<u8> {
        let out = k.key(c, true, false).as_bytes().to_vec();
        assert!(k.key(c, false, false).is_empty(), "a release sends nothing");
        out
    }

    /// Every printable key against the Set-1 table in `hal::keyboard`
    /// (same numbers), unshifted and shifted.
    #[test]
    fn the_whole_layout() {
        let mut k = Keyboard::new();
        let typed: Vec<u8> = (1..=57).flat_map(|c| tap(&mut k, c)).collect();
        assert_eq!(typed, b"\x1b1234567890-=\x7f\tqwertyuiop[]\rasdfghjkl;'`\\zxcvbnm,./* ".to_vec());
        // Right Shift held: the range below taps (and releases) Left Shift.
        k.key(RIGHTSHIFT, true, false);
        let typed: Vec<u8> = (2..=53).flat_map(|c| tap(&mut k, c)).collect();
        assert_eq!(typed, b"!@#$%^&*()_+\x7f\tQWERTYUIOP{}\rASDFGHJKL:\"~|ZXCVBNM<>?".to_vec());
    }

    #[test]
    fn caps_lock_changes_letters_only_and_shift_undoes_it() {
        let mut k = Keyboard::new();
        tap(&mut k, CAPSLOCK);
        assert_eq!(tap(&mut k, A), b"A");
        assert_eq!(tap(&mut k, MINUS), b"-");
        k.key(RIGHTSHIFT, true, false);
        assert_eq!(tap(&mut k, A), b"a");
        assert_eq!(tap(&mut k, MINUS), b"_");
        k.key(RIGHTSHIFT, false, false);
        tap(&mut k, CAPSLOCK);
        assert_eq!(tap(&mut k, A), b"a");
    }

    #[test]
    fn control_keys() {
        let mut k = Keyboard::new();
        k.key(LEFTCTRL, true, false);
        assert_eq!(tap(&mut k, C), [3], "^C");
        assert_eq!(tap(&mut k, D), [4], "^D");
        assert_eq!(tap(&mut k, LBRACE), [0x1B]);
        assert_eq!(tap(&mut k, BACKSLASH), [0x1C]);
        assert_eq!(tap(&mut k, SPACE), [0]);
        assert_eq!(tap(&mut k, BACKSPACE), [0x08]);
        assert_eq!(tap(&mut k, ONE), b"1", "no control code: unchanged");
        k.key(LEFTCTRL, false, false);
        assert_eq!(tap(&mut k, C), b"c");
    }

    #[test]
    fn both_sides_of_a_modifier_count_and_releases_are_per_side() {
        let mut k = Keyboard::new();
        k.key(LEFTSHIFT, true, false);
        k.key(RIGHTSHIFT, true, false);
        k.key(LEFTSHIFT, false, false);
        assert_eq!(tap(&mut k, A), b"A", "right shift still held");
        k.key(RIGHTCTRL, true, false);
        assert_eq!(tap(&mut k, A), [1]);
    }

    #[test]
    fn alt_prefixes_escape() {
        let mut k = Keyboard::new();
        k.key(LEFTALT, true, false);
        assert_eq!(tap(&mut k, A), b"\x1ba");
        assert_eq!(tap(&mut k, BACKSPACE), b"\x1b\x7f");
        assert_eq!(tap(&mut k, UP), b"\x1b[A", "a sequence key is not prefixed");
    }

    #[test]
    fn pty_conventions_enter_backspace_delete() {
        let mut k = Keyboard::new();
        assert_eq!(tap(&mut k, ENTER), b"\r");
        assert_eq!(tap(&mut k, KPENTER), b"\r");
        assert_eq!(tap(&mut k, BACKSPACE), b"\x7f");
        assert_eq!(tap(&mut k, DELETE), b"\x1b[3~");
    }

    #[test]
    fn arrows_follow_decckm() {
        let mut k = Keyboard::new();
        let seqs = |k: &mut Keyboard, app| -> Vec<Vec<u8>> {
            [UP, DOWN, RIGHT, LEFT, HOME, END].iter().map(|&c| k.key(c, true, app).as_bytes().to_vec()).collect()
        };
        assert_eq!(seqs(&mut k, false), [b"\x1b[A", b"\x1b[B", b"\x1b[C", b"\x1b[D", b"\x1b[H", b"\x1b[F"]);
        assert_eq!(seqs(&mut k, true), [b"\x1bOA", b"\x1bOB", b"\x1bOC", b"\x1bOD", b"\x1bOH", b"\x1bOF"]);
        assert_eq!(tap(&mut k, PAGEUP), b"\x1b[5~");
        assert_eq!(tap(&mut k, F1), b"\x1bOP");
        assert_eq!(tap(&mut k, F12), b"\x1b[24~");
    }

    #[test]
    fn release_all_forgets_modifiers_but_not_caps() {
        let mut k = Keyboard::new();
        k.key(LEFTCTRL, true, false);
        tap(&mut k, CAPSLOCK);
        k.release_all();
        assert_eq!(tap(&mut k, C), b"C");
    }

    #[test]
    fn unknown_codes_send_nothing() {
        let mut k = Keyboard::new();
        for c in [0, 69, 70, 99, 113, 200, 0xFFFF_FFFF] {
            assert!(tap(&mut k, c).is_empty(), "code {c}");
        }
    }
}
