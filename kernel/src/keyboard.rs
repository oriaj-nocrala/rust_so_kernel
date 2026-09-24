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
// process_scancode() is called from the keyboard ISR and the USB poll.
// read_key() is the non-blocking consumer API.

use crate::allocator::KernelIrq;
use crate::keyboard_buffer::KEYBOARD_BUFFER;
use diag::IrqMutex;

/// Open `/dev/input/event0` handles holding an `EVIOCGRAB`. While any do,
/// key presses still produce evdev events (and Ctrl-C/Ctrl-\/Ctrl-Z still
/// signal the foreground group), but no characters reach the tty. Without
/// it, everything typed while a game had the keyboard -- arrows, WASD, the
/// `y` of "quit?" -- sat in the tty buffer and `ash` replayed it after the
/// game exited: an up-arrow recalled the last command, the `y` landed at
/// its end, and an Enter ran it. Linux's grab also swallows Ctrl-C; this
/// one keeps it, since there is no second console to switch to if a
/// grabbing program hangs.
static GRABS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

pub fn grab() {
    GRABS.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
}

pub fn ungrab() {
    GRABS.fetch_sub(1, core::sync::atomic::Ordering::SeqCst);
}

/// The decoder's Shift/Ctrl/CapsLock/extended-prefix state. It has two
/// writers: the PS/2 keyboard ISR (IRQ1) and the USB keyboard poll, which
/// runs from the *timer* ISR (`usb::poll` → `process_scancode`). It used to
/// be an `UnsafeCell` justified as "touched only from the keyboard ISR",
/// which stopped being true the day the USB driver arrived; on one CPU the
/// two ISRs still cannot overlap (each runs with IF=0), but that is `cli`
/// doing the work of a lock, and with two CPUs IRQ1 and the tick can land
/// on different ones. An `IrqMutex` makes the exclusion real and costs one
/// uncontended atomic per scancode. The critical section is `process`
/// alone — pure state-machine arithmetic, no allocation, no other lock —
/// so the tty/signal work below runs outside it.
static DECODER: IrqMutex<hal::keyboard::KeyDecoder, KernelIrq> =
    IrqMutex::new(hal::keyboard::KeyDecoder::new());

// ============================================================================
// PUBLIC API
// ============================================================================

/// Called with each raw Set-1 scancode byte, from the PS/2 keyboard ISR
/// and from the USB keyboard poll (timer ISR).
pub fn process_scancode(scancode: u8) {
    let out = DECODER.with(|decoder| decoder.process(scancode));

    // Raw press/release event — see `hal::keyboard::KeyOutput::raw`'s doc
    // comment: always emitted except for the bare 0xE0 prefix byte, before
    // any char-decoding effect below, matching the original unconditional
    // `RAW_KEY_EVENTS.push`.
    if let Some(raw) = out.raw {
        crate::keyboard_buffer::RAW_KEY_EVENTS.push(raw.keycode, raw.pressed);
    }

    let grabbed = GRABS.load(core::sync::atomic::Ordering::SeqCst) != 0;
    for &c in out.chars() {
        if grabbed {
            // Signals only; the character itself is the grabber's.
            let _ = crate::tty::feed_input(c);
        } else {
            push(c);
        }
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
