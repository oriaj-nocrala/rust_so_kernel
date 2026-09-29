// kernel/src/gpu/mod.rs
//
// The kernel adapter of the NVIDIA GA106 (RTX 3050) driver: docs/gpu/gpu-plan.md,
// state in docs/reference/gpu.md. The logic lives in the `nvgpu` crate
// (host-tested); this module maps BARs, reads configuration space, loads
// firmware and logs.
//
// Everything is behind `gpu=` (`bootopts`, default `off`): the GPU is the
// Ryzen's only video output and there is no serial port, so an ordinary
// boot does not touch it. Levels so far:
//
//   probe  (phase 1) — report the firmware files, the IOMMU state (D4), the
//          GPU's capabilities and BARs, map BAR0 (UC) and a BAR1 window
//          (WC), read PMC_BOOT_0. The only writes are the BAR sizing
//          protocol's, in configuration space; no GPU register is written.
//   disp   (phase 2) — also read the VBIOS from the PROM, parse its DCB, and
//          probe every connector by polling: DPCD + EDID over DP AUX, EDID
//          over bit-banged I2C (`nvgpu::display`). The only GPU writes are
//          those transactions' and the pad/AUX bits they need, all put back.
//          Result in /proc/displays.
//   vblank (phase 3) — also report the heads the firmware lit and arm the
//          display's vblank interrupt on them, by MSI to CPU 0
//          (`vblank.rs`); `/dev/vblank` then wakes pollers each frame.
//   dispstate (phase 5.1) — also, before arming vblank, read the display's
//          ARMED method state (core + window 0) the GOP left and compare
//          window 0's surface with the GOP framebuffer. Reads only. Result
//          in /proc/dispstate.
//   chan   (phase 5.2) — also, after reading that state and before arming
//          vblank, bring up the display's instance memory (VRAM through
//          PRAMIN), the core channel and window 0, and push one UPDATE on
//          each that repeats what the GOP left (`evo.rs`): the image must
//          not change. Result in /proc/gpu (`chan:` lines).
//   scanout (phase 5.3) — also, once the channels are up, scan out two
//          VRAM buffers of this kernel's instead of the GOP framebuffer
//          (`scanout.rs`): the framebuffer copies its shadow there and
//          `FBIO_FLUSH` flips at the next vblank. Result: `scanout:` lines,
//          `gpu_flip:` in /proc/kdebug.
//   super  (phase 5.4) — also service the display's supervisor interrupts
//          from the vblank MSI handler, and open `/dev/dispctl`, which
//          detaches the primary head's SOR and attaches it back at the same
//          mode (`supervisor.rs`). Result: `super:` lines in /proc/gpu,
//          `gpu_super:` in /proc/kdebug.
//   vpll   (phase 5.5) — also program the heads' pixel clocks (VPLL) in
//          supervisor 2.1 (`nvgpu::pll`), and let `/dev/dispctl`'s
//          `clock <kHz>` change the primary head's pixel clock on the same
//          raster (1080p at 50 Hz, and back). Same lines and counters.
//   dplink (phase 5.6) — also let `/dev/dispctl`'s `train <lanes> <rate>`
//          retrain the primary head's DP link while its SOR is detached
//          (`dplink.rs`, `nvgpu::dp`). Result: `dplink:` lines in /proc/gpu,
//          `gpu_dplink:` in /proc/kdebug.
//   modes  (phase 5.7) — also let `/dev/dispctl`'s `mode WxH@Hz` set the
//          primary head to an EDID or CVT-RB2 mode of the framebuffer's
//          size: detach, retrain the link if the mode needs it, attach with
//          the new raster and clock (`modeset.rs`, `nvgpu::mode`). Result:
//          `modes:`/`mode:` lines in /proc/gpu, `gpu_mode:` in /proc/kdebug.
//   fwsec  (phase 4c) — also run FWSEC-FRTS on the GSP falcon (`gsp.rs`,
//          `nvgpu::{falcon,fwsec}`): the VBIOS's signed microcode carves
//          the protected memory region (WPR2) the GSP's boot needs. Result:
//          `fwsec:` lines in /proc/gpu, `gpu_fwsec:` in /proc/kdebug.
//   gsp    (phases 4d + 4e) — also build the memory GSP-RM's boot reads and
//          boot it: reset into RISC-V mode, booter on SEC2, RISC-V check
//          (`gsp.rs`, `nvgpu::{gspmem,booter}`). `gsp:` lines, `gpu_gsp:`.
//
// Runs once at boot, after `fs::init` (firmware is on `/mnt`) and before the
// APs are released (BAR sizing turns decoding off for a few microseconds,
// and nothing may draw on the GOP framebuffer meanwhile).

