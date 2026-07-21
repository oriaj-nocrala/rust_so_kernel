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
