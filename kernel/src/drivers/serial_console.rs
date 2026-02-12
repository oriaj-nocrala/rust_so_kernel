// kernel/src/drivers/serial_console.rs
//
// Serial console (COM1, 0x3F8) — write-only for now.

use alloc::boxed::Box;
use crate::process::file::{FileHandle, FileError, FileResult};

pub struct SerialConsole;

impl FileHandle for SerialConsole {
    fn read(&mut self, _buf: &mut [u8]) -> FileResult<usize> {
        // TODO: Implement serial read
        Err(FileError::NotSupported)
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

    fn name(&self) -> &str {
        "serial"
    }
}

pub fn open() -> Box<dyn FileHandle> {
    Box::new(SerialConsole)
}