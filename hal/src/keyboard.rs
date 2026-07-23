//! PS/2 Set-1 keyboard scancode decoder — pure logic, no seam at all.
//!
//! Third driver migrated onto the `hal` pattern (after ACPI's `PhysMem` and
//! ac97's `PortIo`) — see `hal/src/acpi.rs` / `hal/src/ac97.rs` for those and
//! `.claude/skills/kernel-drivers/SKILL.md` for the general playbook. This
//! one is the simplest case yet: `process_scancode` in the original
//! `kernel/src/keyboard.rs` never touches a port or any hardware — the raw
//! scancode byte already arrives from the keyboard ISR — so the whole
//! decoder is just a pure `(state, scancode) -> (new state, output)` state
//! machine. No `PortIo`, no `PhysMem`, no mock: it's host-testable exactly
//! as-is, even more directly than the seam-based drivers.
//!
//! [`KeyDecoder::process`] reproduces `process_scancode` byte-for-byte, but
//! instead of the side effects the original had (pushing into
//! `RAW_KEY_EVENTS` / the char buffer via `tty::feed_input`, mutating global
//! `AtomicBool` modifiers), it mutates its own encapsulated state and
//! *returns* what the caller should do, via [`KeyOutput`]. The kernel
//! adapter (`kernel/src/keyboard.rs`) is responsible for holding the
//! `KeyDecoder` in an ISR-safe static and executing those effects (pushing
//! to `RAW_KEY_EVENTS`, routing chars through `tty::feed_input` +
//! `KEYBOARD_BUFFER`).
//!
//! `KeyOutput` deliberately avoids any allocation (this runs in the
//! keyboard ISR): chars are collected into a fixed 4-element inline array,
//! matching the longest sequence any single scancode can ever produce
//! (`\x1b[5~` / `\x1b[6~` for PgUp/PgDn).

// ── Public data model ────────────────────────────────────────────────────────

/// One raw press/release transition, decoupled from the scancode's
/// original E0-prefix encoding: `keycode` is `scancode & 0x7F` for a base
/// key, or `0x80 | (scancode & 0x7F)` for an E0-extended key — the same
/// "extended = base + 0x80" convention the original `RAW_KEY_EVENTS` ring
/// (and `doomkeys.h`) already uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawKey {
    pub keycode: u8,
    pub pressed: bool,
}

/// Maximum chars a single `process()` call can emit — the longest sequence
/// is 4 (`\x1b`, `[`, `5`, `~` for PgUp/PgDn).
const MAX_CHARS: usize = 4;

/// What one `KeyDecoder::process()` call decided to emit. Never allocates —
/// `chars` is a fixed inline array with `nchars` valid entries, exactly like
/// a tiny inline `SmallVec`. The kernel adapter executes the actual effects
/// (raw-event ring push, tty-routed char buffer push); this struct only
/// describes what those effects should be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyOutput {
    /// Always `Some` unless this scancode was the `0xE0` extended prefix
    /// itself (which produces no raw event on its own — the *next* byte's
    /// raw event carries the extended marker instead, matching the
    /// original code exactly).
    pub raw: Option<RawKey>,
    chars: [char; MAX_CHARS],
    nchars: usize,
}

impl KeyOutput {
    fn empty() -> Self {
        KeyOutput { raw: None, chars: ['\0'; MAX_CHARS], nchars: 0 }
    }

    fn push_char(&mut self, c: char) {
        self.chars[self.nchars] = c;
        self.nchars += 1;
    }

    fn push_chars(&mut self, cs: &[char]) {
        for &c in cs {
            self.push_char(c);
        }
    }

    /// The chars this scancode should emit, in order — zero, one, or (for
    /// a few ANSI escape sequences) up to `MAX_CHARS` of them.
    pub fn chars(&self) -> &[char] {
        &self.chars[..self.nchars]
    }
}

/// The keyboard's modifier state machine, extracted verbatim from the
/// original `kernel/src/keyboard.rs`'s four global `AtomicBool`s
/// (`SHIFT`/`CTRL`/`CAPS`/`EXT`) into one plain struct. The kernel adapter
/// holds exactly one of these in an ISR-safe static — see that module's doc
/// comment for the trust model (single ISR producer, never reentrant).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyDecoder {
    shift: bool,
    ctrl: bool,
    caps: bool,
    ext: bool,
}

impl KeyDecoder {
    pub const fn new() -> Self {
        KeyDecoder { shift: false, ctrl: false, caps: false, ext: false }
    }

