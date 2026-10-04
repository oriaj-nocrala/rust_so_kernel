// kernel/src/network/rtl8168.rs
//
// Realtek RTL8111/8168 (PCI 10ec:8168), the NIC of the AM4 machine. All the
// register and ring logic is `hal::rtl8169` (host-tested against a software
// model); this file owns what only the kernel can do: finding the function,
// mapping BAR2, the DMA arena, the waits, and the log lines a metal run is
// read from. QEMU has no such device, so none of this runs there.
//
// Bring-up is a ladder (`nic=`, `hal::bootopts::NicLevel`), each level
// including the ones before it, and every step logs *before* it touches the
// device, so a hang names itself in the log partition:
//   probe  read-only: BAR2, XID, MAC, PHY status, a hex dump of the window
//   reset  + chip reset, auto-negotiation restart, wait for link; no DMA
//   net    + rings, TX/RX; the network stack runs on it
// See docs/net/rtl8168.md for what to send back after a metal run.

use hal::bootopts::NicLevel;
use hal::rtl8169 as r;
use x86_64::PhysAddr;

use crate::memory::dma::DmaBuf;
use crate::serial_println;

const VENDOR: u16 = 0x10EC;
const DEVICE: u16 = 0x8168;
/// The chip does 64-bit DMA with `PCIDAC` set (`hal::rtl8169::init_rings`).
const DMA_MASK: u64 = u64::MAX >> 17;
/// Longest wait for auto-negotiation, in milliseconds.
const LINK_WAIT_MS: u64 = 6_000;

/// The register window: volatile MMIO.
pub struct KRegs {
    base: *mut u8,
}

// SAFETY: device registers, only used by the owner of the driver.
unsafe impl Send for KRegs {}

impl r::Regs for KRegs {
    fn r8(&self, off: usize) -> u8 {
        // SAFETY: `off` is inside the `REG_WINDOW` bytes `probe` mapped.
        unsafe { core::ptr::read_volatile(self.base.add(off)) }
    }
    fn r16(&self, off: usize) -> u16 {
        // SAFETY: as above; the offsets in `hal::rtl8169` are 2-aligned for 16-bit registers.
        unsafe { core::ptr::read_volatile(self.base.add(off) as *const u16) }
    }
    fn r32(&self, off: usize) -> u32 {
        // SAFETY: as above, 4-aligned.
        unsafe { core::ptr::read_volatile(self.base.add(off) as *const u32) }
    }
    fn w8(&self, off: usize, val: u8) {
        // SAFETY: as `r8`.
        unsafe { core::ptr::write_volatile(self.base.add(off), val) }
    }
    fn w16(&self, off: usize, val: u16) {
        // SAFETY: as `r16`.
        unsafe { core::ptr::write_volatile(self.base.add(off) as *mut u16, val) }
    }
    fn w32(&self, off: usize, val: u32) {
        // SAFETY: as `r32`.
        unsafe { core::ptr::write_volatile(self.base.add(off) as *mut u32, val) }
    }
}

/// The descriptor rings and packet buffers: one DMA block.
pub struct KDma {
    buf: DmaBuf,
}

impl r::DmaMem for KDma {
    fn read(&self, off: usize, out: &mut [u8]) {
        self.buf.read(off, out);
    }
    fn write(&self, off: usize, data: &[u8]) {
        self.buf.write(off, data);
    }
    fn bus_addr(&self, off: usize) -> u64 {
        self.buf.bus_addr() + off as u64
    }
}

pub struct Rtl {
    drv: r::Rtl8168<KRegs, KDma>,
    mac: [u8; 6],
}

impl Rtl {
    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    pub fn link_up(&self) -> bool {
        self.drv.link().up
    }
}

impl Rtl {
    /// `/proc/nic`: the driver's counters, registers, ring cursors and last frames.
    pub fn report(&self) -> alloc::string::String {
        let mut out = alloc::string::String::new();
        let _ = self.drv.report(&mut out);
        out
    }
}

impl net::Nic for Rtl {
    fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
        self.drv.recv(buf)
    }
    fn send(&mut self, frame: &[u8]) -> bool {
        self.drv.send(frame)
    }
}

/// Reserves a vector and aims the function's MSI-X (else MSI) at it. `None`
/// (polled) when none of that works; the reason is logged.
fn setup_irq(bus: u8, dev: u8, func: u8, bars: &[Option<hal::pcicfg::Bar>; 6], apic: u32) -> Option<u8> {
    let vector = crate::interrupts::msi::alloc(crate::network::irq)?;
    let via = match crate::pci::enable_msix(bus, dev, func, bars, apic, vector) {
        Ok(()) => "MSI-X",
        Err(e) => {
            serial_println!("rtl8168: no MSI-X ({}), trying MSI", e);
            match crate::pci::enable_msi(bus, dev, func, apic, vector) {
                Ok(()) => "MSI",
                Err(e) => {
                    serial_println!("rtl8168: no MSI ({}), polling", e);
                    crate::interrupts::msi::free(vector);
                    return None;
                }
            }
        }
    };
    serial_println!("rtl8168: {} to vector {:#x}, APIC {}", via, vector, apic);
    Some(vector)
}

fn log_dump(regs: &KRegs, what: &str) {
    use r::Regs;
    serial_println!("rtl8168: register window ({}):", what);
    for row in 0..(r::REG_WINDOW / 16) {
        let mut line = [0u8; 16];
        for (i, b) in line.iter_mut().enumerate() {
            *b = regs.r8(row * 16 + i);
        }
        serial_println!("rtl8168:  {:02x}: {:02x?}", row * 16, line);
    }
}