use alloc::string::String;
use core::fmt::Write;

use hal::bootopts::GpuLevel;
use hal::pcicfg::{BarKind, MsiCap, MsixCap};
use nvgpu::id::ChipId;

use crate::serial_println;

pub mod dplink;
pub mod evo;
pub mod gsp;
pub mod hdmi;
pub mod modeset;
pub mod scanout;
pub mod supervisor;
pub mod vblank;

/// `10de:2507`, the only GPU this driver is for (plan, "No-objetivos").
const VENDOR_NVIDIA: u16 = 0x10de;
const DEVICE_GA106: u16 = 0x2507;

/// Firmware the later phases load, pinned to D1 (570.144). Phase 1 only
/// proves the loader on the small ones; `gsp-570.144.bin` (63 MB) does not
/// fit on `disk.img` yet and arrives with phase 4.
const FIRMWARE: [&str; 3] = [
    "nvidia/ga106/gsp/bootloader-570.144.bin",
    "nvidia/ga106/gsp/booter_load-570.144.bin",
    "nvidia/ga106/gsp/booter_unload-570.144.bin",
];

/// How much of BAR1 (VRAM, 8 GiB with ReBAR) gets a write-combining
/// mapping. The plan asks for a window, not the whole BAR; 16 MiB covers a
/// 1920×1080×4 scanout buffer with room for a second one.
const BAR1_WINDOW: u64 = 16 << 20;

static REPORT: spin::Once<String> = spin::Once::new();
/// The VBIOS and its DCB, kept after `gpu=disp` read them: the supervisor
/// work runs the VBIOS's IED scripts (`supervisor.rs`).
static VBIOS: spin::Once<(nvgpu::vbios::Bios, nvgpu::dcb::Dcb)> = spin::Once::new();
static DISPLAYS: spin::Once<String> = spin::Once::new();
/// What `gpu=disp` read from each connector (DPCD, EDID): `modeset.rs`
/// takes the modes of the primary head's monitor from it.
static PROBES: spin::Once<alloc::vec::Vec<nvgpu::display::Probe>> = spin::Once::new();
static DISPSTATE: spin::Once<String> = spin::Once::new();

/// `/proc/gpu`: the boot report, then what the supervisors did since.
pub fn render() -> String {
    let mut out = REPORT.get().cloned().unwrap_or_else(|| String::from("gpu: off\n"));
    out.push_str(&supervisor::render_log());
    out
}

/// `/proc/displays`: filled once at boot with `gpu=disp`.
pub fn render_displays() -> String {
    DISPLAYS.get().cloned().unwrap_or_else(|| String::from("displays: not probed (needs gpu=disp and the GA106)\n"))
}

/// `/proc/dispstate`: filled once at boot with `gpu=dispstate`.
pub fn render_dispstate() -> String {
    DISPSTATE.get().cloned().unwrap_or_else(|| String::from("dispstate: not read (needs gpu=dispstate and the GA106)\n"))
}

/// BAR0 through the uncached mapping `memory::mmio` made.
pub struct Bar0 {
    base: *mut u8,
    len: u64,
}

