// kernel/build.rs
//
// Builds all userspace programs before the kernel is compiled, then copies
// the resulting ELF binaries into kernel/embedded/ so that include_bytes!
// can embed them.
//
// Three families:
//   Rust     — built with cargo in userspace/
//   C        — built with clang+mlibc from userspace/c/; sysroot at ../sysroot/
//   BusyBox  — external `make`-based build, see scripts/build-busybox.sh

use std::path::PathBuf;
use std::process::Command;

/// Rust binaries: (cargo bin name, embedded filename)
const RUST_PROGRAMS: &[(&str, &str)] = &[
    ("uname",      "uname.elf"),
    ("shell",      "shell.elf"),
    ("snake",      "snake.elf"),
    ("uptime",     "uptime.elf"),
    ("sleep",      "sleep.elf"),
    ("tsc",        "tsc.elf"),
    ("ipc_ping",   "ipc_ping.elf"),
    ("mmap_test",  "mmap_test.elf"),
    ("poll_test",  "poll_test.elf"),
    ("ls",         "ls.elf"),
    ("pipe_test",  "pipe_test.elf"),
    ("signal_test", "signal_test.elf"),
    ("demo",       "demo.elf"),
];

/// C binaries: (source file stem, embedded filename)
const C_PROGRAMS: &[(&str, &str)] = &[
    ("hello", "hello.elf"),
    ("pthread_test", "pthread_test.elf"),
    ("producer_consumer", "producer_consumer.elf"),
    ("mlibc_signal_test", "mlibc_signal_test.elf"),
    ("stat_test", "stat_test.elf"),
    ("argv_test", "argv_test.elf"),
    ("jobctl_test", "jobctl_test.elf"),
];

/// Not built here at all — see the busybox.elf handling below, which
/// shells out to scripts/build-busybox.sh (a `make`-based external build,
/// nothing like the Rust/C recipes above) only when the output is missing.
const BUSYBOX_ELF: &str = "busybox.elf";

fn main() {
    let kernel_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = kernel_dir.parent().unwrap();
    let userspace_dir = workspace_root.join("userspace");
    let c_dir = userspace_dir.join("c");
    let sysroot_dir = workspace_root.join("sysroot");
    let embedded_dir = kernel_dir.join("embedded");

    // ── Rebuild triggers ──────────────────────────────────────────────────
    for entry in &[
        userspace_dir.join("Cargo.toml"),
        userspace_dir.join("linker.ld"),
        userspace_dir.join("src"),
        c_dir.clone(),
        sysroot_dir.join("usr/lib/libc.a"),
        sysroot_dir.join("usr/lib/crt1.o"),
        workspace_root.join("mlibc-port"),
        workspace_root.join("mlibc-cross.ini"),
        workspace_root.join("scripts/setup-mlibc.sh"),
        workspace_root.join("scripts/build-busybox.sh"),
        workspace_root.join("busybox-config/minimal.config"),
    ] {
        println!("cargo:rerun-if-changed={}", entry.display());
    }

    // ── Build the mlibc sysroot if missing ──────────────────────────────────
    //
    // The mlibc git submodule (.gitmodules) points at upstream managarm/mlibc,
    // which has no support for this kernel's syscall ABI. mlibc-port/ in this
    // repo holds our own out-of-tree sysdeps port; scripts/setup-mlibc.sh
    // copies it into the submodule checkout, registers it in mlibc's
    // meson.build, and builds crt1.o + libc.a + headers into sysroot/.
    if !sysroot_dir.join("usr/lib/libc.a").exists() {
        println!("cargo:warning=sysroot missing — building mlibc (this can take a minute)...");
        let status = Command::new("bash")
            .arg(workspace_root.join("scripts/setup-mlibc.sh"))
            .current_dir(workspace_root)
            .status()
            .expect("Failed to spawn scripts/setup-mlibc.sh");
        assert!(status.success(), "scripts/setup-mlibc.sh failed");
    }

    // ── Create embedded dir ───────────────────────────────────────────────
    std::fs::create_dir_all(&embedded_dir)
        .expect("Failed to create kernel/embedded/");

    // ── Build Rust userspace ──────────────────────────────────────────────
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());

    let status = Command::new(&cargo)
        .current_dir(&userspace_dir)
        .args(["build", "--release"])
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_BUILD_TARGET")
        .env_remove("CARGO_TARGET_DIR")
        .status()
        .expect("Failed to spawn cargo for userspace");

    assert!(status.success(), "Userspace Rust build failed");

    let release_dir = userspace_dir.join("target/x86_64-unknown-none/release");

    for (bin, elf_name) in RUST_PROGRAMS {
        let src = release_dir.join(bin);
        let dst = embedded_dir.join(elf_name);
        std::fs::copy(&src, &dst).unwrap_or_else(|e| {
            panic!("Failed to copy {} -> {}: {}", src.display(), dst.display(), e)
        });
        println!("cargo:warning=userspace(rust): {} -> {}", bin, elf_name);
    }

    // ── Build C userspace ─────────────────────────────────────────────────
    // clang with the mlibc sysroot, fully static, no host libc.
    let sysroot_inc = sysroot_dir.join("usr/include");
    let sysroot_lib = sysroot_dir.join("usr/lib");
    let crt1 = sysroot_lib.join("crt1.o");
    let libc_a = sysroot_lib.join("libc.a");

    for (stem, elf_name) in C_PROGRAMS {
        let src = c_dir.join(format!("{}.c", stem));
        let dst = embedded_dir.join(elf_name);

        let status = Command::new("clang")
            .args([
                "--target=x86_64-constanos-elf",
                "-ffreestanding",
                "-fno-stack-protector",
                "-fomit-frame-pointer",
                "-mno-red-zone",
                "-O2",
                "-static",
                "-nostdlib",
                "-isystem", sysroot_inc.to_str().unwrap(),
                crt1.to_str().unwrap(),
                src.to_str().unwrap(),
                libc_a.to_str().unwrap(),
                "-o", dst.to_str().unwrap(),
            ])
            .status()
            .expect("Failed to spawn clang for C userspace");

        assert!(status.success(), "C userspace build failed for {}", stem);
        println!("cargo:warning=userspace(c): {}.c -> {}", stem, elf_name);
    }

    // ── Build BusyBox if missing ────────────────────────────────────────────
    //
    // A `make`-based external build (BusyBox's own Kconfig+Makefile system),
    // nothing like the Rust/clang recipes above — scripts/build-busybox.sh
    // owns the whole recipe (cross-compiler wrapper, config, build, copy).
    // Only invoked when the output is missing: unlike the fast Rust/C
    // rebuilds above, this takes real time, and BusyBox's own Makefile
    // already does its own incremental rebuilds if re-run, so there's
    // nothing gained by unconditionally shelling out to it every time.
    let busybox_elf = embedded_dir.join(BUSYBOX_ELF);
    if !busybox_elf.exists() {
        println!("cargo:warning=busybox.elf missing — building BusyBox (this can take a minute)...");
        let status = Command::new("bash")
            .arg(workspace_root.join("scripts/build-busybox.sh"))
            .current_dir(workspace_root)
            .status()
            .expect("Failed to spawn scripts/build-busybox.sh");
        assert!(status.success(), "scripts/build-busybox.sh failed");
    }
}
