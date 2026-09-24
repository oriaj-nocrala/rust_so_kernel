//! PCI configuration-header decoding and the `/proc/pci` report.
//!
//! Stage 1 of the "self-improving OS" direction: the system has to be able to
//! say which of its devices nothing drives, in a form an agent can read
//! (`vendor:device` plus class is what finding or porting a driver starts
//! from). The kernel reads the first 64 bytes of each function's
//! configuration space and records which driver claimed it; everything else
//! — decoding, naming, the report's text — is here and host-tested,
//! including against the target board's real headers
//! (`hal/fixtures/ryzen-pci-config.txt`) checked field by field against
//! what Linux's sysfs says about the same functions
//! (`hal/fixtures/ryzen-pci-sysfs.txt`).

use core::fmt::{self, Write};

/// The fields of a type-0 or type-1 configuration header this report uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Function {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor: u16,
    pub device_id: u16,
    pub class: u8,
    pub subclass: u8,
    pub progif: u8,
    pub revision: u8,
    /// Header type with the multifunction bit (7) masked off.
    pub header_type: u8,
    pub multifunction: bool,
    /// Type-0 headers only (offsets 0x2C/0x2E). A bridge's subsystem IDs,
    /// when it has any, live in a capability this decoder does not walk.
    pub subsystem: Option<(u16, u16)>,
    /// Type-1 headers only: secondary and subordinate bus numbers.
    pub bridge_buses: Option<(u8, u8)>,
    /// Interrupt Line (0x3C), only when Interrupt Pin (0x3D) says the
    /// function has a legacy pin at all.
    pub irq_line: Option<u8>,
}

/// Decodes a function from the first 16 dwords of its configuration space,
/// as read through mechanism #1 (little-endian dwords). `None` when the
/// vendor ID is `0xFFFF`: nothing answered at that address.
pub fn decode(bus: u8, device: u8, function: u8, cfg: &[u32; 16]) -> Option<Function> {
    let vendor = cfg[0] as u16;
    if vendor == 0xFFFF {
        return None;
    }
    let header = (cfg[3] >> 16) as u8;
    let header_type = header & 0x7F;
    let subsystem = if header_type == 0 {
        Some((cfg[11] as u16, (cfg[11] >> 16) as u16))
    } else {
        None
    };
    let bridge_buses = if header_type == 1 {
        Some(((cfg[6] >> 8) as u8, (cfg[6] >> 16) as u8))
    } else {
        None
    };
    let irq_pin = (cfg[15] >> 8) as u8;
    Some(Function {
        bus,
        device,
        function,
        vendor,
        device_id: (cfg[0] >> 16) as u16,
        class: (cfg[2] >> 24) as u8,
        subclass: (cfg[2] >> 16) as u8,
        progif: (cfg[2] >> 8) as u8,
        revision: cfg[2] as u8,
        header_type,
        multifunction: header & 0x80 != 0,
        subsystem,
        bridge_buses,
        irq_line: if irq_pin != 0 { Some(cfg[15] as u8) } else { None },
    })
}

impl Function {
    /// Bridges (class 0x06) route buses; nothing here needs a driver for
    /// them, because the kernel's bus sweep is flat, not a bridge walk.
    /// They are listed but left out of the "unclaimed" count, which is meant
    /// to be the list of devices worth writing a driver for.
    pub fn is_bridge(&self) -> bool {
        self.class == 0x06
    }
}

