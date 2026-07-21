// kernel/src/drivers/dev_mouse_event.rs
//
// /dev/input/event1 — Linux evdev-wire-compatible PS/2 mouse device.
//
// Same real `struct input_event` wire format as dev_input_event.rs's
// keyboard device (see evdev.rs and that file's header comment for why
// it's a real evdev record, not a lookalike). Real `EV_REL` (`REL_X`/
// `REL_Y`) for relative motion and `EV_KEY` (`BTN_LEFT`/`BTN_RIGHT`/
// `BTN_MIDDLE`) for buttons, each batch terminated by `EV_SYN`/
// `SYN_REPORT` — the same shape a real Linux mouse's evdev node produces.
//
// One PS/2 packet (`mouse.rs`) can turn into several `struct input_event`
// records at once (an X delta, a Y delta, up to 3 button transitions,
// then a sync), but `read()` only ever returns one fixed-size record —
// so each open instance keeps a small pending queue, refilled one PS/2
// packet at a time from the shared `mouse::read_event()` source.

use alloc::boxed::Box;
use crate::fs::types::Stat;
use crate::process::file::{FileHandle, FileResult};
use crate::mouse::MouseEvent;
use super::evdev::{InputEvent, EV_SYN, EV_KEY, EV_REL, SYN_REPORT, RECORD_SIZE};

const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;

const PENDING_CAPACITY: usize = 8; // dx, dy, 3 buttons, sync — comfortably fits

pub struct MouseEventDevice {
    pending: [InputEvent; PENDING_CAPACITY],
    pending_len: usize,
    pending_pos: usize,
    last_buttons: u8,
}

impl MouseEventDevice {
    fn new() -> Self {
        Self {
            // Placeholder content — never read before `pending_len` grows
            // past `pending_pos`, so the value here is inert padding.
            pending: [InputEvent::now(EV_SYN, SYN_REPORT, 0); PENDING_CAPACITY],
            pending_len: 0,
            pending_pos: 0,
            last_buttons: 0,
        }
    }

    /// Decode one PS/2 packet into its evdev record sequence and buffer
    /// them for `read()` to hand out one at a time.
    fn fill_from(&mut self, ev: MouseEvent) {
        let mut n = 0;
        if ev.dx != 0 {
            self.pending[n] = InputEvent::now(EV_REL, REL_X, ev.dx as i32);
            n += 1;
        }
        if ev.dy != 0 {
            self.pending[n] = InputEvent::now(EV_REL, REL_Y, ev.dy as i32);
            n += 1;
        }
        let changed = ev.buttons ^ self.last_buttons;
        if changed & 0x01 != 0 {
            self.pending[n] = InputEvent::now(EV_KEY, BTN_LEFT, (ev.buttons & 0x01 != 0) as i32);
            n += 1;
        }
        if changed & 0x02 != 0 {
            self.pending[n] = InputEvent::now(EV_KEY, BTN_RIGHT, (ev.buttons & 0x02 != 0) as i32);
            n += 1;
        }
        if changed & 0x04 != 0 {
            self.pending[n] = InputEvent::now(EV_KEY, BTN_MIDDLE, (ev.buttons & 0x04 != 0) as i32);
            n += 1;
        }
        self.last_buttons = ev.buttons;

        // Real evdev only emits a sync after a packet that actually
        // changed something — an all-zero PS/2 packet (which does happen)
        // produces no records at all here, not an empty SYN_REPORT.
        if n > 0 {
            self.pending[n] = InputEvent::now(EV_SYN, SYN_REPORT, 0);
            n += 1;
        }

        self.pending_len = n;
        self.pending_pos = 0;
    }
}

impl FileHandle for MouseEventDevice {
    fn read(&mut self, buf: &mut [u8]) -> FileResult<usize> {
        if buf.len() < RECORD_SIZE {
            return Ok(0);
        }

        loop {
            if self.pending_pos < self.pending_len {
                let ev = self.pending[self.pending_pos];
                self.pending_pos += 1;
                buf[..RECORD_SIZE].copy_from_slice(&ev.to_bytes());
                return Ok(RECORD_SIZE);
            }

            match crate::mouse::read_event() {
                None => return Ok(0), // no packet available — caller should poll again
                Some(ev) => self.fill_from(ev), // may still yield zero records; loop again
            }
        }
    }

    fn write(&mut self, buf: &[u8]) -> FileResult<usize> {
        Ok(buf.len()) // writes are ignored
    }

    fn stat(&self) -> Option<Stat> {
        Some(Stat::chardev(0))
    }

    fn dup(&self) -> Option<Box<dyn FileHandle>> {
        // A fresh instance starts with last_buttons=0, so a dup'd reader
        // could see a spurious "button pressed" transition if a button
        // was already held at dup() time — matches the same
        // per-reader-state limitation dev_input_event.rs's pending_syn
        // has, and nothing dup()s this device today.
        Some(Box::new(MouseEventDevice::new()))
    }

    fn name(&self) -> &str {
        "/dev/input/event1"
    }
}

pub fn open() -> Box<dyn FileHandle> {
    Box::new(MouseEventDevice::new())
}
