// kernel/src/drivers/serial_console.rs
//
// Serial console (COM1, 0x3F8).

use alloc::boxed::Box;
use crate::fs::types::Stat;
use crate::process::file::{FileHandle, FileResult};

pub struct SerialConsole;

impl FileHandle for SerialConsole {
    /// Non-blocking read, mirrors `/dev/kbd`'s poll-style semantics: drains
    /// whatever is buffered and returns `Ok(0)` (not `WouldBlock`) if
    /// nothing is available yet, rather than blocking the caller.
    ///
    /// Bytes come from `keyboard_buffer::KEYBOARD_BUFFER` — the serial IRQ4
    /// handler (`init::devices::serial_interrupt_handler`) pushes received
    /// UART bytes into the same ring buffer the PS/2 keyboard ISR feeds.
    /// That buffer is also what fd 0 (stdin) is hardcoded to read from in
    /// `sys_read`, so a byte typed over the wire is consumed by whichever
    /// side calls `pop()` first — today nothing opens `/dev/console` for
    /// reading directly, so in practice this path only matters if a future
    /// program does `open("/dev/console")` itself.
    fn read(&mut self, buf: &mut [u8]) -> FileResult<usize> {
        let mut n = 0;
        while n < buf.len() {
            match crate::keyboard_buffer::KEYBOARD_BUFFER.pop() {
                Some(c) => { buf[n] = c as u8; n += 1; }
                None => break,
            }
        }
        Ok(n)
    }

    fn write(&mut self, buf: &[u8]) -> FileResult<usize> {
        use x86_64::instructions::port::Port;

        unsafe {
            let mut port = Port::<u8>::new(0x3F8);
            for &byte in buf {
                port.write(byte);
            }
        }

        Ok(buf.len())
    }

    fn stat(&self) -> Option<Stat> {
        Some(Stat::chardev(0))
    }

    fn name(&self) -> &str {
        "serial"
    }
}

pub fn open() -> Box<dyn FileHandle> {
    Box::new(SerialConsole)
}