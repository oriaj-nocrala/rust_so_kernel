// kernel/src/test_framework.rs
//
// QEMU integration test harness (`cargo test --target x86_64-unknown-none`,
// run from inside `kernel/` so its `.cargo/config.toml` `runner` key
// applies — see that file's comment). Only compiled under `#[cfg(test)]`
// (see the `#![cfg_attr(test, ...)]` trio + `mod test_framework` gate at
// the top of `main.rs`).
//
// A normal `cargo run`/`cargo build` kernel never sees this file at all:
// the test build swaps in its own `kernel_main` (`main.rs`, `#[cfg(test)]`
// branch) in place of the real `init::boot` sequence, and its own panic
// handler (below) in place of the real panic screen (`panic.rs`, gated
// `#[cfg(not(test))]`).
//
// PASS/FAIL crosses the guest/host boundary via the `isa-debug-exit`
// device: writing `code` to I/O port 0xf4 makes QEMU exit the whole
// process with status `(code << 1) | 1`. The host-side half
// (`qemu-test-runner/src/main.rs`) launches QEMU with
// `-device isa-debug-exit,iobase=0xf4,iosize=0x04` and translates the
// resulting process exit code back into a pass/fail `cargo test` result.

use x86_64::instructions::port::Port;

/// Values written to the isa-debug-exit port. Picked to stay clear of
/// 0x00/0x01 (which would map to QEMU exit codes 1/3 — easy to confuse
/// with an ordinary QEMU crash or CLI usage error rather than a real
/// pass/fail signal from the guest).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum QemuExitCode {
    Success = 0x10,
    Failed = 0x11,
}

/// Writes `code` to the isa-debug-exit port and never returns — QEMU exits
/// the instant the write lands. The trailing `hlt` loop only matters if
/// something is wrong enough that even isa-debug-exit didn't work (e.g.
/// this binary run outside QEMU, or the device wasn't attached to the
/// command line) — the host-side runner still enforces its own timeout for
/// exactly that case, so a missing device fails the test run instead of
/// hanging it forever.
pub fn exit_qemu(code: QemuExitCode) -> ! {
    unsafe {
        let mut port: Port<u32> = Port::new(0xf4);
        port.write(code as u32);
    }
    loop {
        unsafe { core::arch::asm!("hlt") };
    }
}

/// Blanket-implemented for any `Fn()`, so a plain `#[test_case] fn foo() {
/// ... }` item satisfies it with zero boilerplate per test — the standard
/// `custom_test_frameworks` pattern.
pub trait Testable {
    fn run(&self);
}

impl<T: Fn()> Testable for T {
    fn run(&self) {
        crate::serial_print!("{}...\t", core::any::type_name::<T>());
        self();
        crate::serial_println!("[ok]");
    }
}

/// The `#[test_runner]` — collects every `#[test_case]` in the crate (see
/// `hw_tests.rs`; later APIC/other hardware tests just add more `#[test_case]`
/// functions, no runner changes needed), runs them in order, and exits QEMU
/// with the pass code once all of them return normally. A panicking test
/// never reaches back here — `no_std` has no unwinding, so a failed
/// `assert!` goes straight to `test_panic_handler` below, which exits with
/// the failure code immediately.
pub fn runner(tests: &[&dyn Testable]) {
    crate::serial_println!("[test_framework] running {} test(s)", tests.len());
    for test in tests {
        test.run();
    }
    exit_qemu(QemuExitCode::Success);
}

/// The test-mode panic handler. A failing assertion (or any other panic)
/// anywhere in a `#[test_case]`, or in code it calls, lands here instead of
/// the real kernel's panic screen — log it to serial for a human, then exit
/// QEMU with the failure code so the host-side runner (and therefore
/// `cargo test`) reports a real failure instead of hanging on a wedged VM.
#[panic_handler]
fn test_panic_handler(info: &core::panic::PanicInfo) -> ! {
    crate::serial_println!("[failed]");
    crate::serial_println!("Error: {}", info);
    exit_qemu(QemuExitCode::Failed);
}
