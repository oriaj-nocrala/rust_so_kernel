//! PS/2 auxiliary device (mouse) — 8042 enable sequence over `PortIo`, plus
//! a pure 3-byte packet decoder needing no seam at all.
//!
//! Fourth driver migrated onto the `hal` pattern (after ACPI's `PhysMem`,
//! ac97's `PortIo`, and keyboard's fully-seamless decoder) — see
//! `hal/src/acpi.rs` / `hal/src/ac97.rs` / `hal/src/keyboard.rs` and
//! `.claude/skills/kernel-drivers/SKILL.md`. Two independent pieces of
//! logic live here, mirroring the split in the original
//! `kernel/src/mouse.rs`:
//!
//! - [`enable_aux`] — the 8042 controller round-trip (enable auxiliary
//!   device, tweak the config byte, "set defaults", "enable reporting")
//!   needs real port I/O, so it's generic over [`crate::PortIo`] and
//!   host-tested with `ScriptedIo`.
//! - [`PacketDecoder`] — the 3-byte packet assembly + decode never touches
//!   a port (the byte already arrived from the IRQ12 ISR), so like
//!   `hal::keyboard::KeyDecoder` it's a plain pure state machine, tested
//!   with no mock at all.
//!
//! Neither piece logs or touches a global — the kernel adapter
//! (`kernel/src/mouse.rs`) owns the `spin`-free ISR-safe static, the
//! `pic::enable_irq` calls, and every `serial_println!`.

use crate::PortIo;

// ── Packet decode (pure) ────────────────────────────────────────────────────

/// One decoded PS/2 mouse packet: relative motion + button state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    pub dx: i16,
    pub dy: i16,
    pub buttons: u8, // bit0=left, bit1=right, bit2=middle
}

/// In-progress 3-byte packet assembly state, extracted verbatim from the
/// original `kernel/src/mouse.rs`'s `PacketState` (there backed by an
/// `UnsafeCell`+`AtomicUsize` for ISR-safety — that trust model stays in
/// the kernel adapter; this struct is just the plain state + transition
/// function).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PacketDecoder {
    bytes: [u8; 3],
    index: usize,
}

impl PacketDecoder {
    pub const fn new() -> Self {
        PacketDecoder { bytes: [0; 3], index: 0 }
    }

    /// Feeds one raw byte from the auxiliary device. Reproduces
    /// `process_byte` exactly: a desync guard on the first byte (a real
    /// PS/2 mouse always sets bit 3 there), 3-byte assembly, an overflow
    /// check that drops the whole packet, and 9-bit two's-complement
    /// sign extension for dx/dy (Y is positive-up, per the PS/2
    /// protocol — opposite of typical screen coordinates).
    pub fn push_byte(&mut self, byte: u8) -> Option<MouseEvent> {
        // Desync guard: if byte 0 doesn't have bit 3 set, the stream is
        // desynced (e.g. IRQ12 got unmasked mid-packet) — drop it and wait
        // for a real first byte instead of decoding a shifted packet.
        if self.index == 0 && byte & 0x08 == 0 {
            return None;
        }

        self.bytes[self.index] = byte;

        if self.index < 2 {
            self.index += 1;
            return None;
        }
        self.index = 0;

        let (b0, b1, b2) = (self.bytes[0], self.bytes[1], self.bytes[2]);

        // Overflow bits set → that axis's delta is meaningless; drop the
        // whole packet rather than feed a caller a huge, bogus jump.
        if b0 & 0xC0 != 0 {
            return None;
        }

        let mut dx = b1 as i32;
        if b0 & 0x10 != 0 { dx -= 256; }
        let mut dy = b2 as i32;
        if b0 & 0x20 != 0 { dy -= 256; }

        Some(MouseEvent {
            dx: dx as i16,
            dy: dy as i16,
            buttons: b0 & 0x07,
        })
    }
}

// ── 8042 enable sequence (PortIo seam) ──────────────────────────────────────

const DATA_PORT: u16 = 0x60;
const STATUS_CMD_PORT: u16 = 0x64;

const STATUS_OUTPUT_FULL: u8 = 1 << 0; // a byte is waiting at DATA_PORT
const STATUS_INPUT_FULL: u8 = 1 << 1;  // controller hasn't consumed our last byte yet

/// Bounded polling, same "never hang boot" convention as every other
/// optional-hardware probe in this kernel (ac97, rtc, acpi).
pub const TIMEOUT_POLLS: u32 = 100_000;

/// Reasons [`enable_aux`] can fail — the kernel adapter logs which one and
/// gives up (best-effort, same as every other hardware probe here).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseInitError {
    /// The 8042 controller never responded within `TIMEOUT_POLLS` to one of
    /// the setup round-trips: the aux-enable command (`0xA8`), reading or
    /// writing back the configuration byte, or the "set defaults" mouse
    /// command (`0xF6`) itself not reaching the device (its ACK, unlike
    /// "enable reporting"'s, is optional and doesn't fail init — see below).
    AuxEnableTimeout,
    /// The "enable reporting" mouse command (`0xF4`) was sent but never
    /// ACKed (`0xFA`) within `TIMEOUT_POLLS`.
    ReportingNotAcked,
}