    /// Reproduces `process_scancode` exactly, decision-for-decision, but
    /// returns what to do instead of doing it:
    ///
    /// 1. `0xE0` → latch `ext` for the next call, emit nothing at all (no
    ///    raw event either — matches the original's early `return`).
    /// 2. Otherwise: consume (and reset) the latched `ext` flag, then
    ///    *always* emit a raw press/release event — this happens before any
    ///    char-decoding branch below, matching the original's unconditional
    ///    `RAW_KEY_EVENTS.push` (a client reading raw events wants every
    ///    transition, whether or not it also produces a character).
    /// 3. Release (`scancode >= 0x80`): update Shift/Ctrl modifier state
    ///    only (matching the original's `(ext, base)` match), emit no
    ///    chars.
    /// 4. Press, extended (`ext`): arrow keys / Home / End / PgUp / PgDn /
    ///    Delete → ANSI sequences; Right Ctrl sets the modifier and emits
    ///    nothing.
    /// 5. Press, non-extended: Shift/Left-Ctrl/CapsLock update modifier
    ///    state and emit nothing; anything else decodes through
    ///    `scancode_to_char` using the modifier state read *before* this
    ///    call's own modifier updates (there are none in this branch, so
    ///    this distinction is moot in practice, but it mirrors the
    ///    original's read-then-branch order exactly).
    pub fn process(&mut self, scancode: u8) -> KeyOutput {
        if scancode == 0xE0 {
            self.ext = true;
            return KeyOutput::empty();
        }

        let ext = core::mem::take(&mut self.ext);
        let mut out = KeyOutput::empty();

        // Raw press/release event — always emitted, before any char
        // decoding, exactly as the original unconditional push.
        let pressed = scancode < 0x80;
        let keycode = if ext { 0x80 | (scancode & 0x7F) } else { scancode & 0x7F };
        out.raw = Some(RawKey { keycode, pressed });

        // ── Key release (bit 7 set) ──────────────────────────────────────
        if scancode >= 0x80 {
            let base = scancode & 0x7F;
            match (ext, base) {
                (false, 0x2A) | (false, 0x36) => self.shift = false,
                (_, 0x1D) => self.ctrl = false, // Ctrl (left or right)
                _ => {}
            }
            return out;
        }

        // ── Key press ─────────────────────────────────────────────────────
        let shifted = self.shift;
        let caps = self.caps;
        let ctrl = self.ctrl;

        // Extended (0xE0-prefixed) codes — arrow keys, right Ctrl, extras.
        if ext {
            match scancode {
                0x1D => { self.ctrl = true; return out; } // Right Ctrl
                0x48 => out.push_chars(&['\x1b', '[', 'A']), // Up
                0x50 => out.push_chars(&['\x1b', '[', 'B']), // Down
                0x4D => out.push_chars(&['\x1b', '[', 'C']), // Right
                0x4B => out.push_chars(&['\x1b', '[', 'D']), // Left
                0x47 => out.push_chars(&['\x1b', '[', 'H']), // Home
                0x4F => out.push_chars(&['\x1b', '[', 'F']), // End
                0x49 => out.push_chars(&['\x1b', '[', '5', '~']), // PgUp
                0x51 => out.push_chars(&['\x1b', '[', '6', '~']), // PgDn
                0x53 => out.push_char('\x7f'), // Delete → DEL
                _ => {}
            }
            return out;
        }

        // Modifier key presses.
        match scancode {
            0x2A | 0x36 => { self.shift = true; return out; } // Shift
            0x1D => { self.ctrl = true; return out; } // Left Ctrl
            0x3A => { self.caps = !self.caps; return out; } // CapsLock
            _ => {}
        }

        if let Some(c) = scancode_to_char(scancode, shifted, caps, ctrl) {
            out.push_char(c);
        }

        out
    }
}

// ── Pure char tables (moved verbatim) ───────────────────────────────────────

