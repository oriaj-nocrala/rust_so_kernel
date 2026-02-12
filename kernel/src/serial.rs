// kernel/src/serial.rs
//
// Two writers for COM1 (0x3F8):
//
//   1. `Serial` — behind a Mutex, used by serial_print!/serial_println!.
//      Safe for general kernel code.  Do NOT use from inside allocators
//      or interrupt handlers (risk of deadlock).
//
//   2. `RawSerialWriter` — NO lock, NO allocation.  Implements fmt::Write
//      so it supports full formatting ({}, {:#x}, {:?}, etc.) via
//      format_args!, which is 100% stack-based.
//      Used by serial_print_raw!/serial_println_raw!.
//      Safe from ANY context: allocators, interrupt handlers, panic.
//
//      Trade-off: concurrent writers may interleave at the byte level.
//      In practice this is fine — serial output is for debugging, and
//      interleaving only happens if an interrupt fires mid-write.

use core::fmt;
use x86_64::instructions::port::Port;
use spin::Mutex;

// ============================================================================
// Locked writer (general use)
// ============================================================================

static SERIAL: Mutex<Serial> = Mutex::new(Serial::new());

struct Serial {
    port: Port<u8>,
}

impl Serial {
    const fn new() -> Self {
        Self {
            port: Port::new(0x3F8),
        }
    }

    fn write_byte(&mut self, byte: u8) {
        unsafe {
            self.port.write(byte);
        }
    }
}

impl fmt::Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            self.write_byte(byte);
        }
        Ok(())
    }
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    use fmt::Write;
    SERIAL.lock().write_fmt(args).unwrap();
}

#[macro_export]
macro_rules! serial_print {
    ($($arg:tt)*) => {
        $crate::serial::_print(format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! serial_println {
    () => ($crate::serial_print!("\n"));
    ($($arg:tt)*) => ($crate::serial_print!("{}\n", format_args!($($arg)*)));
}

// ============================================================================
// Lock-free writer (allocators, interrupts, panic)
// ============================================================================

/// Lock-free, allocation-free serial writer.
///
/// Implements `core::fmt::Write` so it supports full formatting:
///
/// ```ignore
/// use core::fmt::Write;
/// let _ = writeln!(RawSerialWriter, "order {}, addr {:#x}", order, addr);
/// ```
///
/// `format_args!` builds its state entirely on the stack — no heap,
/// no locks, no allocator calls.  The `Write::write_fmt` default
/// implementation only calls `write_str`, which is also stack-only.
///
/// SAFETY: Can be called from any context.  Output may interleave if
/// an interrupt fires mid-write — acceptable for debug output.
pub struct RawSerialWriter;

impl fmt::Write for RawSerialWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            unsafe {
                Port::<u8>::new(0x3F8).write(byte);
            }
        }
        Ok(())
    }
}

/// Lock-free print with full formatting support.
///
/// Use this instead of `serial_print!` inside allocators, interrupt
/// handlers, or any context where the `SERIAL` Mutex might be held.
///
/// ```ignore
/// serial_print_raw!("Buddy: OOM for order {}", order);
/// serial_print_raw!("addr = {:#x}", addr.as_u64());
/// ```
#[macro_export]
macro_rules! serial_print_raw {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let _ = write!($crate::serial::RawSerialWriter, $($arg)*);
    }};
}

/// Lock-free println with full formatting support.
#[macro_export]
macro_rules! serial_println_raw {
    () => ($crate::serial_print_raw!("\n"));
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let _ = writeln!($crate::serial::RawSerialWriter, $($arg)*);
    }};
}