/// Human-readable class name, most specific first: prog-IF where it
/// identifies the interface (xHCI vs EHCI, NVMe vs AHCI), then sub-class,
/// then base class. Names follow the PCI-SIG code assignment and read like
/// `lspci`'s.
pub fn class_name(class: u8, subclass: u8, progif: u8) -> &'static str {
    match (class, subclass, progif) {
        (0x0C, 0x03, 0x00) => return "USB controller (UHCI)",
        (0x0C, 0x03, 0x10) => return "USB controller (OHCI)",
        (0x0C, 0x03, 0x20) => return "USB controller (EHCI)",
        (0x0C, 0x03, 0x30) => return "USB controller (xHCI)",
        (0x01, 0x06, 0x01) => return "SATA controller (AHCI)",
        (0x01, 0x08, 0x02) => return "NVMe controller",
        _ => {}
    }
    match (class, subclass) {
        (0x00, _) => "Unclassified device",
        (0x01, 0x00) => "SCSI storage controller",
        (0x01, 0x01) => "IDE interface",
        (0x01, 0x05) => "ATA controller",
        (0x01, 0x06) => "SATA controller",
        (0x01, 0x07) => "Serial Attached SCSI controller",
        (0x01, 0x08) => "Non-Volatile memory controller",
        (0x01, _) => "Mass storage controller",
        (0x02, 0x00) => "Ethernet controller",
        (0x02, _) => "Network controller",
        (0x03, 0x00) => "VGA compatible controller",
        (0x03, 0x02) => "3D controller",
        (0x03, _) => "Display controller",
        (0x04, 0x01) => "Multimedia audio controller",
        (0x04, 0x03) => "Audio device",
        (0x04, _) => "Multimedia controller",
        (0x05, _) => "Memory controller",
        (0x06, 0x00) => "Host bridge",
        (0x06, 0x01) => "ISA bridge",
        (0x06, 0x04) => "PCI bridge",
        (0x06, _) => "Bridge",
        (0x07, 0x00) => "Serial controller",
        (0x07, _) => "Communication controller",
        (0x08, 0x05) => "SD host controller",
        (0x08, 0x06) => "IOMMU",
        (0x08, _) => "System peripheral",
        (0x09, _) => "Input device controller",
        (0x0C, 0x03) => "USB controller",
        (0x0C, 0x05) => "SMBus",
        (0x0C, _) => "Serial bus controller",
        (0x0D, _) => "Wireless controller",
        (0x10, _) => "Encryption controller",
        (0x11, _) => "Signal processing controller",
        (0x12, _) => "Processing accelerator",
        (0x13, _) => "Non-Essential Instrumentation",
        _ => "Unknown class",
    }
}

/// Vendor names for the IDs this kernel is likely to meet: the target
/// board, QEMU's machine, and common NIC/GPU/storage makers. Deliberately
/// small — the report always prints the numeric ID, which is what a driver
/// search needs; the name is only for a human glancing at it.
pub fn vendor_name(vendor: u16) -> Option<&'static str> {
    Some(match vendor {
        0x1022 => "AMD",
        0x1002 => "AMD/ATI",
        0x8086 => "Intel",
        0x10DE => "NVIDIA",
        0x10EC => "Realtek",
        0x14E4 => "Broadcom",
        0x1AF4 => "Red Hat (virtio)",
        0x1B36 => "Red Hat (QEMU)",
        0x1234 => "QEMU (Bochs VGA)",
        0x15AD => "VMware",
        0x1987 => "Phison",
        0x144D => "Samsung",
        0x15B7 => "SanDisk/WD",
        0x1C5C => "SK hynix",
        0x1B21 => "ASMedia",
        0x1102 => "Creative Labs",
        0x168C => "Qualcomm Atheros",
        0x17CB => "Qualcomm",
        0x14C3 => "MediaTek",
        _ => return None,
    })
}

/// One row of the report. Columns are space-separated and every field is
/// present (`-` when it does not apply), so `awk '$7 == "-"'` works.
pub fn write_row(out: &mut impl Write, f: &Function, driver: Option<&str>) -> fmt::Result {
    write!(
        out,
        "{:02x}:{:02x}.{:x} {:02x}{:02x}{:02x} {:04x}:{:04x} ",
        f.bus, f.device, f.function, f.class, f.subclass, f.progif, f.vendor, f.device_id
    )?;
    match f.subsystem {
        Some((v, d)) => write!(out, "{:04x}:{:04x} ", v, d)?,
        None => out.write_str("-         ")?,
    }
    write!(out, "{:02x}  ", f.revision)?;
    match f.irq_line {
        Some(l) => write!(out, "{:<3} ", l)?,
        None => out.write_str("-   ")?,
    }
    write!(out, "{:<12} {}", driver.unwrap_or("-"), class_name(f.class, f.subclass, f.progif))?;
    if let Some(name) = vendor_name(f.vendor) {
        write!(out, " [{}]", name)?;
    }
    if let Some((sec, sub)) = f.bridge_buses {
        write!(out, " (bus {:02x}-{:02x})", sec, sub)?;
    }
    out.write_char('\n')
}

