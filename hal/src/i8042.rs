//! Passive presence probe for the legacy 8042 keyboard controller.
//!
//! Exists for one decision: whether this machine has *any* way to type.
//! When the USB HID driver found no keyboard and there is no 8042 either,
//! the kernel is about to hand control to a shell nobody can reach — and
//! the only useful thing left to do is put the boot log on screen and hold
//! it there (see `init::boot`), because on such a machine that screen is
//! the entire diagnostic channel.
//!
//! **Passive on purpose.** The thorough probe is the controller self-test
//! (`0xAA` → expect `0x55`) followed by the keyboard interface test
//! (`0xAB` → expect `0x00`), which is what Linux does — during *its own*
//! 8042 initialisation, before anything depends on the controller. Running
//! either one here would be different: this probe runs after
//! `init_hardware_interrupts` has already set the keyboard up and while a
//! working PS/2 keyboard may be in use, and both commands can leave the
//! controller's interfaces disabled on real hardware. A probe whose failure
//! mode is "the keyboard that was working now doesn't" is worse than no
//! probe at all, so this one only ever *reads* the status port.
//!
//! The signal it reads is the classic one: an x86 I/O port with nothing
//! decoding it floats high, so `0xFF` from the status register means no
//! controller answered. That is exactly the case this needs to detect —
//! a modern board with the legacy controller fused out entirely. It
//! deliberately cannot detect "an 8042 exists but no keyboard is plugged
//! into it": distinguishing those needs the active commands above.

use crate::PortIo;

/// 8042 status register (read) / command register (write).
pub const STATUS_PORT: u16 = 0x64;

/// Value an unimplemented x86 I/O port reads back as — open bus, all lines
/// floating high.
const OPEN_BUS: u8 = 0xFF;

/// Whether a legacy 8042 controller answers at all.
///
/// `false` means the port floated, i.e. there is certainly no PS/2
/// keyboard. `true` means *something* decodes the port — which is
/// necessary but not sufficient for a working keyboard, see the module
/// comment.
pub fn controller_present<IO: PortIo>(io: &IO) -> bool {
    io.inb(STATUS_PORT) != OPEN_BUS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ScriptedIo;

    #[test]
    fn floating_port_means_no_controller() {
        let io = ScriptedIo::new();
        io.queue_read(STATUS_PORT, 0xFF);
        assert!(!controller_present(&io));
    }

    /// A real controller's status register after POST: system flag (bit 2)
    /// set, buffers empty.
    #[test]
    fn a_responding_controller_is_present() {
        let io = ScriptedIo::new();
        io.queue_read(STATUS_PORT, 0x14);
        assert!(controller_present(&io));
    }

    /// Every value but the open-bus one counts as present — including
    /// `0x00`, which is a legal (if unusual) status.
    #[test]
    fn only_open_bus_reads_as_absent() {
        for value in 0x00..=0xFEu8 {
            let io = ScriptedIo::new();
            io.queue_read(STATUS_PORT, value as u32);
            assert!(controller_present(&io), "status {value:#04x} should read as present");
        }
    }

    /// The probe must not write anything — a command byte here would
    /// disturb a working keyboard. This is the point of the whole module,
    /// so it is asserted rather than left to review.
    #[test]
    fn the_probe_writes_nothing() {
        let io = ScriptedIo::new();
        io.queue_read(STATUS_PORT, 0x14);
        let _ = controller_present(&io);
        assert!(io.writes().is_empty(), "probe must be read-only");
    }
}
