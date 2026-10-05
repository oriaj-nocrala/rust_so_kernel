# Userland roadmap: HTTPS, dynamic linking, Claude Code, a browser

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