/// Finds the NIC and brings it as far as `level` allows. At `Net` with an
/// `irq_apic` the chip's MSI-X (else MSI) goes to `network::irq` on that CPU. `None`: no such
/// function, `level` is `Off`, or the ladder stopped before `Net` (the
/// reason is in the log). Boot-only: it maps MMIO and busy-waits.
pub fn probe(level: NicLevel, irq_apic: Option<u32>) -> Option<Rtl> {
    if level == NicLevel::Off {
        return None;
    }
    let mut found = None;
    crate::pci::for_each_function(|f| {
        if found.is_none() && f.vendor == VENDOR && f.device_id == DEVICE {
            found = Some(f);
        }
    });
    let f = found?;
    let (bus, dev, func) = (f.bus, f.device, f.function);
    serial_println!(
        "rtl8168: {:02x}:{:02x}.{} {:04x}:{:04x} rev {:02x} subsystem {:?}, level {:?}",
        bus, dev, func, f.vendor, f.device_id, f.revision, f.subsystem, level
    );
    crate::pci::claim(bus, dev, func, "rtl8168");

    // BAR2 is the register window on this family (BAR0 is the same registers in I/O space).
    serial_println!("rtl8168: sizing BARs");
    let bars = crate::pci::size_bars(bus, dev, func);
    let Some(bar) = bars[2] else {
        serial_println!("rtl8168: BAR2 is not a memory BAR, giving up");
        return None;
    };
    serial_println!("rtl8168: BAR2 {:?} at {:#x} size {:#x}", bar.kind, bar.addr, bar.size);
    if bar.size < r::REG_WINDOW as u64 {
        serial_println!("rtl8168: BAR2 smaller than the register window, giving up");
        return None;
    }
    // Memory decode is all a read-only probe needs; bus mastering only once DMA is on the table.
    if level >= NicLevel::Net {
        crate::pci::enable_mem_and_bus_master(bus, dev, func);
    } else {
        crate::pci::update_command(bus, dev, func, hal::pcicfg::COMMAND_MEMORY, 0);
    }
    serial_println!("rtl8168: mapping the register window");
    // SAFETY: a device register window of a PCI BAR; boot-only.
    let virt = unsafe { crate::memory::mmio::map(PhysAddr::new(bar.addr), r::REG_WINDOW) };
    let Some(virt) = virt else {
        serial_println!("rtl8168: cannot map BAR2");
        return None;
    };
    let regs = KRegs { base: virt.as_mut_ptr() };

    let dma = if level >= NicLevel::Net {
        serial_println!("rtl8168: allocating the DMA arena ({} KiB)", r::ARENA_BYTES / 1024);
        match DmaBuf::alloc(r::ARENA_BYTES, DMA_MASK) {
            Ok(buf) => buf,
            Err(e) => {
                serial_println!("rtl8168: DMA arena: {:?}", e);
                return None;
            }
        }
    } else {
        // No DMA below `Net`: a one-page block the driver never hands to the chip.
        match DmaBuf::alloc(4096, DMA_MASK) {
            Ok(buf) => buf,
            Err(_) => return None,
        }
    };
    let mut drv = r::Rtl8168::new(regs, KDma { buf: dma });

    serial_println!("rtl8168: reading identification");
    let Some(id) = drv.identify() else {
        serial_println!("rtl8168: the window reads as all ones: the device is not answering");
        return None;
    };
    serial_println!(
        "rtl8168: TxConfig {:#010x} XID {:#05x} ({:?}) mac {:02x?} link {:?}",
        id.tx_config, id.xid, id.family, id.mac, id.link
    );
    log_dump(drv.regs(), "as found");
    if level == NicLevel::Probe {
        serial_println!("rtl8168: probe level, nothing written to the device");
        return None;
    }

    let relax = || {
        crate::memory::tlb::service_pending();
        core::hint::spin_loop();
    };
    serial_println!("rtl8168: resetting");
    if let Err(e) = drv.reset(relax) {
        serial_println!("rtl8168: reset failed: {:?}", e);
        return None;
    }
    serial_println!("rtl8168: reset done, restarting auto-negotiation");
    let aneg = drv.phy_autoneg(relax);
    serial_println!("rtl8168: MDIO writes {}", if aneg { "completed" } else { "TIMED OUT" });
    let start = crate::cpu::tsc::uptime_ms();
    let mut link = drv.link();
    while !link.up && crate::cpu::tsc::uptime_ms() - start < LINK_WAIT_MS {
        relax();
        link = drv.link();
    }
    serial_println!("rtl8168: link {:?} after {} ms", link, crate::cpu::tsc::uptime_ms() - start);
    log_dump(drv.regs(), "after reset");
    // The reset must keep the station address: log it again so a difference shows.
    let after = drv.identify();
    serial_println!("rtl8168: mac after reset {:02x?}", after.map(|i| i.mac));
    if level == NicLevel::Reset {
        serial_println!("rtl8168: reset level, no rings");
        return None;
    }

    serial_println!("rtl8168: programming the rings");
    drv.init_rings();
    serial_println!("rtl8168: rings up, ChipCmd {:#04x}", {
        use r::Regs;
        drv.regs().r8(r::CHIP_CMD)
    });
    log_dump(drv.regs(), "after init");
    let mut irq = None;
    if let (NicLevel::Net, Some(apic)) = (level, irq_apic) {
        irq = setup_irq(bus, dev, func, &bars, apic);
        if irq.is_some() {
            drv.enable_irq();
        }
    }
    serial_println!(
        "rtl8168: irq {}",
        match irq {
            Some(v) => alloc::format!("vector {:#x}", v),
            None => alloc::string::String::from("polled"),
        }
    );
    Some(Rtl { drv, mac: id.mac })
}
