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
    sync_disk_bin_dir(&disk_image);

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

    // Freedoom's IWAD (~29MB, see scripts/fetch-freedoom.sh) is seeded into
    // this same image — fetch it first so mke2fs picks it up below.
    println!("cargo:rerun-if-changed={}", manifest_dir.join("scripts/fetch-freedoom.sh").display());
    if !seed_dir.join("freedoom1.wad").exists() {
        println!("cargo:warning=freedoom1.wad missing — downloading Freedoom...");
        let status = Command::new("bash")
            .arg(manifest_dir.join("scripts/fetch-freedoom.sh"))
            .current_dir(&manifest_dir)
            .status()
            .expect("Failed to spawn scripts/fetch-freedoom.sh");
        assert!(status.success(), "scripts/fetch-freedoom.sh failed");
    }

    // Same idea for Quake's shareware pak0.pak (~18MB) — see
    // scripts/fetch-quake-shareware.sh. Seeded to disk-image-root/id1/
    // (not the root) since that's the real on-disk layout Quake's own
    // COM_InitFilesystem expects (basedir/id1/pak0.pak).
    println!("cargo:rerun-if-changed={}", manifest_dir.join("scripts/fetch-quake-shareware.sh").display());
    if !seed_dir.join("id1/pak0.pak").exists() {
        println!("cargo:warning=id1/pak0.pak missing — downloading Quake shareware...");
        let status = Command::new("bash")
            .arg(manifest_dir.join("scripts/fetch-quake-shareware.sh"))
            .current_dir(&manifest_dir)
            .status()
            .expect("Failed to spawn scripts/fetch-quake-shareware.sh");
        assert!(status.success(), "scripts/fetch-quake-shareware.sh failed");
    }

    // 96MiB: freedoom1.wad (~29MB) + id1/pak0.pak (~18MB) alone are
    // ~47MB — the previous 48MiB image (sized back when freedoom1.wad was
    // the only large asset) would leave almost no headroom for ext2
    // metadata overhead or anything ext2_robust_test/regular use creates
    // afterward.
    println!("cargo:warning=disk.img missing — creating a 96MiB ext2 image seeded from disk-image-root/...");

    let status = Command::new("dd")
        .args(["if=/dev/zero", "bs=1M", "count=96"])
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

