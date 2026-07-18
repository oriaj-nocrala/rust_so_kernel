// kernel/src/drivers/dev_kbd.rs
//
// Non-blocking keyboard device — /dev/kbd
//
// Unlike stdin (fd 0), which blocks the calling process when the keyboard
// buffer is empty, reads from this device never block:
//   - Returns 1 byte when a key is available.
//   - Returns 0 bytes (Ok(0)) when no key is available.
//
// Intended for game loops and other polling consumers.

use alloc::boxed::Box;
use crate::fs::types::Stat;
use crate::process::file::{FileHandle, FileResult};

pub struct KbdDevice;

impl FileHandle for KbdDevice {
    fn read(&mut self, buf: &mut [u8]) -> FileResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        match crate::keyboard::read_key() {
            Some(c) => {
                buf[0] = c as u8;
                Ok(1)
            }
            None => Ok(0), // no key available — caller should poll again
        }
    }

    fn write(&mut self, buf: &[u8]) -> FileResult<usize> {
        Ok(buf.len()) // writes are ignored
    }

    fn stat(&self) -> Option<Stat> {
        Some(Stat::chardev(0))
    }

    fn name(&self) -> &str {
        "/dev/kbd"
    }
}

pub fn open() -> Box<dyn FileHandle> {
    Box::new(KbdDevice)
}
