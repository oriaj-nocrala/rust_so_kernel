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

/// Serialises every CONFIG_ADDRESS/CONFIG_DATA pair. Mechanism #1 is two
/// port accesses, and with processes on every CPU a `cat /proc/pci` on one
/// can retarget CONFIG_ADDRESS between another's write and its read. It
/// also covers the SMN index/data pair ([`smn_read`]), itself two config
/// accesses. `IrqLock`: held for a handful of port accesses, never across
/// anything that sleeps.
static CONFIG: crate::sync::IrqLock<()> = crate::sync::IrqLock::new(());

fn raw_read32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    unsafe {
        Port::<u32>::new(CONFIG_ADDRESS).write(config_address(bus, device, function, offset));
        Port::<u32>::new(CONFIG_DATA).read()
    }
}

fn raw_write32(bus: u8, device: u8, function: u8, offset: u8, value: u32) {
    unsafe {
        Port::<u32>::new(CONFIG_ADDRESS).write(config_address(bus, device, function, offset));
        Port::<u32>::new(CONFIG_DATA).write(value);
    }
}

fn config_read32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    let _g = CONFIG.lock();
    raw_read32(bus, device, function, offset)
}

fn config_write32(bus: u8, device: u8, function: u8, offset: u8, value: u32) {
    let _g = CONFIG.lock();
    raw_write32(bus, device, function, offset, value);
}

/// Writes one byte of bus 0 configuration space — read-modify-write of
/// the containing dword, the only width mechanism #1 guarantees. Used for
/// an ACPI reset register that lives in PCI config space (`crate::reboot`).
///
/// That caller includes the panic handler's reset, where the panicking CPU
/// may be the one holding [`CONFIG`]: so the lock is only *tried* for a
/// bounded while, and the reset goes ahead without it — a torn config
/// cycle on the way to a reset costs nothing.
pub fn config_write8(device: u8, function: u8, offset: u8, value: u8) {
    let mut guard = None;
    for _ in 0..100_000 {
        guard = CONFIG.try_lock();
        if guard.is_some() {
            break;
        }
        core::hint::spin_loop();
    }
    let shift = (offset as u32 & 3) * 8;
    let dword = raw_read32(0, device, function, offset & 0xFC);
    let dword = (dword & !(0xFF << shift)) | ((value as u32) << shift);
    raw_write32(0, device, function, offset & 0xFC, dword);
    drop(guard);
}

/// Vendor ID of a function; `0xFFFF` when nothing answers there.
pub fn vendor_id(bus: u8, device: u8, function: u8) -> u16 {
    config_read16(bus, device, function, 0x00)
}

