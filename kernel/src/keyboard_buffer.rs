use crate::allocator::KernelIrq;
use diag::IrqMutex;
use hal::ring::Ring;

// Also fed by the serial (COM1/IRQ4) ISR — see init::devices::serial_interrupt_handler
// — so this doubles as a general stdin buffer, not just PS/2. Sized generously
// (not 32) because a pasted/piped burst (e.g. a shell `write` heredoc typed
// fast, or scripted debugging input over `-serial stdio`) can queue up many
// characters faster than the consumer's read()-per-byte loop drains them;
// a too-small ring buffer silently drops the tail of the burst (push() is a
// no-op when full) rather than blocking the producer.
const CAPACITY: usize = 1024;

pub static KEYBOARD_BUFFER: KeyboardBuffer = KeyboardBuffer::new();

/// The stdin character queue. A `hal::ring::Ring` behind an `IrqMutex`
/// rather than the lock-free SPSC ring it was: it has three producers
/// (PS/2 ISR, COM1 ISR, USB poll — the last one runs on whichever CPU is
/// draining the xHCI event ring) and any number of reading processes, and
/// only IF=0 on a single CPU kept them from overlapping (stage 6 of
/// `docs/smp/smp-plan.md`). The critical sections are a few loads and
/// stores.
pub struct KeyboardBuffer(IrqMutex<Ring<char, CAPACITY>, KernelIrq>);

impl KeyboardBuffer {
    pub const fn new() -> Self {
        Self(IrqMutex::new(Ring::new()))
    }

    /// Queue `c`; dropped if the queue is full.
    pub fn push(&self, c: char) {
        self.0.with(|r| r.push(c));
    }

    /// Non-consuming readiness check: true if at least one character is buffered.
    pub fn peek(&self) -> bool {
        self.0.with(|r| !r.is_empty())
    }

    pub fn pop(&self) -> Option<char> {
        self.0.with(|r| r.pop())
    }
}

/// A single raw key transition: PC/AT Set-1 scancode (base keys: bits
/// 0-6 of the make code; E0-extended keys: `0x80 | (scancode & 0x7F)`,
/// the same "extended = base + 0x80" convention `doomkeys.h` itself uses
/// for its scancode-derived keys) plus whether this is a press or release.
#[derive(Clone, Copy)]
pub struct RawKeyEvent {
    pub keycode: u8,
    pub pressed: bool,
}

const RAW_CAPACITY: usize = 256;

/// Parallel to `KEYBOARD_BUFFER`: that one feeds the char/ANSI-escape
/// consumers (stdin, `/dev/kbd`, the tty line discipline); this one carries
/// raw press/release transitions for consumers that need real key-up
/// events and can't get them from a char stream (e.g. a game reading
/// movement keys) — see `/dev/kbdraw`. Same locked ring as `KeyboardBuffer`
/// (two producers: IRQ1 and the USB poll).
pub static RAW_KEY_EVENTS: RawKeyBuffer = RawKeyBuffer::new();

pub struct RawKeyBuffer(IrqMutex<Ring<RawKeyEvent, RAW_CAPACITY>, KernelIrq>);

impl RawKeyBuffer {
    pub const fn new() -> Self {
        Self(IrqMutex::new(Ring::new()))
    }

    pub fn push(&self, keycode: u8, pressed: bool) {
        self.0.with(|r| r.push(RawKeyEvent { keycode, pressed }));
    }

    pub fn pop(&self) -> Option<RawKeyEvent> {
        self.0.with(|r| r.pop())
    }

    /// Non-consuming readiness check, for `poll` on `/dev/input/event0`.
    pub fn peek(&self) -> bool {
        self.0.with(|r| !r.is_empty())
    }
}
