# Userspace: programs, BusyBox, mlibc, games

How to add a program or change mlibc/BusyBox: the `userspace-programs` skill. This file describes how things are wired.

## Where programs live

- **Embedded** (in the kernel ELF, `/bin` in initramfs, `PROGRAMS` in `kernel/src/process/user_programs.rs`, `include_bytes!` from `kernel/embedded/`): only what is needed to reach a shell and debug it. That is `shell` (PID 1), `busybox`, `kdebug`, `reboot`, and the small Rust programs in `RUST_PROGRAMS` (smoke tests, `sse_test`, `userlib_test`, `term`).
- **Disk-resident** (`disk-image-root/bin/` → `/mnt/bin` on `disk.img` or the stick): everything else, including `doom`, `quake`, the compositor, `panel`, `cpumon`, `textdemo`, and the C test programs (`*_test`).
- `PATH=/tmp/bin:/bin:/mnt/bin`: BusyBox applet symlinks first, then embedded programs, then the disk.
- Every ELF (not the kernel) is stripped by `kernel/build.rs`. The loader only reads PT_LOAD, and the kernel's own symbols are needed for gdb.

## exec

- `sys_exec` resolves the path through the real VFS: normalize against cwd, follow symlinks by hand (8 hops, then `ELOOP`), open, read the whole file, load it.
- `/proc/self/exe`, relative paths and `$PATH` candidates all go through this one path.
- Process `name` = basename as passed (before symlinks); `exe_name` = the resolved path; `cmdline` = argv. So BusyBox applets re-executed through `/proc/self/exe` show as `exe`, as on Linux.
- `ProgramSource::RawCode` (inline asm tests in `process/user_test_fileio.rs`) is a fallback used when no ELF exists.

## Rust programs (`userspace/`, own Cargo workspace)

- Target `userspace/x86_64-constanos.json`: `x86_64-unknown-none` with **SSE2** (the kernel itself stays soft-float), built with `-Zjson-target-spec`.
- **Entry through `userspace::entry!(main)`**, with `fn main(args: Args) -> i32`. A plain `extern "C" fn _start` starts with a misaligned stack, and SSE's `movaps` then #GPs.
- `userspace::heap` is the global allocator: size classes up to 64 KiB from 1 MiB `mmap` chunks, and one `mmap` per larger block, freed with `munmap` of the exact length.
- `userspace::syscall`: raw wrappers (memfd, shared mmap, ioctl, epoll, `send_fds`/`recv_fds`, `reap_any`).
- `userspace::gfx`, `userspace::text`, `userspace::launch`: see `graphics.md`.
- There is no dynamic linking: shared code is a statically linked crate (`draw`, `text`, `gui`, `vt`, `sched`).

## C programs (`userspace/c/`)

- Compiled with clang against `sysroot/` (mlibc, built by `scripts/setup-mlibc.sh`). Linked static: `crt1.o` + `libc.a`, with `-nostdlib`.
- Headers of our own in `userspace/c/include/`: `constanos_gfx.h`, `constanos_gui_wire.h`.
- **Without `crtbegin.o`, `__cxa_finalize` is never called**, so `generic.cpp` has a `[[gnu::destructor]]` that calls it. It is what flushes stdio at `exit()` when stdout is a file or a pipe.

## BusyBox (`busybox/` submodule, `busybox-config/minimal.config`)

- Built by `scripts/build-busybox.sh` **only when `kernel/embedded/busybox.elf` is missing**. Always embedded.
- PID 1 runs `busybox --install -s /tmp/bin` (real `symlink(2)` calls), then `ash`, and respawns `ash` when it exits.
- Enabled: `ash` with job control and line editing, standalone applets and NOFORK, `vi` (`FEATURE_VI_WIN_RESIZE`, which makes it query `TIOCGWINSZ`), the usual text tools, `tar`/`gzip`, `ps`/`top` with per-CPU `%CPU`, `df`, `free`, `uptime`, `nproc`, `script`/`stty`, and `CONFIG_DESKTOP` (POSIX `ps -o`, full `find`, `ls -l` dates, `touch -d/-t`).
- The target does not define `__linux__` (and must not: BusyBox has ~250K lines of `#ifdef __linux__`). The compiler wrapper passes `-include sys/sysinfo.h` instead, for `free`/`uptime`.

## mlibc (`mlibc/` submodule, our port in `mlibc-port/constanos-sysdeps/`)

- `scripts/setup-mlibc.sh` copies the port into the submodule, applies string patches plus `mlibc-port/patches/*.patch`, and builds `sysroot/`.
- **ABI headers** (`abi-bits/*.h`) were copied from non-Linux ports and have been wrong many times (`SEEK_SET`, `O_CREAT`, `WIFEXITED`, `errno.h`, `socket.h`, `resource.h`, `fcntl.h` `AT_*`, …). `socket.h`, `errno.h` and `resource.h` are now Linux's own. Still unchecked (see memory `posix-compat-pending`): signal, wait, vm-flags, limits, poll, epoll. termios is intentionally *not* Linux's.
- Upstream bugs patched here:
  - `do_scanf`: `%*s` stopped matching, and integer field widths were ignored;
  - `asctime_r`: a missing `:`;
  - `pause()`: a missing sysdep.
- Sysdeps we added (in `generic/generic.cpp`):
  - memfd, ftruncate, a `vm_map` that passes flags/fd/offset through;
  - access, symlink, link/linkat, chmod, statvfs;
  - the id family (`getuid`, `setuid`, `setresuid`, `getgroups`, `setgroups`, …): they call the kernel (`process::creds`);
  - times, getrusage, clock_getres, `sched_getaffinity`, `sysinfo`;
  - `sysconf`: `_SC_CLK_TCK` 100, CPUs from the affinity mask, `_SC_OPEN_MAX` 16, physical pages.
- Userspace-only stubs: uid/gid are always 0; `uname`/`gethostname` are per-process statics; `getmntent` reads a compiled-in mount table.

## DOOM and Quake

- **DOOM**: `doomgeneric/` submodule + `doom-port/`, built by `scripts/build-doom.sh`. WAD: `scripts/fetch-freedoom.sh` → `/mnt/freedoom1.wad`.
  - Sound effects only (no music), through `/dev/dsp`. An empty `stub-include/SDL_mixer.h` satisfies `i_sound.c`.
  - Mouse deltas are passed through unnegated (the PS/2 convention matches).
- **Quake**: `quakegeneric/` submodule + `quake-port/`, built by `scripts/build-quake.sh`. `scripts/fetch-quake-shareware.sh` → `/mnt/id1/pak0.pak` (shareware only).
  - 8bpp paletted → RGB conversion in the port.
  - Its own `S_*` sound mixer; the WAV parser bounds-checks every chunk.
  - The heap is raised to 64 MiB by a build-script patch, and the zone to `-zone 8192`.
- Both are rebuilt when the output is missing or the port file is newer. Both use `FBIO_BLIT` on the console, or a window under the compositor.
- A game must drain `/dev/input/event0` at startup: the ring fills from boot.
- Never edit a submodule checkout: patch it from its build script.

## ABI header fixes (networking)

- `abi-bits/in.h` is Linux's (`mlibc/abis/linux/in.h`, copied into `mlibc-port/constanos-sysdeps/include/abi-bits/`). The port used to ship mlibc's generic table, which numbered the protocols 1..8 (`IPPROTO_TCP` 5, `UDP` 6, `ICMP` 3, `IP` 1): every program passing them to `socket`/`setsockopt` was wrong. Re-check any new `abi-bits` header the same way: diff it against `mlibc/abis/linux/`.
