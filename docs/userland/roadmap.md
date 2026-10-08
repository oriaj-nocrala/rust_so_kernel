# Userland roadmap: HTTPS, dynamic linking, Claude Code, a browser, one libc

Agreed order (2026-10-05), shortest and most enabling first. Each step is useful on its own
and unblocks the next. GPU work is out of this list (it depends on the Ryzen).

## 1. HTTPS client (1-2 sessions) — next

- Today only HTTP: BusyBox `wget` has no TLS (`docs/reference/net.md`).
- Plan: a static `x86_64-unknown-linux-musl` Rust program (`ureq` + `rustls` + `webpki-roots`),
  e.g. `fetch https://...`. Musl Rust std already runs (`linux-abi` skill), `getrandom` exists
  (`docs/reference/syscalls.md`). No OpenSSL port to mlibc needed.
- What it also proves: smoltcp TCP under large transfers (barely exercised so far), and that
  the RTC time is right (certificate validity).
- Test in QEMU (user network) first, then on the Ryzen (RTL8168, gigabit verified).

## 2. Dynamic linking (reviewed 2026-10-08: 3-5 sessions, after NX)

Linking policy (`docs/ux/principles.md` P3.4, P6): **static, except the platform.**

| Layer | Linking | Why |
|---|---|---|
| Platform: musl, the Vulkan driver (NVK), later a UI library with a C interface | **dynamic**, immutable, content-addressed, part of a system generation (B4) | used by many processes at once (the exception Torvalds grants: libc and core GUI libraries); one fix reaches every app; pinned by hash, so no DLL hell and old generations keep their libs |
| Native apps, including generated ones | **static** except the platform | one-off libraries gain nothing shared (DeVault: over half of a distro's libraries serve < 0.1% of executables) |
| three.js apps | n/a | scripts on one runtime |
| Plugins (audio, effects, formats) | **out of process** | Bitwig sandboxes plugins; Apple loads AUv3 out of process by default; P6. `dlopen` only for trusted platform pieces (the Vulkan ICD) |
| Foreign Linux binaries | dynamic through musl's `ld.so` | compatibility |

Measured today: six Vulkan programs in `disk-image-root/bin` are ~16 MB each (`vk_comp` 19 MB),
almost all of it the same statically linked NVK: ~95 MB on disk, and each running copy loads its
own code; an NVK fix means rebuilding every Vulkan app. Rust has no stable ABI between crates,
so a shared Rust library (e.g. the text engine) needs a C (`cdylib`) facade or a service.

### What the kernel lacks (checked in the code 2026-10-08)

1. **`PT_INTERP`**: `elf_loader.rs` refuses it. Load the interpreter (ET_DYN) at its own base,
   not `PIE_BASE`, and enter it. **auxv lacks `AT_BASE`** (it has `AT_PHDR/PHENT/PHNUM/ENTRY/
   PAGESZ/RANDOM`): add it (and `AT_HWCAP`, `AT_SECURE`=0 cheaply).
2. **File-backed `mmap`**: `sys_mmap` returns `EINVAL` for `MAP_PRIVATE` with an fd, and
   `MAP_SHARED` works only on memfd/shm objects. `ld.so` maps each segment from the file with
   an offset, `MAP_PRIVATE`.
3. **Address hints and `MAP_FIXED`**: a nonzero `addr` is treated as fixed and fails on overlap
   (`address_space.rs`, "MAP_FIXED conflict"). Linux: without `MAP_FIXED` it is a hint (pick
   elsewhere); with `MAP_FIXED` it **replaces** what is there (`ld.so` reserves the whole span,
   then maps each segment over it with `MAP_FIXED`); `MAP_FIXED_NOREPLACE` fails with `EEXIST`.
4. **`PROT_EXEC` and RELRO**: `ld.so` maps code with `PROT_EXEC` and `mprotect`s RELRO
   read-only. NX is done (handoff stage 2): `PROT_EXEC` is honoured, so W^X holds from the
   first dynamic binary.
5. **A page cache for shared code**: there is none. Today `exec` copies every segment into fresh
   frames, so two runs of the same program share nothing, and a "copy the file into private
   pages at map time" `mmap` would give dynamic linking all its costs and none of the memory
   benefit. Scoped design: share pages **only for immutable files** (the platform's
   content-addressed store, see "Layout"): no coherence with writes to solve, COW on a private
   write. Mutable files keep the copy-at-map path. `exec` of a store binary can use the same
   cache, which also shares the text of two instances of one static program.

### Layout

- A store per generation: `/system/<hash>-<name>-<version>/lib/...` (immutable; the hash
  pins, the name and version keep it readable, as Nix does). Native dynamic binaries carry
  `PT_INTERP` and `DT_RUNPATH` into the store path they were built and tested against (as Nix
  does), so an app is pinned to exact library versions and a new generation can't break it.
- `/lib/ld-musl-x86_64.so.1` (the path foreign musl binaries expect) is a symlink into the
  current generation's store.

### Observability (P1, `docs/ux/principles.md`)

- **Missing interpreter or library:** Linux returns `ENOENT` from `exec` when the *interpreter*
  is missing ("No such file" for a file that exists), and `ld.so` reports a missing library on
  stderr, lost when the panel launched the app. Here the `exec` failure carries its cause
  (the missing interpreter or library, backbone B2), and the launcher keeps each app's stderr
  and exit status and shows them when a launch fails.
- **What an app is linked against** is visible in the inspector (a `ldd` with names and
  versions from the store paths).
- **Shared pages are attributed** proportionally and by library name (B2), never as "other".
- **Pinned to a flawed version:** when a platform library version has a known problem, apps
  still pinned to it say so and offer to try the current generation; staying pinned is a
  visible choice.

### Steps

1. ~~NX + `PROT_EXEC`~~ (done, handoff stage 2). 2. `mmap`: hints, `MAP_FIXED` replace,
`MAP_FIXED_NOREPLACE`, file-backed `MAP_PRIVATE` by copy. 3. `PT_INTERP` + `AT_BASE`: a C
program dynamically linked against musl runs; `dlopen` of a test `.so` works. 4. Immutable store
+ page cache for it; measure: two processes mapping the same library share frames (a
`/proc/kdebug` counter), sabotage the sharing and see the counter drop. 5. Platform libs:
musl's `libc.so`, then Mesa/NVK as a shared ICD (the mesa-port builds static today; decide
between the Khronos loader and linking `libvulkan_nouveau.so` directly), then rebuild the
Vulkan apps against it and measure disk and RAM again.

