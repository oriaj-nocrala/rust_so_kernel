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
// state: the `X86PortIo` construction, the `interrupts::enable_isa_irq` call (a
// different seam/module than the 8042 protocol itself — see
// `hal::mouse::enable_aux`'s doc comment), every `serial_println!`, and the
// ISR-safe decoder + event-ring statics. The 8042 round-trip and the
// 3-byte packet decode/assembly now live in `hal::mouse`, where they're
// unit tested on the host with `cargo test` (see `hal/src/mouse.rs`).

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

pub use hal::mouse::MouseEvent;

use diag::IrqMutex;

use crate::hal::{Driver, DriverError, X86PortIo};
use crate::allocator::KernelIrq;

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
                crate::interrupts::enable_isa_irq(12);
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

/// In-progress 3-byte packet decoder. Its only writer is the IRQ12 ISR,
/// and one IRQ line is never delivered to two CPUs at once, so a plain
/// cell is enough — even with SMP, as long as that stays true. The USB
/// mouse does not go through it (its reports arrive whole, see
/// `push_usb_event`); it only shares the event queue below, whose
/// producers are serialised by `PUSH`.
struct DecoderCell(UnsafeCell<hal::mouse::PacketDecoder>);
unsafe impl Sync for DecoderCell {}

static DECODER: DecoderCell = DecoderCell(UnsafeCell::new(hal::mouse::PacketDecoder::new()));

/// Called from the IRQ12 ISR with each raw byte from the auxiliary device.
pub fn process_byte(byte: u8) {
    // SAFETY: only ever called from the IRQ12 ISR, which never reentrs
    // itself.
    let decoder = unsafe { &mut *DECODER.0.get() };
    let before = decoder.resyncs();
    // TSC uptime: calibrated long before IRQ12 is unmasked-and-live.
    if let Some(ev) = decoder.push_byte_at(byte, crate::cpu::tsc::uptime_ms()) {
        push(ev);
    }
    if decoder.resyncs() != before {
        RESYNCS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Partial packets the decoder discarded to get back into step (see
/// `hal::mouse::RESYNC_GAP_MS`). Mirrored out of the decoder so
/// `/proc/kdebug` can read it without touching the ISR's cell.
static RESYNCS: AtomicUsize = AtomicUsize::new(0);

pub fn resyncs() -> usize {
    RESYNCS.load(Ordering::Relaxed)
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

/// Serialises the queue's *producers*. The ring is single-producer by
/// construction (`write` is loaded, the slot filled, then `write` stored),
/// and it has two: the PS/2 IRQ12 ISR and the USB mouse, decoded from the
/// timer ISR's `usb::poll` (or from whoever is draining the xHCI event
/// ring). On one CPU both run with IF=0 and cannot overlap, but that is
/// `cli` doing a lock's job — the SMP rule `keyboard::DECODER` was
/// converted under. The consumer (`read_event`) stays lock-free.
static PUSH: IrqMutex<(), KernelIrq> = IrqMutex::new(());

fn push(ev: MouseEvent) {
    PUSH.with(|_| MOUSE_EVENTS.push(ev));
}

/// Entry point for the USB boot mouse (`usb::xhci`): one decoded report,
/// already in PS/2 sign convention (see `hal::hid::decode_boot_mouse`).
/// A report with no motion and the same buttons is still queued — the
/// evdev layer drops it, the same as an all-zero PS/2 packet.
pub fn push_usb_event(ev: MouseEvent) {
    push(ev);
}

/// Non-blocking read of the next decoded packet, or `None` if the queue
/// is empty.
pub fn read_event() -> Option<MouseEvent> {
    MOUSE_EVENTS.pop()
}