/// Convert a Set-1 scancode to a character given the current modifier state.
///
/// `shifted`: any Shift key is currently held.
/// `caps`:    CapsLock is active (affects letters only).
/// `ctrl`:    any Ctrl key is currently held — turns a letter (or `\`/`[`/
///            `]`) into the corresponding C0 control character (e.g.
///            Ctrl-C → 0x03), the same mapping a real PS/2 tty driver uses.
fn scancode_to_char(sc: u8, shifted: bool, caps: bool, ctrl: bool) -> Option<char> {
    // For letters, CapsLock XORs with Shift to determine case.
    // For symbols, only Shift matters (CapsLock has no effect).
    let upper = shifted ^ caps; // true → uppercase / shifted symbol

    let base = scancode_to_base_char(sc, shifted, upper);

    if ctrl {
        if let Some(c) = base {
            if c.is_ascii_alphabetic() {
                let up = c.to_ascii_uppercase() as u8;
                return Some((up - b'A' + 1) as char);
            }
            match c {
                '\\' => return Some('\x1c'), // Ctrl-\  (SIGQUIT by default)
                '['  => return Some('\x1b'), // Ctrl-[ == Esc
                ']'  => return Some('\x1d'),
                _ => {}
            }
        }
    }
    base
}

fn scancode_to_base_char(sc: u8, shifted: bool, upper: bool) -> Option<char> {
    match sc {
        // ── Number row ───────────────────────────────────────────────────
        0x02 => Some(if shifted { '!' } else { '1' }),
        0x03 => Some(if shifted { '@' } else { '2' }),
        0x04 => Some(if shifted { '#' } else { '3' }),
        0x05 => Some(if shifted { '$' } else { '4' }),
        0x06 => Some(if shifted { '%' } else { '5' }),
        0x07 => Some(if shifted { '^' } else { '6' }),
        0x08 => Some(if shifted { '&' } else { '7' }),
        0x09 => Some(if shifted { '*' } else { '8' }),
        0x0A => Some(if shifted { '(' } else { '9' }),
        0x0B => Some(if shifted { ')' } else { '0' }),
        0x0C => Some(if shifted { '_' } else { '-' }), // ← el famoso _
        0x0D => Some(if shifted { '+' } else { '=' }),

        // ── Control keys ─────────────────────────────────────────────────
        0x0E => Some('\x08'), // Backspace → BS
        0x0F => Some('\t'),   // Tab
        0x1C => Some('\n'),   // Enter
        0x01 => Some('\x1b'), // Escape
        0x39 => Some(' '),    // Space

        // ── Top row (QWERTY) ─────────────────────────────────────────────
        0x10 => Some(if upper { 'Q' } else { 'q' }),
        0x11 => Some(if upper { 'W' } else { 'w' }),
        0x12 => Some(if upper { 'E' } else { 'e' }),
        0x13 => Some(if upper { 'R' } else { 'r' }),
        0x14 => Some(if upper { 'T' } else { 't' }),
        0x15 => Some(if upper { 'Y' } else { 'y' }),
        0x16 => Some(if upper { 'U' } else { 'u' }),
        0x17 => Some(if upper { 'I' } else { 'i' }),
        0x18 => Some(if upper { 'O' } else { 'o' }),
        0x19 => Some(if upper { 'P' } else { 'p' }),
        0x1A => Some(if shifted { '{' } else { '[' }),
        0x1B => Some(if shifted { '}' } else { ']' }),
        0x2B => Some(if shifted { '|' } else { '\\' }),

        // ── Home row ─────────────────────────────────────────────────────
        0x1E => Some(if upper { 'A' } else { 'a' }),
        0x1F => Some(if upper { 'S' } else { 's' }),
        0x20 => Some(if upper { 'D' } else { 'd' }),
        0x21 => Some(if upper { 'F' } else { 'f' }),
        0x22 => Some(if upper { 'G' } else { 'g' }),
        0x23 => Some(if upper { 'H' } else { 'h' }),
        0x24 => Some(if upper { 'J' } else { 'j' }),
        0x25 => Some(if upper { 'K' } else { 'k' }),
        0x26 => Some(if upper { 'L' } else { 'l' }),
        0x27 => Some(if shifted { ':' } else { ';' }),
        0x28 => Some(if shifted { '"' } else { '\'' }),
        0x29 => Some(if shifted { '~' } else { '`' }),

        // ── Bottom row ───────────────────────────────────────────────────
        0x2C => Some(if upper { 'Z' } else { 'z' }),
        0x2D => Some(if upper { 'X' } else { 'x' }),
        0x2E => Some(if upper { 'C' } else { 'c' }),
        0x2F => Some(if upper { 'V' } else { 'v' }),
        0x30 => Some(if upper { 'B' } else { 'b' }),
        0x31 => Some(if upper { 'N' } else { 'n' }),
        0x32 => Some(if upper { 'M' } else { 'm' }),
        0x33 => Some(if shifted { '<' } else { ',' }),
        0x34 => Some(if shifted { '>' } else { '.' }),
        0x35 => Some(if shifted { '?' } else { '/' }),

        _ => None,
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_char_table_spot_checks() {
        let mut d = KeyDecoder::new();
        // Digit row, unshifted vs shifted.
        assert_eq!(d.process(0x02).chars(), &['1']);
        assert_eq!(d.process(0x0C).chars(), &['-']);
        // Letters, unshifted lowercase.
        assert_eq!(d.process(0x1E).chars(), &['a']); // A key
        assert_eq!(d.process(0x2E).chars(), &['c']); // C key
        // Control keys.
        assert_eq!(d.process(0x1C).chars(), &['\n']); // Enter
        assert_eq!(d.process(0x0E).chars(), &['\x08']); // Backspace
        assert_eq!(d.process(0x39).chars(), &[' ']); // Space
        assert_eq!(d.process(0x01).chars(), &['\x1b']); // Escape
    }

    #[test]
    fn shift_press_makes_symbols_and_letters_uppercase_shifted() {
        let mut d = KeyDecoder::new();
        // Shift press: no chars emitted, modifier latched.
        let out = d.process(0x2A);
        assert!(out.chars().is_empty());

        // 0x0C -> '_' famoso, shifted.
        assert_eq!(d.process(0x0C).chars(), &['_']);
        // Letter uppercase while shifted.
        assert_eq!(d.process(0x1E).chars(), &['A']);

        // Digit shifted symbol.
        assert_eq!(d.process(0x02).chars(), &['!']);
    }

    #[test]
    fn capslock_xors_with_shift_on_letters_but_not_symbols() {
        let mut d = KeyDecoder::new();
        // CapsLock press toggles caps on, emits no chars.
        assert!(d.process(0x3A).chars().is_empty());

        // Letters: caps alone -> uppercase.
        assert_eq!(d.process(0x1E).chars(), &['A']);

        // Symbols unaffected by caps alone (0x0C stays '-').
        assert_eq!(d.process(0x0C).chars(), &['-']);

        // Now also hold shift: caps XOR shift on letters -> lowercase.
        d.process(0x2A); // shift press
        assert_eq!(d.process(0x1E).chars(), &['a']);
        // Symbols still follow shift only, now shifted -> '_'.
        assert_eq!(d.process(0x0C).chars(), &['_']);
    }

    #[test]
    fn extended_prefix_then_up_arrow_emits_ansi_sequence() {
        let mut d = KeyDecoder::new();
        let prefix_out = d.process(0xE0);
        assert_eq!(prefix_out.raw, None);
        assert!(prefix_out.chars().is_empty());

        let out = d.process(0x48); // Up arrow
        assert_eq!(out.chars(), &['\x1b', '[', 'A']);
        // Raw event for the extended key carries the 0x80-marked keycode.
        assert_eq!(out.raw, Some(RawKey { keycode: 0x80 | 0x48, pressed: true }));
    }

    #[test]
    fn pgup_pgdn_emit_four_char_sequences() {
        let mut d = KeyDecoder::new();
        d.process(0xE0);
        assert_eq!(d.process(0x49).chars(), &['\x1b', '[', '5', '~']); // PgUp
        d.process(0xE0);
        assert_eq!(d.process(0x51).chars(), &['\x1b', '[', '6', '~']); // PgDn
    }

    #[test]
    fn shift_release_clears_modifier() {
        let mut d = KeyDecoder::new();
        d.process(0x2A); // Shift press
        assert_eq!(d.process(0x1E).chars(), &['A']); // uppercase while held

        d.process(0xAA); // Shift release (0x2A | 0x80)
        assert_eq!(d.process(0x1E).chars(), &['a']); // back to lowercase
    }

    #[test]
    fn ctrl_c_produces_c0_control_char() {
        let mut d = KeyDecoder::new();
        assert!(d.process(0x1D).chars().is_empty()); // Left Ctrl press
        assert_eq!(d.process(0x2E).chars(), &['\x03']); // 'c' key -> Ctrl-C
    }

    #[test]
    fn raw_event_reports_keycode_and_press_release_for_base_keys() {
        let mut d = KeyDecoder::new();
        let press = d.process(0x1E);
        assert_eq!(press.raw, Some(RawKey { keycode: 0x1E, pressed: true }));

        let release = d.process(0x9E); // 0x1E | 0x80
        assert_eq!(release.raw, Some(RawKey { keycode: 0x1E, pressed: false }));
    }
}
