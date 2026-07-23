#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]
// QEMU integration test harness (`cargo test --target x86_64-unknown-none`,
// run from inside `kernel/` — see `kernel/.cargo/config.toml`'s
// `[target.x86_64-unknown-none] runner` and `kernel/src/test_framework.rs`).
// Only active in test builds; a normal `cargo run`/`cargo build` never sees
// `custom_test_frameworks` enabled at all.
#![cfg_attr(test, feature(custom_test_frameworks))]
#![cfg_attr(test, test_runner(crate::test_framework::runner))]
#![cfg_attr(test, reexport_test_harness_main = "test_main")]

extern crate alloc;

mod ac97;
mod acpi;
mod allocator;
mod block;
mod cpu;
mod debug;
mod drivers;
mod framebuffer;
mod fs;
mod hal;
#[cfg(test)]
mod hw_tests;
mod init;
mod interrupts;
mod ipc;
mod keyboard;
mod keyboard_buffer;
mod memory;
mod mouse;
#[cfg(not(test))]
mod panic;
mod pci;
mod process;
mod pit;
mod rtc;
mod serial;
#[cfg(test)]
mod test_framework;
mod time;
mod tty;

use bootloader_api::{BootInfo, BootloaderConfig, config::Mapping, entry_point};

pub static BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config
};

entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);

#[cfg(not(test))]
fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    init::boot(boot_info)
}

/// Test-build entry point: boots only as much of the kernel as hardware
/// integration tests need (`init::test_support::boot_for_tests` — see its
/// doc comment for exactly what and why), then hands off to the
/// `custom_test_frameworks`-generated `test_main` (collects every
/// `#[test_case]` in the crate, see `hw_tests.rs`, and runs them via
/// `test_framework::runner`). `runner` always exits QEMU itself via
/// isa-debug-exit — the `hlt` loop below is unreachable in practice and
/// only exists so this function still satisfies `-> !`.
#[cfg(test)]
fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    init::test_support::boot_for_tests(boot_info);
    test_main();
    loop {
        unsafe { core::arch::asm!("hlt") };
    }
}