// SAFETY: a register window. The boot uses it alone (before the APs run);
// afterwards the vblank MSI handler (never overlapping itself, `vblank.rs`),
// flips (window 0's registers), dispctl pushes (the core's PUT) and a link
// training (SOR, AUX, VGA CR: only while nothing else touches them,
// `dplink.rs`) use disjoint registers.
unsafe impl Send for Bar0 {}
unsafe impl Sync for Bar0 {}

impl nvgpu::Mmio for Bar0 {
    fn rd32(&self, offset: u32) -> u32 {
        assert!((offset as u64) + 4 <= self.len && offset % 4 == 0);
        // SAFETY: in range and aligned per the assert; the mapping is UC.
        unsafe { core::ptr::read_volatile(self.base.add(offset as usize) as *const u32) }
    }
    fn wr32(&self, offset: u32, value: u32) {
        assert!((offset as u64) + 4 <= self.len && offset % 4 == 0);
        // SAFETY: as above.
        unsafe { core::ptr::write_volatile(self.base.add(offset as usize) as *mut u32, value) }
    }
    /// Byte accesses: the VGA ports at `0x601000` that VBIOS scripts use
    /// (`nvgpu::init`, opcode CR).
    fn rd08(&self, offset: u32) -> u8 {
        assert!((offset as u64) < self.len);
        // SAFETY: in range per the assert; the mapping is UC.
        unsafe { core::ptr::read_volatile(self.base.add(offset as usize)) }
    }
    fn wr08(&self, offset: u32, value: u8) {
        assert!((offset as u64) < self.len);
        // SAFETY: as above.
        unsafe { core::ptr::write_volatile(self.base.add(offset as usize), value) }
    }
    /// TSC busy-wait. Runs at boot before the APs are released, but still
    /// answers TLB shootdowns (CLAUDE.md: any busy-wait with IF=0).
    fn udelay(&self, us: u32) {
        let cycles = crate::cpu::tsc::freq_hz() / 1_000_000 * us as u64;
        let t0 = crate::cpu::tsc::read();
        while crate::cpu::tsc::read().wrapping_sub(t0) < cycles {
            crate::memory::tlb::service_pending();
            core::hint::spin_loop();
        }
    }
}

/// Milliseconds since `t0` (TSC), for the report.
fn ms_since(t0: u64) -> u64 {
    crate::cpu::tsc::read().wrapping_sub(t0) / (crate::cpu::tsc::freq_hz() / 1000).max(1)
}

pub fn probe() {
    let level = crate::bootopts::gpu_level();
    if level == GpuLevel::Off {
        serial_println!("gpu: off (gpu= not set)");
        return;
    }
    let mut r = String::new();
    let _ = writeln!(r, "level: {:?}", level);
    report_firmware(&mut r);
    report_iommu(&mut r);
    probe_device(&mut r, level);
    for line in r.lines() {
        serial_println!("gpu: {}", line);
    }
    REPORT.call_once(|| r);
}

fn report_firmware(r: &mut String) {
    for rel in FIRMWARE {
        match crate::firmware::load(rel) {
            Ok(data) => {
                let head: [u8; 4] = data.get(..4).and_then(|h| h.try_into().ok()).unwrap_or([0; 4]);
                // FNV-1a 64: compared by hand (or by a metal job) with the
                // host's copy, so the loader is checked byte for byte.
                let fnv = data.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3));
                let _ = writeln!(
                    r,
                    "firmware: {} {} bytes, first word {:#010x}, fnv1a64 {:016x}",
                    rel,
                    data.len(),
                    u32::from_le_bytes(head),
                    fnv
                );
            }
            Err(e) => {
                let _ = writeln!(r, "firmware: {} MISSING ({:?})", rel, e);
            }
        }
    }
}

