// kernel/src/drivers/dev_input_event.rs
//
// /dev/input/event0 — Linux evdev-wire-compatible keyboard event device.
//
// Replaces the earlier /dev/kbdraw (a bespoke [scancode, pressed] 2-byte
// format). This emits real `struct input_event` records — the exact wire
// format the real Linux kernel's evdev layer produces on x86_64 (16-byte
// `struct timeval` + u16 type + u16 code + i32 value, 24 bytes, no padding)
// — with real `EV_KEY`/`EV_SYN` types and real linux/input-event-codes.h
// `KEY_*` codes. An unmodified evdev client (SDL's linux evdev backend, a
// statically-linked Linux binary parsing /dev/input/eventN directly, ...)
// can read this device with zero protocol translation.
//
// Linux keycode note: for the "base" (non-E0-prefixed) block of the
// keyboard, the AT/PS-2 Set-1 scancode this kernel already decodes IS the
// Linux KEY_* value — linux/input-event-codes.h's numbering was carried
// over directly from the original AT scancode table (KEY_ESC=1, KEY_1=2,
// ... KEY_SPACE=57, exactly scancodes 0x01, 0x02, ... 0x39). Only the
// E0-prefixed extended keys (arrows, right Ctrl/Alt, Home/End/PgUp/PgDn/
// Ins/Del, keypad Enter/Slash) need real translation, since Linux gives
// those distinct standalone codes (KEY_UP=103, etc.) unrelated to their
// raw scancode — see `extended_to_linux_code`.
//
// Known simplification carried over from /dev/kbdraw: `keyboard::RAW_KEY_EVENTS`
// is one global ring buffer consumed destructively, not fanned out per
// open file description the way real evdev's per-client queues are — two
// processes concurrently reading this device would race over the same
// events. No caller does that today (DOOM opens it once), so this hasn't
// been built out; a real multi-client evdev would need a queue per open().

use alloc::boxed::Box;
use crate::fs::types::Stat;
use crate::process::file::{FileHandle, FileResult};
use crate::keyboard_buffer::RawKeyEvent;
use super::evdev::{InputEvent, EV_SYN, EV_KEY, SYN_REPORT, RECORD_SIZE};

/// E0-prefixed scancode (low 7 bits, i.e. `RawKeyEvent::keycode & 0x7F`
/// with the `0x80` extended marker already stripped) → real Linux KEY_*
/// code. The non-extended block doesn't need a table — see module doc.
fn extended_to_linux_code(low7: u8) -> Option<u16> {
    Some(match low7 {
        0x1D => 97,  // KEY_RIGHTCTRL
        0x38 => 100, // KEY_RIGHTALT
        0x1C => 96,  // KEY_KPENTER
        0x35 => 98,  // KEY_KPSLASH
        0x47 => 102, // KEY_HOME
        0x48 => 103, // KEY_UP
        0x49 => 104, // KEY_PAGEUP
        0x4B => 105, // KEY_LEFT
        0x4D => 106, // KEY_RIGHT
        0x4F => 107, // KEY_END
        0x50 => 108, // KEY_DOWN
        0x51 => 109, // KEY_PAGEDOWN
        0x52 => 110, // KEY_INSERT
        0x53 => 111, // KEY_DELETE
        0x5B => 125, // KEY_LEFTMETA
        0x5C => 126, // KEY_RIGHTMETA
        0x5D => 127, // KEY_COMPOSE
        _ => return None,
    })
}

fn to_linux_code(ev: &RawKeyEvent) -> Option<u16> {
    if ev.keycode & 0x80 != 0 {
        extended_to_linux_code(ev.keycode & 0x7F)
    } else if ev.keycode != 0 {
        Some(ev.keycode as u16)
    } else {
        None
    }
}

pub struct InputEventDevice {
    // Real evdev always closes an input "packet" with an EV_SYN/SYN_REPORT
    // once the simultaneous changes it describes are done. We hand back
    // exactly one fixed-size record per read() (never partial, same
    // contract /dev/kbdraw had), so a KEY event's SYN has to wait for the
    // *next* read() call — tracked here rather than queued globally so it
    // stays a pure per-reader concern, not a second kind of record mixed
    // into the shared RAW_KEY_EVENTS ring.
    pending_syn: bool,
}

impl FileHandle for InputEventDevice {
    fn read(&mut self, buf: &mut [u8]) -> FileResult<usize> {
        if buf.len() < RECORD_SIZE {
            return Ok(0);
        }

        if self.pending_syn {
            self.pending_syn = false;
            buf[..RECORD_SIZE].copy_from_slice(&InputEvent::now(EV_SYN, SYN_REPORT, 0).to_bytes());
            return Ok(RECORD_SIZE);
        }

        loop {
            match crate::keyboard::read_raw_event() {
                None => return Ok(0), // no event available — caller should poll again
                Some(ev) => {
                    if let Some(code) = to_linux_code(&ev) {
                        buf[..RECORD_SIZE].copy_from_slice(
                            &InputEvent::now(EV_KEY, code, ev.pressed as i32).to_bytes(),
                        );
                        self.pending_syn = true;
                        return Ok(RECORD_SIZE);
                    }
                    // Unmapped scancode (NumLock, unassigned E0 codes, ...)
                    // — drop it and keep draining rather than surface a
                    // meaningless code to the client.
                }
            }
        }
    }

    fn write(&mut self, buf: &[u8]) -> FileResult<usize> {
        Ok(buf.len()) // writes (e.g. LED state, EVIOCSKEYCODE) are ignored
    }

    fn stat(&self) -> Option<Stat> {
        Some(Stat::chardev(0))
    }

    fn dup(&self) -> Option<Box<dyn FileHandle>> {
        Some(Box::new(InputEventDevice { pending_syn: false }))
    }

    fn name(&self) -> &str {
        "/dev/input/event0"
    }
}

pub fn open() -> Box<dyn FileHandle> {
    Box::new(InputEventDevice { pending_syn: false })
}
