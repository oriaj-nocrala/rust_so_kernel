extern crate ovmf_prebuilt;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    // set by cargo, build scripts should use this directory for output files
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());

    let kernel = build_kernel();

    // Prefer OVMF firmware already installed on the system (e.g. the
    // `edk2-ovmf` Arch package) — no network required. Only fall back to
    // downloading a prebuilt copy if nothing is found locally, since that
    // fetch has no timeout and can hang indefinitely in restricted-network
    // environments.
    let (ovmf_code, ovmf_vars) = find_system_ovmf().unwrap_or_else(|| {
        match try_get_ovmf() {
            Ok((code, vars)) => (code, vars),
            Err(e) => panic!(
                "No OVMF firmware found on the system and the network fetch failed: {}.\n\
                 Install one, e.g. on Arch: sudo pacman -S edk2-ovmf",
                e
            ),
        }
    });

    // QEMU opens the VARS file read-write (it stores UEFI variables across
    // boots) — the system copy is root-owned and not writable, so work on a
    // private copy in OUT_DIR instead of pointing QEMU at it directly.
    let ovmf_vars_writable = out_dir.join("OVMF_VARS.fd");
    std::fs::copy(&ovmf_vars, &ovmf_vars_writable)
        .expect("Failed to copy OVMF_VARS to a writable location");
    let ovmf_vars = ovmf_vars_writable;

    // create an UEFI disk image (optional)
    let uefi_path = out_dir.join("uefi.img");
    bootloader::UefiBoot::new(&kernel).create_disk_image(&uefi_path).unwrap();

    let disk_image = ensure_ext2_disk_image();

    // pass the disk image paths as env variables to the `main.rs`
    println!("cargo:rustc-env=UEFI_PATH={}", uefi_path.display());
    println!("cargo:rustc-env=OVMF_CODE={}", ovmf_code.display());
    println!("cargo:rustc-env=OVMF_VARS={}", ovmf_vars.display());
    println!("cargo:rustc-env=EXT2_DISK_PATH={}", disk_image.display());
}

/// Create `disk.img` (repo root) — a small ext2 filesystem, seeded from
/// `disk-image-root/` — if it doesn't already exist. Attached by
/// `src/main.rs` to QEMU's secondary IDE channel; read by the kernel's
/// `fs::ext2` driver, mounted at `/mnt`.
///
/// Deliberately created ONCE, never regenerated: the whole point is a disk
/// that persists across separate `cargo run` invocations (today: to prove
/// the read path works against a real, persistent image; later, once
/// write support exists, to actually keep written data). Delete the file
/// yourself to reset it.
fn ensure_ext2_disk_image() -> PathBuf {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let disk_path = manifest_dir.join("disk.img");
    let seed_dir = manifest_dir.join("disk-image-root");

    if disk_path.exists() {
        return disk_path;
    }

    println!("cargo:warning=disk.img missing — creating a 16MiB ext2 image seeded from disk-image-root/...");

    let status = Command::new("dd")
        .args(["if=/dev/zero", "bs=1M", "count=16"])
        .arg(format!("of={}", disk_path.display()))
        .status()
        .expect("Failed to spawn dd for disk.img");
    assert!(status.success(), "dd failed to create disk.img");

    // -O ^resize_inode,^dir_index: keep the on-disk layout as close to
    // "vanilla" ext2 as possible — this kernel's fs::ext2 reader is a
    // from-scratch minimal implementation, not a full ext2 stack, and
    // there's no reason to exercise features it doesn't need to.
    let status = Command::new("mke2fs")
        .args(["-q", "-t", "ext2", "-b", "1024", "-O", "^resize_inode,^dir_index"])
        .arg("-d").arg(&seed_dir)
        .arg(&disk_path)
        .status();

    match status {
        Ok(s) if s.success() => {}
        Ok(_) | Err(_) => {
            // Don't fail the whole build over this — /mnt just won't be
            // there (fs::ext2::init() logs and continues). Remove the
            // half-formed image so the next build retries cleanly instead
            // of picking up a zeroed, non-ext2 file forever.
            let _ = std::fs::remove_file(&disk_path);
            println!(
                "cargo:warning=mke2fs not found or failed — /mnt won't be available. \
                 Install e2fsprogs (e.g. `sudo pacman -S e2fsprogs`) and rebuild to get it."
            );
        }
    }

    disk_path
}

/// Builds the kernel crate for the bare-metal `x86_64-unknown-none` target
/// and returns the path to the resulting ELF binary.
///
/// This shells out to a nested `cargo build` (mirroring the pattern
/// kernel/build.rs already uses to build userspace/) instead of using
/// cargo's `artifact-dependency` feature (`bindeps` + `-Z build-std`):
/// that combination panics inside cargo itself ("no entry found for key" in
/// unit_dependencies.rs) on every nightly tested — a known upstream
/// limitation, not something fixable from this repo's config.
fn build_kernel() -> PathBuf {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let kernel_dir = manifest_dir.join("kernel");

    println!("cargo:rerun-if-changed={}", kernel_dir.join("src").display());
    println!("cargo:rerun-if-changed={}", kernel_dir.join("Cargo.toml").display());
    println!("cargo:rerun-if-changed={}", kernel_dir.join(".cargo/config.toml").display());
    println!("cargo:rerun-if-changed={}", kernel_dir.join("build.rs").display());
    println!("cargo:rerun-if-changed={}", kernel_dir.join("embedded").display());

    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());

    // --target-dir is explicit (rather than relying on the default) because
    // `kernel` is also a workspace member of the root package, which would
    // otherwise unify its output into the *root* target/ dir instead of
    // kernel/target/.
    let kernel_target_dir = kernel_dir.join("target");

    let mut cmd = Command::new(&cargo);
    cmd.current_dir(&kernel_dir)
        .arg("build")
        .arg("--target")
        .arg("x86_64-unknown-none")
        .arg("--target-dir")
        .arg(&kernel_target_dir)
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_BUILD_TARGET")
        .env_remove("CARGO_TARGET_DIR");
    if profile == "release" {
        cmd.arg("--release");
    }

    let status = cmd.status().expect("Failed to spawn cargo for kernel build");
    assert!(status.success(), "Kernel build failed");

    kernel_target_dir
        .join("x86_64-unknown-none")
        .join(&profile)
        .join("kernel")
}

/// Looks for OVMF_CODE/OVMF_VARS in the usual distro install locations.
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

fn try_get_ovmf() -> Result<(PathBuf, PathBuf), Box<dyn std::error::Error>> {
    use ovmf_prebuilt::{Arch, FileType, Source, Prebuilt};
    
    let prebuilt = Prebuilt::fetch(Source::LATEST, "target/ovmf")?;
    let code = prebuilt.get_file(Arch::X64, FileType::Code);
    let vars = prebuilt.get_file(Arch::X64, FileType::Vars);
    Ok((code, vars))
}