Tests: raw C tests per syscall change (`linux-abi` skill), proven by sabotage; an unmodified
dynamically linked Alpine binary (e.g. its `busybox`) as the compatibility gate.

## 3. Claude Code (scope first, then decide)

A Bun-compiled binary: needs dynamic linking, JIT (W+X memory: needs `PROT_EXEC` asked for explicitly,
now that NX is on), threads, `epoll`, many more syscalls, TLS to the API,
and a terminal that handles its TUI. First, cheap step: on the Linux host,
`strace -f -c -o claude-syscalls.txt claude --version` (and a short interactive run), then diff
the list against `docs/reference/syscalls.md`. Decide after seeing the gap.

## 4. Browser, in steps

1. Text: `links` or `lynx` with TLS, once step 1 settles the TLS story for C (or link a C TLS
   library: BearSSL/mbedTLS against mlibc).
2. `links -g`: graphics mode drawing on the framebuffer, no toolkit.
3. NetSurf (framebuffer frontend) for something closer to a real browser.
Chromium/Firefox: out of reach for a long time.

## 5. One libc: musl only, mlibc retired gradually (not started)

Why (2026-10-07):
- The kernel speaks Linux's ABI (G1 closed: `linux-abi` skill), so unmodified musl binaries run.
  mlibc was the right choice for a kernel with its own ABI; it no longer is.
- Two libcs today: mlibc (+ the `constanos-sysdeps` port) for the 67 C programs in
  `userspace/c/`, BusyBox, ncurses, DOOM, Quake; static musl for Rust std programs and
  Mesa/NVK (`docs/gpu/phase7-3d-decision.md`, G3). Step 2 (dynamic linking) uses musl's `ld.so`.
- mlibc's ABI headers differing from Linux caused many bugs (`docs/reference/userspace.md`), and
  some still differ. **termios is the live one**: the kernel uses the port's own layout
  (`kernel/src/tty.rs`, `docs/reference/syscalls.md`), so a musl program's `tcgetattr` gets
  the wrong struct today.
- BusyBox is built without `__linux__` (`docs/reference/userspace.md`); with musl it gets its
  normal Linux code paths.

Order (each step leaves everything working):
1. **Kernel to Linux's layout where it still follows mlibc** (termios/winsize first), switching
   the mlibc port's matching `abi-bits` to `mlibc/abis/linux/` in the same change, so both libcs
   agree while they coexist. Audit the rest of the port's headers against `mlibc/abis/linux/`.
2. **A musl sysroot for C** (clang `--target=x86_64-linux-musl`, static), next to `sysroot/`.
   Move the raw-syscall C tests first (`scripts/run-abi-suite.sh` must stay green on both).
3. **BusyBox on musl** with a standard config (`__linux__` defined). Expect missing syscalls and
   `/proc` files: that is useful G1-style work, not a reason to stop.
4. ncurses, DOOM, Quake, cmatrix, the GUI C headers (`gui-capi`, `constanos_gui*.h`).
5. Remove mlibc: the submodule, `mlibc-port/`, `scripts/setup-mlibc.sh`, `sysroot/`, and the
   kernel code that exists only for it (grep `mlibc` in `kernel/src`: e.g. the `TCGETS`-with-null
   `isatty` path in `syscall/fs.rs`). Update the `userspace-programs` skill and docs.

6. **Then the Rust no_std programs** in `userspace/src/bin` (target `x86_64-constanos.json`, raw
   syscalls, no libc) move to Rust std on musl too (decided 2026-10-07), so one toolchain and one
   ABI cover all of userspace.
The upstream mlibc PR (managarm/mlibc#1925) is unaffected.