/// Column header for [`write_row`].
pub const HEADER: &str =
    "bdf     class  ven:dev   subsystem rev irq driver       description\n";

/// The whole report: a summary an agent can read in three lines, then one
/// row per function. `driver_of` answers which driver claimed a function.
pub fn write_report<'a>(
    out: &mut impl Write,
    functions: &[Function],
    driver_of: impl Fn(&Function) -> Option<&'a str>,
) -> fmt::Result {
    let claimed = functions.iter().filter(|f| driver_of(f).is_some()).count();
    let unclaimed = functions
        .iter()
        .filter(|f| !f.is_bridge() && driver_of(f).is_none())
        .count();
    writeln!(out, "functions: {}", functions.len())?;
    writeln!(out, "claimed: {}", claimed)?;
    writeln!(out, "unclaimed (bridges excluded): {}", unclaimed)?;
    out.write_str(HEADER)?;
    for f in functions {
        write_row(out, f, driver_of(f))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use alloc::vec::Vec;

    fn parse_config_line(line: &str) -> (u8, u8, u8, [u32; 16]) {
        let mut it = line.split_whitespace();
        let bdf = it.next().unwrap();
        let hex = it.next().unwrap();
        let bus = u8::from_str_radix(&bdf[0..2], 16).unwrap();
        let dev = u8::from_str_radix(&bdf[3..5], 16).unwrap();
        let func = u8::from_str_radix(&bdf[6..7], 16).unwrap();
        assert_eq!(hex.len(), 128, "{bdf}: expected 64 bytes");
        let bytes: Vec<u8> = (0..64)
            .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
            .collect();
        let mut cfg = [0u32; 16];
        for (i, d) in cfg.iter_mut().enumerate() {
            *d = u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
        }
        (bus, dev, func, cfg)
    }

    fn fixture_functions() -> Vec<Function> {
        include_str!("../fixtures/ryzen-pci-config.txt")
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .map(|l| {
                let (b, d, f, cfg) = parse_config_line(l);
                decode(b, d, f, &cfg).expect("fixture function present")
            })
            .collect()
    }

    fn hex(s: &str) -> u32 {
        u32::from_str_radix(s.trim_start_matches("0x"), 16).unwrap()
    }

    /// Every field the decoder produces, for every function on the target
    /// board, against Linux's own reading of the same function.
    #[test]
    fn decode_matches_linux_sysfs_on_the_target_board() {
        let funcs = fixture_functions();
        let oracle: Vec<&str> = include_str!("../fixtures/ryzen-pci-sysfs.txt")
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .collect();
        assert_eq!(funcs.len(), oracle.len());
        assert_eq!(funcs.len(), 42);
        for (f, line) in funcs.iter().zip(&oracle) {
            let c: Vec<&str> = line.split_whitespace().collect();
            let bdf = format!("{:02x}:{:02x}.{:x}", f.bus, f.device, f.function);
            assert_eq!(bdf, c[0]);
            assert_eq!(f.vendor as u32, hex(c[1]), "{bdf} vendor");
            assert_eq!(f.device_id as u32, hex(c[2]), "{bdf} device");
            let class = ((f.class as u32) << 16) | ((f.subclass as u32) << 8) | f.progif as u32;
            assert_eq!(class, hex(c[3]), "{bdf} class");
            assert_eq!(f.revision as u32, hex(c[4]), "{bdf} revision");
            if let Some((v, d)) = f.subsystem {
                assert_eq!(v as u32, hex(c[5]), "{bdf} subsystem vendor");
                assert_eq!(d as u32, hex(c[6]), "{bdf} subsystem device");
            }
            match f.bridge_buses {
                Some((sec, sub)) => {
                    assert_eq!(sec.to_string(), c[7], "{bdf} secondary");
                    assert_eq!(sub.to_string(), c[8], "{bdf} subordinate");
                }
                None => assert_eq!((c[7], c[8]), ("-", "-"), "{bdf} is a bridge per sysfs"),
            }
        }
    }

    #[test]
    fn target_board_nic_and_bridges() {
        let funcs = fixture_functions();
        let nic = funcs.iter().find(|f| (f.bus, f.device, f.function) == (8, 0, 0)).unwrap();
        assert_eq!((nic.vendor, nic.device_id), (0x10EC, 0x8168));
        assert_eq!(class_name(nic.class, nic.subclass, nic.progif), "Ethernet controller");
        assert_eq!(nic.subsystem, Some((0x1043, 0x8677)));
        assert!(!nic.is_bridge());

        let xhci = funcs.iter().find(|f| (f.bus, f.device, f.function) == (2, 0, 0)).unwrap();
        assert_eq!(class_name(xhci.class, xhci.subclass, xhci.progif), "USB controller (xHCI)");

        let nvme = funcs.iter().find(|f| (f.bus, f.device, f.function) == (1, 0, 0)).unwrap();
        assert_eq!(class_name(nvme.class, nvme.subclass, nvme.progif), "NVMe controller");

        let gpp = funcs.iter().find(|f| (f.bus, f.device, f.function) == (0, 1, 1)).unwrap();
        assert!(gpp.is_bridge());
        assert_eq!(gpp.bridge_buses, Some((1, 1)));
        assert_eq!(gpp.subsystem, None);
    }

    #[test]
    fn absent_function_is_none() {
        let mut cfg = [0u32; 16];
        cfg[0] = 0xFFFF_FFFF;
        assert_eq!(decode(0, 0, 0, &cfg), None);
    }

    #[test]
    fn multifunction_bit_is_split_from_header_type() {
        let mut cfg = [0u32; 16];
        cfg[0] = 0x1234_8086;
        cfg[3] = 0x0081_0000; // header type 1 + multifunction
        let f = decode(0, 0, 0, &cfg).unwrap();
        assert_eq!(f.header_type, 1);
        assert!(f.multifunction);
        assert_eq!(f.subsystem, None);
    }

    #[test]
    fn irq_line_only_when_a_pin_exists() {
        let mut cfg = [0u32; 16];
        cfg[0] = 0x1234_8086;
        cfg[15] = 0x0000_000B; // line 11, pin 0
        assert_eq!(decode(0, 0, 0, &cfg).unwrap().irq_line, None);
        cfg[15] = 0x0000_010B; // line 11, pin INTA#
        assert_eq!(decode(0, 0, 0, &cfg).unwrap().irq_line, Some(11));
    }

    #[test]
    fn class_name_prefers_the_prog_if() {
        assert_eq!(class_name(0x0C, 0x03, 0x20), "USB controller (EHCI)");
        assert_eq!(class_name(0x0C, 0x03, 0xFE), "USB controller");
        assert_eq!(class_name(0x01, 0x08, 0x02), "NVMe controller");
        assert_eq!(class_name(0x01, 0x08, 0x03), "Non-Volatile memory controller");
        assert_eq!(class_name(0xFE, 0, 0), "Unknown class");
    }

    #[test]
    fn row_has_a_fixed_column_count() {
        let funcs = fixture_functions();
        for f in &funcs {
            for driver in [None, Some("xhci")] {
                let mut s = String::new();
                write_row(&mut s, f, driver).unwrap();
                let cols: Vec<&str> = s.split_whitespace().collect();
                assert!(cols.len() >= 8, "{s}");
                assert_eq!(cols[6], driver.unwrap_or("-"), "{s}");
            }
        }
    }

    #[test]
    fn report_counts_exclude_bridges_from_unclaimed() {
        let funcs = fixture_functions();
        let mut s = String::new();
        write_report(&mut s, &funcs, |f| {
            ((f.bus, f.device, f.function) == (2, 0, 0)).then_some("xhci")
        })
        .unwrap();
        // Counted by hand from `lspci -nn` on the board: 16 host bridges
        // (8 root/dummy + 8 data-fabric functions), 11 PCI bridges, 1 ISA
        // bridge; the other 14 functions are devices.
        let bridges = funcs.iter().filter(|f| f.is_bridge()).count();
        assert_eq!(bridges, 28);
        let mut lines = s.lines();
        assert_eq!(lines.next(), Some("functions: 42"));
        assert_eq!(lines.next(), Some("claimed: 1"));
        assert_eq!(lines.next(), Some("unclaimed (bridges excluded): 13"));
        assert_eq!(lines.next(), Some(HEADER.trim_end()));
        let xhci = s.lines().find(|l| l.starts_with("02:00.0")).unwrap();
        assert!(xhci.contains(" xhci "), "{xhci}");
        assert!(xhci.contains("[AMD]"), "{xhci}");
    }
}