/// Sync `disk-image-root/bin/` (populated fresh on every build by
/// `kernel/build.rs` — see that file's module doc comment and
/// `DISK_C_PROGRAMS`/`DOOM_NAME`/`QUAKE_NAME`) onto `disk.img`'s `/bin`
/// directory, using `debugfs -w` to write directly into the existing
/// filesystem image.
///
/// This exists because `ensure_ext2_disk_image` above is deliberately
/// create-once: it returns the existing `disk.img` untouched if one is
/// already on disk, so `mke2fs -d disk-image-root` (which would otherwise
/// pick up `disk-image-root/bin/` automatically) never runs again after the
/// first `cargo build` in a checkout. Without this function, dropping new
/// binaries into `disk-image-root/bin/` would silently do nothing on any
/// tree that already has a `disk.img` — exactly the trap this was written
/// to close, since `cargo run` on an existing tree must still end up with
/// every disk-resident program actually reachable.
///
/// Three alternatives considered and rejected:
///   - Regenerating `disk.img` whenever the seed content looks newer would
///     defeat the entire point of `ensure_ext2_disk_image`'s create-once
///     design (a persistent image proving the ext2 *write* path survives
///     across `cargo run` invocations — see that function's doc comment) —
///     it would nuke any state a prior boot's ext2 test/session wrote.
///   - Requiring the developer to `rm disk.img` by hand is bad DX and,
///     more importantly, contradicts this task's own requirement that an
///     existing checkout's `cargo run` "must end up with the programs
///     actually reachable, not silently missing".
///   - `e2cp` (a smaller, more targeted tool for exactly this) isn't
///     installed on this dev machine; `debugfs -w` ships with the same
///     `e2fsprogs` package this build already requires for `mke2fs`, so it
///     adds no new dependency.
///
/// Idempotent by construction: `rm <name>` (ignored if the file doesn't
/// exist yet — the first sync on any given `disk.img`) then `write <host>
/// <name>` on every call, so a changed program's content is really
/// replaced, not just left as a stale first copy. `debugfs -f <script>`
/// exits 0 unconditionally (verified directly — even a completely missing
/// image or a `write` of a nonexistent host path still exits 0), so
/// success is checked for real afterward: `debugfs -R "ls -l /bin"` is
/// parsed and every synced file's on-disk size is compared against its
/// host-side size.
fn sync_disk_bin_dir(disk_path: &std::path::Path) {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let bin_seed_dir = manifest_dir.join("disk-image-root/bin");

    let Ok(read_dir) = std::fs::read_dir(&bin_seed_dir) else {
        // kernel/build.rs (which populates this dir) hasn't run, or
        // produced nothing — nothing to sync.
        return;
    };

    if !disk_path.exists() {
        // mke2fs was missing/failed above; ensure_ext2_disk_image already
        // warned about /mnt being unavailable. Nothing to sync into.
        return;
    }

    let mut entries: Vec<(String, PathBuf, u64)> = Vec::new();
    for entry in read_dir {
        let entry = entry.expect("reading disk-image-root/bin/ entry");
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name().into_string()
            .expect("non-UTF8 filename in disk-image-root/bin/");
        let size = entry.metadata().expect("stat disk-image-root/bin entry").len();
        entries.push((name, entry.path(), size));
    }
    if entries.is_empty() {
        return;
    }

    if Command::new("debugfs").arg("-V").output().is_err() {
        println!(
            "cargo:warning=debugfs (e2fsprogs) not found — cannot sync disk-image-root/bin/ \
             onto the existing disk.img. {} program(s) built there ({}) won't be reachable at \
             /mnt/bin until e2fsprogs is installed and this build reruns.",
            entries.len(),
            entries.iter().map(|(n, _, _)| n.as_str()).collect::<Vec<_>>().join(", "),
        );
        return;
    }

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let script_path = out_dir.join("sync_disk_bin.debugfs");
    let mut script = String::from("mkdir /bin\ncd /bin\n");
    for (name, host_path, _) in &entries {
        script.push_str(&format!("rm {name}\nwrite {} {name}\n", host_path.display()));
    }
    std::fs::write(&script_path, &script).expect("writing debugfs sync script");

    Command::new("debugfs")
        .arg("-w")
        .arg("-f").arg(&script_path)
        .arg(disk_path)
        .output()
        .expect("Failed to spawn debugfs");

    // Real verification, since debugfs's own exit code is not trustworthy
    // here (see doc comment above — it exits 0 even for a `write` of a
    // nonexistent host path or a completely unopenable image): re-list
    // `/bin` with a fresh `-R` invocation and check every synced file
    // actually landed at its expected size.
    let relist = Command::new("debugfs")
        .arg("-R").arg("ls -l /bin")
        .arg(disk_path)
        .output()
        .expect("Failed to spawn debugfs for verification");
    let relisting = String::from_utf8_lossy(&relist.stdout);

    let missing_or_wrong: Vec<&str> = entries.iter()
        .filter(|(name, _, expected_size)| {
            // debugfs `ls -l` line shape:
            // "  <ino>  <mode> (<x>)  <uid> <gid> <size> <date...> <name>"
            !relisting.lines().any(|line| {
                let mut fields = line.split_whitespace();
                let size_field = fields.nth(5); // ino, mode, (x), uid, gid, size
                let name_field = line.trim_end().rsplit(char::is_whitespace).next();
                name_field == Some(name.as_str())
                    && size_field == Some(expected_size.to_string().as_str())
            })
        })
        .map(|(name, _, _)| name.as_str())
        .collect();

    if !missing_or_wrong.is_empty() {
        panic!(
            "sync_disk_bin_dir: failed to sync {:?} onto {}:/bin (debugfs `ls -l /bin` follows)\n{}",
            missing_or_wrong,
            disk_path.display(),
            relisting,
        );
    }

    println!(
        "cargo:warning=synced {} userspace program(s) into disk.img:/bin ({})",
        entries.len(),
        entries.iter().map(|(n, _, _)| n.as_str()).collect::<Vec<_>>().join(", "),
    );
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
/// Emit `cargo:rerun-if-changed` for every file under `dir`, recursively.
///
/// Same helper (and same lesson) as kernel/build.rs: a directory-level
/// `rerun-if-changed` only notices files being added/removed — editing an
/// existing file in place doesn't touch the directory's mtime, so this
/// build script silently skips rerunning and QEMU boots a stale image
/// (confirmed live: an edit to doom-port/doomgeneric_constanos.c alone
/// produced a 0.04s "Finished" no-op build). `target`/`.git` are outputs,
/// not inputs — watching them would dirty every build.
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

fn build_kernel() -> PathBuf {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let kernel_dir = manifest_dir.join("kernel");

    // Every *input* of the nested kernel build (which itself builds all
    // userspace) must be watched from up here too — the nested build only
    // runs at all if this script reruns. NOT kernel/embedded: those files
    // are rewritten by the nested build on every run (outputs, not
    // inputs), so watching them would make this script permanently dirty.
    watch_dir_recursive(&kernel_dir.join("src"));
    watch_dir_recursive(&manifest_dir.join("userspace/src"));
    watch_dir_recursive(&manifest_dir.join("userspace/c"));
    watch_dir_recursive(&manifest_dir.join("mlibc-port"));
    watch_dir_recursive(&manifest_dir.join("doom-port"));
    watch_dir_recursive(&manifest_dir.join("quake-port"));
    watch_dir_recursive(&manifest_dir.join("scripts"));
    watch_dir_recursive(&manifest_dir.join("busybox-config"));
    println!("cargo:rerun-if-changed={}", kernel_dir.join("Cargo.toml").display());
    println!("cargo:rerun-if-changed={}", kernel_dir.join(".cargo/config.toml").display());
    println!("cargo:rerun-if-changed={}", kernel_dir.join("build.rs").display());

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