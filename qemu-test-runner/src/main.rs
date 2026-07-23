// qemu-test-runner/src/main.rs
//
// Host-side half of the QEMU integration test framework
// (`kernel/src/test_framework.rs` is the guest side; see that file's doc
// comment for the full picture). Invoked by `scripts/run-kernel-tests.sh`
// with the path to the compiled test kernel ELF
// (`cargo build --target x86_64-unknown-none --tests`'s output — see that
// script for why `cargo test` itself isn't used to drive this).
//
// Usage: qemu-test-runner <path-to-test-kernel-elf>
//
// Steps:
//   1. Wrap the ELF in a UEFI disk image (`bootloader::UefiBoot` — the
//      same call the root `build.rs` makes for the normal boot image).
//   2. Find OVMF firmware already installed on the system (a deliberately
//      minimal, standalone copy of `build.rs`'s `find_system_ovmf` — see
//      `Cargo.toml`'s comment for why this crate doesn't share code with
//      the root package instead).
//   3. Launch QEMU headless with `-device isa-debug-exit`, serial captured
//      to a temp file, and a timeout so a hung/looping kernel fails the
//      test run instead of blocking forever.
//   4. Map QEMU's process exit code back to a runner exit code `cargo
//      test`-style tooling understands (0 = pass, nonzero = fail).
//
// Deliberately does NOT attach the ext2 disk or an AC97 audio device the
// way `src/main.rs`/`scripts/qemu-debug.sh` do for a normal boot:
// `kernel::init::test_support::boot_for_tests` stops long before
// `fs::init()` or `ac97::init()` would ever run in a test build, so
// neither device is reachable from any `#[test_case]` today. Keeping the
// command line minimal also sidesteps needing a host audio backend just
// to run tests.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// isa-debug-exit: QEMU exits the whole process with `(code << 1) | 1` for
/// whatever `code` the guest wrote to the port. Must match
/// `kernel::test_framework::QemuExitCode` exactly.
const QEMU_EXIT_SUCCESS: i32 = (0x10 << 1) | 1; // 33
const QEMU_EXIT_FAILED: i32 = (0x11 << 1) | 1; // 35

/// Generous — a healthy test boot reaches isa-debug-exit in a couple of
/// seconds. This only needs to be long enough that dev-machine jitter
/// never trips it, while still failing a truly wedged kernel in finite
/// time instead of hanging the caller forever.
const BOOT_TIMEOUT: Duration = Duration::from_secs(60);