/// Decision D4: bus address = physical address only while no IOMMU
/// translates. This kernel never enables one; this reads whether the
/// firmware did.
fn report_iommu(r: &mut String) {
    let units = crate::acpi::iommus();
    if units.is_empty() {
        let _ = writeln!(r, "iommu: no IVRS (no AMD IOMMU described)");
        return;
    }
    let mut seen: [u64; 8] = [0; 8];
    for (i, unit) in units.iter().enumerate() {
        if seen[..i].contains(&unit.mmio_phys) {
            continue;
        }
        seen[i] = unit.mmio_phys;
        // SAFETY: the IVRS names this as the IOMMU's register block.
        let Some(v) = (unsafe { crate::memory::mmio::map(x86_64::PhysAddr::new(unit.mmio_phys), 0x1000) }) else {
            let _ = writeln!(r, "iommu: {:#x}: cannot map", unit.mmio_phys);
            continue;
        };
        // SAFETY: freshly mapped UC page; the control register is 8 bytes
        // at `IOMMU_MMIO_CONTROL`.
        let ctrl = unsafe {
            core::ptr::read_volatile((v.as_u64() + hal::acpi::IOMMU_MMIO_CONTROL) as *const u64)
        };
        let en = ctrl >> hal::acpi::IOMMU_CONTROL_EN_BIT & 1;
        let _ = writeln!(
            r,
            "iommu: devid {:#06x} mmio {:#x} control {:#018x} IommuEn={} ({})",
            unit.devid,
            unit.mmio_phys,
            ctrl,
            en,
            if en == 0 { "off: bus address = physical, D4 holds" } else { "ON: D4 does not hold" }
        );
    }
}

