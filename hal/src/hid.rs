//! USB HID boot-protocol keyboard decoder — pure logic, no seam at all.
//!
//! Same shape as [`crate::keyboard`] (the PS/2 Set-1 decoder): a
//! `(state, input) -> output` state machine whose input already arrives
//! from elsewhere — there, a scancode from the 8042 ISR; here, an 8-byte
//! report the xHCI driver read out of a DMA buffer. No `PortIo`, no
//! `PhysMem`, no mock: host-testable exactly as written.
//!
//! Two jobs, and the second one is the load-bearing design decision:
//!
//! 1. **Level → edge.** A HID boot report is *state*, not events: byte 0 is
//!    a modifier bitmap, bytes 2..8 are up to six currently-held keycodes in
//!    no particular order. Presses and releases only exist as the
//!    difference between consecutive reports, which is what
//!    [`BootKeyboard::process`] computes.
//!
//! 2. **HID usage → PS/2 Set-1 scancode.** Rather than grow a second
//!    keyboard pipeline, each decoded transition is translated into the
//!    scancode the equivalent PS/2 key would have produced, and fed into
//!    the existing [`crate::keyboard::KeyDecoder`] path. Everything
//!    downstream then works unchanged and identically for both keyboards:
//!    the Shift/Ctrl/CapsLock state machine, the ANSI arrow sequences, the
//!    tty line discipline's Ctrl-C handling, and
//!    `/dev/input/event0`'s evdev translation (whose `KEY_*` codes are
//!    themselves derived from Set-1 — see `drivers/dev_input_event.rs`).
//!    One translation table replaces a parallel copy of all of that.

/// A press/release transition of one HID usage code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HidKeyEvent {
    /// Keyboard/Keypad page usage (HID Usage Tables §10) — `0x04` = 'a',
    /// `0xE0..=0xE7` = the eight modifiers.
    pub usage: u8,
    pub pressed: bool,
}

/// A Set-1 scancode, split into the two bytes the PS/2 wire format would
/// have used: extended keys are prefixed with `0xE0`, and a release sets
/// bit 7 of the code byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Set1Code {
    pub extended: bool,
    pub code: u8,
}

/// Maximum transitions one report can describe: six keys can be released
/// and six different ones pressed at once, plus eight modifier changes.
pub const MAX_EVENTS: usize = 20;

/// Offsets into the 8-byte boot report (HID 1.11 Appendix B.1).
const REPORT_LEN: usize = 8;
const KEYS: core::ops::Range<usize> = 2..8;

/// `ErrorRollOver` — what a keyboard fills every key slot with when more
/// keys are held than the boot protocol can report. The held-key set is
/// unknown for the duration, so the report carries no usable information;
/// treating it as "these six keys are down" would stick six phantom keys.
const ERROR_ROLLOVER: u8 = 0x01;

/// The eight modifier usages, in the bit order of report byte 0.
const MODIFIER_USAGES: [u8; 8] = [0xE0, 0xE1, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7];

/// Held-key state carried between reports. One of these per keyboard
/// interface; the kernel adapter owns it (an ISR-safe static, same trust
/// model as `keyboard.rs`'s `KeyDecoder`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BootKeyboard {
    modifiers: u8,
    keys: [u8; 6],
}

impl BootKeyboard {
    pub const fn new() -> Self {
        BootKeyboard { modifiers: 0, keys: [0; 6] }
    }

    /// Diffs `report` against the previously held state and writes the
    /// transitions into `out`, returning how many were written.
    ///
    /// Releases are emitted before presses, so a report that both releases
    /// Shift and presses a letter can't produce the shifted letter by
    /// accident.
    ///
    /// Returns 0 without disturbing the held state when the report is
    /// unusable: wrong length, or a rollover error (see [`ERROR_ROLLOVER`]).
    /// A short report is a real case — a keyboard that NAKs mid-transfer
    /// can hand back fewer bytes than requested — and it must not be read
    /// as "everything was released".
    pub fn process(&mut self, report: &[u8], out: &mut [HidKeyEvent; MAX_EVENTS]) -> usize {
        if report.len() < REPORT_LEN {
            return 0;
        }
        let keys = &report[KEYS];
        if keys.iter().any(|&k| k == ERROR_ROLLOVER) {
            return 0;
        }

        let mut n = 0usize;
        let mut emit = |usage: u8, pressed: bool, n: &mut usize| {
            if *n < MAX_EVENTS {
                out[*n] = HidKeyEvent { usage, pressed };
                *n += 1;
            }
        };

        // Releases first: keys we held that this report no longer lists.
        for &old in self.keys.iter() {
            if old != 0 && !keys.contains(&old) {
                emit(old, false, &mut n);
            }
        }

        // Modifier transitions, in bit order.
        let changed = self.modifiers ^ report[0];
        for (bit, &usage) in MODIFIER_USAGES.iter().enumerate() {
            if changed & (1 << bit) != 0 {
                emit(usage, report[0] & (1 << bit) != 0, &mut n);
            }
        }

        // Presses: keys this report lists that we weren't holding.
        for &new in keys.iter() {
            if new != 0 && !self.keys.contains(&new) {
                emit(new, true, &mut n);
            }
        }

        self.modifiers = report[0];
        self.keys.copy_from_slice(keys);
        n
    }

