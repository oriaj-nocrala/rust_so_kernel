use core::{cell::UnsafeCell, sync::atomic::{AtomicUsize, Ordering}};

// Also fed by the serial (COM1/IRQ4) ISR — see init::devices::serial_interrupt_handler
// — so this doubles as a general stdin buffer, not just PS/2. Sized generously
// (not 32) because a pasted/piped burst (e.g. a shell `write` heredoc typed
// fast, or scripted debugging input over `-serial stdio`) can queue up many
// characters faster than the consumer's read()-per-byte loop drains them;
// a too-small ring buffer silently drops the tail of the burst (push() is a
// no-op when full) rather than blocking the producer.
const CAPACITY: usize = 1024;

pub static KEYBOARD_BUFFER: KeyboardBuffer = KeyboardBuffer::new();

pub struct KeyboardBuffer {
    buffer: UnsafeCell<[Option<char>; CAPACITY]>,  // ✅ Explícito sobre interior mutability
    read: AtomicUsize,
    write: AtomicUsize,
}

unsafe impl Sync for KeyboardBuffer {}  // ✅ Documenta que es thread-safe bajo SPSC

impl KeyboardBuffer {
    pub const fn new() -> Self {
        Self {
            buffer: UnsafeCell::new([None; CAPACITY]),
            read: AtomicUsize::new(0),
            write: AtomicUsize::new(0),
        }
    }

    pub fn push(&self, c: char) {
        let write = self.write.load(Ordering::Acquire);
        let next_write = (write + 1) % CAPACITY;

        if next_write != self.read.load(Ordering::Acquire) {
            unsafe {
                let buf = &mut *self.buffer.get();
                buf[write] = Some(c);
            }
            self.write.store(next_write, Ordering::Release);
        }
    }
    
    /// Non-consuming readiness check: true if at least one character is buffered.
    pub fn peek(&self) -> bool {
        self.read.load(Ordering::Acquire) != self.write.load(Ordering::Acquire)
    }

    pub fn pop(&self) -> Option<char> {
        let read = self.read.load(Ordering::Acquire);
        let write = self.write.load(Ordering::Acquire);

        if read == write {
            return None;
        }

        unsafe {
            let buf = &*self.buffer.get();
            let c = buf[read];
            self.read.store((read + 1) % CAPACITY, Ordering::Release);
            c
        }
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
/// movement keys) — see `/dev/kbdraw`. Same lock-free SPSC ring shape as
/// `KeyboardBuffer` (ISR is the sole producer).
pub static RAW_KEY_EVENTS: RawKeyBuffer = RawKeyBuffer::new();

pub struct RawKeyBuffer {
    buffer: UnsafeCell<[Option<RawKeyEvent>; RAW_CAPACITY]>,
    read: AtomicUsize,
    write: AtomicUsize,
}

unsafe impl Sync for RawKeyBuffer {}

impl RawKeyBuffer {
    pub const fn new() -> Self {
        Self {
            buffer: UnsafeCell::new([None; RAW_CAPACITY]),
            read: AtomicUsize::new(0),
            write: AtomicUsize::new(0),
        }
    }

    pub fn push(&self, keycode: u8, pressed: bool) {
        let write = self.write.load(Ordering::Acquire);
        let next_write = (write + 1) % RAW_CAPACITY;

        if next_write != self.read.load(Ordering::Acquire) {
            unsafe {
                let buf = &mut *self.buffer.get();
                buf[write] = Some(RawKeyEvent { keycode, pressed });
            }
            self.write.store(next_write, Ordering::Release);
        }
    }

    pub fn pop(&self) -> Option<RawKeyEvent> {
        let read = self.read.load(Ordering::Acquire);
        let write = self.write.load(Ordering::Acquire);

        if read == write {
            return None;
        }

        unsafe {
            let buf = &*self.buffer.get();
            let ev = buf[read];
            self.read.store((read + 1) % RAW_CAPACITY, Ordering::Release);
            ev
        }
    }
}