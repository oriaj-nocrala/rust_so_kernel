// kernel/src/mouse.rs
//
// PS/2 mouse (auxiliary device) driver: 8042 controller enable sequence +
// standard 3-byte packet decode. Parallel to keyboard.rs's role for the
// primary PS/2 port — process_byte() is called from the IRQ12 ISR,
// read_event() is the non-blocking consumer API (mirrors
// keyboard::read_raw_event), backing /dev/input/event1
// (drivers/dev_mouse_event.rs).

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};
use x86_64::instructions::port::Port;

// ============================================================================
// 8042 CONTROLLER INIT
// ============================================================================

const DATA_PORT: u16 = 0x60;
const STATUS_CMD_PORT: u16 = 0x64;

const STATUS_OUTPUT_FULL: u8 = 1 << 0; // a byte is waiting at DATA_PORT
const STATUS_INPUT_FULL: u8 = 1 << 1;  // controller hasn't consumed our last byte yet

// Every controller round-trip below is bounded by this many polls instead
// of looping forever — a machine with no PS/2 mouse (or a controller that
// never ACKs, e.g. some non-QEMU virtualizers) must not hang boot; it
// should just leave the mouse unusable, same "best-effort, log and move
// on" contract as the rest of this kernel's optional device init (ext2
// mount, Freedoom fetch, ...).
const TIMEOUT_POLLS: u32 = 100_000;

fn wait_write(max: u32) -> bool {
    for _ in 0..max {
        if unsafe { Port::<u8>::new(STATUS_CMD_PORT).read() } & STATUS_INPUT_FULL == 0 {
            return true;
        }
    }
    false
}

fn wait_read(max: u32) -> bool {
    for _ in 0..max {
        if unsafe { Port::<u8>::new(STATUS_CMD_PORT).read() } & STATUS_OUTPUT_FULL != 0 {
            return true;
        }
    }
    false
}

fn write_command(cmd: u8) -> bool {
    if !wait_write(TIMEOUT_POLLS) { return false; }
    unsafe { Port::<u8>::new(STATUS_CMD_PORT).write(cmd); }
    true
}

fn write_data(data: u8) -> bool {
    if !wait_write(TIMEOUT_POLLS) { return false; }
    unsafe { Port::<u8>::new(DATA_PORT).write(data); }
    true
}

fn read_data() -> Option<u8> {
    if !wait_read(TIMEOUT_POLLS) { return None; }
    Some(unsafe { Port::<u8>::new(DATA_PORT).read() })
}

/// 0xD4 tells the controller "the next byte on the data port is for the
/// second PS/2 port (the mouse), not the keyboard".
fn mouse_write(data: u8) -> bool {
    write_command(0xD4) && write_data(data)
}

/// Enables the PS/2 auxiliary device, puts it in default streaming mode,
/// and unmasks its IRQ (12, behind the master PIC's cascade line 2).
/// Best-effort — see `TIMEOUT_POLLS`.
pub fn init() {
    if !write_command(0xA8) { // enable auxiliary device
        crate::serial_println!("mouse: 8042 aux-enable timed out — no PS/2 mouse?");
        return;
    }

    if !write_command(0x20) { return; } // "read controller configuration byte"
    let Some(mut config) = read_data() else { return; };
    config |= 0b0000_0010;  // bit1: enable IRQ12
    config &= !0b0010_0000; // bit5: enable aux device clock (0 = enabled)
    if !write_command(0x60) || !write_data(config) { return; } // write it back

    if !mouse_write(0xF6) { return; } // "use default settings"
    if read_data() != Some(0xFA) {
        crate::serial_println!("mouse: 'set defaults' not ACKed, continuing anyway");
    }

    if !mouse_write(0xF4) { return; } // "enable data reporting"
    if read_data() != Some(0xFA) {
        crate::serial_println!("mouse: 'enable reporting' not ACKed — giving up");
        return;
    }

    crate::interrupts::pic::enable_irq(2);  // cascade: master's slave-PIC input
    crate::interrupts::pic::enable_irq(12); // the mouse's own line
    crate::serial_println!("mouse: PS/2 auxiliary device enabled (IRQ12)");
}

// ============================================================================
// PACKET DECODE
// ============================================================================

/// In-progress 3-byte packet state. ISR-only writer (single IRQ line,
/// never reentrant on one core), so a plain cell is enough — same trust
/// model `keyboard.rs`'s modifier `AtomicBool`s already rely on.
struct PacketState {
    bytes: UnsafeCell<[u8; 3]>,
    index: AtomicUsize,
}
unsafe impl Sync for PacketState {}

static PACKET: PacketState = PacketState {
    bytes: UnsafeCell::new([0; 3]),
    index: AtomicUsize::new(0),
};

/// Called from the IRQ12 ISR with each raw byte from the auxiliary device.
pub fn process_byte(byte: u8) {
    let idx = PACKET.index.load(Ordering::Relaxed);

    // A real PS/2 mouse always sets bit 3 on a packet's first byte — if
    // byte 0 doesn't have it, the stream is desynced (e.g. IRQ12 got
    // unmasked mid-packet); drop it and wait for a real first byte
    // instead of decoding a shifted, garbage packet.
    if idx == 0 && byte & 0x08 == 0 {
        return;
    }

    unsafe { (*PACKET.bytes.get())[idx] = byte; }

    if idx < 2 {
        PACKET.index.store(idx + 1, Ordering::Relaxed);
        return;
    }
    PACKET.index.store(0, Ordering::Relaxed);

    let (b0, b1, b2) = unsafe {
        let p = &*PACKET.bytes.get();
        (p[0], p[1], p[2])
    };

    // Overflow bits set → that axis's delta is meaningless; drop the
    // whole packet rather than feed a caller a huge, bogus jump.
    if b0 & 0xC0 != 0 {
        return;
    }

    // 9-bit two's complement: the sign lives in byte0, the magnitude in
    // byte1/byte2. Y is positive-up in the PS/2 protocol (opposite of
    // typical screen coordinates).
    let mut dx = b1 as i32;
    if b0 & 0x10 != 0 { dx -= 256; }
    let mut dy = b2 as i32;
    if b0 & 0x20 != 0 { dy -= 256; }

    MOUSE_EVENTS.push(MouseEvent {
        dx: dx as i16,
        dy: dy as i16,
        buttons: b0 & 0x07, // bit0=left, bit1=right, bit2=middle
    });
}

// ============================================================================
// EVENT QUEUE
// ============================================================================

#[derive(Clone, Copy)]
pub struct MouseEvent {
    pub dx: i16,
    pub dy: i16,
    pub buttons: u8, // bit0=left, bit1=right, bit2=middle
}

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
