// kernel/src/serial.rs (nuevo archivo)

use core::fmt;
use x86_64::instructions::port::Port;
use spin::Mutex;

static SERIAL: Mutex<Serial> = Mutex::new(Serial::new());

pub struct Serial {
    port: Port<u8>,
}

impl Serial {
    const fn new() -> Self {
        Self {
            port: Port::new(0x3F8), // COM1
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

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    use core::fmt::Write;
    SERIAL.lock().write_fmt(args).unwrap();
}