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
    ("tsc",        "tsc.elf"),
    ("ipc_ping",   "ipc_ping.elf"),
    ("mmap_test",  "mmap_test.elf"),
    ("poll_test",  "poll_test.elf"),
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
    ("kdebug", "kdebug.elf"),
    ("ext2_robust_test", "ext2_robust_test.elf"),
    ("fpu_test", "fpu_test.elf"),
];

/// Not built here at all — see the busybox.elf handling below, which
/// shells out to scripts/build-busybox.sh (a `make`-based external build,
/// nothing like the Rust/C recipes above) only when the output is missing.
const BUSYBOX_ELF: &str = "busybox.elf";

/// Same "external build, only if missing/stale" shape as DOOM_ELF below,
/// but for quakegeneric (git submodule + our own quake-port/ platform
/// file) via scripts/build-quake.sh.
const QUAKE_ELF: &str = "quake.elf";

/// Same "external build, only if missing" shape as BUSYBOX_ELF, but for
/// doomgeneric (git submodule + our own doom-port/ platform file) via
/// scripts/build-doom.sh — a whole-engine multi-file C build that doesn't
/// fit the one-.c-file-per-program C_PROGRAMS loop below.
const DOOM_ELF: &str = "doom.elf";

/// Emit `cargo:rerun-if-changed` for every file under `dir`, recursively.
///
/// Pointing `rerun-if-changed` straight at a *directory* only ever catches
/// entries being added/removed — editing an existing file in place doesn't
/// change the containing directory's own mtime on Linux, so cargo's
/// freshness check sees nothing to react to and silently skips rerunning
/// this build script (confirmed directly: editing `userspace/src/bin/
/// shell.rs` left `kernel/embedded/shell.elf` stale through a full
/// `cargo build` — the fix had to be `touch`ing this very file to force a
/// rerun). Watching every individual file instead makes in-place edits
/// visible the same way top-level `Cargo.toml`/`linker.ld` entries already
/// are. `target/`/`.git` are skipped: they're the *output* of the cargo
/// invocation below, not an input — watching them would make this build
/// script look dirty on every single run (its own output always changes)
/// without ever actually catching a missed rebuild.
fn watch_dir_recursive(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if name == "target" || name == ".git" {
            continue;
        }
        if path.is_dir() {
            watch_dir_recursive(&path);
        } else {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

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
        sysroot_dir.join("usr/lib/libc.a"),
        sysroot_dir.join("usr/lib/crt1.o"),
        workspace_root.join("mlibc-cross.ini"),
        workspace_root.join("scripts/setup-mlibc.sh"),
        workspace_root.join("scripts/build-busybox.sh"),
        workspace_root.join("busybox-config/minimal.config"),
        workspace_root.join("scripts/build-doom.sh"),
        workspace_root.join("scripts/fetch-freedoom.sh"),
        workspace_root.join("scripts/build-quake.sh"),
        workspace_root.join("scripts/fetch-quake-shareware.sh"),
    ] {
        println!("cargo:rerun-if-changed={}", entry.display());
    }
    // userspace/src, userspace/c, and mlibc-port hold many files that get
    // edited in place — recurse into them individually (see
    // watch_dir_recursive's doc comment) instead of one coarse
    // directory-level entry each.
    watch_dir_recursive(&userspace_dir.join("src"));
    watch_dir_recursive(&c_dir);
    watch_dir_recursive(&workspace_root.join("mlibc-port"));
    watch_dir_recursive(&workspace_root.join("doom-port"));
    watch_dir_recursive(&workspace_root.join("quake-port"));

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

    // The Freedoom IWAD is no longer embedded in the kernel image — DOOM
    // reads it from /mnt/freedoom1.wad (ext2, seeded from
    // disk-image-root/freedoom1.wad by the workspace-root build.rs, which
    // already runs scripts/fetch-freedoom.sh on its own). See
    // doom-port/doomgeneric_constanos.c's header comment for why the
    // earlier kernel-embedded-device workaround existed and why it's gone.

    // ── Build doomgeneric if missing or stale ───────────────────────────────
    //
    // Same rationale as BusyBox above: an external, from-scratch multi-file
    // C build (no incremental object cache of its own, unlike BusyBox's own
    // Makefile). Unlike BusyBox, though, our own platform port files get
    // edited in place — compare their mtimes against the output so those
    // edits actually make it into doom.elf (a from-scratch rebuild is only
    // a few seconds; upstream doomgeneric/ is a pinned submodule, so the
    // port files are the only inputs that change in practice).
    let doom_elf = embedded_dir.join(DOOM_ELF);
    let port_srcs = [
        workspace_root.join("doom-port/doomgeneric_constanos.c"),
        workspace_root.join("doom-port/doomgeneric_sound_constanos.c"),
    ];
    let doom_stale = !doom_elf.exists()
        || port_srcs.iter().any(|port_src| {
            match (doom_elf.metadata().and_then(|m| m.modified()),
                   port_src.metadata().and_then(|m| m.modified())) {
                (Ok(elf), Ok(src)) => src > elf,
                _ => true,
            }
        });
    if doom_stale {
        println!("cargo:warning=doom.elf missing/stale — building doomgeneric...");
        let status = Command::new("bash")
            .arg(workspace_root.join("scripts/build-doom.sh"))
            .current_dir(workspace_root)
            .status()
            .expect("Failed to spawn scripts/build-doom.sh");
        assert!(status.success(), "scripts/build-doom.sh failed");
    }

    // ── Build quakegeneric if missing or stale ──────────────────────────────
    //
    // Same rationale as doom.elf above: quakegeneric/ is a pinned
    // submodule, so quake-port/quakegeneric_constanos.c is the only input
    // that changes in practice — compare its mtime against the output.
    let quake_elf = embedded_dir.join(QUAKE_ELF);
    let quake_port_src = workspace_root.join("quake-port/quakegeneric_constanos.c");
    let quake_stale = !quake_elf.exists()
        || match (quake_elf.metadata().and_then(|m| m.modified()),
                  quake_port_src.metadata().and_then(|m| m.modified())) {
            (Ok(elf), Ok(src)) => src > elf,
            _ => true,
        };
    if quake_stale {
        println!("cargo:warning=quake.elf missing/stale — building quakegeneric...");
        let status = Command::new("bash")
            .arg(workspace_root.join("scripts/build-quake.sh"))
            .current_dir(workspace_root)
            .status()
            .expect("Failed to spawn scripts/build-quake.sh");
        assert!(status.success(), "scripts/build-quake.sh failed");
    }
}