/// Reads an AMD System Management Network register through the root
/// complex's index/data pair (Linux's `amd_smn_read`; see `hal::k10temp`).
/// Only meaningful when 00:00.0 is AMD's — the caller checks.
pub fn smn_read(addr: u32) -> u32 {
    use hal::k10temp::{SMN_DATA, SMN_INDEX};
    let _g = CONFIG.lock();
    raw_write32(0, 0, 0, SMN_INDEX, addr);
    raw_read32(0, 0, 0, SMN_DATA)
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

/// Calls `found` with every function on every bus, decoded from the first
/// 64 bytes of its configuration space.
///
/// A flat 0..=255 bus sweep rather than a recursive bridge walk: brute
/// force costs one config read per (bus, device) pair that has no device —
/// 8192 port reads worst case, microseconds — and unlike a recursive walk
/// it cannot miss a bus behind a bridge this kernel doesn't understand.
pub fn for_each_function(mut found: impl FnMut(hal::pci::Function)) {
    for bus in 0..=255u8 {
        for dev in 0..32u8 {
            if config_read16(bus, dev, 0, 0x00) == 0xFFFF {
                continue;
            }
            let header_type = (config_read32(bus, dev, 0, 0x0C) >> 16) as u8;
            let max_function = if header_type & 0x80 != 0 { 8 } else { 1 };

            for func in 0..max_function {
                let mut cfg = [0u32; 16];
                for (i, dword) in cfg.iter_mut().enumerate() {
                    *dword = config_read32(bus, dev, func, (i * 4) as u8);
                }
                if let Some(f) = hal::pci::decode(bus, dev, func, &cfg) {
                    found(f);
                }
            }
        }
    }
}

/// Every function matching a class/sub-class/prog-IF triple, with its
/// memory BAR assembled, up to `limit` of them. Returns how many were
/// reported.
pub fn for_each_by_class(
    class: u8,
    subclass: u8,
    progif: u8,
    limit: usize,
    mut found: impl FnMut(PciFunction),
) -> usize {
    let mut count = 0usize;
    for_each_function(|f| {
        if count >= limit || (f.class, f.subclass, f.progif) != (class, subclass, progif) {
            return;
        }
        found(PciFunction {
            bus: f.bus,
            device: f.device,
            function: f.function,
            vendor: f.vendor,
            device_id: f.device_id,
            bar0: memory_bar0(f.bus, f.device, f.function),
            interrupt_line: config_read32(f.bus, f.device, f.function, 0x3C) as u8,
        });
        count += 1;
    });
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

// ── Claims and /proc/pci ──────────────────────────────────────────────────────
//
// Which driver owns which function, so `/proc/pci` can say what nothing
// drives (stage 1 of the self-improving-OS direction: the system has to
// know what it is missing before anything can go and get it). A driver
// claims a function once it has actually taken it over — enabled decoding,
// found its registers — not merely matched its ID.

/// More than every driver here claims today (≤ 4 xHCI + ac97 + watchdog +
/// IDE); a claim past it is dropped and logged rather than panicking.
const MAX_CLAIMS: usize = 16;

#[derive(Clone, Copy)]
struct Claim {
    bdf: (u8, u8, u8),
    driver: &'static str,
}

/// A real lock, not IF=0: claims are made from driver init and read from
/// process context, never from an ISR, and never while allocating.
static CLAIMS: crate::sync::Mutex<[Option<Claim>; MAX_CLAIMS]> = crate::sync::Mutex::new([None; MAX_CLAIMS]);

/// Records that `driver` owns function `bus:device.function`.
pub fn claim(bus: u8, device: u8, function: u8, driver: &'static str) {
    let mut claims = CLAIMS.lock();
    let bdf = (bus, device, function);
    if let Some(slot) = claims.iter_mut().find(|c| c.map_or(true, |c| c.bdf == bdf)) {
        *slot = Some(Claim { bdf, driver });
    } else {
        drop(claims);
        crate::serial_println!(
            "pci: claim table full, {:02x}:{:02x}.{} ({}) not recorded",
            bus, device, function, driver
        );
    }
}

/// Claims every function `matches` accepts — for a driver that reaches its
/// device through legacy ports rather than by finding it on the bus.
pub fn claim_matching(driver: &'static str, matches: impl Fn(&hal::pci::Function) -> bool) {
    let mut hits: [(u8, u8, u8); 4] = [(0, 0, 0); 4];
    let mut n = 0;
    for_each_function(|f| {
        if n < hits.len() && matches(&f) {
            hits[n] = (f.bus, f.device, f.function);
            n += 1;
        }
    });
    for &(b, d, f) in &hits[..n] {
        claim(b, d, f, driver);
    }
}

/// Renders `/proc/pci`: every function on the bus and who drives it.
pub fn render_report() -> alloc::string::String {
    let mut functions = alloc::vec::Vec::new();
    for_each_function(|f| functions.push(f));
    let claims = *CLAIMS.lock();
    let driver_of = |f: &hal::pci::Function| {
        claims
            .iter()
            .flatten()
            .find(|c| c.bdf == (f.bus, f.device, f.function))
            .map(|c| c.driver)
    };
    let mut out = alloc::string::String::new();
    let _ = hal::pci::write_report(&mut out, &functions, driver_of);
    out
}
