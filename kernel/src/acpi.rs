// kernel/src/acpi.rs
//
// Thin kernel-side adapter around `hal::acpi`'s pure parser: this module
// owns hardware access (reading physical memory via the bootloader's fixed
// offset, through `crate::hal::KernelPhysMem`), the `spin::Once` global that
// holds the parsed topology for the rest of the kernel's lifetime, and the
// boot-time serial summary + `[acpi] SELFTEST` smoke test. All the actual
// RSDP/XSDT/RSDT/MADT parsing logic now lives in `hal::acpi`, where it can
// be unit tested on the host with `cargo test` (see `hal/src/acpi.rs`).
//
// Does NOT touch the existing 8259 PIC / IDT / interrupt setup in any way;
// nothing here reprograms hardware — parse-only, same as before this
// refactor.

// Re-exported so callers outside this module (`fs/procfs.rs::render_acpi`,
// notably) keep compiling unchanged against `crate::acpi::{AcpiTopology,
// CpuInfo, IoApic, Iso}` even though the types now live in `hal::acpi`.
// `#[allow(unused_imports)]`: nothing inside this binary crate names
// `CpuInfo`/`IoApic`/`Iso` directly (callers only access fields through an
// `AcpiTopology` value), so rustc sees these three as unused — but this
// re-export is the whole point (API preservation), not dead code.
#[allow(unused_imports)]
pub use hal::acpi::{AcpiTopology, CpuInfo, IoApic, Iso};

use hal::acpi::AcpiError;

use crate::hal::{Driver, DriverError, KernelPhysMem};
use crate::serial_println;

static TOPOLOGY: spin::Once<AcpiTopology> = spin::Once::new();

/// Returns the parsed topology, if ACPI parsing succeeded at boot.
pub fn topology() -> Option<&'static AcpiTopology> {
    TOPOLOGY.get()
}

/// `crate::hal::Driver` adapter around the ACPI parse — this is what
/// `init/mod.rs` runs through the new best-effort driver registry, as the
/// pilot/proof-of-concept for that pattern (see the HAL refactor plan).
/// Owns just the one thing this driver's `init()` needs that isn't
/// discoverable on its own: the RSDP physical address the bootloader found.
pub struct AcpiDriver {
    rsdp_addr: Option<u64>,
}

impl AcpiDriver {
    pub fn new(rsdp_addr: Option<u64>) -> Self {
        AcpiDriver { rsdp_addr }
    }
}

impl Driver for AcpiDriver {
    fn name(&self) -> &str {
        "acpi"
    }

    fn init(&mut self) -> Result<(), DriverError> {
        let Some(rsdp_pa) = self.rsdp_addr else {
            serial_println!("[acpi] no RSDP address from bootloader — skipping ACPI parse");
            return Err(DriverError::NotFound);
        };

        let topo = match hal::acpi::parse(&KernelPhysMem, rsdp_pa) {
            Ok(topo) => topo,
            Err(e) => {
                log_error(e);
                return Err(match e {
                    AcpiError::NoRootTable | AcpiError::NoMadt => DriverError::NotFound,
                    AcpiError::BadSignature | AcpiError::BadChecksum => DriverError::Invalid,
                });
            }
        };

        log_summary(&topo);
        run_selftest(&topo);

        TOPOLOGY.call_once(|| topo);
        Ok(())
    }
}

/// The individual self-check results against known QEMU i440fx values.
/// Computed once by `selftest_checks` and consumed by both `run_selftest`
/// (boot-time human-readable log, `[acpi] SELFTEST ...`) and the QEMU
/// integration test (`hw_tests.rs`'s `acpi_selftest_passes`, via
/// `selftest_ok`) — one set of assertions instead of two copies that could
/// drift apart.
struct SelftestChecks {
    local_apic_ok: bool,
    io_apic_ok: bool,
    cpus_ok: bool,
    iso_ok: bool,
}

