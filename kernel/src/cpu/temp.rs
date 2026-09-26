// kernel/src/cpu/temp.rs
//
// CPU temperatures on AMD Zen, the way Linux's k10temp reads them: SMN
// registers through the root complex's index/data pair (`pci::smn_read`;
// the register layout, the per-model table and the arithmetic are
// `hal::k10temp`). What `/proc/sensors` reports (with `cpu::idle`'s package
// energy) and `cpumon` draws.
//
// Read on demand — every open of `/proc/sensors` — rather than sampled:
// a reading is three or four config-space round trips, and nothing needs
// a history. Decided once at boot by the BSP; everything else (QEMU, Intel,
// pre-Zen AMD) gets an empty file.

use hal::k10temp::{self, Model, Reading};
use spin::Once;

struct Sensor {
    model: Model,
    /// Which CCDs answered the boot-time probe.
    ccds: u16,
}

static SENSOR: Once<Option<Sensor>> = Once::new();

/// Decide whether this machine has k10temp's sensors, and which. BSP,
/// once, any time after PCI config access works.
pub fn init() {
    SENSOR.call_once(|| {
        let sensor = detect();
        match &sensor {
            Some(s) => {
                let r = read_with(s);
                crate::serial_println!(
                    "[k10temp] present: {} CCD(s) (mask {:#x}), Tctl {} mC",
                    s.ccds.count_ones(), s.ccds, r.tctl
                );
            }
            None => crate::serial_println!("[k10temp] absent: not an AMD family 17h/19h CPU with an AMD root complex"),
        }
        sensor
    });
}

fn detect() -> Option<Sensor> {
    use core::arch::x86_64::__cpuid;
    use hal::cpuid::{self, Regs};

    let r = |l: u32| {
        let c = __cpuid(l);
        Regs { eax: c.eax, ebx: c.ebx, ecx: c.ecx, edx: c.edx }
    };
    let leaf0 = r(0);
    let sig = cpuid::signature(r(1).eax);
    let brand = (r(0x8000_0000).eax >= 0x8000_0004)
        .then(|| cpuid::brand([r(0x8000_0002), r(0x8000_0003), r(0x8000_0004)]))
        .unwrap_or([0; 48]);
    let brand = cpuid::trimmed(&brand).unwrap_or("");
    let model = k10temp::model(&cpuid::vendor(leaf0), sig.family, sig.model, brand)?;

    // Under a hypervisor the CPUID can say Zen while 00:00.0 is an emulated
    // Intel host bridge; the SMN pair only exists on AMD's root complex.
    if crate::pci::vendor_id(0, 0, 0) != k10temp::AMD_VENDOR {
        return None;
    }
    let ccds = k10temp::probe_ccds(&model, crate::pci::smn_read);
    // Linux's k10temp binds to the data fabric's function 3; claim it the
    // same way so `/proc/pci` and `lspci -k` agree.
    if crate::pci::vendor_id(0, 0x18, 3) == k10temp::AMD_VENDOR {
        crate::pci::claim(0, 0x18, 3, "k10temp");
    }
    Some(Sensor { model, ccds })
}

fn read_with(s: &Sensor) -> Reading {
    k10temp::read(&s.model, s.ccds, crate::pci::smn_read)
}

/// One reading now, or `None` without the sensors.
pub fn read() -> Option<Reading> {
    SENSOR.get()?.as_ref().map(read_with)
}

/// `/proc/sensors`: one `chip<TAB>type<TAB>label<TAB>value` line per
/// sensor — k10temp's temperatures in millidegrees, then the package
/// energy in µJ (`cpu::idle`'s RAPL counter) — empty without any.
pub fn render() -> alloc::string::String {
    let mut out = alloc::string::String::new();
    if let Some(r) = read() {
        let _ = k10temp::render(&r, &mut out);
    }
    if let Some(uj) = super::idle::package_uj() {
        let _ = hal::amd_power::render_energy(uj, &mut out);
    }
    out
}
