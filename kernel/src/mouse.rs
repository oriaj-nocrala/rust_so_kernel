// kernel/src/mouse.rs
//
// PS/2 mouse (auxiliary device) driver — thin kernel-side adapter around
// `hal::mouse`'s pure packet decoder + PortIo-generic 8042 enable
// sequence. Parallel to keyboard.rs's role for the primary PS/2 port:
// process_byte() is called from the IRQ12 ISR, read_event() is the
// non-blocking consumer API (mirrors keyboard::read_raw_event), backing
// /dev/input/event1 (drivers/dev_mouse_event.rs).
//
// This module owns everything that's genuinely hardware access or global
// state: the `X86PortIo` construction, the `pic::enable_irq` calls (a
// different seam/module than the 8042 protocol itself — see
// `hal::mouse::enable_aux`'s doc comment), every `serial_println!`, and the
// ISR-safe decoder + event-ring statics. The 8042 round-trip and the
// 3-byte packet decode/assembly now live in `hal::mouse`, where they're
// unit tested on the host with `cargo test` (see `hal/src/mouse.rs`).

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

pub use hal::mouse::MouseEvent;

use crate::hal::{Driver, DriverError, X86PortIo};

// ============================================================================
// 8042 CONTROLLER INIT
// ============================================================================

/// `crate::hal::Driver` adapter around `hal::mouse::enable_aux` — same
/// shape as `Ac97Driver`/`AcpiDriver`. Best-effort: enables the PS/2
/// auxiliary device, puts it in default streaming mode, and (only on
/// success) unmasks its IRQ line. Returns `Err` and logs on any failure —
/// no PS/2 mouse (or a controller that never ACKs) just means the mouse
/// stays unusable; boot continues either way.
pub struct MouseDriver;

impl MouseDriver {
    pub fn new() -> Self {
        MouseDriver
    }
}

impl Default for MouseDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl Driver for MouseDriver {
    fn name(&self) -> &str {
        "mouse"
    }

    fn init(&mut self) -> Result<(), DriverError> {
        let io = X86PortIo;
        match hal::mouse::enable_aux(&io) {
            Ok(()) => {
                crate::interrupts::pic::enable_irq(2); // cascade: master's slave-PIC input
                crate::interrupts::pic::enable_irq(12); // the mouse's own line
                crate::serial_println!("mouse: PS/2 auxiliary device enabled (IRQ12)");
                Ok(())
            }
            Err(hal::mouse::MouseInitError::AuxEnableTimeout) => {
                crate::serial_println!("mouse: 8042 aux-enable timed out — no PS/2 mouse?");
                Err(DriverError::NotFound)
            }
            Err(hal::mouse::MouseInitError::ReportingNotAcked) => {
                crate::serial_println!("mouse: 'enable reporting' not ACKed — giving up");
                Err(DriverError::NotFound)
            }
        }
    }
}

// ============================================================================
// PACKET DECODE
// ============================================================================

/// In-progress 3-byte packet decoder. ISR-only writer (single IRQ line,
/// never reentrant on one core), so a plain cell is enough — same trust
/// model `keyboard.rs`'s `DecoderCell` uses.
struct DecoderCell(UnsafeCell<hal::mouse::PacketDecoder>);
unsafe impl Sync for DecoderCell {}

static DECODER: DecoderCell = DecoderCell(UnsafeCell::new(hal::mouse::PacketDecoder::new()));

/// Called from the IRQ12 ISR with each raw byte from the auxiliary device.
pub fn process_byte(byte: u8) {
    // SAFETY: only ever called from the IRQ12 ISR, which never reentrs
    // itself.
    let decoder = unsafe { &mut *DECODER.0.get() };
    if let Some(ev) = decoder.push_byte(byte) {
        MOUSE_EVENTS.push(ev);
    }
}

// ============================================================================
// EVENT QUEUE
// ============================================================================

const CAPACITY: usize = 64;

struct MouseEventBuffer {
    buffer: UnsafeCell<[Option<MouseEvent>; CAPACITY]>,
    read: AtomicUsize,
    write: AtomicUsize,
}
unsafe impl Sync for MouseEventBuffer {}

impl MouseEventBuffer {
    const fn new() -> Self {
        Self {
            buffer: UnsafeCell::new([None; CAPACITY]),
            read: AtomicUsize::new(0),
            write: AtomicUsize::new(0),
        }
    }

    fn push(&self, ev: MouseEvent) {
        let write = self.write.load(Ordering::Acquire);
        let next_write = (write + 1) % CAPACITY;
        if next_write != self.read.load(Ordering::Acquire) {
            unsafe { (*self.buffer.get())[write] = Some(ev); }
            self.write.store(next_write, Ordering::Release);
        }
    }

    fn pop(&self) -> Option<MouseEvent> {
        let read = self.read.load(Ordering::Acquire);
        let write = self.write.load(Ordering::Acquire);
        if read == write {
            return None;
        }
        let ev = unsafe { (*self.buffer.get())[read] };
        self.read.store((read + 1) % CAPACITY, Ordering::Release);
        ev
    }
}

static MOUSE_EVENTS: MouseEventBuffer = MouseEventBuffer::new();

/// Non-blocking read of the next decoded packet, or `None` if the queue
/// is empty.
pub fn read_event() -> Option<MouseEvent> {
    MOUSE_EVENTS.pop()
}