fn main() {
    let elf_path = match std::env::args().nth(1) {
        Some(p) => PathBuf::from(p),
        None => {
            eprintln!("usage: qemu-test-runner <path-to-test-kernel-elf>");
            std::process::exit(1);
        }
    };
    if !elf_path.exists() {
        eprintln!("qemu-test-runner: no such file: {}", elf_path.display());
        std::process::exit(1);
    }

    let out_dir = std::env::temp_dir().join(format!("qemu-test-runner-{}", std::process::id()));
    std::fs::create_dir_all(&out_dir).expect("failed to create temp dir for UEFI test image");

    let uefi_path = out_dir.join("uefi-test.img");
    bootloader::UefiBoot::new(&elf_path)
        .create_disk_image(&uefi_path)
        .expect("failed to build UEFI test disk image");

    let (ovmf_code, ovmf_vars) = find_system_ovmf().unwrap_or_else(|| {
        eprintln!(
            "qemu-test-runner: no OVMF firmware found on the system (checked the usual distro \
             paths). Install one, e.g. on Arch: sudo pacman -S edk2-ovmf"
        );
        std::process::exit(1);
    });
    // QEMU opens VARS read-write (stores UEFI variables across boots) — the
    // system copy is typically root-owned/read-only, so work on a private
    // copy (same reason as root build.rs).
    let ovmf_vars_writable = out_dir.join("OVMF_VARS.fd");
    std::fs::copy(&ovmf_vars, &ovmf_vars_writable).expect("failed to copy OVMF_VARS");

    let serial_log = out_dir.join("serial.log");

    let mut child = Command::new("qemu-system-x86_64")
        .arg("-drive")
        .arg(format!("if=pflash,format=raw,readonly=on,file={}", ovmf_code.display()))
        .arg("-drive")
        .arg(format!("if=pflash,format=raw,file={}", ovmf_vars_writable.display()))
        .arg("-drive")
        .arg(format!("format=raw,file={}", uefi_path.display()))
        .arg("-m")
        .arg("512M")
        .arg("-cpu")
        .arg("max")
        .arg("-display")
        .arg("none")
        .arg("-serial")
        .arg(format!("file:{}", serial_log.display()))
        .arg("-device")
        .arg("isa-debug-exit,iobase=0xf4,iosize=0x04")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn qemu-system-x86_64 (is it installed and on PATH?)");

    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("failed to poll qemu-system-x86_64") {
            break status;
        }
        if start.elapsed() > BOOT_TIMEOUT {
            eprintln!(
                "qemu-test-runner: TIMEOUT after {:?} — killing QEMU (kernel likely hung before \
                 reaching isa-debug-exit)",
                BOOT_TIMEOUT
            );
            let _ = child.kill();
            let _ = child.wait();
            print_serial_log(&serial_log);
            std::process::exit(1);
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    print_serial_log(&serial_log);

    match status.code() {
        Some(QEMU_EXIT_SUCCESS) => {
            eprintln!("qemu-test-runner: PASS (qemu exit code {})", QEMU_EXIT_SUCCESS);
            std::process::exit(0);
        }
        Some(QEMU_EXIT_FAILED) => {
            eprintln!("qemu-test-runner: FAIL (qemu exit code {})", QEMU_EXIT_FAILED);
            std::process::exit(1);
        }
        other => {
            eprintln!(
                "qemu-test-runner: unexpected qemu exit ({:?}) — kernel likely panicked/crashed \
                 before reaching isa-debug-exit, or QEMU itself failed to start",
                other
            );
            std::process::exit(1);
        }
    }
}

fn print_serial_log(path: &Path) {
    let mut buf = String::new();
    if std::fs::File::open(path).and_then(|mut f| f.read_to_string(&mut buf)).is_ok() {
        eprintln!("--- guest serial output ---");
        eprintln!("{}", buf);
        eprintln!("--- end guest serial output ---");
    }
}

/// Looks for OVMF_CODE/OVMF_VARS in the usual distro install locations —
/// deliberately a minimal, standalone copy of `find_system_ovmf` in the
/// root `build.rs` rather than a shared module (see `Cargo.toml`'s
/// comment for why this crate stands alone). No network-fetch fallback
/// here (unlike `build.rs`'s `try_get_ovmf`, which pulls in the
/// `ovmf-prebuilt` crate): a dev/CI box running integration tests is
/// expected to already have OVMF installed for `cargo run` to work at
/// all, so keeping this copy minimal was preferred over threading a
/// second fetch path through a second crate.
fn find_system_ovmf() -> Option<(PathBuf, PathBuf)> {
    const CANDIDATES: &[(&str, &str)] = &[
        ("/usr/share/edk2/x64/OVMF_CODE.4m.fd", "/usr/share/edk2/x64/OVMF_VARS.4m.fd"),
        ("/usr/share/OVMF/OVMF_CODE.fd", "/usr/share/OVMF/OVMF_VARS.fd"),
        ("/usr/share/ovmf/x64/OVMF_CODE.fd", "/usr/share/ovmf/x64/OVMF_VARS.fd"),
        ("/usr/share/ovmf/OVMF_CODE.fd", "/usr/share/ovmf/OVMF_VARS.fd"),
    ];
    CANDIDATES
        .iter()
        .map(|(code, vars)| (PathBuf::from(code), PathBuf::from(vars)))
        .find(|(code, vars)| code.exists() && vars.exists())
}
