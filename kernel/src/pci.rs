// kernel/src/pci.rs
//
// Minimal PCI config-space access + bus 0 device enumeration. Written from
// scratch for ac97.rs — nothing in this kernel touched PCI before (every
// other device driver targets a fixed legacy ISA port, e.g. block/ata.rs's
// hardcoded 0x170/0x376, keyboard/mouse's 0x60/0x64).
//
// Legacy mechanism #1 (CONFIG_ADDRESS/CONFIG_DATA, ports 0xCF8/0xCFC) —
// universally supported, no MMCONFIG/ECAM needed for a handful of devices
// on bus 0, which is all QEMU's i440fx machine has.

use x86_64::instructions::port::Port;

const CONFIG_ADDRESS: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

fn config_address(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    debug_assert!(device < 32 && function < 8);
    (1u32 << 31)
        | ((bus as u32) << 16)
        | ((device as u32) << 11)
        | ((function as u32) << 8)
        | (offset as u32 & 0xFC)
}

fn config_read32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    unsafe {
        Port::<u32>::new(CONFIG_ADDRESS).write(config_address(bus, device, function, offset));
        Port::<u32>::new(CONFIG_DATA).read()
    }
}

fn config_write32(bus: u8, device: u8, function: u8, offset: u8, value: u32) {
    unsafe {
        Port::<u32>::new(CONFIG_ADDRESS).write(config_address(bus, device, function, offset));
        Port::<u32>::new(CONFIG_DATA).write(value);
    }
}

/// Writes one byte of bus 0 configuration space — read-modify-write of
/// the containing dword, the only width mechanism #1 guarantees. Used for
/// an ACPI reset register that lives in PCI config space (`crate::reboot`).
pub fn config_write8(device: u8, function: u8, offset: u8, value: u8) {
    let shift = (offset as u32 & 3) * 8;
    let dword = config_read32(0, device, function, offset & 0xFC);
    let dword = (dword & !(0xFF << shift)) | ((value as u32) << shift);
    config_write32(0, device, function, offset & 0xFC, dword);
}

fn config_read16(bus: u8, device: u8, function: u8, offset: u8) -> u16 {
    let dword = config_read32(bus, device, function, offset & 0xFC);
    (dword >> ((offset as u32 & 2) * 8)) as u16
}

/// A PCI function found during enumeration, with the fields `ac97.rs`
/// actually needs — not a general-purpose config-space cache.
#[derive(Clone, Copy)]
pub struct PciDevice {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    /// BAR0, already masked for I/O space (`& 0xFFFFFFFC`) — callers must
    /// confirm bit0 of the raw BAR was 1 (I/O, not memory) before trusting
    /// this; `find_device` only returns devices where that held for both
    /// BAR0 and BAR1, since AC97's NAM/NABM windows are always I/O space.
    pub bar0: u32,
    pub bar1: u32,
    /// Interrupt Line register (offset 0x3C) — legacy IRQ number the BIOS
    /// routed this function to. Read but unused by the current polling-mode
    /// `ac97.rs`; kept for a future interrupt-driven refill.
    pub interrupt_line: u8,
}

/// Scans bus 0 (the only bus QEMU's i440fx machine has) for a function
/// matching `vendor`/`device`. Checks the multifunction bit (header type,
/// offset 0x0E, bit 7) before probing functions 1-7, same as any minimal
/// PCI scanner.
pub fn find_device(vendor: u16, device: u16) -> Option<PciDevice> {
    for dev in 0..32u8 {
        let vendor_id = config_read16(0, dev, 0, 0x00);
        if vendor_id == 0xFFFF {
            continue; // no device in this slot
        }

        let header_type = (config_read32(0, dev, 0, 0x0C) >> 16) as u8;
        let is_multifunction = header_type & 0x80 != 0;
        let max_function = if is_multifunction { 8 } else { 1 };

        for func in 0..max_function {
            let vid = config_read16(0, dev, func, 0x00);
            if vid == 0xFFFF {
                continue;
            }
            let did = config_read16(0, dev, func, 0x02);
            if vid != vendor || did != device {
                continue;
            }

            let bar0_raw = config_read32(0, dev, func, 0x10);
            let bar1_raw = config_read32(0, dev, func, 0x14);
            if bar0_raw & 1 == 0 || bar1_raw & 1 == 0 {
                continue; // not I/O-space BARs — not the device shape we expect
            }

            let interrupt_line = config_read32(0, dev, func, 0x3C) as u8;

            return Some(PciDevice {
                bus: 0,
                device: dev,
                function: func,
                bar0: bar0_raw & 0xFFFF_FFFC,
                bar1: bar1_raw & 0xFFFF_FFFC,
                interrupt_line,
            });
        }
    }
    None
}

/// Sets the Command register's I/O Space Enable (bit0) and Bus Master
/// Enable (bit2) bits — required before the device will respond to I/O
/// port access or perform DMA. Offset 0x04 is a 32-bit-aligned dword
/// holding Command (low 16 bits) + Status (high 16 bits, mostly RW1C) —
/// only the low bits are touched; the high bits are written back exactly
/// as read.
pub fn enable_bus_master_and_io(dev: &PciDevice) {
    let dword = config_read32(dev.bus, dev.device, dev.function, 0x04);
    let command = (dword as u16) | 0b0000_0101; // bit0: I/O space, bit2: bus master
    let new_dword = (dword & 0xFFFF_0000) | command as u32;
    config_write32(dev.bus, dev.device, dev.function, 0x04, new_dword);
}

