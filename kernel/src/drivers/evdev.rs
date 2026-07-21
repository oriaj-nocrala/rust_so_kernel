// kernel/src/drivers/evdev.rs
//
// Shared wire-format helper for this kernel's Linux-evdev-compatible
// input devices (dev_input_event.rs's keyboard, dev_mouse_event.rs's
// mouse). Factored out so both drivers build the exact same
// `struct input_event` bytes instead of two copies that could drift.

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const SYN_REPORT: u16 = 0;

pub const RECORD_SIZE: usize = 24;

/// Wire-compatible with the real Linux `struct input_event` on x86_64:
/// `struct timeval { long tv_sec; long tv_usec; }` (16 bytes) followed by
/// `__u16 type; __u16 code; __s32 value;` — 24 bytes total, no padding.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct InputEvent {
    tv_sec:  i64,
    tv_usec: i64,
    type_:   u16,
    code:    u16,
    value:   i32,
}

impl InputEvent {
    pub fn now(type_: u16, code: u16, value: i32) -> Self {
        let ms = crate::cpu::tsc::uptime_ms();
        Self {
            tv_sec:  (ms / 1000) as i64,
            tv_usec: ((ms % 1000) * 1000) as i64,
            type_,
            code,
            value,
        }
    }

    /// Built field-by-field (not transmuted) so this stays correct
    /// regardless of struct layout/padding assumptions.
    pub fn to_bytes(self) -> [u8; RECORD_SIZE] {
        let mut out = [0u8; RECORD_SIZE];
        out[0..8].copy_from_slice(&self.tv_sec.to_ne_bytes());
        out[8..16].copy_from_slice(&self.tv_usec.to_ne_bytes());
        out[16..18].copy_from_slice(&self.type_.to_ne_bytes());
        out[18..20].copy_from_slice(&self.code.to_ne_bytes());
        out[20..24].copy_from_slice(&self.value.to_ne_bytes());
        out
    }
}