fn probe_device(r: &mut String, level: GpuLevel) {
    let mut found = None;
    crate::pci::for_each_function(|f| {
        if found.is_none() && f.vendor == VENDOR_NVIDIA && f.device_id == DEVICE_GA106 && f.class == 0x03 {
            found = Some(f);
        }
    });
    let Some(f) = found else {
        let _ = writeln!(r, "device: no {:04x}:{:04x} on the bus", VENDOR_NVIDIA, DEVICE_GA106);
        return;
    };
    let (b, d, fun) = (f.bus, f.device, f.function);
    let _ = writeln!(r, "device: {:02x}:{:02x}.{} {:04x}:{:04x} rev {:02x}", b, d, fun, f.vendor, f.device_id, f.revision);

    let cfg = crate::pci::config_space(b, d, fun);
    let cmd = crate::pci::command(b, d, fun);
    let _ = writeln!(r, "command: {:#06x}", cmd);
    let _ = write!(r, "capabilities:");
    for (id, off) in hal::pcicfg::capabilities(&cfg) {
        let _ = write!(r, " {:02x}@{:02x}", id, off);
    }
    let _ = writeln!(r);
    match MsiCap::decode(&cfg) {
        Some(m) => {
            let _ = writeln!(
                r,
                "msi: @{:02x} enabled={} 64bit={} maskable={} vectors={} address={:#x} data={:#06x}",
                m.offset, m.enabled, m.is_64, m.per_vector_mask, m.vectors_capable, m.address, m.data
            );
        }
        None => {
            let _ = writeln!(r, "msi: none");
        }
    }
    match MsixCap::decode(&cfg) {
        Some(m) => {
            let _ = writeln!(r, "msi-x: @{:02x} enabled={} table_size={} bar={} offset={:#x}", m.offset, m.enabled, m.table_size, m.table_bar, m.table_offset);
        }
        None => {
            let _ = writeln!(r, "msi-x: none");
        }
    }

    // Drain write-combined framebuffer stores before BAR sizing switches
    // decoding off (they would be lost, not misdirected, but pixels are
    // pixels).
    // SAFETY: a fence.
    unsafe { core::arch::asm!("sfence", options(nostack, preserves_flags)) };
    let bars = crate::pci::size_bars(b, d, fun);
    for bar in bars.iter().flatten() {
        let kind = match bar.kind {
            BarKind::Io => "io",
            BarKind::Mem32 { prefetchable: false } => "mem32",
            BarKind::Mem32 { prefetchable: true } => "mem32 pref",
            BarKind::Mem64 { prefetchable: false } => "mem64",
            BarKind::Mem64 { prefetchable: true } => "mem64 pref",
        };
        let _ = writeln!(r, "bar{}: {} {:#x} size {:#x}", bar.index, kind, bar.addr, bar.size);
    }

    if cmd & hal::pcicfg::COMMAND_MEMORY == 0 {
        let _ = writeln!(r, "memory decoding is off; not touching the BARs (probe writes nothing)");
        return;
    }

    let Some(bar0) = bars[0].filter(|b| !matches!(b.kind, BarKind::Io)) else {
        let _ = writeln!(r, "bar0: not a memory BAR");
        return;
    };
    // SAFETY: BAR0 is the GPU's register window.
    let Some(v0) = (unsafe { crate::memory::mmio::map(x86_64::PhysAddr::new(bar0.addr), bar0.size as usize) }) else {
        let _ = writeln!(r, "bar0: cannot map {:#x} bytes", bar0.size);
        return;
    };
    let _ = writeln!(r, "bar0: mapped UC at {:#x}", v0.as_u64());
    let regs = Bar0 { base: v0.as_mut_ptr(), len: bar0.size };
    let boot0 = nvgpu::Mmio::rd32(&regs, nvgpu::id::PMC_BOOT_0);
    let chip = ChipId::decode(boot0);
    match chip {
        Some(id) => {
            let _ = writeln!(
                r,
                "PMC_BOOT_0: {:#010x} chipset {:#x} rev {:#04x} arch {} impl {} chip {}",
                boot0,
                id.chipset,
                id.chiprev,
                id.arch().unwrap_or("?"),
                id.implementation(),
                id.name().unwrap_or("?")
            );
        }
        None => {
            let _ = writeln!(r, "PMC_BOOT_0: {:#010x} (not an NVIDIA chip id)", boot0);
        }
    }

    if let Some(bar1) = bars[1].filter(|b| matches!(b.kind, BarKind::Mem64 { .. } | BarKind::Mem32 { .. })) {
        let len = bar1.size.min(BAR1_WINDOW);
        use crate::memory::memtype::PatProgram;
        let pat_wc = matches!(
            crate::memory::memtype::pat_program_status(),
            Some(PatProgram::Programmed { .. }) | Some(PatProgram::AlreadyWc { .. })
        );
        // SAFETY: the start of BAR1 (VRAM aperture); only mapped, not accessed.
        match unsafe { crate::memory::mmio::map(x86_64::PhysAddr::new(bar1.addr), len as usize) } {
            Some(v1) => {
                if !pat_wc {
                    let _ = writeln!(r, "bar1: mapped UC at {:#x}: the PAT has no WC entry", v1.as_u64());
                } else { match crate::memory::memtype::set_pat_index_range(v1.as_u64(), len, hal::memtype::PAT_WC_INDEX) {
                    Ok(_) => {
                        let _ = writeln!(r, "bar1: {:#x} bytes mapped WC at {:#x} (not accessed)", len, v1.as_u64());
                    }
                    Err(e) => {
                        let _ = writeln!(r, "bar1: mapped at {:#x} but not WC: {}", v1.as_u64(), e);
                    }
                } }
            }
            None => {
                let _ = writeln!(r, "bar1: cannot map a {:#x}-byte window", len);
            }
        }
    }

    crate::pci::claim(b, d, fun, "nvgpu");

    if level >= GpuLevel::Disp {
        if chip.and_then(|c| c.name()).is_some() {
            probe_displays(r, &regs);
            if level >= GpuLevel::Dispstate {
                read_dispstate(r, &regs, bars[1].map(|b| (b.addr, b.size)));
            }
            let put = if level >= GpuLevel::Chan { evo::bring_up(r, &regs, (b, d, fun), level >= GpuLevel::Hdmi) } else { None };
            if level >= GpuLevel::Scanout {
                match put {
                    Some(put) => scanout::setup(r, &regs, bars[1].map(|b| (b.addr, b.size)), put),
                    None => {
                        let _ = writeln!(r, "scanout: not attempted: the channels are not up (see chan:)");
                    }
                }
            }
            if level >= GpuLevel::Super {
                match put {
                    Some(_) => {
                        supervisor::setup(r, &regs, level >= GpuLevel::Vpll, level >= GpuLevel::Dplink);
                        if level >= GpuLevel::Modes {
                            modeset::setup(r, &regs);
                        }
                        if level >= GpuLevel::Hdmi {
                            hdmi::setup(r, &regs, bars[1].map(|b| (b.addr, b.size)));
                        }
                    }
                    None => {
                        let _ = writeln!(r, "super: not attempted: the channels are not up (see chan:)");
                    }
                }
            }
            // Before the vblank interrupt is armed: FWSEC busy-waits with IF=0
            // for a few hundred ms, and no MSI should be in flight meanwhile.
            if level >= GpuLevel::Fwsec {
                gsp::setup(
                    r,
                    &regs,
                    (b, d, fun),
                    level >= GpuLevel::Gsp,
                    gsp::PciInfo {
                        bar0: bar0.addr,
                        bar1: bars[1].map_or(0, |b| b.addr),
                        bar3: bars[3].map_or(0, |b| b.addr),
                        bus: b,
                        device: d,
                        function: fun,
                        vendor: f.vendor,
                        device_id: f.device_id,
                        sub_vendor: u16::from_le_bytes([cfg[0x2c], cfg[0x2d]]),
                        sub_device: u16::from_le_bytes([cfg[0x2e], cfg[0x2f]]),
                        revision: f.revision,
                    },
                );
            }
            if level >= GpuLevel::Vblank {
                vblank::setup(r, &regs, (b, d, fun));
            }
        } else {
            let _ = writeln!(r, "displays: not a GA106; not probing");
        }
    }
}