fn wait_write<IO: PortIo>(io: &IO, max: u32) -> bool {
    for _ in 0..max {
        if io.inb(STATUS_CMD_PORT) & STATUS_INPUT_FULL == 0 {
            return true;
        }
    }
    false
}

fn wait_read<IO: PortIo>(io: &IO, max: u32) -> bool {
    for _ in 0..max {
        if io.inb(STATUS_CMD_PORT) & STATUS_OUTPUT_FULL != 0 {
            return true;
        }
    }
    false
}

fn write_command<IO: PortIo>(io: &IO, cmd: u8) -> bool {
    if !wait_write(io, TIMEOUT_POLLS) { return false; }
    io.outb(STATUS_CMD_PORT, cmd);
    true
}

fn write_data<IO: PortIo>(io: &IO, data: u8) -> bool {
    if !wait_write(io, TIMEOUT_POLLS) { return false; }
    io.outb(DATA_PORT, data);
    true
}

fn read_data<IO: PortIo>(io: &IO) -> Option<u8> {
    if !wait_read(io, TIMEOUT_POLLS) { return None; }
    Some(io.inb(DATA_PORT))
}

/// `0xD4` tells the controller "the next byte on the data port is for the
/// second PS/2 port (the mouse), not the keyboard".
fn mouse_write<IO: PortIo>(io: &IO, data: u8) -> bool {
    write_command(io, 0xD4) && write_data(io, data)
}

