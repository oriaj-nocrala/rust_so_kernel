// kernel/src/hw_tests.rs
//
// QEMU integration test cases — `cargo test --target x86_64-unknown-none`
// (run from `kernel/`), collected via `#[test_case]`
// (`custom_test_frameworks`, see `test_framework.rs`). Only compiled under
// `#[cfg(test)]` (see `mod hw_tests` in `main.rs`).
//
// These assert real hardware-path behavior against a real QEMU boot — the
// `hal/` host tests already cover the pure parsing/decoding logic in
// milliseconds with no QEMU involved; this file is for the part that can't
// be tested that way. `init::test_support::boot_for_tests` (called from
// `kernel_main` before `test_main()` runs these) performs whatever subset
// of the real boot sequence a case here needs already live.

/// Case 1 (Phase 2 of `docs/drivers/roadmap.md`): the ACPI parse against
/// QEMU's real i440fx MADT — Local APIC address, one I/O APIC at the
/// expected base, at least one enabled CPU, and the legacy IRQ0->GSI2
/// PIT/timer override. Previously only a human eyeballing
/// `[acpi] SELFTEST PASS/FAIL` in serial output could catch a regression
/// here; this is the same set of checks (`acpi::selftest_ok`, shared with
/// the boot-time log path) as a real assertion with a real exit code.
#[test_case]
fn acpi_selftest_passes() {
    let topo = crate::acpi::topology()
        .expect("ACPI parse did not populate topology during test boot");
    assert!(
        crate::acpi::selftest_ok(topo),
        "ACPI SELFTEST failed one or more assertions against known QEMU i440fx values"
    );
}