    /// Forgets all held keys, emitting a release for each — what a
    /// disconnect has to do, so a key held at unplug time doesn't stay
    /// latched in the downstream decoder's modifier state forever.
    pub fn release_all(&mut self, out: &mut [HidKeyEvent; MAX_EVENTS]) -> usize {
        // An all-zero report is exactly "nothing held" — reuse the diff.
        self.process(&[0u8; REPORT_LEN], out)
    }
}

/// HID Keyboard/Keypad usage → the PS/2 Set-1 scancode for the same key.
///
/// Derived from the two tables side by side (HID Usage Tables §10 and the
/// AT Set-1 scancode table); the shape of it is that the alphanumeric
/// block is dense in both encodings but in *different* orders, so it has
/// to be spelled out rather than computed.
///
/// `None` for usages this kernel's keyboard pipeline has nothing to say
/// about — reserved codes, Pause (whose Set-1 encoding is a 6-byte `0xE1`
/// sequence the PS/2 decoder does not handle either), and the
/// international/media keys.
pub fn usage_to_set1(usage: u8) -> Option<Set1Code> {
    let base = |code: u8| Some(Set1Code { extended: false, code });
    let ext = |code: u8| Some(Set1Code { extended: true, code });

    match usage {
        // a..z — Set-1's letter codes follow the physical QWERTY layout,
        // HID's follow the alphabet, so this is a plain lookup.
        0x04..=0x1D => base(
            [
                0x1E, 0x30, 0x2E, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32,
                0x31, 0x18, 0x19, 0x10, 0x13, 0x1F, 0x14, 0x16, 0x2F, 0x11, 0x2D, 0x15, 0x2C,
            ][(usage - 0x04) as usize],
        ),
        // 1..9 then 0 — both encodings agree on the order, so this one is
        // arithmetic: HID 0x1E ('1') → Set-1 0x02, through HID 0x27 ('0')
        // → Set-1 0x0B.
        0x1E..=0x27 => base(usage - 0x1C),

        0x28 => base(0x1C), // Enter
        0x29 => base(0x01), // Escape
        0x2A => base(0x0E), // Backspace
        0x2B => base(0x0F), // Tab
        0x2C => base(0x39), // Space
        0x2D => base(0x0C), // - _
        0x2E => base(0x0D), // = +
        0x2F => base(0x1A), // [ {
        0x30 => base(0x1B), // ] }
        0x31 => base(0x2B), // \ |
        0x32 => base(0x2B), // non-US # ~ — same position as backslash
        0x33 => base(0x27), // ; :
        0x34 => base(0x28), // ' "
        0x35 => base(0x29), // ` ~
        0x36 => base(0x33), // , <
        0x37 => base(0x34), // . >
        0x38 => base(0x35), // / ?
        0x39 => base(0x3A), // Caps Lock

        0x3A..=0x43 => base(usage - 0x3A + 0x3B), // F1..F10
        0x44 => base(0x57),                       // F11
        0x45 => base(0x58),                       // F12

        0x46 => ext(0x37),  // PrintScreen (the E0 2A / E0 37 pair, short form)
        0x47 => base(0x46), // Scroll Lock
        // 0x48 Pause — no single-byte Set-1 encoding, see doc comment.
        0x49 => ext(0x52), // Insert
        0x4A => ext(0x47), // Home
        0x4B => ext(0x49), // Page Up
        0x4C => ext(0x53), // Delete
        0x4D => ext(0x4F), // End
        0x4E => ext(0x51), // Page Down
        0x4F => ext(0x4D), // Right
        0x50 => ext(0x4B), // Left
        0x51 => ext(0x50), // Down
        0x52 => ext(0x48), // Up

        0x53 => base(0x45), // Num Lock
        0x54 => ext(0x35),  // Keypad /
        0x55 => base(0x37), // Keypad *
        0x56 => base(0x4A), // Keypad -
        0x57 => base(0x4E), // Keypad +
        0x58 => ext(0x1C),  // Keypad Enter
        0x59..=0x61 => base(
            // Keypad 1..9 — the numeric-pad block, in its physical order.
            [0x4F, 0x50, 0x51, 0x4B, 0x4C, 0x4D, 0x47, 0x48, 0x49][(usage - 0x59) as usize],
        ),
        0x62 => base(0x52), // Keypad 0
        0x63 => base(0x53), // Keypad .
        0x64 => base(0x56), // non-US \ | (the key ISO layouts add by the left Shift)
        0x65 => ext(0x5D),  // Application / Menu

        0xE0 => base(0x1D), // Left Ctrl
        0xE1 => base(0x2A), // Left Shift
        0xE2 => base(0x38), // Left Alt
        0xE3 => ext(0x5B),  // Left GUI
        0xE4 => ext(0x1D),  // Right Ctrl
        0xE5 => base(0x36), // Right Shift
        0xE6 => ext(0x38),  // Right Alt
        0xE7 => ext(0x5C),  // Right GUI

        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(modifiers: u8, keys: [u8; 6]) -> [u8; 8] {
        [modifiers, 0, keys[0], keys[1], keys[2], keys[3], keys[4], keys[5]]
    }

    fn events() -> [HidKeyEvent; MAX_EVENTS] {
        [HidKeyEvent { usage: 0, pressed: false }; MAX_EVENTS]
    }

    #[test]
    fn a_single_press_then_release() {
        let mut kb = BootKeyboard::new();
        let mut out = events();

        let n = kb.process(&report(0, [0x04, 0, 0, 0, 0, 0]), &mut out);
        assert_eq!(&out[..n], &[HidKeyEvent { usage: 0x04, pressed: true }]);

        // The same report again is not a repeat — nothing changed.
        assert_eq!(kb.process(&report(0, [0x04, 0, 0, 0, 0, 0]), &mut out), 0);

        let n = kb.process(&report(0, [0, 0, 0, 0, 0, 0]), &mut out);
        assert_eq!(&out[..n], &[HidKeyEvent { usage: 0x04, pressed: false }]);
    }

    #[test]
    fn modifier_bits_become_their_own_usages() {
        let mut kb = BootKeyboard::new();
        let mut out = events();

        // Left Shift (bit 1) + Right Alt (bit 6) down together.
        let n = kb.process(&report(0b0100_0010, [0; 6]), &mut out);
        assert_eq!(
            &out[..n],
            &[
                HidKeyEvent { usage: 0xE1, pressed: true },
                HidKeyEvent { usage: 0xE6, pressed: true },
            ]
        );

        // Shift released, Alt still held.
        let n = kb.process(&report(0b0100_0000, [0; 6]), &mut out);
        assert_eq!(&out[..n], &[HidKeyEvent { usage: 0xE1, pressed: false }]);
    }

    /// Order matters: a report that releases Shift while pressing a letter
    /// must release first, or the letter is decoded shifted downstream.
    #[test]
    fn releases_are_emitted_before_presses() {
        let mut kb = BootKeyboard::new();
        let mut out = events();
        kb.process(&report(0b0000_0010, [0x04, 0, 0, 0, 0, 0]), &mut out);

        let n = kb.process(&report(0, [0x05, 0, 0, 0, 0, 0]), &mut out);
        assert_eq!(
            &out[..n],
            &[
                HidKeyEvent { usage: 0x04, pressed: false },
                HidKeyEvent { usage: 0xE1, pressed: false },
                HidKeyEvent { usage: 0x05, pressed: true },
            ]
        );
    }

    /// The slots are unordered — a keyboard is free to compact them when a
    /// key is released, and that reshuffle must not read as press+release.
    #[test]
    fn slot_order_does_not_matter() {
        let mut kb = BootKeyboard::new();
        let mut out = events();
        kb.process(&report(0, [0x04, 0x05, 0x06, 0, 0, 0]), &mut out);

        let n = kb.process(&report(0, [0x06, 0x04, 0x05, 0, 0, 0]), &mut out);
        assert_eq!(n, 0);
    }

    #[test]
    fn six_keys_at_once_then_all_released() {
        let mut kb = BootKeyboard::new();
        let mut out = events();
        let held = [0x04, 0x05, 0x06, 0x07, 0x08, 0x09];

        assert_eq!(kb.process(&report(0, held), &mut out), 6);
        let n = kb.process(&report(0, [0; 6]), &mut out);
        assert_eq!(n, 6);
        assert!(out[..n].iter().all(|e| !e.pressed));
    }

    #[test]
    fn rollover_reports_are_ignored_entirely() {
        let mut kb = BootKeyboard::new();
        let mut out = events();
        kb.process(&report(0, [0x04, 0, 0, 0, 0, 0]), &mut out);

        assert_eq!(kb.process(&report(0, [ERROR_ROLLOVER; 6]), &mut out), 0);
        // State untouched: 'a' is still held, so releasing it still fires.
        let n = kb.process(&report(0, [0; 6]), &mut out);
        assert_eq!(&out[..n], &[HidKeyEvent { usage: 0x04, pressed: false }]);
    }

    #[test]
    fn short_reports_are_ignored_not_read_as_all_released() {
        let mut kb = BootKeyboard::new();
        let mut out = events();
        kb.process(&report(0, [0x04, 0, 0, 0, 0, 0]), &mut out);

        assert_eq!(kb.process(&[0u8; 4], &mut out), 0);
        assert_eq!(kb.process(&[], &mut out), 0);
        let n = kb.process(&report(0, [0; 6]), &mut out);
        assert_eq!(&out[..n], &[HidKeyEvent { usage: 0x04, pressed: false }]);
    }

    #[test]
    fn release_all_clears_held_keys_and_modifiers() {
        let mut kb = BootKeyboard::new();
        let mut out = events();
        kb.process(&report(0b0000_0001, [0x04, 0x05, 0, 0, 0, 0]), &mut out);

        let n = kb.release_all(&mut out);
        assert_eq!(n, 3);
        assert!(out[..n].iter().all(|e| !e.pressed));
        assert_eq!(kb, BootKeyboard::new());
    }

    /// Spot-checks against the two tables this mapping was built from, on
    /// the rows most likely to be off by one: the ends of each dense run,
    /// and the keys whose Set-1 code is extended.
    #[test]
    fn usage_to_set1_spot_checks() {
        let base = |c| Some(Set1Code { extended: false, code: c });
        let ext = |c| Some(Set1Code { extended: true, code: c });

        assert_eq!(usage_to_set1(0x04), base(0x1E)); // a
        assert_eq!(usage_to_set1(0x1D), base(0x2C)); // z
        assert_eq!(usage_to_set1(0x1E), base(0x02)); // 1
        assert_eq!(usage_to_set1(0x26), base(0x0A)); // 9
        assert_eq!(usage_to_set1(0x27), base(0x0B)); // 0
        assert_eq!(usage_to_set1(0x28), base(0x1C)); // Enter
        assert_eq!(usage_to_set1(0x2C), base(0x39)); // Space
        assert_eq!(usage_to_set1(0x3A), base(0x3B)); // F1
        assert_eq!(usage_to_set1(0x43), base(0x44)); // F10
        assert_eq!(usage_to_set1(0x44), base(0x57)); // F11 — not 0x45
        assert_eq!(usage_to_set1(0x45), base(0x58)); // F12
        assert_eq!(usage_to_set1(0x52), ext(0x48)); // Up
        assert_eq!(usage_to_set1(0x51), ext(0x50)); // Down
        assert_eq!(usage_to_set1(0x4C), ext(0x53)); // Delete
        assert_eq!(usage_to_set1(0xE0), base(0x1D)); // Left Ctrl
        assert_eq!(usage_to_set1(0xE4), ext(0x1D)); // Right Ctrl
        assert_eq!(usage_to_set1(0xE1), base(0x2A)); // Left Shift
        assert_eq!(usage_to_set1(0xE5), base(0x36)); // Right Shift
    }

    /// Every usage the table claims to cover must map, and nothing outside
    /// it may — a missing row shows up here as a dead key on real hardware.
    #[test]
    fn usage_table_coverage_is_contiguous_where_it_should_be() {
        for usage in 0x04..=0x65u8 {
            if usage == 0x48 {
                continue; // Pause, deliberately unmapped
            }
            assert!(usage_to_set1(usage).is_some(), "usage {usage:#04x} unmapped");
        }
        for usage in 0xE0..=0xE7u8 {
            assert!(usage_to_set1(usage).is_some(), "modifier {usage:#04x} unmapped");
        }
        assert_eq!(usage_to_set1(0x00), None);
        assert_eq!(usage_to_set1(ERROR_ROLLOVER), None);
        assert_eq!(usage_to_set1(0x48), None); // Pause
        assert_eq!(usage_to_set1(0x66), None); // Power
        assert_eq!(usage_to_set1(0xFF), None);
    }

    /// No two distinct keys may share a Set-1 code, or one of them types
    /// the other. The two deliberate aliases are the keys that genuinely
    /// occupy the same position on different physical layouts.
    #[test]
    fn set1_codes_are_unique_except_for_known_layout_aliases() {
        let mut seen: [Option<u8>; 512] = [None; 512];
        for usage in (0x04..=0x65u8).chain(0xE0..=0xE7) {
            let Some(c) = usage_to_set1(usage) else { continue };
            let slot = (c.extended as usize) << 8 | c.code as usize;
            if let Some(other) = seen[slot] {
                // 0x31 '\|' and 0x32 non-US '#~' are the same physical key
                // on ANSI vs ISO layouts.
                assert_eq!((other, usage), (0x31, 0x32), "duplicate Set-1 code");
            }
            seen[slot] = Some(usage);
        }
    }
}