/// Enables the PS/2 auxiliary device and puts it in default streaming mode.
/// Reproduces the original `mouse::init`'s controller round-trip exactly,
/// same order: `0xA8` (enable aux device) → read config (`0x20`) → modify →
/// write back (`0x60`) → `0xF6` ("use default settings", ACK optional — the
/// original logged "continuing anyway" and proceeded regardless of whether
/// this was ACKed, which is why a dropped ACK here is *not* surfaced as an
/// error) → `0xF4` ("enable data reporting", ACK required).
///
/// Does not touch the PIC — unmasking IRQ2/IRQ12 stays in the kernel adapter
/// (a different module/seam entirely), matching the plan's split between
/// "hardware protocol" (here) and "interrupt wiring" (kernel side).
///
/// Pure over the seam — no logging, no globals; the kernel adapter logs
/// `Ok`/`Err` and drives `pic::enable_irq`.
pub fn enable_aux<IO: PortIo>(io: &IO) -> Result<(), MouseInitError> {
    if !write_command(io, 0xA8) {
        return Err(MouseInitError::AuxEnableTimeout);
    }

    if !write_command(io, 0x20) {
        return Err(MouseInitError::AuxEnableTimeout);
    }
    let Some(mut config) = read_data(io) else {
        return Err(MouseInitError::AuxEnableTimeout);
    };
    config |= 0b0000_0010;  // bit1: enable IRQ12
    config &= !0b0010_0000; // bit5: enable aux device clock (0 = enabled)
    if !write_command(io, 0x60) || !write_data(io, config) {
        return Err(MouseInitError::AuxEnableTimeout);
    }

    if !mouse_write(io, 0xF6) {
        return Err(MouseInitError::AuxEnableTimeout);
    }
    let _ = read_data(io); // ACK optional — proceed regardless, as the original did.

    if !mouse_write(io, 0xF4) {
        return Err(MouseInitError::AuxEnableTimeout);
    }
    if read_data(io) != Some(0xFA) {
        return Err(MouseInitError::ReportingNotAcked);
    }

    Ok(())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ScriptedIo;

    // ── Packet decode ────────────────────────────────────────────────────

    #[test]
    fn desynced_first_byte_without_bit3_is_discarded() {
        let mut d = PacketDecoder::new();
        // No bit 3 set -> discarded, index stays 0.
        assert_eq!(d.push_byte(0x00), None);
        assert_eq!(d.index, 0);
        // A real first byte now starts a fresh packet.
        assert_eq!(d.push_byte(0x08), None); // has bit3
        assert_eq!(d.index, 1);
    }

    #[test]
    fn full_valid_packet_decodes() {
        let mut d = PacketDecoder::new();
        // byte0: bit3 set, no overflow, no sign bits; buttons: left+right.
        assert_eq!(d.push_byte(0b0000_1011), None);
        assert_eq!(d.push_byte(10), None); // dx = 10
        let ev = d.push_byte(20).expect("third byte completes the packet");
        assert_eq!(ev, MouseEvent { dx: 10, dy: 20, buttons: 0b011 });
        // Decoder resets for the next packet.
        assert_eq!(d.index, 0);
    }

    #[test]
    fn overflow_bit_drops_the_packet() {
        let mut d = PacketDecoder::new();
        // bit6 (0x40) = X overflow.
        assert_eq!(d.push_byte(0b0100_1000), None);
        assert_eq!(d.push_byte(5), None);
        assert_eq!(d.push_byte(5), None); // dropped, not Some(..)
        assert_eq!(d.index, 0); // still resets for the next packet
    }

    #[test]
    fn negative_dx_via_sign_extension() {
        let mut d = PacketDecoder::new();
        // bit4 (0x10) set -> dx negative: dx = b1 - 256.
        assert_eq!(d.push_byte(0b0001_1000), None); // bit3 + bit4
        assert_eq!(d.push_byte(1), None); // b1 = 1 -> dx = 1 - 256 = -255
        let ev = d.push_byte(0).unwrap();
        assert_eq!(ev.dx, -255);
        assert_eq!(ev.dy, 0);
    }

    #[test]
    fn negative_dy_via_sign_extension() {
        let mut d = PacketDecoder::new();
        // bit5 (0x20) set -> dy negative: dy = b2 - 256.
        assert_eq!(d.push_byte(0b0010_1000), None); // bit3 + bit5
        assert_eq!(d.push_byte(0), None);
        let ev = d.push_byte(2).unwrap(); // b2 = 2 -> dy = 2 - 256 = -254
        assert_eq!(ev.dx, 0);
        assert_eq!(ev.dy, -254);
    }

    #[test]
    fn button_bits_are_masked_to_low_three_bits() {
        let mut d = PacketDecoder::new();
        // bit3 set (required) + all three button bits + both sign bits (not
        // overflow) -> buttons must come out as exactly 0b111.
        assert_eq!(d.push_byte(0b0011_1111), None);
        assert_eq!(d.push_byte(0), None);
        let ev = d.push_byte(0).unwrap();
        assert_eq!(ev.buttons, 0b111);
    }

    // ── 8042 enable sequence ─────────────────────────────────────────────

    #[test]
    fn enable_aux_full_success_sequence() {
        let io = ScriptedIo::new();
        // Config byte read after "read controller configuration" (0x20).
        io.queue_read(DATA_PORT, 0b0010_0000); // aux clock bit set (disabled)
        // "set defaults" (0xF6) ACK, then "enable reporting" (0xF4) ACK.
        io.queue_reads(DATA_PORT, &[0xFA, 0xFA]);
        // STATUS_CMD_PORT reads: input-empty (bit1=0) for every wait_write,
        // output-full (bit0=1) for every wait_read. ScriptedIo returns 0
        // (sticky default) for un-queued reads, which already satisfies
        // wait_write (bit1=0 means "not full" -> ready). wait_read needs
        // bit0=1, so queue that explicitly, sticky across all reads.
        io.queue_read(STATUS_CMD_PORT, STATUS_OUTPUT_FULL as u32);

        assert_eq!(enable_aux(&io), Ok(()));

        let writes = io.writes();
        assert_eq!(
            writes,
            alloc::vec![
                (STATUS_CMD_PORT, 0xA8u32),        // enable aux device
                (STATUS_CMD_PORT, 0x20),           // read config
                (STATUS_CMD_PORT, 0x60),           // write config back
                (DATA_PORT, 0b0000_0010),          // config: IRQ12 bit set, aux-clock bit cleared (enabled)
                (STATUS_CMD_PORT, 0xD4),           // "next byte is for the mouse" (0xF6)
                (DATA_PORT, 0xF6),
                (STATUS_CMD_PORT, 0xD4),           // "next byte is for the mouse" (0xF4)
                (DATA_PORT, 0xF4),
            ]
        );
    }

    #[test]
    fn enable_aux_defaults_not_acked_continues_anyway() {
        let io = ScriptedIo::new();
        io.queue_read(DATA_PORT, 0); // config byte
        // First DATA_PORT read after 0xF6 (set defaults) is NOT 0xFA -> the
        // original logs "continuing anyway" but does not fail. The next
        // read (after 0xF4) IS 0xFA -> overall success.
        io.queue_reads(DATA_PORT, &[0x00, 0xFA]);
        io.queue_read(STATUS_CMD_PORT, STATUS_OUTPUT_FULL as u32);

        assert_eq!(enable_aux(&io), Ok(()));
    }

    #[test]
    fn enable_aux_reporting_never_acked_fails() {
        let io = ScriptedIo::new();
        io.queue_read(DATA_PORT, 0); // config byte
        // "set defaults" ACK, then "enable reporting" never ACKed (0x00
        // sticks for every subsequent read).
        io.queue_reads(DATA_PORT, &[0xFA, 0x00]);
        io.queue_read(STATUS_CMD_PORT, STATUS_OUTPUT_FULL as u32);

        assert_eq!(enable_aux(&io), Err(MouseInitError::ReportingNotAcked));
    }

    #[test]
    fn enable_aux_controller_never_ready_fails_within_timeout_not_hang() {
        let io = ScriptedIo::new();
        // STATUS_CMD_PORT sticks at INPUT_FULL forever -> every wait_write
        // times out immediately on the very first command (0xA8). Must
        // return Err promptly, not hang (this test itself would hang on a
        // real infinite loop).
        io.queue_read(STATUS_CMD_PORT, STATUS_INPUT_FULL as u32);

        assert_eq!(enable_aux(&io), Err(MouseInitError::AuxEnableTimeout));
    }
}
