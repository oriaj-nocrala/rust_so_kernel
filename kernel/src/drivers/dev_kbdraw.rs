// kernel/src/drivers/dev_kbdraw.rs
//
// Non-blocking raw keyboard event device — /dev/kbdraw
//
// Unlike /dev/kbd (translated ASCII + ANSI escape sequences for arrows,
// no key-up events), this exposes real press/release transitions as
// fixed-size 2-byte records: [keycode, pressed]. `keycode` is a PC/AT
// Set-1 scancode (E0-extended keys have 0x80 added — see
// `keyboard_buffer::RawKeyEvent`), `pressed` is 1 on make / 0 on break.
// Intended for clients that need atomic key-up/key-down events a char
// stream can't represent (e.g. a ported game's movement/action keys).
//
// Reads never block: returns 2 bytes for one event, 0 bytes if the queue
// is empty, and never a partial (1-byte) record.

use alloc::boxed::Box;
use crate::fs::types::Stat;
use crate::process::file::{FileHandle, FileResult};

pub struct KbdRawDevice;

impl FileHandle for KbdRawDevice {
    fn read(&mut self, buf: &mut [u8]) -> FileResult<usize> {
        if buf.len() < 2 {
            return Ok(0);
        }
        match crate::keyboard::read_raw_event() {
            Some(ev) => {
                buf[0] = ev.keycode;
                buf[1] = ev.pressed as u8;
                Ok(2)
            }
            None => Ok(0), // no event available — caller should poll again
        }
    }

    fn write(&mut self, buf: &[u8]) -> FileResult<usize> {
        Ok(buf.len()) // writes are ignored
    }

    fn stat(&self) -> Option<Stat> {
        Some(Stat::chardev(0))
    }

    fn dup(&self) -> Option<Box<dyn FileHandle>> {
        Some(Box::new(KbdRawDevice))
    }

    fn name(&self) -> &str {
        "/dev/kbdraw"
    }
}

pub fn open() -> Box<dyn FileHandle> {
    Box::new(KbdRawDevice)
}