impl SelftestChecks {
    fn all_ok(&self) -> bool {
        self.local_apic_ok && self.io_apic_ok && self.cpus_ok && self.iso_ok
    }
}

fn selftest_checks(topo: &AcpiTopology) -> SelftestChecks {
    SelftestChecks {
        local_apic_ok: topo.local_apic_addr == 0xFEE00000,
        io_apic_ok: topo.io_apics.iter().any(|io| io.address == 0xFEC00000 && io.gsi_base == 0),
        cpus_ok: !topo.cpus.is_empty(),
        iso_ok: topo.overrides.iter().any(|iso| iso.source == 0 && iso.gsi == 2),
    }
}

/// `true` iff every self-check against known QEMU i440fx values passes —
/// no printing. This is what the QEMU integration test (`hw_tests.rs`)
/// asserts on; see `SelftestChecks`.
pub fn selftest_ok(topo: &AcpiTopology) -> bool {
    selftest_checks(topo).all_ok()
}

/// Logs why `hal::acpi::parse` failed, at the same detail level the
/// original inline implementation logged at each of its early returns.
fn log_error(e: AcpiError) {
    match e {
        AcpiError::BadSignature => serial_println!("[acpi] RSDP signature mismatch — skipping ACPI parse"),
        AcpiError::BadChecksum => serial_println!("[acpi] checksum validation failed — skipping ACPI parse"),
        AcpiError::NoRootTable => serial_println!("[acpi] no usable RSDT/XSDT address — skipping ACPI parse"),
        AcpiError::NoMadt => serial_println!("[acpi] no MADT (APIC) table found — skipping ACPI parse"),
    }
}

/// Prints the same boot-time summary the original inline implementation
/// printed: Local APIC address, enabled CPUs, I/O APICs, interrupt source
/// overrides.
fn log_summary(topo: &AcpiTopology) {
    use alloc::vec::Vec;

    serial_println!("[acpi] Local APIC @ {:#010x}", topo.local_apic_addr);
    let enabled_ids: Vec<u8> = topo.cpus.iter().map(|c| c.apic_id).collect();
    serial_println!("[acpi] CPUs: {} (apic_id {:?} enabled)", topo.cpus.len(), enabled_ids);
    for io in &topo.io_apics {
        serial_println!(
            "[acpi] I/O APIC {} @ {:#010x} gsi_base={}",
            io.id, io.address, io.gsi_base
        );
    }
    for iso in &topo.overrides {
        serial_println!(
            "[acpi] override: bus {} IRQ {} -> GSI {} (flags {:#x})",
            iso.bus, iso.source, iso.gsi, iso.flags
        );
    }
}

/// Self-check against known QEMU i440fx values — same checks, same log
/// lines, as the original inline implementation. This is the integration
/// smoke test the boot log is grepped for (`[acpi] SELFTEST PASS`).
fn run_selftest(topo: &AcpiTopology) {
    let checks = selftest_checks(topo);

    serial_println!(
        "[acpi] SELFTEST: local_apic={:#010x} {}",
        topo.local_apic_addr,
        if checks.local_apic_ok { "OK" } else { "FAIL" }
    );

    serial_println!(
        "[acpi] SELFTEST: io_apic(0xFEC00000, gsi_base=0) {}",
        if checks.io_apic_ok { "OK" } else { "FAIL" }
    );

    serial_println!(
        "[acpi] SELFTEST: cpus>=1 (found {}) {}",
        topo.cpus.len(),
        if checks.cpus_ok { "OK" } else { "FAIL" }
    );

    serial_println!(
        "[acpi] SELFTEST: override(IRQ0->GSI2) {}",
        if checks.iso_ok { "OK" } else { "FAIL" }
    );

    let all_ok = checks.all_ok();
    if all_ok {
        serial_println!("[acpi] SELFTEST PASS");
    } else {
        serial_println!("[acpi] SELFTEST FAIL");
    }
}