/// Phase 2: VBIOS → DCB → connectors → DPCD/EDID (`nvgpu::display`).
fn probe_displays(r: &mut String, regs: &Bar0) {
    let t0 = crate::cpu::tsc::read();
    let rom = match nvgpu::vbios::read_prom(regs) {
        Ok(rom) => rom,
        Err(e) => {
            let _ = writeln!(r, "vbios: PROM read failed: {:?}", e);
            return;
        }
    };
    let prom_ms = ms_since(t0);
    // The phase 0 dump (sysfs) holds the first two images: 148 992 bytes.
    let head = &rom[..rom.len().min(148_992)];
    let _ = writeln!(
        r,
        "vbios: {} bytes from the PROM in {} ms, fnv1a64[..{}] {:016x}",
        rom.len(),
        prom_ms,
        head.len(),
        nvgpu::display::fnv1a64(head)
    );
    let bios = match nvgpu::vbios::Bios::new(rom) {
        Ok(b) => b,
        Err(e) => {
            let _ = writeln!(r, "vbios: {:?}", e);
            return;
        }
    };
    for img in &bios.images {
        let _ = writeln!(r, "vbios: image @{:#07x} type {:02x} {} bytes{}", img.base, img.kind, img.size, if img.last { " (last)" } else { "" });
    }
    match bios.version() {
        Some(v) => {
            let _ = writeln!(r, "vbios: version {:02x}.{:02x}.{:02x}.{:02x}.{:02x}", v[0], v[1], v[2], v[3], v[4]);
        }
        None => {
            let _ = writeln!(r, "vbios: no BIT version");
        }
    }
    let dcb = match nvgpu::dcb::Dcb::parse(&bios) {
        Ok(d) => d,
        Err(e) => {
            let _ = writeln!(r, "dcb: {:?}", e);
            return;
        }
    };
    let _ = writeln!(r, "dcb: version {:#x}, {} outputs, {} ccb entries, {} connectors", dcb.version, dcb.outputs.len(), dcb.ccb.len(), dcb.connectors.len());
    for o in &dcb.outputs {
        let _ = writeln!(
            r,
            "dcb: outp {:02x} type {:02x} loc {} or {} link {} con {:x} edid {:x} bus {} head {:x}",
            o.index, o.kind, o.location, o.or, o.link, o.connector, o.i2c_index, o.bus, o.heads
        );
    }
    let mut probes = alloc::vec::Vec::new();
    for c in nvgpu::display::connectors(&dcb) {
        let t = crate::cpu::tsc::read();
        let p = nvgpu::display::probe(regs, &c);
        let _ = writeln!(r, "displays: {} {:?} in {} ms", c.name, p.status, ms_since(t));
        probes.push(p);
    }
    let text = nvgpu::display::render(&probes);
    for line in text.lines() {
        crate::serial_println!("displays: {}", line);
    }
    DISPLAYS.call_once(|| text);
    PROBES.call_once(|| probes);
    VBIOS.call_once(|| (bios, dcb));
}

