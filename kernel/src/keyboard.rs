// kernel/src/keyboard.rs
//
// PS/2 Set-1 scancode decoder with full keyboard support:
//   • All printable ASCII (letters, digits, symbols)
//   • Shift (left + right) for uppercase and shifted symbols
//   • CapsLock (toggle)
//   • Backspace, Enter, Tab, Escape
//   • Arrow keys → ANSI escape sequences (\x1b[A/B/C/D)
//   • Key-release events (scancode | 0x80) used to track modifier state
//
// process_scancode() is called from the keyboard ISR.
// read_key() is the non-blocking consumer API.

use core::sync::atomic::{AtomicBool, Ordering};
use crate::keyboard_buffer::KEYBOARD_BUFFER;

// ============================================================================
// MODIFIER STATE
// ============================================================================

/// True while any Shift key is held down.
static SHIFT: AtomicBool = AtomicBool::new(false);

/// CapsLock toggle.
static CAPS: AtomicBool = AtomicBool::new(false);

/// True after receiving an 0xE0 "extended" prefix byte.
static EXT: AtomicBool = AtomicBool::new(false);

// ============================================================================
// PUBLIC API
// ============================================================================

/// Called from the keyboard ISR with each raw scancode byte.
pub fn process_scancode(scancode: u8) {
    // ── Extended prefix ──────────────────────────────────────────────────
    if scancode == 0xE0 {
        EXT.store(true, Ordering::Relaxed);
        return;
    }

    let ext = EXT.swap(false, Ordering::Relaxed);

    // ── Key release (bit 7 set) ──────────────────────────────────────────
    if scancode >= 0x80 {
        let base = scancode & 0x7F;
        match (ext, base) {
            (false, 0x2A) | (false, 0x36) => SHIFT.store(false, Ordering::Relaxed),
            _ => {}
        }
        return;
    }

    // ── Key press ────────────────────────────────────────────────────────
    let shifted = SHIFT.load(Ordering::Relaxed);
    let caps    = CAPS.load(Ordering::Relaxed);

    // Extended (0xE0-prefixed) codes — arrow keys and a few extras
    if ext {
        match scancode {
            0x48 => { push('\x1b'); push('['); push('A'); } // Up
            0x50 => { push('\x1b'); push('['); push('B'); } // Down
            0x4D => { push('\x1b'); push('['); push('C'); } // Right
            0x4B => { push('\x1b'); push('['); push('D'); } // Left
            0x47 => { push('\x1b'); push('['); push('H'); } // Home
            0x4F => { push('\x1b'); push('['); push('F'); } // End
            0x49 => { push('\x1b'); push('['); push('5'); push('~'); } // PgUp
            0x51 => { push('\x1b'); push('['); push('6'); push('~'); } // PgDn
            0x53 => { push('\x7f'); }                       // Delete → DEL
            _ => {}
        }
        return;
    }

    // Modifier key presses
    match scancode {
        0x2A | 0x36 => { SHIFT.store(true,  Ordering::Relaxed); return; } // Shift
        0x3A        => { let c = CAPS.load(Ordering::Relaxed); CAPS.store(!c, Ordering::Relaxed); return; } // CapsLock
        _ => {}
    }

    if let Some(c) = scancode_to_char(scancode, shifted, caps) {
        push(c);
    }
}

/// Non-blocking read: returns the next buffered character, or None.
pub fn read_key() -> Option<char> {
    KEYBOARD_BUFFER.pop()
}

/// Non-consuming readiness check: true if keyboard buffer has data.
/// Used by poll/epoll to check POLLIN readiness for fd=0 (stdin).
pub fn read_key_peek() -> bool {
    KEYBOARD_BUFFER.peek()
}

// ============================================================================
// HELPERS
// ============================================================================

fn push(c: char) {
    KEYBOARD_BUFFER.push(c);
}

/// Convert a Set-1 scancode to a character given the current modifier state.
///
/// `shifted`: any Shift key is currently held.
/// `caps`:    CapsLock is active (affects letters only).
fn scancode_to_char(sc: u8, shifted: bool, caps: bool) -> Option<char> {
    // For letters, CapsLock XORs with Shift to determine case.
    // For symbols, only Shift matters (CapsLock has no effect).
    let upper = shifted ^ caps; // true → uppercase / shifted symbol

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
