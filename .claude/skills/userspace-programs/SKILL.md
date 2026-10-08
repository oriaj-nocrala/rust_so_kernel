---
name: userspace-programs
description: Playbook for adding or changing a userspace program in rust_so_kernel (Rust in userspace/src/bin, C in userspace/c against mlibc, BusyBox, DOOM/Quake ports), choosing embedded vs disk-resident, getting it onto disk.img, and changing the mlibc port (sysdeps, ABI headers, patches) without leaving stale static binaries. Use when adding a program or test program, editing kernel/build.rs program lists, editing mlibc-port/, or changing busybox-config. Keywords: add program, DISK_C_PROGRAMS, DISK_RUST_PROGRAMS, PROGRAMS, include_bytes, disk.img, mlibc, sysroot, setup-mlibc.sh, abi-bits, busybox.elf.
---

# Userspace programs and the mlibc port

How it is wired (embedded vs disk, exec, BusyBox, mlibc's quirks): `docs/reference/userspace.md`.

## Adding a program

**Default: disk-resident.** Embed only what is needed to reach and debug a shell.

| Kind | Disk-resident (default) | Embedded |
|------|-------------------------|----------|
| Rust (`userspace/src/bin/<name>.rs`) | add to `DISK_RUST_PROGRAMS` in `kernel/build.rs` | add to `RUST_PROGRAMS` **and** to `PROGRAMS` in `kernel/src/process/user_programs.rs`: `("name", ProgramSource::Elf(include_bytes!("../../embedded/name.elf")))` |
| C (`userspace/c/<name>.c`) | add to `DISK_C_PROGRAMS` | add to `C_PROGRAMS` + `PROGRAMS` |
| Rust with std (a crate at the repo root) | add `("<crate dir>", "<bin>")` to `STD_PROGRAMS` in `kernel/build.rs`, and the crate dir to the `gui-client` watch loop in the root `build.rs` | — |
| external build (like doom/quake) | a build script + a `*_NAME` block in `kernel/build.rs` writing `disk-image-root/bin/<name>` | — |

- Disk-resident programs land in `disk-image-root/bin/`. The root `build.rs` syncs them into `disk.img`'s `/bin` on every build (`sync_disk_bin_dir`, `debugfs rm`+`write`, with sizes verified afterwards), and they run as `/mnt/bin/<name>` through `$PATH`. On the Ryzen, `scripts/sync-usb-data.sh` copies them to the stick.
- Other data for `/mnt` (fonts, terminfo, `etc/gui/apps`, WADs) goes under `disk-image-root/`, synced the same way.
- **Rust**: `userspace::entry!(main)` with `fn main(args: Args) -> i32`, never a bare `_start`. Anything that links `userspace::text` (+1.2 MB) must be disk-resident.
- **Rust with std**: an ordinary `x86_64-unknown-linux-musl` program (static-pie; the kernel speaks Linux's ABI, `linux-abi` skill). Its crate is its own workspace (`[workspace]` table, `panic = "abort"` + `strip = true` in its release profile; add `/<crate>/target/` to `.gitignore`), so `cd <crate> && cargo test` runs its host tests. `kernel/build.rs` runs `cargo build --release --target x86_64-unknown-linux-musl --bin <bin>` in it (from inside the repo: only the pinned nightly has the musl std). Calls std lacks (`sendmsg` with fds, `poll`, `memfd_create`, `mmap`) are `extern "C"` declarations, no `libc` crate (`gui-client/src/sys.rs`). A window: the `gui-client` crate (`docs/reference/graphics.md`). Example: `gui-client`'s `hello-window`.
- **C**: clang against `sysroot/` (built by `scripts/setup-mlibc.sh` if missing), statically linked. Name test programs `<thing>_test`.
- Build, then run it in QEMU (`qemu-debug` skill) and check the output in serial.log.

## Rebuilding things that only build when missing

BusyBox (`kernel/embedded/busybox.elf`) and ncurses (`build-libtinfo/prefix/lib/lib{tinfo,ncurses}w.a`) are built **only if missing**. DOOM/Quake are rebuilt only if missing or if their port file is newer. Every one of them statically links mlibc, so after changing sysroot headers, mlibc sysdeps or `busybox-config/minimal.config`:

```bash
rm -f kernel/embedded/busybox.elf disk-image-root/bin/doom disk-image-root/bin/quake
rm -rf build-libtinfo/prefix        # only if the change affects ncurses (terminfo, termios, …)
cargo build
```

After editing `busybox-config/minimal.config`, check it still equals what `make oldconfig` produces (diff the two).

## Changing mlibc

1. **Never edit the `mlibc/` submodule checkout**: `git submodule update` resets it. Change it through one of:
   - files copied into it: `mlibc-port/constanos-sysdeps/` (sysdeps in `generic/generic.cpp`, headers in `include/`, `abi-bits/`);
   - small upstream fixes: a string patch in `scripts/setup-mlibc.sh`. It must be idempotent (a `grep` check, a Python replace, and a hard error if the upstream text stops matching; the `do_scanf` patch is the example);
   - larger upstream fixes: a unified diff in `mlibc-port/patches/*.patch`, applied after the string patches (skipped if it already reverses cleanly).
2. A declaration hidden behind mlibc's Linux-only option is moved out of the guard by `setup-mlibc.sh` (the `memfd_create`/`sched_getaffinity` pattern).
3. **ABI headers must match Linux** (the kernel uses Linux numbers). Check every value against `mlibc/abis/linux/` and the kernel's own constants; prefer adopting Linux's whole header over patching single constants. termios is intentionally *not* Linux's.
4. Run `scripts/setup-mlibc.sh`, delete the static binaries above, then `cargo build`.
5. Run the C tests that touch what you changed.