/// Phase 5.1: the ARMED display state the GOP left (`nvgpu::dispstate`),
/// read before anything arms an interrupt. Window 0's surface is compared
/// with the GOP framebuffer's physical address, taken as an offset into
/// BAR1 (the plan's criterion: they should be the same memory).
fn read_dispstate(r: &mut String, regs: &Bar0, bar1: Option<(u64, u64)>) {
    let snap = nvgpu::dispstate::Snapshot::read_armed(regs);
    let mut text = nvgpu::dispstate::render(&snap);
    let surf = snap.surface0();
    let gop = crate::framebuffer::FRAMEBUFFER.lock().as_ref().map(|fb| (fb.virt_addr(), fb.stride() * fb.bytes_per_pixel(), fb.dimensions()));
    let mut summary = String::new();
    match gop {
        Some((virt, pitch, (w, h))) => {
            let phys = crate::memory::memtype::leaf_for(x86_64::VirtAddr::new(virt)).map(|l| l.phys + (virt - l.virt_base));
            let _ = write!(summary, "gop: {}x{} pitch {} phys ", w, h, pitch);
            match phys {
                Some(p) => {
                    let _ = write!(summary, "{:#x}", p);
                    if let Some((b1, len)) = bar1.filter(|(b1, len)| (*b1..b1 + len).contains(&p)) {
                        let off = p - b1;
                        let _ = write!(
                            summary,
                            " = BAR1 {:#x} + {:#x} (of {:#x}); window0 offset {} it, pitch {}",
                            b1,
                            off,
                            len,
                            if off == surf.offset { "matches" } else { "DIFFERS from" },
                            if pitch as u32 == surf.pitch { "matches" } else { "DIFFERS" }
                        );
                    } else {
                        let _ = write!(summary, " (not inside BAR1)");
                    }
                }
                None => {
                    let _ = write!(summary, "? (virt {:#x} not mapped)", virt);
                }
            }
        }
        None => {
            let _ = write!(summary, "gop: no framebuffer");
        }
    }
    let _ = writeln!(summary);
    text.insert_str(0, &summary);
    // The summary lines go to the report and the log; the per-method rows
    // (324) only to /proc/dispstate and the log.
    for line in text.lines().take_while(|l| !l.starts_with("core ")) {
        let _ = writeln!(r, "dispstate: {}", line);
    }
    for line in text.lines().skip_while(|l| !l.starts_with("core ")) {
        crate::serial_println!("dispstate: {}", line);
    }
    DISPSTATE.call_once(|| text);
}
