// kernel/src/init/test_support.rs
//
// The reusable "how much of `init::boot` does a test boot need" answer —
// see `kernel/src/test_framework.rs` for the surrounding harness and
// `kernel/src/hw_tests.rs` for the test cases that run against this.
//
// A QEMU integration test binary boots through the exact same
// `entry_point!` as the real kernel (`main.rs`), so it still needs *some*
// of `init::boot`'s early steps before any `#[test_case]` can safely run:
// the physical memory offset must be live before any `PhysMem`-seam driver
// (ACPI today, APIC next per `docs/drivers/roadmap.md`) can read physical
// memory at all, and the heap needs the Buddy allocator seeded before
// anything alloc-using (ACPI's topology `Vec`s, for one) can run. This is
// exactly the same subset every future hardware-path test will need again,
// so it lives here once — reusable, not a one-off inlined into a single
// test — instead of being re-derived per test file.
//
// Deliberately stops well short of full `init::boot`: no framebuffer setup
// (a headless `-display none` test boot has nothing worth drawing to, and
// skipping it removes one more way a test boot could differ from a normal
// one in something irrelevant to what's being tested), no
// `init_hardware_interrupts` (PIC/PIT unmasked + IDT actually loaded — no
// test case here needs a live timer or keyboard IRQ yet), no VFS, no
// processes.
//
// Add steps here (not per-test-case) if a future test genuinely needs more
// of the boot sequence live — e.g. an APIC migration test will likely need
// this function to also load the IDT and bring up interrupts. Keep it one
// shared, explicit boot path rather than every test file hand-rolling its
// own slice of `init::boot`.

use bootloader_api::BootInfo;
use x86_64::VirtAddr;

/// Boots just enough of the kernel for hardware-path integration tests:
/// IDT *built* (not loaded — no interrupts are enabled in test mode, so
/// there is nothing to route to it yet), physical memory offset recorded,
/// Buddy allocator seeded from the bootloader's memory map, the shared
/// zero-page frame allocated, and the ACPI driver run through the same
/// best-effort `hal::run_all` registry real boot uses — the one driver
/// today's test cases need already initialized before `test_main` runs.
pub fn boot_for_tests(boot_info: &'static mut BootInfo) {
    super::devices::init_idt();

    let phys_mem_offset = VirtAddr::new(
        boot_info
            .physical_memory_offset
            .into_option()
            .expect("bootloader did not report a physical memory offset"),
    );
    super::memory::init_core(phys_mem_offset, &boot_info.memory_regions);

    // Needed by anything that touches user address spaces (COW) — cheap
    // and harmless for tests that don't, so it's included unconditionally
    // here rather than threaded through as a per-test opt-in.
    unsafe {
        crate::memory::cow::init_zero_frame();
    }

    // Same driver, same registry call, as the real boot's ACPI step
    // (`init/mod.rs`) — see `hw_tests.rs`'s `acpi_selftest_passes`, the
    // consumer. A future APIC test would add its own driver to this same
    // `run_all([...])` list rather than inventing a second boot path.
    let mut acpi_driver = crate::acpi::AcpiDriver::new(boot_info.rsdp_addr.into_option());
    crate::hal::run_all(&mut [&mut acpi_driver]);
}