// ── Class-code discovery + memory BARs (added for the xHCI driver) ───────────
//
// Everything above this line was written for ac97, whose device is found
// by an exact vendor/device ID at a fixed slot on bus 0 and whose BARs are
// both I/O space. A USB host controller is the opposite on all three
// counts, so it needs its own discovery path rather than a loosened
// `find_device`:
//
//   * It is identified by *class*, not ID — nobody knows the device ID of
//     whatever xHCI silicon a given motherboard carries, and the class
//     code (0x0C/0x03/0x30) is exactly what "any xHCI controller" means.
//   * Its registers live in a memory BAR, frequently a 64-bit one (the
//     low BAR's bits 2:1 = 0b10, with the high half in the next BAR).
//     Reading only BAR0 there yields a truncated address.
//   * On a real AM4/Ryzen board it is not on bus 0 — `lspci` puts the
//     chipset controllers several buses deep — so the scan has to cover
//     more than the single bus QEMU's i440fx machine has.

/// Base Class 0x0C (Serial Bus), Sub-Class 0x03 (USB), Prog-IF 0x30 (xHCI).
pub const CLASS_SERIAL_BUS: u8 = 0x0C;
pub const SUBCLASS_USB: u8 = 0x03;
pub const PROGIF_XHCI: u8 = 0x30;

/// A PCI function located by class code, with its memory BAR already
/// assembled from however many 32-bit BAR registers it occupies.
#[derive(Clone, Copy)]
pub struct PciFunction {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor: u16,
    pub device_id: u16,
    /// BAR0's address with the type bits masked off, widened to 64 bits by
    /// folding in BAR1 when BAR0 declares itself 64-bit. Zero if BAR0 is
    /// an I/O BAR or unprogrammed.
    pub bar0: u64,
    pub interrupt_line: u8,
}

/// Revision ID (config offset 0x08, low byte). Drivers that pick a register
/// layout by chipset revision need it (the SP5100 TCO watchdog does).
pub fn revision_id(bus: u8, device: u8, function: u8) -> u8 {
    config_read32(bus, device, function, 0x08) as u8
}

fn class_triple(bus: u8, device: u8, function: u8) -> (u8, u8, u8) {
    let dword = config_read32(bus, device, function, 0x08);
    (
        (dword >> 24) as u8, // base class
        (dword >> 16) as u8, // sub-class
        (dword >> 8) as u8,  // prog-IF
    )
}

/// Reads BAR0 as a memory BAR, following the 64-bit form into BAR1.
/// Returns 0 for an I/O BAR — callers wanting ports use `find_device`.
fn memory_bar0(bus: u8, device: u8, function: u8) -> u64 {
    let low = config_read32(bus, device, function, 0x10);
    if low & 1 != 0 {
        return 0; // I/O space BAR, not memory
    }
    // Bits 2:1 encode the BAR's width: 0b00 = 32-bit, 0b10 = 64-bit.
    let is_64bit = (low >> 1) & 0x3 == 0x2;
    let base = (low & 0xFFFF_FFF0) as u64;
    if is_64bit {
        base | ((config_read32(bus, device, function, 0x14) as u64) << 32)
    } else {
        base
    }
}

/// Scans every PCI bus for functions matching a class/sub-class/prog-IF
/// triple, calling `found` with each. Stops early (returning `false` from
/// `found` is not supported — the caller simply ignores extras) once
/// `limit` functions have been reported.
///
/// A flat 0..=255 bus sweep rather than a recursive bridge walk: brute
/// force costs one config read per (bus, device) pair that has no device —
/// 8192 port reads worst case, microseconds — and unlike a recursive walk
/// it cannot miss a bus behind a bridge this kernel doesn't understand.
pub fn for_each_by_class(
    class: u8,
    subclass: u8,
    progif: u8,
    limit: usize,
    mut found: impl FnMut(PciFunction),
) -> usize {
    let mut count = 0usize;
    for bus in 0..=255u8 {
        for dev in 0..32u8 {
            let vendor_id = config_read16(bus, dev, 0, 0x00);
            if vendor_id == 0xFFFF {
                continue;
            }
            let header_type = (config_read32(bus, dev, 0, 0x0C) >> 16) as u8;
            let max_function = if header_type & 0x80 != 0 { 8 } else { 1 };

            for func in 0..max_function {
                let vid = config_read16(bus, dev, func, 0x00);
                if vid == 0xFFFF {
                    continue;
                }
                if class_triple(bus, dev, func) != (class, subclass, progif) {
                    continue;
                }
                found(PciFunction {
                    bus,
                    device: dev,
                    function: func,
                    vendor: vid,
                    device_id: config_read16(bus, dev, func, 0x02),
                    bar0: memory_bar0(bus, dev, func),
                    interrupt_line: config_read32(bus, dev, func, 0x3C) as u8,
                });
                count += 1;
                if count >= limit {
                    return count;
                }
            }
        }
    }
    count
}

/// Sets Memory Space Enable (bit 1) and Bus Master Enable (bit 2) in the
/// Command register — the memory-BAR counterpart of
/// `enable_bus_master_and_io`. Also clears the Interrupt Disable bit's
/// opposite: nothing here enables interrupts, so bit 10 (Interrupt
/// Disable) is *set*, making it explicit that this controller must not
/// raise a legacy INTx line the kernel has no handler for.
pub fn enable_mem_and_bus_master(bus: u8, device: u8, function: u8) {
    let dword = config_read32(bus, device, function, 0x04);
    let command = ((dword as u16) | 0b0000_0110) | (1 << 10);
    let new_dword = (dword & 0xFFFF_0000) | command as u32;
    config_write32(bus, device, function, 0x04, new_dword);
}
