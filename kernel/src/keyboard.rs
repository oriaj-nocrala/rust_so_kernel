// kernel/src/keyboard.rs
//
// PS/2 Set-1 scancode decoder adapter. All the actual decoding — the
// SHIFT/CTRL/CAPS/EXT state machine, arrow-key/ANSI sequences, Ctrl-C0
// mapping, the char tables — now lives in `hal::keyboard` (see that
// module's doc comment: it needs no `PortIo`/`PhysMem` seam at all, since
// the raw scancode already arrives from the ISR). This file just holds the
// `hal::keyboard::KeyDecoder` in an ISR-safe static and executes the
// effects `KeyDecoder::process` describes: pushing the raw press/release
// transition into `RAW_KEY_EVENTS`, and routing each decoded char through
// the tty line discipline (`tty::feed_input`) into `KEYBOARD_BUFFER`.
//
// process_scancode() is called from the keyboard ISR.
// read_key() is the non-blocking consumer API.

use core::cell::UnsafeCell;
use crate::keyboard_buffer::KEYBOARD_BUFFER;

/// The decoder's modifier state, touched only from the keyboard ISR (one
/// IRQ line, never reentrant on one core) — same trust model the original
/// `AtomicBool`s relied on, and the same one `mouse.rs`'s own `DecoderCell`
/// uses for its ISR-only packet decoder state.
struct DecoderCell(UnsafeCell<hal::keyboard::KeyDecoder>);
unsafe impl Sync for DecoderCell {}

static DECODER: DecoderCell = DecoderCell(UnsafeCell::new(hal::keyboard::KeyDecoder::new()));

// ============================================================================
// PUBLIC API
// ============================================================================

/// Called from the keyboard ISR with each raw scancode byte.
pub fn process_scancode(scancode: u8) {
    // SAFETY: only ever called from the keyboard ISR, which never reentrs
    // itself (single IRQ line, interrupts stay off for the ISR's duration).
    let decoder = unsafe { &mut *DECODER.0.get() };
    let out = decoder.process(scancode);

    // Raw press/release event — see `hal::keyboard::KeyOutput::raw`'s doc
    // comment: always emitted except for the bare 0xE0 prefix byte, before
    // any char-decoding effect below, matching the original unconditional
    // `RAW_KEY_EVENTS.push`.
    if let Some(raw) = out.raw {
        crate::keyboard_buffer::RAW_KEY_EVENTS.push(raw.keycode, raw.pressed);
    }

    for &c in out.chars() {
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

/// Non-blocking read of the next raw press/release transition — see
/// `keyboard_buffer::RAW_KEY_EVENTS`.
pub fn read_raw_event() -> Option<crate::keyboard_buffer::RawKeyEvent> {
    crate::keyboard_buffer::RAW_KEY_EVENTS.pop()
}

// ============================================================================
// HELPERS
// ============================================================================

/// Routes every character through the tty's ISIG line discipline
/// (`tty::feed_input`) before queueing it — a byte that matches the
/// current VINTR/VQUIT/VSUSP setting is turned into a real signal to the
/// foreground process group instead of becoming input (Ctrl-C/Ctrl-\/
/// Ctrl-Z). See `tty.rs`'s module doc comment.
fn push(c: char) {
    if crate::tty::feed_input(c) {
        KEYBOARD_BUFFER.push(c);
    }
}
