// kernel/build.rs
//
// Builds all userspace programs before the kernel is compiled, then copies
// the resulting ELF binaries into kernel/embedded/ so that include_bytes!
// can embed them.
//
// Trigger: any change to userspace/src/** or userspace/Cargo.toml.

use std::path::PathBuf;
use std::process::Command;

/// (binary name, destination filename inside kernel/embedded/)
const PROGRAMS: &[(&str, &str)] = &[
    ("uname",     "uname.elf"),
    ("shell",     "shell.elf"),
    ("snake",     "snake.elf"),
    ("uptime",    "uptime.elf"),
    ("sleep",     "sleep.elf"),
    ("tsc",       "tsc.elf"),
    ("ipc_ping",  "ipc_ping.elf"),
    ("mmap_test",  "mmap_test.elf"),
    ("poll_test",  "poll_test.elf"),
];

fn main() {
    let kernel_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = kernel_dir.parent().unwrap();
    let userspace_dir = workspace_root.join("userspace");
    let embedded_dir = kernel_dir.join("embedded");

    // ── Rebuild triggers ──────────────────────────────────────────────────
    // Watch key files/dirs so cargo knows when to re-run this script.
    for entry in &[
        userspace_dir.join("Cargo.toml"),
        userspace_dir.join("linker.ld"),
        userspace_dir.join("src"),
    ] {
        println!("cargo:rerun-if-changed={}", entry.display());
    }

    // ── Create embedded dir ───────────────────────────────────────────────
    std::fs::create_dir_all(&embedded_dir)
        .expect("Failed to create kernel/embedded/");

    // ── Build userspace ───────────────────────────────────────────────────
    // Use the same cargo binary that's running this build script.
    // CWD is set to userspace/ so that userspace/.cargo/config.toml is found,
    // which provides: target = x86_64-unknown-none, build-std, linker flags.
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());

    // Unset cargo-injected vars that would bleed the kernel's build settings
    // into the userspace build:
    //   CARGO_ENCODED_RUSTFLAGS — overrides all rustflags from config files
    //   RUSTFLAGS               — same, older form
    //   CARGO_BUILD_TARGET      — would override userspace's target
    //   CARGO_TARGET_DIR        — would redirect output to the kernel's target/
    let status = Command::new(&cargo)
        .current_dir(&userspace_dir)
        .args(["build", "--release"])
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_BUILD_TARGET")
        .env_remove("CARGO_TARGET_DIR")
        .status()
        .expect("Failed to spawn cargo for userspace");

    assert!(status.success(), "Userspace build failed — check output above");

    // ── Copy ELFs to kernel/embedded/ ─────────────────────────────────────
    let release_dir = userspace_dir.join("target/x86_64-unknown-none/release");

    for (bin, elf_name) in PROGRAMS {
        let src = release_dir.join(bin);
        let dst = embedded_dir.join(elf_name);

        std::fs::copy(&src, &dst).unwrap_or_else(|e| {
            panic!("Failed to copy {} -> {}: {}", src.display(), dst.display(), e)
        });

        println!("cargo:warning=userspace: {} -> {}", bin, elf_name);
    }
}
