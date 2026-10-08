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

## 2. Dynamic linking (1 session of planning, 1-2 to implement)

Use musl's `ld.so`; the kernel side is small but three things are missing:
1. **`PT_INTERP`**: the loader refuses it today (`docs/reference/memory.md`). Load the
   interpreter (ET_DYN) at a base, pass `AT_BASE`/`AT_PHDR`/`AT_PHNUM`/`AT_ENTRY`, enter it.
2. **File-backed `mmap(MAP_PRIVATE)`**: only anonymous and memfd mappings exist. A first
   version may copy the file into private pages at map time (no page cache).
3. **`mmap` address hints**: a nonzero `addr` is treated as `MAP_FIXED` today; Linux treats
   it as a hint unless `MAP_FIXED`/`MAP_FIXED_NOREPLACE`. `ld.so` relies on that.

Gate for most non-static Linux binaries (and for step 3).

Scope: dynamic linking is for **running foreign Linux binaries**. constanos-native and generated
apps stay **static** (`docs/ai/software-on-demand.md`): nothing is resolved by name at install
or load time (no room for slopsquatting), and an app keeps running unchanged for decades
(P3.4, P6 in `docs/ux/principles.md`).

## 3. Claude Code (scope first, then decide)

A Bun-compiled binary: needs dynamic linking, JIT (W+X memory: `PROT_EXEC` is ignored and NX is
off today, so it may work by accident), threads, `epoll`, many more syscalls, TLS to the API,
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
