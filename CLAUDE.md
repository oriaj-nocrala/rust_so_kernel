# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build and Run

Requires Rust **nightly** toolchain (set in `rust-toolchain.toml`). Requires `qemu-system-x86_64` installed.

```bash
# Build + launch in QEMU (UEFI, 512 MB RAM, serial to stdout)
cargo run

# Build only (kernel binary + disk image)
cargo build

# Build the kernel crate alone (bare-metal target)
cd kernel && cargo build --target x86_64-unknown-none
```

The top-level `cargo run` builds the kernel ELF, wraps it in a UEFI disk image via the `bootloader` crate, then spawns `qemu-system-x86_64`. Serial output appears in the terminal. The kernel ELF is built by `build.rs` shelling out to a **nested** `cargo build` (not cargo's artifact-dependency/`bindeps` feature) — see `build_kernel()` in the root `build.rs` for why (`bindeps` + `-Z build-std` panics inside cargo itself on every nightly tested).

**A root `cargo build` does NOT verify the eight extracted crates.** `watch_dir_recursive` in the root `build.rs` (call sites at `build.rs:351-358`) watches `kernel/src`, `userspace/{src,c}`, `mlibc-port`, `doom-port`, `quake-port`, `scripts`, and `busybox-config` — but **not** `hal/`, `ext2/`, `mm/`, `vfs/`, `diag/`, `sched/`, `usock/`, or `tty/`. Editing any of those changes none of `build.rs`'s declared `rerun-if-changed` inputs, so cargo skips the build script entirely and its nested kernel build never runs: the root `cargo build` returns exit 0 without compiling the edited crate at all. Measured, not reasoned — appending a syntax error to `sched/src/core.rs` gave root `cargo build` exit **0** while `cd kernel && cargo build --target x86_64-unknown-none` gave exit **101**. **Verify changes to those crates with the `cd kernel` form** (or `touch build.rs` first); the same "a 0.03s build proves nothing" rule already documented for stale embedded ELFs applies here with a different cause.

### Headless interactive debugging (no display/keyboard)

For non-interactive sessions (agents, CI) that need to type into the shell and read output — not just watch `cargo run`'s serial stream — use `scripts/qemu-debug.sh` instead of hand-building a `qemu-system-x86_64` command line or a one-off key-sending script. It wraps the whole flow: headless boot (`-display none`, serial to a log file, monitor over a unix socket, `-d int` exception trace), a `sendkey`-based `send "text"` that maps characters to QEMU keynames (including shift-combos) and paces them so the PS/2 ISR doesn't drop events, plus `screendump`/`log`/`wait-for`. See the script's header comment for the full subcommand list and an example session. Background/monitor-socket gotchas are recorded in the `debugging-technique-qemu-monitor` memory.

```bash
scripts/qemu-debug.sh start                                    # cargo build + launch headless
scripts/qemu-debug.sh wait-for "About to start first process"  # poll serial.log instead of a blind sleep
scripts/qemu-debug.sh send "busybox ash" && scripts/qemu-debug.sh enter
scripts/qemu-debug.sh log 50                                   # tail serial.log
scripts/qemu-debug.sh stop
```

**Reproducing a real-hardware-only failure:** this kernel is also brought up
on a physical AM4/Ryzen machine, where there is no serial capture at all —
every observation has to be text drawn on the framebuffer and read off the
screen by eye. Before iterating on that loop (edit → build → `dd` to USB →
physical reboot → read screen), try to make QEMU look like the target
machine: `QEMU_DEBUG_MEM=8G` (RAM size — the default here is 512M and *every*
real machine has more), `QEMU_DEBUG_NO_DISK=1` (the board has NVMe+AHCI, no
ATA/IDE), `QEMU_DEBUG_NO_AC97=1` (it has HDA), `QEMU_DEBUG_EXTRA_ARGS` for
anything else. This is not hypothetical leverage: the 2026-09-21 bring-up
blocker looked hardware-specific (TSC calibration against a real 3.7 GHz
clock, no PS/2, USB-only keyboard) and was none of those — `-m 768M`
reproduced it on the first try, which put serial.log, gdb and boot-matrix
back in play. Sweep one variable at a time; the device knobs were clean and
only memory mattered.

**Measuring an intermittent boot failure:** `scripts/boot-matrix.sh N M` runs N QEMU
instances in parallel, M boots each, classifies every boot (`OK`/`HANG`/`PANIC`/
`DOUBLE_FAULT`) by grepping its own serial.log, and prints an aggregate. Isolation per
instance composes the overrides above plus a qcow2 overlay per instance
(`qemu-img create -f qcow2 -b disk.img -F raw`) — kilobytes each, base image untouched, and
one per instance is mandatory since mounting `/mnt` writes to it. Parallelising QEMU is the
right lever here because it costs no extra tokens (same command, same aggregated output) and
turns a 20-boot measurement from ~15 minutes into ~2. Serial logs of non-OK boots are
preserved for inspection. **Read the preserved log before believing a failure verdict** — a
harness that classifies boots can be wrong about them, and has been.

**Concurrent sessions:** `STATE_DIR` (serial.log/monitor.sock/qemu.pid) defaults to
`/tmp/qemu-debug-rust_so_kernel` but is overridable via `QEMU_DEBUG_STATE_DIR` — set it to a
different path so two independent investigations in the same checkout don't clobber each
other's log/socket/pid or kill each other's QEMU. The ext2 disk image is likewise overridable
via `QEMU_DEBUG_DISK_IMG` (point it at a `cp disk.img /tmp/foo.img` scratch copy) — two QEMUs
writing the real `disk.img` at once can corrupt it. The shared UEFI boot image and OVMF VARS
pflash (under `target/*/build/so2-*/out/`, not independently overridable since they're
build.rs's own outputs) are opened with `file.locking=off` for exactly this reason — both are
effectively read-only at runtime, so two sessions reading the same build concurrently is safe.

**GDB.** `start --gdb` adds QEMU's gdbstub (`-gdb tcp::1234`, port via `QEMU_GDB_PORT`) without
freezing the CPU — boots normally, stub just sits there so you can attach at any later point
(e.g. once a hang is detected). `start --gdb-freeze` additionally passes `-S` to halt at the
reset vector for early-boot single-stepping — not the default, since it would hang every
`start` waiting for a debugger that usually isn't there. The `gdb` subcommand is the
non-interactive half, built for agents without an interactive terminal: it runs
`rust-gdb`/`gdb` (whichever is on `$PATH`, `rust-gdb` preferred for its Rust pretty-printers;
errors out with a clear message if neither exists) in `-batch` mode against
`target remote localhost:<port>`, with the kernel's own (never-stripped, see Userspace
Programs below) debug symbols loaded from whichever of
`kernel/target/x86_64-unknown-none/{debug,release}/kernel` was built most recently, and prints
the result to stdout:

```bash
scripts/qemu-debug.sh gdb "info registers" "bt" "p \$rip"   # no args: same 3 as a default
```

Symbol loading needs one extra step because the kernel ELF is a PIE — `bootloader` 0.11 loads
it as `ET_DYN` at a runtime-chosen `virtual_address_offset` (not the addresses recorded in the
file, and not necessarily the same across boots). The bootloader logs the exact offset it
picked to serial at boot (`virtual_address_offset: 0x...`); the `gdb` subcommand greps the
current session's serial.log for it and loads symbols via `add-symbol-file <elf> -o <offset>`
so addresses actually resolve to real function names instead of bare hex.

### QEMU integration tests

Real hardware-path behavior (drivers that need actual QEMU devices, not just host-testable
pure logic — see `hal/`'s host tests via `cd hal && cargo test`, 340 tests, <1s, no QEMU) is
asserted by a `#![feature(custom_test_frameworks)]` harness that boots the real kernel in
QEMU and reports PASS/FAIL as a process exit code:

```bash
scripts/run-kernel-tests.sh
```

This builds `kernel`'s test binary (`cargo build --target x86_64-unknown-none --tests`, run
from `kernel/`), boots it headless in QEMU with `-device isa-debug-exit`, and exits 0 (every
`#[test_case]` passed) or nonzero (a test failed, or the kernel hung/crashed before reporting)
— see the guest side (`kernel/src/test_framework.rs`, `kernel/src/hw_tests.rs`,
`kernel/src/init/test_support.rs`) and host side (`qemu-test-runner/`, a standalone crate).

**Plain `cargo test --target x86_64-unknown-none` does not work here** — verified, not
assumed: it builds the `kernel` bin target twice in one invocation (once normally, once under
`--cfg test`), and with this crate's `-Z build-std`, that produces two independently-built
`core` crates that collide (`error[E0152]: duplicate lang item in crate 'core': 'sized'`) the
moment a shared dependency needs both. `cargo build --tests` doesn't hit this, so
`scripts/run-kernel-tests.sh` drives that instead of `cargo test` itself — see that script's
header comment and `kernel/.cargo/config.toml`'s `[target.x86_64-unknown-none] runner`
comment for the full diagnosis. `[acpi] SELFTEST` (`kernel/src/acpi.rs`, the boot-time ACPI
self-check against known QEMU i440fx values) is the first real test case
(`kernel/src/hw_tests.rs::acpi_selftest_passes`). The second, `ext2_memdisk_roundtrip`, mounts
`fs::ext2` on a `hal::block::MemDisk` carrying a hand-built minimal image
(`ext2::testimg::build_minimal_image`) and drives create/mkdir/rename/symlink/unlink/rmdir through
the real VFS — see the storage-stack seam entry below. The third,
`ext2_reclaim_orphans_clears_injected_disk_img_shape`, is described with the ext2 repair
passes below. The fourth,
`unix_socket_handle_roundtrip`, covers the AF_UNIX adapter (`kernel/src/ipc/unix.rs`) — the
half of the socket stack the `usock` crate's host tests cannot reach: `UnixSocketHandle` as a
real `FileHandle`, `socket_id()` reporting through a `Box<dyn FileHandle>`, and reference
counting living in `Drop` rather than `close()`; see the AF_UNIX section below. The fifth,
`framebuffer_primitives_touch_exactly_their_own_pixels`, asserts `fill_rect`/`draw_char`/
`scroll_up` against a RAM-backed `Framebuffer` (the same technique `MemDisk` gives ext2) with
a **`stride` deliberately larger than `width`** and the buffer pre-filled with `0xAA`, so the
padding columns a naive `row * width` would corrupt are checked and "untouched" is something
the test can actually assert — see the framebuffer console section below. The sixth,
`framebuffer_shadow_mode_flushes_exactly_what_changed`, repeats that in RAM-shadow mode
with a second buffer standing in for VRAM: nothing reaches "VRAM" inside a batch, the
outermost `end_batch` leaves it identical to the shadow, padding is never written, an
unbatched primitive flushes itself, and a flush copies only its own rectangle. The last,
`tlb_shootdown_leaves_no_stale_translation`, is the TLB shootdown's (see TLB Shootdown
below) — which is why the runner starts QEMU with **`-smp 4`** and `boot_for_tests` now
runs the real boot's APIC and `smp::start_aps` steps. See
`docs/drivers/architecture.md`'s
Testing section and `docs/drivers/roadmap.md`'s Phase 2 for more.

## Crate Layout

| Crate | Path | Purpose |
|-------|------|---------|
| `so2` | `/` (host) | Build script + QEMU launcher |
| `kernel` | `kernel/` | Bare-metal kernel (`#![no_std]`, `x86_64-unknown-none`) |
| `hal` | `hal/` | Host-testable hardware-access seams (`PortIo`/`PhysMem`) + pure driver logic, including the xHCI/USB/HID logic behind the USB keyboard, the MTRR/PAT memory-type decoding behind `/proc/fbinfo`, the `cpuid` decoding behind `/proc/cpuinfo` and the k10temp arithmetic behind `/proc/sensors` (`cd hal && cargo test`) |
| `ext2` | `ext2/` | Host-testable ext2 filesystem core (`cd ext2 && cargo test` — known intermittent failure, see `docs/fs/ext2-test-flake.md`) |
| `mm` | `mm/` | Host-testable buddy (physical) + slab (heap) allocators (`cd mm && cargo test`; the optional `slab-debug` feature adds a redzone + free-object quarantine, off by default so the default slot layout stays the reference one) |
| `vfs` | `vfs/` | Host-testable VFS core: `Inode`/`Filesystem`/`FileHandle` traits, mount table + path resolution, and ramfs (`cd vfs && cargo test`) |
| `diag` | `diag/` | Host-testable always-on diagnostic instruments (`LockDiag`, `DirLockDiag`, `TfRewindDiag`, `IfViolationDiag`, `OpStat`) extracted out of `kernel/src/debug.rs` (`cd diag && cargo test`) |
| `usock` | `usock/` | Host-testable AF_UNIX socket core: socket state machines, stream/datagram queues, backlog, `sockaddr_un` parsing, the abstract namespace and `SCM_RIGHTS`, generic over the passed-descriptor type (`cd usock && cargo test`) |
| `sched` | `sched/` | Host-testable scheduler core: priority run queues, wait queue, decay-on-preemption, aging, quantum arithmetic, and an invariant checker + property tests, generic over the scheduled entity; plus the one-shot `WaitCell` and the EINTR-or-restart rule behind interruptible waits; plus `cputime` (tick classification, per-CPU and per-process times, `/proc/stat`'s `cpu` lines rendered *and* parsed) and `loadavg` (Linux's fixed-point averages) — see CPU Time Accounting (`cd sched && cargo test`). Also a path dependency of `userspace` (`cpumon` parses `/proc/stat` with it) |
| `tty` | `tty/` | Host-testable terminal core (`docs/gui/gui-plan.md` phase 3.1): `termios`/`winsize` in this port's ABI (not Linux's flag values), the line discipline (canonical editing, echo, `ISIG`, `VMIN`/`VTIME`, `OPOST`), the job-control rules (`SIGTTIN`/`SIGTTOU`/`EIO`, `TIOCSCTTY`, `tcsetpgrp`) and a pty pair's semantics (hangup, `EIO`/`POLLHUP`, `SIGWINCH`); nothing blocks, wakeups and signals come back as data (`cd tty && cargo test`). The kernel's pty (`kernel/src/ipc/pty.rs`) is its adapter |
| `gui` | `gui/` | Host-testable core of the userspace compositor (`docs/gui/gui-plan.md` phase 2.4): Wayland's wire format, rectangle regions, a minimal Wayland-named protocol and the compositor state machine with `compose` into a plain `&mut [u32]`; nothing blocks, effects come back as data (`cd gui && cargo test`). A path dependency of `userspace`, not `kernel`, and watched by the root `build.rs` |
| `vt` | `vt/` | Host-testable terminal emulator for the windowed terminal (`docs/gui/gui-plan.md` phase 3.4): cell `Grid` (xterm semantics — deferred wrap, scroll region, alternate screen, per-row damage), the escape-sequence `Parser` (the console's set plus what `vi`/`less`/`top` need; `DSR`/`DA` answers come back as data), `render` into a plain `&mut [u32]` with the console's Noto font and palette, and the evdev `keymap` (Enter `\r`, Backspace `DEL`, Delete `ESC[3~` — pty conventions, not the console's) (`cd vt && cargo test`). A path dependency of `userspace` (for `term`), watched by the root `build.rs` like `gui/src`. The kernel console does **not** use it |
| `draw` | `draw/` | Host-testable software 2D drawing for graphical userspace programs: `Canvas` over a plain `&mut [u32]` with a stride (clipped `rect`/`frame`/`blit`/`dim`, antialiased and shaded `disc`, additive `glow`, in fixed point), colour arithmetic (`mix`/`add`/`scale`/`hsv`), 3x5 and 5x7 pixel fonts, and — feature `noto`, which `userspace` enables — `smooth`: antialiased Noto Sans Mono (the console's font) blended over whatever is drawn; `no_std`, no `alloc`, integer only (written when the userspace target was soft-float) (`cd draw && cargo test`). With `userspace::gfx` (window or console, events, present) it is this system's small SDL — no dynamic linking, so each program links it statically. A path dependency of `userspace`, watched by the root `build.rs` |
| `text` | `text/` | Host-testable proportional text for userspace programs (`docs/gui/text-plan.md`): a thin layer over `parley` (shaping, GPOS kerning, line breaking, bidi) and `swash` (rasterisation) — `Fonts` (registered from bytes, no system discovery), `layout`/`measure`, and `GlyphCache` (masks keyed by font/glyph/size/quarter-pixel phase, byte-bounded CLOCK eviction) drawing onto a `draw::Canvas`. `no_std` + `alloc`, ~1.2 MB linked. Its tests use the real Noto fonts: run `scripts/fetch-fonts.sh` once (`cd text && cargo test`). A path dependency of `userspace`, watched by the root `build.rs`: `userspace::text::Text::load()` reads the fonts from `/mnt/usr/share/fonts` (the root `build.rs` fetches them if missing and `sync_disk_tree`s them onto `disk.img`, as it does terminfo) and falls back to `draw`'s bitmap font without them. Only programs that use it link it, so they belong on the disk |
| `qemu-test-runner` | `qemu-test-runner/` | Host-side driver for the QEMU integration tests (`scripts/run-kernel-tests.sh`) |

Despite the heading, only `so2`+`kernel` form the actual Cargo workspace (`members = ["kernel"]` in the root `Cargo.toml`). `hal`/`ext2`/`mm`/`vfs`/`diag`/`sched`/`usock`/`tty`/`gui`/`vt`/`draw`/`text`/`qemu-test-runner` are each deliberately their own workspace root (empty `[workspace]` table in their own `Cargo.toml`, see the root `Cargo.toml`'s `exclude` comment for why — mainly that this workspace's `panic = "abort"` profile would break their unwinding `cargo test` harnesses) and are pulled into `kernel` via plain `path` dependencies instead: `kernel` depends on `hal`, `ext2`, `mm`, `vfs`, `diag`, `sched`, `usock`, and `tty`; `ext2` also depends on `hal` (`hal::block::BlockDevice`). `vfs` depends on neither `hal`, `ext2`, nor `mm` — only `spin`. Each of the extracted logic crates (`hal`/`ext2`/`mm`/`vfs`/`diag`/`sched`/`usock`/`tty`) exists for the same reason: `kernel` itself cannot run `cargo test` on the host (see `## QEMU integration tests` above — the `-Z build-std` + double bin-target-build lang-item collision), so logic that can be made to speak in plain types instead of this kernel's concrete globals gets moved out where a plain `cargo test` reaches it.

The host crate's `build.rs` creates a UEFI boot image; `src/main.rs` only launches QEMU with the image paths injected by the build script.

Kernel crate config in `kernel/.cargo/config.toml` enables `-Z build-std` to rebuild `core`/`alloc`/`compiler_builtins` for the bare-metal target.

## Boot Sequence (`kernel/src/init/mod.rs`)

`kernel_main` → `init::boot`:
1. `devices::init_idt()` — load IDT (exceptions, PIC IRQs); syscalls go through the `syscall` instruction (MSR LSTAR, wired later in `process::tss::init()`), not an IDT gate
2. Framebuffer setup (inline, requires `&'static mut` lifetime from BootInfo)
3. `memory::init_core()` — store physical memory offset, seed Buddy allocator
4. `memory::test_allocators()` — smoke test slab + Vec + String
5. `devices::draw_boot_screen()`
6. `devices::init_hardware_interrupts()` — init PIC + PIT (the PIT is what the TSC is calibrated against)
6b. `mouse::init()` — best-effort PS/2 auxiliary device enable (IRQ12); bounded polls, never hangs boot on hardware with no PS/2 mouse
6c. `ac97::init()` — best-effort PCI AC97 audio codec enable; bounded polls, never hangs boot on hardware/QEMU configs with no AC97 device
6d. `cpu::tsc::init()` (calibrated against the PIT), then `interrupts::apic::init()` — retires the 8259 + PIT in favour of the LAPIC timer + I/O APIC (see Interrupt Controllers below)
6e. `cpu::init_this_cpu(0)` — everything the CPU holds for itself (GDT + its TSS slot, IDT, GS, syscall MSRs, PAT, SSE, LAPIC + timer), then read back; see Per-CPU Init below
6f. `smp::start_aps()` — wakes every AP in the MADT (INIT-SIPI-SIPI through a low-memory trampoline), each runs `init_this_cpu` and parks in `hlt` until step 10; see Application Processors below
7. REPL initial prompt
8. `process::fpu::init()` — captures the FXSAVE template
9. `processes::init_all()` — create one idle process per scheduling CPU, then the shell
10. `process::start_first_process()` — start the shell on the BSP, release the APs into the scheduler (`smp::release_aps`), enable interrupts, jump to first trapframe

## Memory Subsystem (`kernel/src/memory/`, `kernel/src/allocator/`)

**Both allocators live in the standalone `mm` crate** (`mm/src/buddy.rs`, `mm/src/slab.rs`; `cd mm && cargo test` — 34 unit tests plus a few integration tests, no QEMU), extracted out of `kernel/src/allocator/{buddy_allocator,slab}.rs` following the exact precedent the `ext2` crate extraction set (`docs/fs/ext2-extraction-plan.md`; see `mm`'s own crate doc comment for the full rationale). `kernel/src/allocator/mod.rs` is now a thin adapter, the same shape `kernel/src/fs/ext2.rs` became after its extraction: it owns the global state (`BUDDY`, `SLAB_ALLOCATOR`, the `#[global_allocator]` registration) and the two seams `mm` needed in place of calling straight into `crate::memory`/`crate::serial_println_raw!`. `mm` is `no_std` with **no `alloc` dependency at all** (unlike `hal`/`ext2`, which both link `alloc`) — the slab allocator *is* the kernel's global allocator, so any internal allocation there would recurse into itself before the first `Vec`/`Box`/`String` ever completed; both allocators stay built entirely out of fixed-size arrays and intrusive linked lists for exactly this reason, unchanged from before the extraction.

- **`mm::PhysMap`** (implemented kernel-side by `KernelPhysMap`, wrapping `physical_memory_offset()`) replaces the direct `crate::memory::physical_memory_offset()` calls — same shape as `hal::PhysMem`.
- **Logging moved to the adapter.** `mm` can't call `crate::serial_println_raw!`, so recoverable conditions come back as data instead: `mm::buddy::PhantomEvent` (a stale/"phantom" bitmap entry found while coalescing on `deallocate` — bitmap said a buddy block was free but the intrusive free list didn't actually contain it) and `mm::slab::AllocEvent`/`DeallocEvent` (large-object alloc/dealloc, cache expansion). `kernel/src/allocator/mod.rs` matches on these: failures (a large allocation that failed, a cache that could not expand, a free in the "hot range") always print, while routine successes are `ktrace!(MM)` (`kdebug mm on`) — they used to print unconditionally, two lines per kernel buffer over 2 KiB, which wrapped the 64 KiB `klog` ring within seconds of starting `doom`. The one genuinely unrecoverable condition, a double-free caught by the bitmap, used to print then `loop { hlt }`; `kernel/src/panic.rs`'s handler is verified to touch neither `BUDDY` nor the heap, so `mm` just `panic!`s there now, the same way `ext2` panics/returns `Ext2Error` for its own hard errors.
- **`mm::FrameSource`** is the slab allocator's only path to physical frames — `kernel::allocator::KernelFrameSource` forwards it straight through to `phys_alloc`/`phys_free`, preserving that two-function facade as the real slab↔buddy boundary instead of letting `mm::slab` call `mm::buddy` directly now that both live in one crate.
- Addresses are `x86_64::PhysAddr`/`VirtAddr` (pinned to `=0.15.4`, matching the kernel's resolved version — `0.15.5` fails to build against this repo's pinned nightly, a real, verified incompatibility, not a hypothetical one), not raw `u64` like `hal`/`ext2` use — chosen because nearly every call site moving into `mm` already had a `PhysAddr` in hand, and converting all of them to/from `u64` at the boundary would have touched far more of `kernel/src/memory/page_table_manager.rs` than this move needed to.

**Physical allocator:** Buddy allocator (`mm::buddy::BuddyAllocator`), orders 12–28 (4 KiB–256 MiB). Single global `BUDDY: Mutex<mm::buddy::BuddyAllocator>` (`kernel/src/allocator/mod.rs`) is the **sole** owner of physical frames after boot. Uses a compile-time O(1) bitmap (covers 0–512 MiB) for fast free-block lookup. **That bound is a real limitation on real hardware, not just a QEMU-sized convenience:** above 512 MiB the bitmap ops are no-ops, so `is_free` always says false — the allocator stays correct (the intrusive free list is the source of truth) but never coalesces and can never catch a double free. On a machine whose RAM all sits above the bound, that is the whole uptime. The consequence to watch for is a progressive large-allocation failure (the slab asks for order-20/1 MiB blocks) — reasoned from the code, not yet measured. See `mm/src/buddy.rs`'s header comment.

**Heap allocator:** Slab allocator (`mm::slab::SlabAllocator`) backed by Buddy through `mm::FrameSource` (see above). Registered as the global `#[global_allocator]` (`kernel::allocator::SlabGlobalAlloc`), enabling `alloc` (Vec, Box, String, etc.) throughout the kernel.

**Page tables:** `OwnedPageTable` (`memory/page_table_manager.rs`) wraps `x86_64::OffsetPageTable`. Kernel address space uses `from_current()` (captures CR3); new user spaces use `new_user()` which clones kernel mappings into a fresh PML4.

**Address space:** `AddressSpace` (`memory/address_space.rs`) bundles an `OwnedPageTable` + `VmaList`. Each `Process` owns one.

**VMAs** (`memory/vma.rs`): Up to 64 VMAs per process, in a `Vec` (an inline `[Option<Vma>; 64]` overflowed the 80 KiB boot stack at `opt-level 0` once `Vma` grew). Kinds: `Code` (pre-loaded, not demand-paged), `Anonymous` (zero-filled on demand — heap), `GrowableStack`, `Huge2M`, and `Shared` (below). `Vma` is not `Copy`: a `Shared` one holds its object.

**Shared memory** (`memory/shm.rs`, `ipc/memfd.rs`; phase 1 of `docs/gui/gui-plan.md`): a `ShmObject` is a size plus one lazily-allocated frame per page, behind a `memfd_create` fd or a `MAP_SHARED|MAP_ANONYMOUS` mapping. Every `Shared` VMA maps the object's own frames — never the zero frame, never COW, and `fork` does not write-protect them. **Frame lifetime rides on the COW refcounts:** the object holds one reference per frame and each PTE another, so the existing unmap/`release_user_pages` paths free a shared frame only when nothing holds it, without knowing shared memory exists. The refcounts are saturating `u8`s, so an object allows `MAX_MAPPINGS` (200) VMAs at once (`mmap` → `ENOMEM`, `fork` fails past it). `ftruncate` shrinking a mapped object is `EBUSY` (Linux `SIGBUS`es instead). Lock order: address space → `ShmObject::inner` → `BUDDY`. `sys_mmap` reaches the object through `FileHandle::shm_object()` (an `Arc<dyn Any>`, downcast — `socket_id()`'s technique). Tested by `userspace/c/shm_test.c` (11 cases).

**COW frame refcounts** (`memory/cow.rs`): one byte per physical frame, sized at boot from the highest *usable* physical address the bootloader reported and allocated out of the Buddy allocator (`init_refcount_table`, called from `init::memory::init_core` — before the first `fork()`, after the Buddy can serve the table's own frames). Coverage is reported as `cow_tracked_frames` in `/proc/kdebug` and in `panic.rs`'s snapshot. It was a fixed `[u8; 512 MiB / 4 KiB]` BSS array until 2026-09-21, and that ceiling did not degrade above the bound — it broke `fork()` outright, because every accessor failed *unsafely* on an untracked index: `inc_ref` no-op (the share was never recorded), `get_ref` → 0 so the COW fault handler read "sole owner" and restored WRITABLE **without copying** (two processes writing one physical frame), and `dec_ref` → 0, which by this module's convention means "free it" (a child's exit handed the parent's still-mapped frames back to the Buddy allocator). Measured: `-m 768M`, the first QEMU size with any RAM above the old bound, killed PID 1 with `SEGFAULT (no VMA)` the instant `busybox --install`'s child exited, deterministically, while `-m 512M` was clean — and that was the sole blocker to reaching a shell prompt on the real AM4/Ryzen machine, where all of RAM is above 512 MiB. Out-of-range indices now fail **safe** instead (`get_ref` → 2 so COW copies, `dec_ref` → 1 so nothing is freed underneath a live mapping): unreachable once the table covers all of RAM, and a leaked frame rather than a shared one if it ever is. The counts are `AtomicU8`s (stage 6): their read-modify-writes used to be kept whole only by IF=0; the decisions made on a count ("1 → I am the last owner") are made under the address-space lock below, and `fork` — the only way a count rises — holds the same lock.

**Demand paging** (`memory/demand_paging.rs`): Page fault handler (in `init/devices.rs`) reads CR2 and calls `AddressSpace::handle_not_present_fault`/`handle_cow_fault` on the running process's address space, which find the VMA and call `map_demand_page` (allocate a frame from Buddy, zero it, map it — into *that* address space's table, not whatever CR3 holds) under the address space's lock. Kernel-mode faults panic; user-mode faults outside any VMA kill the process.

**The address-space lock** (`AddressSpace::vmas`, an `IrqMutex`, stage 6 of `docs/smp/smp-plan.md`): every VMA lookup and every PTE change of a user address space — demand paging, COW resolution, `fork`'s write-protect, `mmap`/`munmap` — happens inside it, so two threads faulting on one page serialise and the second finds it resolved (a spurious fault is success, not a failed `map_to`). **Kernel code that writes into another process's user memory goes through `AddressSpace::copy_to_user`/`copy_from_user`/`prepare_user_write`**, never `translate_page` + a physmap write: the frame behind a translated page can be the shared zero frame or a COW frame still shared with a fork sibling. `pipe.rs` did exactly that and wrote readers' data into the global zero frame (every untouched anonymous page then read it) and into fork parents' pages — `userspace/c/pipe_cow_test.c`. Lock order: scheduler → address space → `BUDDY`/`SLAB_ALLOCATOR`; never touch user memory by virtual address while holding it (that fault takes it again).

**ELF loader** (`memory/elf_loader.rs`): Parses ELF64 PT_LOAD segments, maps them into a fresh `AddressSpace`, zeros BSS, and registers demand-paged stack. Static executables only (no dynamic linker). `build_initial_stack` writes a real, dynamically-sized SysV ABI initial stack frame (argc/argv/envp/auxv) onto the pre-mapped top stack page — sized from whatever `sys_exec` read out of the caller's argv/envp arrays, capped to fit in one page (`E2BIG` if it doesn't).

## Process Subsystem (`kernel/src/process/`)

**`Process`** struct: PID, state, privilege (Kernel/User), base+effective priority (0–10), 16-byte name, `Box<TrapFrame>`, kernel stack, `AddressSpace`, `FileDescriptorTable`.

**Scheduler** (`process/scheduler.rs`): **one scheduler for every CPU** (stage 7 of `docs/smp/smp-plan.md`, decision 1) — a single `SCHEDULER` lock, one `SchedCore`, `running[cpu]` and `idle[cpu]` slots; see SMP Scheduling below. The accounting half — multi-level priority run queues (`run_queues[0..=10]`, only Ready processes), a `wait_queue` holding Blocked and Zombie processes, decay-on-preemption, periodic aging, and the quantum arithmetic (`BASE_QUANTUM + eff_pri * BONUS` ticks) — now lives in the standalone, host-testable `sched` crate as `sched::SchedCore<Process>` (`cd sched && cargo test`), following the exact `hal`/`ext2`/`mm`/`vfs` extraction precedent (see `docs/sched/sched-extraction-plan.md`). `kernel/src/process/scheduler.rs`'s `Scheduler` struct is now the thin adapter around it: `core: sched::SchedCore<Process>` plus everything that can't leave the kernel because it's entangled with real hardware or per-CPU state — each CPU's `running` process (tied to `activate()`/TSS/FPU/the per-CPU fast-path pointers), `static SCHEDULER` and `TrackedSchedulerGuard` (the lock plus its always-on IF=0 diagnostics), `TrapFrame`, and the `fxsave`/`fxrstor`/`fs_base`/CR3/TSS context-switch machinery. `impl sched::SchedEntity for Process` is the seam between the two: a pure field-accessor bridge (pid, base/effective priority, `is_idle`, `is_ready`) with no logic of its own. `sched`'s own doc comment (`sched/src/lib.rs`) has the full module-by-module breakdown of what moved, including a "Known limitations" section on measured, not-yet-fixed aging/starvation defects (`docs/sched/sched-bugs-plan.md`) — this refactor changed no scheduling behavior, so those defects are unchanged from before the extraction.

**Context switch** (`process/trapframe.rs`, `process/timer_preempt.rs`): The timer ISR (hand-written asm, pushes all GPRs) calls `timer_tick`. On preemption, `switch_to_next()` returns a `*const TrapFrame`; `jump_to_trapframe` restores all registers + `iretq`. The same path is used for process kill/switch.

**FPU/SSE** (`process/fpu.rs`): `Process::fpu_state` (`Box<fpu::FpuState>`, a 512-byte `#[repr(align(16))]` FXSAVE image) is saved/restored via `fxsave`/`fxrstor` at every context-switch point that also saves/restores `fs_base` (`switch_to_next`, `block_current`, `stop_and_switch_tf` save-and-restore; `kill_and_switch_tf`/`start_first` restore-only — both restore `fs_base` too, which `kill_and_switch_tf` did not until 2026-09-24: the next process ran on the dead one's TLS pointer, and `ash` running a script faulted in mlibc's `get_current_tcb` whenever an external command exited). `sys_fork` likewise takes the parent's FS base from the live MSR, not the switch-time copy in `Process::fs_base`. `fpu::init()` enables SSE (`CR0.EM=0`/`MP=1`, `CR4.OSFXSR=1`/`OSXMMEXCPT=1`) and captures one real `fxsave` of the resulting clean state as the template every new `Process` starts from — must run before the first `Process` exists (wired into `init::boot()` right before `processes::init_all()`). `sys_fork` captures the parent's *live* registers with a fresh `fpu::save()` (real `fork()` semantics — the stored `Process::fpu_state` is stale as of its last preemption, not necessarily current); `sys_clone` (new thread) gets the default template instead (a fresh thread doesn't inherit register contents); `sys_exec` resets to the template, written directly to live hardware next to the `fs_base`/TLS reset since exec continues on the same CPU without an intervening switch. Verified via `fpu_test` (`userspace/c/fpu_test.c`): loads a distinctive 128-bit pattern into `xmm0` via inline asm, spins through a pure-integer loop long enough to span hundreds of real preemptions (confirmed via the `switches_total` counter below, not just elapsed time), and checks it survived intact. **Signal frames carry it too** (`signal::SignalFrame::fpu`, since 2026-09-26): delivery `fxsave`s the interrupted state into the frame and `sigreturn` restores it, with MXCSR's reserved bits cleared first against the boot template's `MXCSR_MASK` (`fpu::sanitize` — the frame is user memory, and an unsanitized `fxrstor` #GPs in the kernel). The mask is the processor's, not a constant: the Ryzen's is `0x2FFFF` (AMD's bit 17, the misaligned-exception mask), measured on metal (boot #58). Until then a handler using XMM — any mlibc `memcpy`/`printf` — returned its values into the interrupted code.

**TSS** (`process/tss.rs`): one per CPU, each with its own `DOUBLE_FAULT_IST_INDEX` IST stack and the kernel RSP0 stack used on ring-3 → ring-0 transitions (`set_kernel_stack` writes the current CPU's); all in one GDT — see Per-CPU Init.

## Syscall Interface (`kernel/src/process/syscall.rs`)

Triggered via the `syscall` instruction (not `int 0x80` — no IDT entry involved). `process/tss.rs` wires `IA32_LSTAR` to `syscall_entry_fast` at boot. The assembly stub pushes all GPRs onto the current stack, calls the Rust dispatcher, writes the return value back into the saved RAX slot, pops, and `sysretq`/`iretq`.

Implemented syscalls (Linux-compatible numbers — see `SyscallNumber` enum for the authoritative list):

| Number | Name | Description |
|--------|------|-------------|
| 0 | `read` | Read from fd |
| 1 | `write` | Write to fd |
| 2 | `open` | Open device/file by path |
| 3 | `close` | Close fd |
| 4/5/6 | `stat`/`fstat`/`lstat` | File metadata; `lstat` genuinely doesn't follow a symlink at the final path component (real symlink support, see below) |
| 7 | `poll` | Wait for events on up to 16 fds. `POLLHUP`/`POLLERR` are reported unasked, by `epoll` too (it dropped `POLLHUP` until 2026-09-25, and an `epoll` on a pty master whose shell exited never woke). Real readiness for sockets, stdin and `/dev/input/event*` (`FileHandle::event_source`: the queue behind the handle, woken by its producers — keyboard ISR, IRQ12, the USB poll); every other device is always ready. `epoll_wait` shares it. `input_poll_test` |
| 8 | `lseek` | Reposition file offset |
| 9/11 | `mmap`/`munmap` | Private anonymous memory, or `MAP_SHARED` of a memfd / `MAP_SHARED\|MAP_ANONYMOUS` (see Shared memory above). A nonzero `addr` is taken as `MAP_FIXED`; `munmap` needs an exact VMA |
| 77 | `ftruncate` | memfds only (`EINVAL` otherwise); shrinking a mapped object is `EBUSY` |
| 319 | `memfd_create` | A shared-memory object behind a new fd; `read`/`write`/`lseek`/`fstat` work on it |
| 12 | `brk` | Heap break |
| 13/14/15 | `sigaction`/`sigprocmask`/`sigreturn` | POSIX signals. `sigaction` reads the handler and `sa_flags` (`SA_RESTART` only, at offset 16 of this port's `struct sigaction`, whose value is `1<<3`, not Linux's); dispositions, `SA_RESTART` and the mask are inherited by `fork` (and copied into a `clone`d thread) since 2026-09-25 — every child used to start all-default with an empty mask. `sigset_t` crosses the boundary in Linux's layout (bit N-1 = signal N, what mlibc's `sigaddset` writes) and is shifted to the kernel's bit-N masks (`signal::mask_from_user`); until 2026-09-25 it was taken as-is, so every C program's mask named the signal one below the one it meant |
| 34 | `pause` | `rt_sigsuspend` with the mask the process already has (`signal::suspend(None)`), always `EINTR`. mlibc's `pause()` hit a missing-sysdep `__ensure` that *returns*, so `for (;;) pause();` spun printing it |
| 130 | `rt_sigsuspend` | Swap the mask and sleep until a signal that runs a handler, terminates or stops (an ignored one does not wake it); returns `EINTR` with the old mask restored — through the handler frame's saved mask (`Process::saved_sigmask`, Linux's `TIF_RESTORE_SIGMASK`) or by `deliver_pending` if no handler runs. Check and block under the scheduler lock; every signal sender calls `Scheduler::interrupt_blocked` after queueing (see Interruptible waits below). Backs ash's `wait` (`userspace/c/sigsuspend_test.c`) |
| 16 | `ioctl` | TCGETS/TCSETS* (termios, `isatty()`), TIOCGWINSZ, TIOCG/SPGRP, plus the custom `FBIO_BLIT` (`0x4642_0001`) on `/dev/fb` — full-frame scaled blit for the DOOM port, see `FbBlitArgs` | On a pty (`/dev/ptmx`, `/dev/pts/N`) the handle answers every tty ioctl itself (`FileHandle::ioctl`): termios, `TCFLSH`, `TIOCSCTTY`/`TIOCNOTTY`, `TIOCG/SPGRP`, `TIOCGSID`, `TIOCG/SWINSZ`, `FIONREAD`, `FIONBIO`, `TIOCGPTN`/`TIOCSPTLCK` — see the pty section below
| 20 | `writev` | Vectored write |
| 22 | `pipe` | Anonymous pipe |
| 24 | `yield` | Voluntary context switch |
| 32/33 | `dup`/`dup2` | Duplicate fd (real shared-offset semantics) |
| 35 | `nanosleep` | Sleep via hrtimer. Takes plain nanoseconds, not a `timespec` (this port's ABI); a signal ends it with `EINTR`, and mlibc's `sys_sleep` measures what was left (it used to discard the result, so an interrupted sleep looked finished) |
| 39 | `getpid` | Return current PID |
| 110 | `getppid` | `Process::parent_pid` (1 after reparenting, 0 for PID 1). mlibc's sysdep returned 1 unconditionally until 2026-09-25, so `kill(getppid(), sig)` signalled init |
| 41-55, 288 | `socket`/`connect`/`accept`/`sendto`/`recvfrom`/`sendmsg`/`recvmsg`/`shutdown`/`bind`/`listen`/`getsockname`/`getpeername`/`socketpair`/`setsockopt`/`getsockopt`/`accept4` | Real AF_UNIX sockets — see the AF_UNIX section below. Linux signatures throughout (`struct sockaddr_un`, `struct msghdr` with a real iovec array and `SCM_RIGHTS` control messages); `AF_INET` is `EAFNOSUPPORT`, there being no network stack |
| 56/57 | `clone`/`fork` | Threads (shared AddressSpace+fds) / COW process fork |
| 59 | `exec` | `(path, argv, envp)` — real argc/argv/envp built onto the new stack, see `memory/elf_loader.rs::build_initial_stack`. Caught signals go back to `SIG_DFL` (ignored stay ignored; mask and pending carry over) — until 2026-09-25 handlers survived exec, and `sh -c 'true & sleep 1'` died of SIGSEGV every time in ash's handler inside the exec'd `sleep` |
| 60 | `exit` | Terminate process (immediate switch). A process killed by a signal or a fault closes its files at death too: `kill_current` moves its fd table to `process::dead_files`, drained with no lock held and IF=0 at every syscall entry and in the idle loop — the zombie used to keep it until the reap, which dropped it under `SCHEDULER` (a socket's `Drop` retakes it: every CPU hung when the compositor was `SIGKILL`ed), and its peers saw no EOF until then (`lifecycle_test` case E). Its children go to PID 1 (`Scheduler::reparent_children`, from `kill_current`, so every death path); a zombie a blocked `waitpid` is woken for is reaped right there (`reap_zombie`) — it used to stay queued until some later `waitpid` found it again, so PID 1's `busybox --install` child was a zombie for the whole uptime (`userspace/c/lifecycle_test.c`) |
| 61 | `waitpid` | Real POSIX pid overloads (`>0` exact/`0` own pgid/`-1` any child/`<-1` group), `WNOHANG`/`WUNTRACED`, real exit status incl. `WIFSIGNALED`. No matching child is `ECHILD` even with `WNOHANG` (ash's `wait` reaps until it sees it) |
| 109/121/112/124 | `setpgid`/`getpgid`/`setsid`/`getsid` | Real sessions since 2026-09-25 (`Process::sid`, inherited by `fork`/`clone`; PID 1 leads session 1): `setsid` is `EPERM` if a group with the caller's pid exists, else a new session and group; `setpgid` follows POSIX — caller or a child only (`ESRCH`), same session and not a session leader (`EPERM`), an existing group only within the session (`EPERM`); no `EACCES`-after-exec. No controlling terminal yet (phase 3.3 of `docs/gui/gui-plan.md`). `session_test` |
| 62 | `kill` | Send a signal (`pid > 0`, `0` and `< -1` for groups). Ends the target's wait if it is Blocked in an interruptible one — see Interruptible waits below. `SIGCONT` and `SIGKILL` resume a stopped target (`signal::resumes_stopped`); until 2026-09-25 only `SIGCONT` did, and `kill -9` of a stopped process left it stopped forever with the signal pending |
| 72 | `fcntl` | Only `F_DUPFD`/`F_DUPFD_CLOEXEC` do something; rest are validity-checked stubs |
| 21 | `access` | `F_OK`/`R_OK`/`X_OK` just mean "resolves" (no uid/permission model); `W_OK` actually probes writability — opens the path `O_WRONLY` and issues a zero-length `write()`, since every read-only filesystem's regular-file handle unconditionally errors on `write()` regardless of length, while `RamFileHandle`'s `write()` with an empty buffer is a true no-op |
| 82/83/84/87 | `rename`/`mkdir`/`rmdir`/`unlink` | VFS mutation — ramfs (`/tmp`) and ext2 (`/mnt`) both support these (real alloc/free of blocks+inodes on ext2, see the ext2 section below); devfs/initramfs/procfs remain read-only |
| 88 | `symlink` | `(target, linkpath)` — real symlink creation on ramfs and ext2 (`Inode::symlink`, default `EROFS` elsewhere, same convention as `create`/`mkdir`); `target` is stored verbatim, unresolved, exactly like real `symlink(2)` |
| 89 | `readlink` | Real symlink target read (`fs::vfs::resolve_no_follow` + `Inode::readlink`) |
| 90/91 | `chmod`/`fchmod` | Real on ext2 (persists `i_mode`'s permission bits, see below); on every other filesystem, validity-checked stubs (path/fd must resolve) — no per-inode permission-bits storage exists there to actually change |
| 158 | `arch_prctl` | `ARCH_SET_FS` (TLS base) |
| 202 | `futex` | Wait/wake, backs mlibc mutexes/condvars |
| 213/232/233 | `epoll_create`/`epoll_wait`/`epoll_ctl` | Epoll |
| 217 | `getdents64` | Directory entries, `linux_dirent64` layout. Deliberately does NOT use `with_current_process`: that would hold the `SCHEDULER` lock across the call into `FileHandle::getdents64`, and `fs::procfs`'s live-pid listing needs a *fresh* `SCHEDULER` lock of its own (`scheduler::all_pids()`) — self-deadlocks otherwise (spin locks aren't reentrant). Same clone-the-fd-table-Arc-then-drop-the-scheduler-lock shape as `sys_read`'s generic path |
| 218 | `set_tid_address` | Stub for TLS/thread bookkeeping |
| 228 | `clock_gettime` | `CLOCK_REALTIME` is a real wall-clock reading (CMOS RTC read once at boot, see Time Subsystem below, plus uptime since); `CLOCK_MONOTONIC`/`_RAW`/`_COARSE`/`CLOCK_BOOTTIME` are uptime; `CLOCK_PROCESS_CPUTIME_ID` (the thread group) and `CLOCK_THREAD_CPUTIME_ID` (the caller) are run time measured at every switch (`Process::exec_ns`) — what `clock()` reads. See CPU Time Accounting |
| 229 | `clock_getres` | 1 ns for every clock above |
| 100 | `times` | `struct tms` in ticks of 100 Hz (the thread group's user/system, waited-for children's), uptime in ticks as the return value. `NULL` allowed |
| 98 | `getrusage` | `ru_utime`/`ru_stime` for `RUSAGE_SELF`/`CHILDREN`/`THREAD` from the same ticks; every other field 0 |
| 99 | `sysinfo` | Linux's struct: uptime, load averages (`<< 16`), total/free RAM in bytes (`mem_unit` 1), process count. mlibc's `sysinfo()` calls it (it used to be assembled from `statvfs`, loads and procs 0) |
| 204 | `sched_getaffinity` | The CPUs that run processes (`scheduler::scheduling_cpus`) for every pid; Linux's buffer rules (`EINVAL` below `MAX_CPUS` bits or not a multiple of 8), returns 8 (bytes written). No `sched_setaffinity`. Backs `sysconf(_SC_NPROCESSORS_*)` and `nproc` |
| 400/401/402 | `uptime_ms`/`uptime_sec`/`meminfo_kb` | Custom, above the Linux syscall range — debug/introspection only |
| 162 | `sync` | No write-back cache exists to flush (ext2 writes are synchronous), so this copies the kernel log ring to the USB stick's `constanos-log` partition — see the kernel-log-on-the-stick section. Reports failure, unlike Linux: `ENODEV` (no log partition), `EBUSY`, `EIO`. `kdebug sync` calls it |
| 169 | `reboot` | Linux magic numbers + commands. `RESTART` flushes the kernel log to the USB stick (reason `reboot`), then resets: ACPI FADT `RESET_REG` → port `0xCF9` → 8042 `0xFE` (only if one answers) → triple fault (`kernel/src/reboot.rs`). `HALT`/`POWER_OFF` flush and stop (no S5 without AML). Backs the embedded `reboot` program. QEMU i440fx's FADT is ACPI 1.0 (no `RESET_REG`), so there `0xCF9` does the reset |
| 403 | `kdebug_ctl` | Get/set `kernel::debug`'s runtime tracing mask (get: `cmd=0`; set: `cmd=1`, subsystem name + on/off) — backs the `kdebug` userspace program. `cmd=2` panics the kernel on purpose (`kdebug panic`, Linux's sysrq-c) to exercise the panic path on demand. `cmd=3` runs the TLB-shootdown self-test against every AP (`kdebug tlbtest`; report in `/proc/dmesg` as `tlb_selftest:`). `cmd=4` sets the idle wait (`kdebug idle hlt|c2`, see Idle below) |
| 280 | `utimensat` | Linux's `(dirfd, path, times[2], flags)`: `UTIME_NOW`/`UTIME_OMIT`, `times` NULL = now, `AT_SYMLINK_NOFOLLOW`; a NULL `path` is `futimens(dirfd)`. Real on ext2 and ramfs, `EROFS` elsewhere (`Inode::set_times`/`FileHandle::set_times`). A relative path against a real `dirfd` is `ENOSYS`. See File Timestamps |
| 404 | `statvfs` | Custom (real `statvfs(2)` has no fixed Linux syscall number of its own — glibc/mlibc implement it over `statfs`, which this port doesn't wire). One physical-memory pool backs every mount, so every path reports the same Buddy-allocator-derived total/free block counts — enough for `df` to run and show live numbers, not a real per-mount breakdown |

Helpers `with_current_process` and `with_scheduler` guarantee `cli` before lock and `sti` after lock is dropped to prevent deadlocks with the timer ISR. `sys_close`/`sys_dup2` deliberately avoid `with_current_process` (see their doc comments) — closing a handle can run a `Drop` impl that needs a fresh `SCHEDULER` lock, which would self-deadlock if the outer helper were still holding it.

## AF_UNIX Sockets (`usock/`, `kernel/src/ipc/unix.rs`, `kernel/src/process/syscall/ipc.rs`)

Real AF_UNIX, replacing the former `kernel/src/ipc/channel.rs` — a bespoke
IPC of fixed 64-byte messages whose `socket()` took no arguments at all (no
domain, no type, no `sockaddr`, no `listen()`). Nothing about it was AF_UNIX
except the syscall numbers it borrowed.

**What lives where.** The `usock` crate (`cd usock && cargo test` — 71 host
tests, no QEMU) owns every state machine: `SOCK_STREAM` byte streams and
`SOCK_DGRAM` datagrams, the bind registry (filesystem paths and Linux's
abstract namespace in one table), backlog/accept queues, half-close,
`SO_*` options, `SCM_RIGHTS`, and `sockaddr_un` parsing. It is generic over
the passed-descriptor payload (`SocketTable<F>`) exactly the way
`sched::SchedCore<Process>` is generic over the scheduled entity: the kernel
instantiates `F = Box<dyn FileHandle>`, host tests use integers, and fd
passing becomes testable without a kernel. Two properties make that work:
**nothing blocks** (an operation that can't complete returns
`SockError::Again`) and **wakeups come back as data** (`Wakes`, naming the
sockets that became readable/writable/acceptable — the same
report-the-condition technique as `mm`'s `PhantomEvent`).

`kernel/src/ipc/unix.rs` is the adapter: the global `SOCKETS` table,
`UnixSocketHandle` (the `FileHandle` that puts a socket behind an fd, so
plain `read`/`write`/`dup`/`poll` work on one), and blocking.
`process/syscall/ipc.rs` is the syscall boundary — `sockaddr_un` in and out
of user memory, iovec walking, `SCM_RIGHTS` parsing, and the
`EAGAIN`-vs-park decision.

**`SOCKETS` is a `diag::IrqMutex`, not a `spin::Mutex`** — same reasoning as
`BUDDY`/`SLAB_ALLOCATOR` (see Key Design Invariants): it is taken on
allocating paths, and `with`/`try_with` disable interrupts before touching
the real lock. Lock order is `SOCKETS` → (release) → `SCHEDULER`, never
nested.

**Blocking restarts the syscall instead of completing it from the waker.**
`pipe.rs` blocks by having whoever wakes the sleeper finish its work
(translate its user buffer through its own `AddressSpace`, copy, set `rax`).
Sockets instead rewind the saved TrapFrame's `rip` by 2 — the width of
`syscall` — before parking, so the call re-executes from the beginning when
the process runs again (`rax` still holds the syscall number: the entry stub
only overwrites that slot on a normal return). Two reasons: `accept`,
`connect`, `sendmsg` and `recvfrom` each have a different completion (install
an fd; encode a `sockaddr`; consume an iovec), all of which would have to be
reproduced from inside another process's address space; and the old channel
code's single global `ACCEPT_WAITER`/`RECV_WAITER` slots meant a second
process blocking on the same operation silently overwrote the first, which
then never woke. Signals still work — a woken process passes through
`scheduler::resolve_signals` on its way back to user mode, so a handler runs
first and the rewound syscall re-executes after its `sigreturn`, which is
Linux's `SA_RESTART` behavior. `register_retry` (the half `FileHandle::read`/
`write` use, since they cannot block themselves) **requires every caller to
genuinely block afterwards**: rewinding and then returning normally would
re-enter `syscall` with `rax` holding a return value.

**`FileHandle::socket_id()`** (`vfs/src/file.rs`) is how a socket syscall
gets from an fd to a socket: `dyn FileHandle` can't be downcast in `no_std`.
It replaced a global `[[ChannelId; MAX_FILES]; MAX_PROCS]` side table indexed
by pid, which silently did nothing past its bound and had to be
hand-maintained at every fd-allocating call site; `poll`/`epoll` snapshot the
mapping into their waiter instead (`SocketMap` in `syscall/poll.rs`), since a
wakeup can't reach another process's fd table.

**`bind()` has two halves.** A pathname address gets a real `S_IFSOCK` node
in the filesystem (`FileType::Socket`, `Inode::mksocket`, implemented by
ramfs — so `ls -l` shows it, `stat` reports it, `unlink` removes it) *plus*
an entry in `usock`'s bind registry, which is what `connect()` resolves.
That split is what gives Linux's error semantics for free: `connect()` to a
path with no node is `ENOENT` (the server was never started), to a node with
no live socket is `ECONNREFUSED` (it died), and a second `bind()` to a name
whose node still exists is `EADDRINUSE`. Abstract addresses
(`sun_path[0] == '\0'`) skip the filesystem entirely and free their name
when the socket closes.

**Interrupts on the wake path are saved and restored, never unconditionally
re-enabled** (`dispatch_wakes` uses `without_interrupts`, not
`SchedGuard`/`InterruptGuard`). One caller is `UnixSocketHandle::drop`, which
runs inside `sys_exit`, on the kernel stack of a process already queued for
deferred free — and CLAUDE.md's "`sys_exit` must keep IF=0 all the way to the
`iretq`" invariant is exactly that hazard. It was not theoretical: the first
boot of this code panicked in `timer_preempt`'s corrupt-frame detector the
moment a process holding a socket exited.

**Out of scope, deliberately:** `SOCK_SEQPACKET`, `SO_PEERCRED`/
`SCM_CREDENTIALS` (no uid model — every process is root), `MSG_OOB` (AF_UNIX
has none in Linux either), and non-blocking `connect()` handshakes
(`EINPROGRESS`): a stream connect completes or fails immediately here,
because `connect()` itself creates the server-side socket and queues it,
exactly as Linux's `unix_stream_connect` does.

**Tests.** `cd usock && cargo test` (71, the state machines);
`kernel/src/hw_tests.rs::unix_socket_handle_roundtrip` (the adapter, in a
real boot: `FileHandle` behavior, `socket_id()` through a trait object,
refcounting in `Drop`); `userspace/c/socket_test.c` (46 checks end-to-end
through real mlibc — socketpair, datagram boundaries, a pathname server,
the abstract namespace, error codes, `SO_*`, half-close, and passing a
descriptor with `SCM_RIGHTS`); `ipc_ping` and `poll_test` (the blocking
paths: 100 stream round-trips, and `poll()` woken by a socket rather than by
its own timeout).

## Pseudo-terminals (`tty/`, `kernel/src/ipc/pty.rs`)

Phase 3.3 of `docs/gui/gui-plan.md`. `/dev/ptmx` makes a pair (up to 16),
`/dev/pts/<n>` is its slave (listed while the master is open), `/dev/tty`
is the caller's controlling terminal (`ENXIO` without one). Every rule —
line discipline, a pair's hangup/`EIO`/`POLLHUP`/`SIGWINCH`, job control —
is in the host-tested `tty` crate (`cd tty && cargo test`); `ipc/pty.rs`
is the adapter, shaped like `ipc/unix.rs`: `PTYS`/`WAITERS` are
`diag::IrqMutex`es, **blocking restarts the syscall** (register, `rip -= 2`,
`WAKE_EPOCH`), and `tty::pty::Effects` become wakeups and signals applied
with `PTYS` released (SCHEDULER is never taken while it is held).

- **Sessions and the controlling terminal** (`Process::sid`/`ctty`,
  inherited by `fork`/`clone`, cleared by `setsid`): a session leader
  without one acquires a slave by opening it without `O_NOCTTY`, or with
  `TIOCSCTTY`. A background read (`SIGTTIN`), a background write with
  `TOSTOP`, or a background `TCSETS*`/`TIOCSPGRP` (`SIGTTOU`) signals the
  caller's group and restarts the call; an ignored/blocked signal or an
  orphaned group gets `EIO` (writes go through). An ioctl cannot block, so
  it restarts by rewinding `rip` and returning the syscall number.
- **The console is not a pty and has no controlling-terminal state**: its
  termios and foreground group stay the globals in `kernel/src/tty.rs`,
  its input the keyboard ring, as before.
- **Not implemented:** `VTIME` timing (a read that would wait for it
  returns what is there), `IXON`/`IXOFF`, and hanging up a terminal when
  its session leader dies (closing the master is the hangup).
- `poll`/`epoll` are real on both ends (`PollSource::Pty`,
  `FileHandle::pty_end`). mlibc: `posix_openpt`/`grantpt`/`unlockpt`/
  `ptsname`, `ttyname` (from the slave's inode number) and a real
  `tcflush`. BusyBox has `script`, `stty`, `tty`, `reset` and `ttysize`
  (`CONFIG_FEATURE_DEVPTS`); `script` needs `SHELL=/tmp/bin/sh` (there is
  no `/bin/sh`). Test: `userspace/c/pty_test.c` (11 cases, ash on a slave
  included).

## Runtime Tracing & Counters (`kernel/src/debug.rs`)

Named, independently-toggleable tracing subsystems (`MM`, `SCHED`, `FS`, `PROC`), gated by a runtime bitmask that defaults to all-off — tracepoints stay in the code permanently instead of being hand-added and stripped out per bug (which is what happened repeatedly before this module existed, and made a 2026-07-19 leak/panic investigation slow: the relevant line was buried under thousands of always-on `[COW]` lines). Add a tracepoint with `crate::ktrace!(crate::debug::MM, "...", args)` — a no-op (one relaxed atomic load + branch) when that subsystem is off. Toggle live, no rebuild: `kdebug mm on` / `kdebug mm off` (userspace program, `userspace/c/kdebug.c`, backed by syscall 403 `kdebug_ctl`). A handful of permanent counters (`forks_total`, `execs_total`, `reaps_total`, `cow_faults_resolved/failed`, `orphan_blocks_reclaimed`/`orphan_inodes_reclaimed`, `switches_total`) are always on and readable via `/proc/kdebug` (`cat /proc/kdebug`), same convention as `/proc/meminfo`. `switches_total` (full context switches since boot) exists because a per-switch `serial_println!` — the first thing tried to confirm `fpu_test` (see FPU/SSE above) was actually exercising the context-switch path — made exec()/page-fault-heavy boot phases crawl, running on literally every timer preemption; a plain atomic counter is free by comparison. The former always-on `[COW]`/`[RPU]`/`[EXEC]` debug prints in `memory/address_space.rs`, `memory/page_table_manager.rs`, and `process/syscall.rs` are now `ktrace!(MM, ...)`/`ktrace!(SCHED, ...)` calls under this module.

Beyond the counters, three always-on **lock/invariant diagnostics** render into the same `/proc/kdebug` report (and into `panic.rs`'s allocation-free panic snapshot): `LockDiag`/`SCHEDULER_LOCK` (acquires/releases/outstanding + last acquirer's `file:line`), `DirLockDiag`/`RAMFS_ENTRIES_LOCK` (same, but keyed by PID + operation name — `file:line` can't tell two `mkdir()`s apart when they all lock from the same inlined site), and `TfRewindDiag`/`TF_REWIND` (a process resumed from a `TrapFrame` older than its last save; fed by `scheduler::tf_note_save`/`tf_note_resume` and the `Process::tf_seq`/`tf_awaiting_resume`/`tf_last_resumed_seq` fields). All three are passive — a couple of relaxed atomics per operation, no prints except `TF_REWIND`, which prints only when it actually fires (it should never). They are what survived the 2026-08-05 hang hunt (`docs/hang-hunt-bug2-findings.md`); the rule that hunt produced is that a detector whose validity depends on the test harness must be retired *with* the harness — the orphan-lock panics assumed "only PID 1 touches the VFS", true under the amplifier and false the moment real busybox runs.

## Device Driver Framework (`kernel/src/drivers/`)

Drivers implement `FileHandle` (trait in `process/file.rs`): `read`, `write`, `close`, plus optional `stat`/`dup`/`getdents64` (defaults: no metadata, not dup-able, `ENOTDIR`). Device drivers are stateless (state lives in kernel globals), so their `dup()` impls just construct a fresh instance of the same type.

**`/dev/fb0` and graphics mode** (`drivers/dev_fb0.rs`; phase 2.1 of `docs/gui/gui-plan.md`): exclusive (second `open` is `EBUSY`; no shadow is `ENODEV` — `DeviceEntry::open` is fallible now), and **holding it is graphics mode** (`framebuffer_console::enter/leave_graphics_mode`, Linux's `KD_GRAPHICS`): the console parses and mirrors `/dev/fb` writes to serial/`klog` but draws nothing, the cursor stops, `FBIO_BLIT` is `EBUSY`; `kalert!` and the panic screen still draw. The mode ends in `Drop` of the last handle (so `SIGKILL` of the holder hands the console back, cleared). `mmap(MAP_SHARED)` maps the **RAM shadow**, not VRAM, through a *pinned* `ShmObject` (`ShmObject::pinned`: built once from the shadow's frames, checked page by page against the live page table, kept in a `static`, never resized or released) — so it is an ordinary shared mapping with phase 1's fault/fork/teardown paths, and the object's own reference keeps unmaps from ever freeing the frames. `FBIO_GET_INFO` (`0x4642_0010`: geometry + offset of pixel (0,0) in the mapping, the shadow's `SHADOW_SKEW` within its page) and `FBIO_FLUSH` (`0x4642_0011`: up to 16 rects → `Framebuffer::flush_rect`); `flush` stays the only VRAM writer. `/proc/fbinfo` has `mode: text|graphics`. Tested by `userspace/c/fb0_test.c` (`fb0_test hold` keeps its picture up 5 s for a `screendump`).

**The compositor** (`userspace/src/bin/compositor.rs`, `gui_demo.rs`; phase 2.5 of `docs/gui/gui-plan.md`): `compositor [prog...]` holds `/dev/fb0`, grabs `event0`, reads `event1`, listens on `/tmp/gui-0` and runs `gui::compositor::Compositor` under one `epoll`; Ctrl+Alt+Backspace quits. A bare program name is looked for in `/bin`, then `/mnt/bin` (where everything linking `userspace::text` lives). Children it starts close every fd ≥ 3 before `exec` (no close-on-exec here). `REL_Y` is PS/2-signed (up positive) and is negated for the screen. Tested end to end by `scripts/gui-e2e.sh` (screendumps + serial log; PS/2 or USB input through the usual `QEMU_*` variables). The compositor ignores `SIGINT`/`SIGTSTP` (the keyboard grab still lets ^C/^Z signal the console's foreground group — its own) and starts each child in a group of its own with the defaults back; `^\` (`SIGQUIT`) is the escape hatch that stays.

**The windowed terminal** (`userspace/src/bin/term.rs`, phase 3.5): `compositor term` — a pty, `busybox ash` on the slave as a session leader with it as controlling terminal (`TERM=xterm-256color`), the `vt` crate in between, one frame per `frame` callback, key repeat of its own; exits when the master reports `EIO`/hangup. `scripts/gui-e2e.sh term` checks it on screendumps (colour rows drawn with `printf`, `^C`, `vi`'s alternate screen, `exit`). Verified on the Ryzen by hand.

**Proportional text** (`userspace::text`, `text/`, `docs/gui/text-plan.md`): `Text::load()` reads Noto Sans/Sans Mono from `/mnt/usr/share/fonts`; `textdemo` (disk-resident, `DISK_RUST_PROGRAMS` in `kernel/build.rs`, run as `compositor textdemo`) and `cpumon` use it shows it and logs every box `measure` gave; `scripts/gui-e2e.sh text` checks those boxes against the ink on a screendump.

**DOOM, Quake and `fire` in a window** (`userspace/c/include/constanos_gfx.h`, header-only, over `constanos_gui_wire.h`): `gfx_present()` a `0x00RRGGBB` frame, `gfx_next_event()` evdev-shaped events (PS/2 sign convention). With `$GUI_DISPLAY` set (the compositor passes it to its children, `term` to its shell, like `WAYLAND_DISPLAY`) it is a window, integer-scaled into shared memory; otherwise `FBIO_BLIT` + `event0`/`event1` as before. Games lock the pointer (`lock_pointer`/`relative_motion` in the `surface` interface — Wayland's pointer-constraints folded in): active only while focused, Ctrl+Alt releases it, a click takes it back. The C wire code is tested against the `gui` crate by `gui/tests/c_wire.rs` (needs the host `cc`). Verified on the Ryzen. Rust programs get the same through `userspace::gfx` (`Gfx::open(args.env(b"GUI_DISPLAY"), ...)`, `present`, `next_event`) and draw their frame with the `draw` crate — `snake` uses both.

**Kernel-originated on-screen notices** (`drivers/framebuffer_console.rs`): `crate::kalert!("...", args)` renders one bright-red line through the console's normal text path (ANSI parser, scrolling, cursor bookkeeping — `render_bytes`, split out of `FramebufferConsole::write` so there is one renderer, not two). It exists for the real-hardware bring-up case, where there is no serial capture at all: on that machine a killed process and a hard hang look *identical* — the screen simply stops changing while the PIT-driven cursor keeps blinking — and telling those apart cost real time on 2026-09-21. The one caller is `init::devices::kill_current_user_process`, so every fault-induced death (divide-by-zero, invalid opcode, GPF, unhandled page fault) is announced on screen; a normal `exit()` is not. It takes **`try_lock` on both `FB_STATE` and `FRAMEBUFFER`, never `lock`** — the caller is a fault handler that can run while the interrupted process sits mid-`write` holding either one, and blocking there would turn a process death into exactly the freeze it exists to rule out (same "skip this beat" strategy as `tick_cursor_blink`). It writes no `core::fmt` into a heap buffer either — `ConsoleWriter` formats straight through `render_bytes`. Deliberately does not mirror to serial: every caller already has its own `serial_println!`.

**Framebuffer console performance, and the instrument that measures it**
(`kernel/src/framebuffer.rs`, `/proc/fbinfo`, `hal::memtype`, `diag::OpStat`;
full record in `docs/fb/console-perf.md`): the console is imperceptibly fast in
QEMU, where the framebuffer is host RAM, and was a second per screen-clear on
the physical AM4 machine, where it lives across PCIe — `ash` redraws its whole
line after a backspace and emits `ESC[J`, which used to mean ~14,000 cells x 64
pixels of individual bounds-checked stores. `Framebuffer::fill_rect` replaced
that with one contiguous byte range per scanline (`memset` for black,
a repeated pre-built pixel pattern otherwise) and `clear`/`clear_row_from`/
`clear_rows`/`ESC[J`/`ESC[K` all route through it. **Measured A/B, same build
but for that one function: 1,975,481,800 cycles (534 ms) → 4,811,369 (1.3 ms),
410x** — in QEMU, which is exactly the environment that hid the problem, so the
magnitude on the target machine still has to come from the target machine.

`cat /proc/fbinfo` is how it does: real geometry (`stride` is not `width`),
physical address, the PTE's own PAT/PCD/PWT bits, `IA32_PAT`, and whichever
MTRR covers the aperture — decoded by `hal::memtype` (pure, host-tested), read
by `kernel/src/memory/memtype.rs` (`rdmsr` + `memtype::leaf_for`, a raw page-table walk: `x86_64::PageTableFlags` drops bit 12, a large page's PAT bit).
It deliberately reports the MTRR and PAT types **separately** rather than
combining them into one verdict: the SDM's MTRR x PAT table is the one thing
here that is easy to get wrong from memory, and the ground truth is the
measured `MB/s` beside it, which needs no table. Every primitive carries a
`diag::OpStat` (calls/bytes/cycles/**min**/**max**), plus an
`instrument_overhead` line measuring an empty measurement live — 74 cycles in
QEMU, so nothing below it is instrument artifact. `min`/`max` exist because the
deltas are wall-clock and the console does not run with interrupts off: a
preemption inside a measured call charges it for another process's time
(`draw_char` averages ~115k cycles idle and ~1.5M under load, for identical
work), and showing the spread is what keeps the mean from silently lying.

What the instrument then *decided*, rather than what seemed plausible: the
cursor's `xor_rect` read-modify-write is 4% of console time and the serial
mirror 0.9% — both left alone despite both being on the original suspect list
— while **`scroll_up` is ~83%**, at a measured ~1.06 ms per scrolled line
(it reads the entire framebuffer back, and a VRAM read is non-posted). That one
has no local fix: it needs either write-combining (`/proc/fbinfo` already
reports the two facts that decide it — the reset PAT has *no* WC entry at all,
so `IA32_PAT` must be reprogrammed first, and QEMU's own aperture MTRR is UC)
or a RAM shadow buffer. **The shadow is now done** (phase 1 of
`docs/fb/wc-shadow-plan.md`, measured on metal first: there a scroll took
2.18 s, 99.9% of the console's time, reading VRAM back at ~4 MB/s).
`framebuffer::attach_shadow()` (in `init::boot`, right after
`test_allocators`) gives the `Framebuffer` a WB-RAM copy of the aperture;
every primitive draws there and marks a `hal::fbdirty::DirtyRect`, and
`Framebuffer::flush` is then the **only** code that touches VRAM, write-only,
measured as `fb_flush`. Correct by default: outside a batch each primitive
flushes its own rectangle, so callers that never heard of the shadow
(`panic.rs`, `draw_boot_screen`, `FBIO_BLIT`, the cursor ISR) need no change;
`render_bytes` wraps each write in `begin_batch`/`end_batch` (nesting) so a
400-line write is 400 RAM `memmove`s and one flush. **Any new code that
writes VRAM without going through `Framebuffer`'s primitives desyncs the
shadow.** Best-effort: if the allocation fails the console stays in direct
mode, and `/proc/fbinfo` says which mode is live (`shadow:`). Two things
found on the way: the shadow starts `SHADOW_SKEW` bytes into its allocation
because a same-aligned shadow and aperture made `fb_flush` 8.6x slower in
QEMU (TLB-slot collisions; see the constant's comment), and
`mm::buddy::allocate` now returns `None` for an order above `MAX_ORDER`
instead of panicking. In QEMU the shadow is slightly *slower* (VRAM is host
RAM there, so it only adds a copy). **On the Ryzen it took `seq 1 400` from
875 s to 8.2 s** (one 400-line `write()`: 0.32 s); what remains is
`fb_flush` writing UC VRAM at 412 MB/s, ~98% of the time. Phase 2 (PAT) is done and verified on the Ryzen: `memory::memtype::program_pat()`
(in `init::boot` right after `test_allocators`) makes PAT entry 1 —
`PWT` set, `PCD` clear, at any page size — **WC**, changing only that entry
(`WB WC UC- UC WB WT UC- UC`), after walking the live page table (tables,
leaves and CR3) to prove nothing selects index 1 yet; the outcome is
`pat_program:` in `/proc/fbinfo`. **So any new mapping with
`WRITE_THROUGH` but not `NO_CACHE` is write-combining, not write-through.**
Phase 3 is done too, and measured on the Ryzen: `fb_flush` 412 → ~5,600 MB/s, `seq 1 400` 8.2 s → 0.75 s (QEMU does not model WC and shows no change):
`framebuffer::map_write_combining()`, right after `program_pat`, points
the aperture's leaves at index 1 through `memtype::set_pat_index_range`
(all-or-nothing; refuses a large leaf that reaches outside the range),
reported as `fb_wc:` in `/proc/fbinfo`; `flush` and direct-mode
primitives end with `sfence`, since WC stores are weakly ordered. Phase 4 did only what the numbers asked for: `blit_scaled` builds one
destination scanline per source row with aligned 32-bit stores and
replicates it with `memcpy` — 26 ms → 5.2 ms per DOOM frame on the Ryzen.
At `opt-level 0` with `build-std`, `write_unaligned` is a call into
`copy_nonoverlapping` with runtime UB checks, which cost most of that win
until it was replaced by a plain aligned store; per-pixel hot loops here
need that care.

**Console font and colours** (`drivers/framebuffer_console.rs`): text is
Noto Sans Mono, pre-rasterised with antialiasing by the `noto-sans-mono-bitmap`
crate (no_std, no alloc; regular + bold, 16/20/24/32 px, basic Latin only),
drawn by `Framebuffer::draw_glyph` (coverage blended fg-over-bg, aligned 32-bit
stores). The size is picked once from the screen height by `init_font`, called
in `init::boot` right after the framebuffer is registered: 1920x1080 gets
24 px (11x24 cells, 174x45), QEMU's 1280x800 gets 20 px. The crate must be >= 0.3: 0.2 rasterised every glyph low in its box, so descenders (`g j p q y`) ran past the bottom row and were cut off. SGR 1/22 (bold) and
7/27 (reverse) are honoured; the palette is tuned for black (One Dark-like)
because VGA's blue `(0,0,170)` — what `ls --color` gives directories — was
unreadable on it. The 8x8 `font8x8` path (`draw_char`/`draw_text`) remains
only for the panic screen. `/proc/fbinfo`'s `text_grid` reports the live cell
size.

Register a new driver by:
1. Creating `kernel/src/drivers/<name>.rs` implementing `FileHandle`
2. Adding one entry to the `DEVICES` static slice in `drivers/mod.rs`

Current devices: `/dev/ptmx` + `/dev/pts/N` + `/dev/tty` (pseudo-terminals, see below), `/dev/null`, `/dev/zero`, `/dev/console` (serial), `/dev/fb` (framebuffer console), `/dev/fb0` (the compositor's screen — see below), `/dev/kbd` (non-blocking keyboard, char/ANSI stream — fed by the PS/2 ISR *and* by the polled USB HID keyboard driver, see the USB section below: USB key presses are translated to Set-1 scancodes and enter through the same `keyboard::process_scancode`, so every device here behaves identically whichever keyboard is attached), `/dev/input/event0` and `/dev/input/event1` (non-blocking, wire-compatible with real Linux evdev — each `read()` returns one real `struct input_event`, 24-byte-record layout shared via `drivers/evdev.rs`). `event0` is the keyboard (`EV_KEY` + a real `linux/input-event-codes.h` `KEY_*` code + press/release value, followed by an `EV_SYN`/`SYN_REPORT`, sourced from the PS/2 IRQ's raw scancode decode — see `drivers/dev_input_event.rs`; note the underlying ring buffer fills from every keypress since boot, so a game must drain the backlog at startup, see `doom-port/doomgeneric_constanos.c::DG_Init`). `event1` is the PS/2 mouse (`EV_REL` `REL_X`/`REL_Y` for relative motion, `EV_KEY` `BTN_LEFT`/`BTN_RIGHT`/`BTN_MIDDLE` for buttons — see `mouse.rs` for the 8042 aux-device enable sequence + 3-byte packet decode, and `drivers/dev_mouse_event.rs` for the evdev translation; the decoder drops a partial packet after 500 ms of silence, as Linux's psmouse does, since the bit-3 guard alone stays shifted forever after one lost or extra byte — `mouse_resyncs` in `/proc/kdebug` counts it; test with `qemu-debug.sh mouse-move` + `cat /dev/input/event1 | wc -c` — this busybox has no `dd`). Both back the DOOM port's input (keyboard + mouse-look). `/dev/input/*` lives under a one-level-deep devfs subdirectory (`fs/devfs.rs::InputDirInode`) — devfs is otherwise flat, so this is a hardcoded special case, not a general nested-device mechanism. `/dev/dsp` (`drivers/dev_dsp.rs`) is a write-only, fixed-format (48000 Hz stereo s16le) PCM sink backed by the AC97 PCI driver (`ac97.rs`) — see below.

**`/proc/pci`: what nothing drives** (`hal::pci`, `pci::claim`/`render_report`): every function on every bus — BDF, class, `vendor:device`, subsystem, revision, IRQ line, **the driver that claimed it or `-`** — under a three-line summary whose `unclaimed (bridges excluded)` is the list of devices worth a driver. Stage 1 of the self-improving-OS direction: the system has to know what it lacks before anything can go and get it. **A driver that takes over a PCI function must `crate::pci::claim` it** (xhci, ac97 and sp5100_tco do; `block::ata`, which reaches its channel through legacy ports, claims the compatibility-mode IDE function via `claim_matching`); otherwise it reports as unclaimed. The display controller stays unclaimed on purpose — the GOP framebuffer is not a driver for it, nothing here programs it. Decoding is host-tested against the target board's real configuration headers (`hal/fixtures/ryzen-pci-config.txt`), field by field against Linux's sysfs for the same 42 functions. On that board the unclaimed list is where the next drivers are: `10ec:8168` (RTL8111 Ethernet), `1987:5012` (NVMe), the SATA and HD-audio functions.

**PCI + AC97 audio** (`pci.rs`, `ac97.rs`): this kernel's only PCI-aware code — `pci.rs` does raw 0xCF8/0xCFC config-space access and a bus-0 device scan (nothing else in this kernel enumerates PCI; every other driver targets a fixed legacy ISA port). `ac97.rs` finds the Intel 82801AA AC'97 codec (`-device AC97` in QEMU), does the cold-reset + PCM-out-stream-reset + mixer-unmute sequence, and runs a **polling**, not interrupt-driven, bus-master DMA ring: the IDT is a `spin::Once`, populated once as literally the first line of `boot()` before `memory::init_core` — wiring up a PCI IRQ whose vector is only known after enumeration doesn't fit that without either an early pre-memory PCI scan or a bigger IDT refactor, so `write_pcm()` instead polls the hardware's CIV register directly and blocks (spinning, no lock held across the spin, so the timer ISR/scheduler still preempts normally) until a buffer-descriptor slot frees. The 32-entry hardware BDL aliases only 8 real physical ring buffers (`entry[i].addr = slot_phys[i % 8]`) so the hardware's native mod-32 index wraparound still works correctly without needing all 32 to be distinct allocations. Fixed format only (48000 Hz stereo s16le, AC97's native non-VRA operating point) — no `ioctl` negotiation, matching the same "one client, one format, document it" simplification `/dev/input/event0`+`event1` already use.

VFS mounts (`kernel/src/fs/mod.rs`): `/dev` (devfs), `/` (initramfs, embedded ELFs — a real two-level tree: root contains a real `bin` subdirectory and an `etc` one (`ETC_FILES`, data compiled in: `localtime`, a 54-byte UTC TZif — mlibc's `localtime()` ignores `TZ` and *panics* without `/etc/localtime`, which killed `uptime` and `date`; `passwd` and `group` with `root` (uid/gid 0, home `/tmp`, shell `/tmp/bin/sh` — PID 1 exports `HOME=/tmp` to match), so `id`, `ps`, `ls -l` and `find -user` print names), `/bin/<name>` is a genuine directory lookup, not a second mount aliasing the same flat namespace, see `fs::initramfs`), `/tmp` (ramfs, writable — the filesystem itself is `vfs::ramfs::RamFs`, host-testable; `kernel/src/fs/ramfs.rs` only supplies the `DirLockObserver` that wires its lock diagnostic into `/proc/kdebug`, see below), `/mnt` (ext2, read-write, best-effort — see the ext2 section below), `/proc` (procfs, read-only, synthetic — `/proc/meminfo` generated fresh on every `open()` from the live Buddy allocator stats; `/proc/self` and `/proc/<pid>/exe` are real symlinks, `/proc/dmesg` is the kernel log ring, `/proc/fbinfo` is the framebuffer console's instrument panel, `/proc/pci` lists every PCI function with the driver that claimed it, `/proc/sensors` the CPU temperatures (see CPU Temperature), and `/proc/stat`, `/proc/uptime`, `/proc/loadavg`, `/proc/cpuinfo`, `/proc/<pid>/stat` (all 52 fields, real times and `rss`), `/proc/<pid>/statm` and `/proc/<pid>/cmdline` are in Linux's formats (see CPU Time Accounting and Resident Set Size) — see `fs::procfs`, the kernel-log section below, the framebuffer-performance note above, and the PCI note in the Device Driver Framework section). `ls /` also shows every other mount (`dev`, `tmp`, `mnt`, `proc`) as an entry — `fs::vfs::direct_children` (a thin delegate onto `vfs::mount::MountTable::direct_children`, see below) lets initramfs's root directory list them dynamically, same idea as a real Linux rootfs pre-creating empty `/proc`, `/dev`, etc. that mounts later overlay; actual traversal into them is still redirected by the mount table before ever reaching initramfs, so they only need to look like directories, not serve one.

**Storage stack seam** (`hal::block::BlockDevice`, `hal/src/block.rs`; `kernel::block::AtaBlockDevice`, `kernel/src/block/mod.rs`): `fs::ext2` no longer calls `block::ata::{read_sectors,write_sectors,present}` directly — it goes through `Ext2Fs::core.device: Box<dyn BlockDevice>` instead (`Ext2Core`, from the standalone `ext2` crate — see below), the same seam shape as `hal::PortIo`/`hal::PhysMem` (see `docs/drivers/architecture.md`'s storage-stack section), sector-granular (512 bytes) rather than filesystem-block-granular. `AtaBlockDevice` (zero-sized, wraps `block::ata`'s existing free functions) is what `fs::ext2::init()` mounts against at real boot; `hal::block::MemDisk` (`Vec<u8>`-backed, host-tested in `hal`) is what both the `ext2` crate's own host tests and the QEMU integration tests (`kernel/src/hw_tests.rs::ext2_memdisk_roundtrip` and `ext2_reclaim_orphans_clears_injected_disk_img_shape`) mount instead, exercising ext2's full read-write path with zero risk to the real `disk.img`. Explicitly a *partial* migration: `block::ata.rs` itself is still not seamed onto `PortIo` the way the six drivers in `docs/drivers/architecture.md`'s "Current status" are — only the layer above it (`fs::ext2`) moved.

**Filesystem: ext2 (`kernel/src/fs/ext2.rs`, mounted read-write at `/mnt`).** Split across two crates as of `docs/fs/ext2-extraction-plan.md`'s (now complete) extraction: the standalone `ext2` crate (`ext2/src/`, `no_std` + `alloc`, `cd ext2 && cargo test` — 94 host tests, no QEMU; two of them share temp-file paths and can fail intermittently, see `docs/fs/ext2-test-flake.md`) owns every byte-level detail — on-disk layout/parsing, block/inode allocation, direct/singly/doubly/triply-indirect addressing (~16 GiB+ files at this driver's 1024-byte block size), directory operations, symlinks, and the mount-time repair passes described below — as methods on `ext2::Ext2Core`, speaking only in inode numbers/byte ranges/its own `Ext2Error`, never VFS types. `kernel/src/fs/ext2.rs` is a thin adapter on top: `impl Filesystem/Inode/FileHandle for` types wrapping an `Ext2Core`, `From<Ext2Error> for Errno`, the `EXT2: Once<Ext2Fs>` global + `EXT2_LOCK`, and the raw-`file_type: u8`↔`fs::types::FileType` conversion at the directory-op boundary, and the wall-clock reads (`crate::time::now_unix_secs`) the core can't do for itself. Supports `create`/`mkdir`/`unlink`/`rmdir`/`rename`, real symlinks (`Ext2Inode::symlink`/`readlink`, both ext2's "fast" representation — target inline in `i_block`'s own bytes, under 60 bytes, no data block allocated — and "slow" — target stored as ordinary file content, this driver writes whichever fits and reads both), and real `chmod`/`fchmod` (persists `i_mode`'s permission bits — the one filesystem here where `stat()` reports genuine per-file mode instead of a hardcoded constant). A single coarse `EXT2_LOCK` serializes every mutating op (bitmap scans aren't atomic and this kernel is preemptible); read-only paths (`lookup`/`readdir`) don't take it, since every mutating method already holds it while calling them internally and `spin::Mutex` isn't reentrant. Test-only hand-built disk images (`ext2::testimg::build_minimal_image`/`build_image_with_orphans`) are a single shared source in the `ext2` crate, imported both by that crate's own tests and by `kernel/src/hw_tests.rs`'s QEMU integration tests — there is no more kernel-local `TestFs`/duplicate image-builder copy.

No journal, so a crash mid-operation can still leak an allocated-but-unlinked block/inode — every multi-step mutation orders its writes "allocate & write content, then link" so a crash can only ever leak, never dangle. Two passes at mount time (`Ext2Fs::mount()`'s callers in `init()`, before `/mnt` is exposed to the VFS) clean up after exactly that: `reconcile_free_counts` recomputes the BGD/superblock free block/inode counters from the bitmaps directly (those are separate, independently-flushed writes from what they summarize, so a crash between them drifts the counts), and `reclaim_orphans` walks every inode actually reachable from root (mirroring real `e2fsck`'s passes 1-4) and frees any block/inode the bitmaps mark used that the walk never reached — zeroing each reclaimed inode's on-disk record (`i_mode`/links/pointers) and stamping `i_dtime` as it goes, exactly as `unlink`/`rmdir` do, because `e2fsck`'s Pass 1 scans the raw inode table rather than the bitmap and reads a left-behind record as a disconnected inode needing `lost+found` no matter how correct the bitmap is. Its two sweep phases are ordered inodes-then-blocks for the same "leak, never dangle" reason the mutations are: freeing an orphan's blocks first and crashing there would leave a live-looking record pointing into blocks a later allocation can hand to someone else. Both passes now have a real `e2fsck -fn` oracle test in `ext2::repair` (a genuine `mke2fs` image; the orphan fixture is built by `debugfs`'s own `unlink`, whose documented refusal to adjust link counts produces exactly this shape). What stays out of scope, by construction: an inode whose bitmap bit is *already clear* but whose record still holds live content — see the `disk-img-phantom-orphan` diagnosis.

**Critical ordering invariant in `reclaim_orphans`:** the reachability walk (`mark_reachable(ROOT_INO, ...)`) must run *before* the reserved-inode range (`1..first_ino`, which includes root's own inode 2) gets pre-marked "used" — `mark_reachable`'s own cycle guard treats an already-marked bit as "already visited, nothing more to do here." Pre-marking root first used to make the very first call return immediately without ever reading root's blocks or descending into a single child, silently treating the *entire* real directory tree as unreachable — the sweep then freed nearly every live block/inode on every fresh mount, and the next allocation handed out an already-live block to unrelated file data, corrupting whatever legitimately owned it. This is exactly what produced an `add_dir_entry` "range end index ... out of range" panic the first time this surfaced: root directory's own data block had been reused for a new file's content. Also guarded: the superblock's own block (at `first_data_block`, easy to mis-place one-off with "everything strictly before it") and sparse_super's backup superblock+BGDT copies in other block groups, both reserved unconditionally per group rather than replicating mke2fs's exact backup-placement rule (group 0, 1, and powers of 3/5/7) — reserving a slot that turns out not to have a backup costs nothing, since the real per-group bitmap never marks it used anyway.

`unlink`/`rmdir` must persist the deleted inode's zeroed record (`write_inode`) *before* clearing its bitmap bit (`free_inode`) — `free_all_blocks` only updates the in-memory copy; skipping the write-back left a stale, pre-delete record (nonzero mode, dangling block pointers into blocks the bitmap already shows free) that a real `e2fsck` flags as a disconnected inode. `i_dtime` (deletion timestamp) is stamped with a real Unix epoch (`crate::time::now_unix_secs()`) — a raw boot-relative uptime value there is small enough to collide with a different on-disk use of that same field (ext3+ threads its in-progress orphan-inode list through `i_dtime` as a next-inode-number link), which `e2fsck` misdiagnoses as a corrupted orphan chain purely because the value looks too small to be a real calendar time.

## USB Keyboard (`hal/src/{xhci,usb,hid}.rs`, `kernel/src/usb/`, `kernel/src/memory/mmio.rs`)

Written because the physical AM4/Ryzen machine this kernel is brought up on
has **no PS/2 port at all** — it booted to a shell nobody could type into,
every input path in this kernel having gone through the 8042.

**The design decision worth knowing:** a USB key press is translated into
the PS/2 Set-1 scancode the same key would have produced and fed into the
existing `keyboard::process_scancode`. Nothing downstream learns USB
exists — `hal::keyboard::KeyDecoder`'s Shift/Ctrl/CapsLock state machine,
the ANSI arrow sequences, `tty::feed_input`'s Ctrl-C handling, `/dev/kbd`
and `/dev/input/event0`'s evdev records (whose `KEY_*` codes are themselves
derived from Set-1, see `drivers/dev_input_event.rs`) all work unchanged
and identically for both keyboards. One translation table
(`hal::hid::usage_to_set1`) replaces a parallel copy of all of that, and a
machine with both keyboards gets both merged into one stream with no
arbitration, exactly where two PS/2 keyboards would merge.

**Split across the usual seam.** `hal::xhci` (register/TRB/ring/context
arithmetic), `hal::usb` (descriptor parsing + setup packets) and
`hal::hid` (boot-report diffing + the Set-1 table) are pure and host-tested
— most of `hal`'s 340 tests (with `hal::msc`/`hal::gpt`, below). `kernel/src/usb/xhci.rs` owns the MMIO window,
DMA pages, doorbells and waiting. That line is drawn hard here because an
xHCI bring-up failure is nearly unobservable (a wrong bit in a device
context yields no fault, no log, just a Transfer Event that never arrives)
and the target machine has no serial capture, so a mistake costs a reboot
to find rather than a second.

**Polled, not interrupt-driven**, for exactly `ac97.rs`'s reason (the IDT
is a `spin::Once` filled before PCI enumeration exists): `usb::poll()` runs
off the 100 Hz PIT tick in `timer_preempt_handler`, before the scheduler
lock is taken. It `try_lock`s (a tick landing mid-enumeration skips its
turn — the `tick_cursor_blink` strategy) and **returns** decoded scancodes
rather than dispatching them, so the driver's lock is released before
`process_scancode` runs — that path can take the scheduler lock to deliver
SIGINT. A keyboard's interrupt endpoint has an 8 ms service interval, so a
10 ms poll adds at most one interval.

**`memory::mmio::map`** is new and exists for this driver: the first
memory-BAR device here. It maps 4 KiB pages **uncached** (PWT|PCD) in a
free higher-half PML4 slot, rather than reusing the bootloader's
physical-memory window like ac97's DMA buffers do — that window is
write-back cacheable, and a cached read of a status register can return a
stale value. Picking an *unused* PML4 entry is what makes the mapping
inherited by every later process (`OwnedPageTable::new_user` copies
non-user kernel entries), which is what lets the timer ISR touch these
registers under any address space. DMA buffers deliberately stay on the
cacheable window — x86 DMA is cache-coherent. Measured, not assumed: QEMU's
`qemu-xhci` BAR lands at `0xc0_0000_0000`, above the 4 GiB the bootloader
window is only guaranteed to cover.

**`pci.rs` gained a second discovery path** (`for_each_by_class`,
`enable_mem_and_bus_master`): by class code (0x0C/0x03/0x30) rather than
vendor/device ID, across all 256 buses rather than bus 0, assembling a
64-bit memory BAR from BAR0+BAR1. All three differ from what ac97 needed,
which is why `find_device` was left alone instead of loosened.

**The event ring is read ownership-bit first, and that is load-bearing.**
`next_event` reads dword 3 (which carries the cycle bit) *before* the
payload dwords, because the controller writes the cycle bit last — that
write is what hands the TRB over. Reading the payload first races the DMA
write: dwords 0-2 get sampled before the controller writes them and dword 3
after, producing an event whose type/slot/endpoint are fresh while its
completion code and TRB pointer are still zero. Each torn read also
*consumes* the ring slot, so the real event that follows is lost and its
transfer times out a second later.

This is the bug that made the driver fail on real hardware while passing in
QEMU, where the device model writes the whole TRB atomically with respect to
the guest and the window does not exist. It was invisible for three
bare-metal cycles because every symptom was a plain `Timeout`; it only
became findable once unmatched events were logged instead of silently
dropped, and then showed as
`slot 3 ep0 unknown-trb stage failed: Failed(0) ? (trb=0x0)` — completion
code 0, which the specification never assigns, beside a null pointer. Note
the asymmetry it fixed: `Dma::write_trb` already wrote the cycle bit last
for the mirror-image reason, so the producer side was right and the consumer
side was backwards.

**Errors are matched by slot + endpoint, never by TRB pointer alone.** The
original `control_transfer` recognised a Transfer Event only by matching the
data or status TRB's address, so an error reported against the *Setup Stage*
TRB (whose address was never recorded) fell through to the discard path and
the genuine completion code was thrown away — every failure then read as a
timeout. `handle_async_event` now logs (bounded) rather than dropping.

**Recovery:** a `Stall` leaves the endpoint Halted and silently completing
nothing, so `control_transfer` issues Reset Endpoint + Set TR Dequeue
Pointer and retries once, logging both. The port reset likewise retries up
to three times (`hub_port_reset` in Linux does the same), and Address Device
once — every retry is logged, so a boot that only works because of one says
so rather than looking like it always worked.

**Deliberately out of scope:** hot-plug (ports are enumerated once at boot
— enumeration waits milliseconds on hardware, which cannot happen in the
timer ISR, and this kernel has no kernel-thread context to defer it to),
external hubs (root-hub ports only; a hub needs the hub class driver plus
route strings), and anything but boot keyboards, boot mice and storage
(addressed so they appear in the boot log, then left alone).

**USB mouse** (`hal::usb::find_boot_mouse`, `hal::hid::decode_boot_mouse`):
the boot-protocol mouse joins the PS/2 mouse's event queue behind
`/dev/input/event1` (`mouse::push_usb_event`), the keyboard's "feed the
existing pipeline" decision again. **HID Y is positive down, so it is
negated** into `MouseEvent`'s PS/2 convention (positive up), which the
DOOM/Quake ports were written against — verified in QEMU that the same
`mouse-move 10 -5` yields identical records through either mouse. The
queue now has two producers — a `hal::ring::Ring` behind an `IrqMutex` (`mouse::MOUSE_EVENTS`; the keyboard queues likewise, see Key Design Invariants' SMP rules).
Keyboard and mouse are looked up **independently per device**, and both
endpoints go into one Configure Endpoint: a keyboard+mouse receiver has
one interface of each, and the target machine's HyperX Pulsefire Core
mouse declares a boot *keyboard* on interface 1 (for its macro buttons) —
which is why the Ryzen reported two USB keyboards. An interface that
refuses `SET_PROTOCOL` is dropped alone, never its sibling.
`/proc/kdebug`: `usb_mice`, `usb_mouse_reports`. QEMU:
`QEMU_USB_MOUSE=1` (QEMU has no composite device, so the two-interface
path is only exercised on metal).

**Testing it in QEMU:** both launchers attach `qemu-xhci` by default, so
the bring-up path runs on every boot; `QEMU_USB_KBD=1` additionally
attaches a `usb-kbd`. That flag is opt-in rather than default because QEMU
routes monitor `sendkey` events to whichever keyboard it considers current
— with it set, `scripts/qemu-debug.sh send "text"` arrives through xHCI
instead of the 8042, which is exactly how the driver is tested end to end
(`QEMU_DEBUG_NO_USB=1` omits the controller entirely). Verified: typing,
backspace, arrow-key history recall, `usb_key_reports` in `/proc/kdebug`
counting the reports, 12/12 clean boots under `boot-matrix.sh 4 3`, and
the USB 2 port-reset branch via
`-device qemu-xhci,id=xhci,p2=4,p3=0` (QEMU puts a high-speed keyboard on a
USB 3 port otherwise).

**The on-screen summary reports counts, not a verdict** — controllers up,
ports, connected, addressed, other devices, setup errors, keyboards. The
first bare-metal attempt produced nothing typeable with no way to tell
whether the controller was missing, the ports were empty, or a keyboard had
been addressed and then failed to deliver: three problems with three
different next steps. (That attempt also showed nothing on screen at all —
see the kernel-log section below for the screen-clear bug that ate it.)

**`/proc/kdebug` carries `usb_keyboards` and `usb_key_reports`** — the
counter that separates the two failure modes of a USB keyboard that types
nothing: zero reports means the controller is not delivering transfers at
all, nonzero means they arrive and the fault is in the decode or below.
That distinction is otherwise unobservable on the serial-less target.

## USB Mass Storage: `/mnt` from the boot pendrive (`hal/src/{msc,gpt}.rs`, `kernel/src/usb/xhci/msc.rs`, `kernel/src/block/usb.rs`)

The target machine has no IDE, so `block::ata` finds nothing there and
`/mnt` (doom, quake, the C tests on `$PATH`) used to be absent on metal.
The pendrive it boots from carries an ext2 partition named
`constanos-data`; this reads it. Plan and history:
`docs/storage/usb-msc-plan.md`.

**Layers.** `hal::msc` (CBW/CSW, SCSI CDBs, INQUIRY/READ CAPACITY/sense
decoding — CBW fields little-endian, CDB fields big-endian, each tested),
`hal::usb::find_mass_storage` (class 08/06/50 at alt 0, SuperSpeed
companion `bMaxBurst` attached to the right endpoint), `hal::gpt` (both
CRCs checked, bad primary falls back to the backup, backup must claim the
LBA it was read from; lookup by partition name, else the *only*
Linux-filesystem partition) and `hal::block::Partition` (offset + refuses,
never clamps, a request outside its window) are pure and host-tested,
including against `sfdisk` output and the real stick's own GPT regions
(`hal/fixtures/`). `kernel/src/usb/xhci/msc.rs` is the hardware half —
a child module of `xhci` so it can use the private rings and
`service_events`.

**One reader of the event ring** (`Xhci::service_events`). Before this, the
keyboard poll and every waiter each read the ring and treated what they
found as theirs or noise: a keyboard report arriving during another
device's enumeration was dropped *with its only outstanding transfer*, so
the keyboard went silent for good, and a disk completion would have been
eaten by the timer's poll. Now routing lives in one place — keyboard events
are decoded into a pending buffer and re-armed whoever is draining; the
caller gets its own event; the rest is logged. `usb_keys_dropped` in
`/proc/kdebug` should stay 0.

**Every transfer runs with interrupts off and `CONTROLLERS` held**
(`usb::storage_read`/`storage_write`, one ≤64 KiB transfer at a time,
released between them). The lock makes the transfer the ring's sole
reader; IF=0 makes the holder unpreemptible, so no other reader can ever
spin on a preempted holder. Cost: ~1 ms of interrupt latency per 64 KiB.

**Mounted read-only** (`fs::ext2::init_read_only`): no repair passes, every
mutation goes through `write_lock()` which returns `EROFS`, and a write
`open()` fails with `EROFS` like Linux. `Partition` refuses writes too, as
a second guard. Writing is step 6 of the plan — the stick is also the boot
key and there is no journal.

**Bring-up recovery** follows BOT, not hope: STALL in data/status → clear
halt on both sides (Reset Endpoint or Stop Endpoint chosen from the
endpoint's real state in the output context, then Set TR Dequeue, then
CLEAR_FEATURE); bad CSW or Phase Error → Reset Recovery; UNIT ATTENTION /
becoming-ready → retried. QEMU never exercises any of it.

**Testing in QEMU:** `QEMU_USB_STORAGE=<img>` attaches a `usb-storage`
stick. Combine with `QEMU_DEBUG_NO_DISK=1` so ATA can't mount first
(`fs::ext2::init` tries USB first, ATA second). Verified: SuperSpeed and
USB 2 ports (`-device qemu-xhci,p2=4,p3=0`), `-m 8G` (DMA above 4 GiB),
md5 of the WAD/pak identical to the host, `doom` running from a copy of
the real stick's GPT + data partition. Deploying: write a FAT to
`/dev/disk/by-partlabel/boot` (34816 sectors) — never `dd` the whole
image to the device, which replaces the GPT and drops `constanos-data` —
then `scripts/sync-usb-data.sh`. **The UEFI image's own FAT no longer
fits:** the `dev` kernel outgrew 17 MiB (18 MB on 2026-09-23), and
`bootloader` sizes its FAT to 18 MiB, so a `dd ... count=34816` of it
truncates the kernel. **`scripts/deploy-usb-boot.sh`** does it right:
a fresh FAT16 of the partition's exact size holding `efi/boot/bootx64.efi`
from the image and the kernel through `strip --strip-debug` (~6 MB; the
PT_LOAD segments are byte-identical, only DWARF goes — nothing on the
machine reads it), boot-tested in QEMU from a `usb-storage` stick, then
written and read back (`--image-only` to build and test without writing).

**Block cache** (`hal::blockcache::CachedDevice`, host-tested): every
ext2 mount goes through a write-through cache that `Ext2Core::mount`
installs itself — 4 KiB chunks, 32 MiB cap, CLOCK eviction, read-ahead of
up to 64 KiB per miss — and `read_file_range` reads whole blocks straight
into the caller's buffer, physically contiguous runs as one request.
Before it, ext2 issued one device request per 1 KiB block and re-read the
indirect blocks for every data block: starting `doom` from the stick was
~126,000 SCSI commands; it is now ~300, and a second `doom` is zero.
`/proc/kdebug` reports it as `ext2_cache:`. Coherent only because the
cache owns the device: **nothing may write the mounted partition except
through `Ext2Core::device`**.

## Kernel Log on the USB Stick (`kernel/src/block/logpart.rs`, `hal/src/logpart.rs`, `scripts/usb-log.sh`)

The target machine has no serial capture, and until this every result
there had to be photographed off the screen. Now the kernel copies the
`klog` ring onto a third, **raw** GPT partition of the boot pendrive,
`constanos-log` (no filesystem: a torn write spoils at most the log, never
`constanos-data` or `boot`), and `scripts/usb-log.sh read`, run on the
machine's own Linux after a reboot, prints it. `list` shows every boot kept.

- **When:** every 5 s from the **idle task** if the ring grew (idle, not
  the timer ISR: a failing transfer logs through `serial_println!`, whose
  `SERIAL` lock the interrupted code may hold — cost: a process spinning at
  100% CPU starves the periodic flush); on `sync(2)` (`kdebug sync`); and
  from the **panic handler** after the panic screen is drawn, `try_lock`
  everywhere and skipped if `SERIAL` is held. Three consecutive periodic
  failures disable periodic flushing (a dead stick would otherwise cost a
  5 s IF=0 timeout every period).
- **Format** (`hal::logpart`, host-tested): sector 0 is a marker only the
  host writes; then up to 16 slots of 128 KiB, **one per boot**, reused
  oldest-first, so a second boot doesn't overwrite the one you wanted. The
  ring is stored raw with `write_pos` in the slot header, which makes
  flushes incremental (only the sectors the ring touched). A boot claims its
  slot with an empty header before any data lands.
- **Guards against writing the wrong sectors:** exact name lookup (no
  fallback, unlike the data partition), the host's marker must be in sector
  0, and every write goes through `hal::block::Partition`, which refuses
  out-of-window requests. The partition's type is Linux *reserved*, so it
  never makes the data partition's "only Linux filesystem" fallback
  ambiguous.
- **User output is in the log** because `FramebufferConsole`'s serial
  mirror now also pushes to `klog` (it used to go straight to port 0x3F8,
  so `[fb]` lines were on COM1 but in neither `/proc/dmesg` nor the stick).
- **Setup (once, on the host):** `scripts/usb-log.sh mkpart /dev/sdX`
  (backs up the GPT with `sfdisk --dump`, then `sfdisk --append` of 64 MiB —
  existing partitions untouched; asks for `yes`), then
  `scripts/usb-log.sh init`. **QEMU:** `scripts/usb-log.sh mkimage
  <out.img>` builds a stick with the real shape (disk.img as the data
  partition), then `QEMU_USB_STORAGE=<out.img> QEMU_DEBUG_NO_DISK=1` and
  `scripts/usb-log.sh read --image <out.img>`. Verified in QEMU: periodic,
  sync and panic flushes; the on-disk log byte-identical to serial.log;
  boot #N landing in slot N-1; an unformatted partition left untouched; and
  every sector outside the log partition (both GPTs, `boot`, data) hashing
  the same before and after a boot. Works on metal too: four Ryzen boots
  so far, with periodic, sync and panic flushes all read back.

## Unattended Bare-Metal Runs (`kernel/src/autorun.rs`, `userspace/src/bin/shell.rs`, `docs/metal/autonomous-loop-plan.md`)

Linux on the target machine can run a job on constanos and read the result
back with nobody at the keyboard: it drops a script at `/mnt/autorun/job`
(plus a one-line `/mnt/autorun/nonce`) on the stick's data partition,
`efibootmgr --bootnext`s into the stick, and reads `constanos-log` after the
machine comes back. PID 1 prints `METAL-BEGIN <nonce>`, `sync`s, runs the job
with `ash`, prints `METAL-DONE <nonce> exit=N|signal=N`, and `reboot(2)`s.
The kernel's half (`autorun::detect`, right after `fs::init`) is one flag: in
autorun mode the panic handler resets after `logpart::on_panic` instead of
halting (`reboot::restart_from_panic`, lock-free). The job is never deleted
by constanos (`/mnt` is read-only from the stick) — the host removes it.
Measured on the Ryzen: `BootNext` needs no menu, `reboot` returns via the
FADT reset register (SMI port `0xB2`), and the SP5100 TCO watchdog does
*not* survive a reset — so constanos arms it itself: in autorun mode only,
right after `autorun::detect`, `kernel/src/watchdog.rs` (protocol in
`hal::sp5100_tco`, Linux's `efch_mmio` layout, the one on this board) arms
the FCH TCO for `TIMEOUT_SECS` (300) and never pings it — **armed on every
boot**, right after the framebuffer setup (`watchdog::arm_early`, before
ACPI/USB/storage), and disarmed by `watchdog::settle` after `autorun::detect`
when there is no job, since whether a boot is unattended is only known once
`/mnt` is mounted. Measured: a job
spinning forever came back to Linux ~300 s later with nobody at the machine,
and Linux's `sp5100_tco` reported `bootstatus=32` (`WatchDogFired` survives
the reset), which `metal-run.sh --collect` appends to the verdict.
`/proc/kdebug` shows the watchdog's state and time left. Early-hang path
measured too: a kernel built with `CONSTANOS_TEST_HANG_BEFORE_FS=1` (build-time
hook, spins right before `fs::init`; the QEMU boot test in
`deploy-usb-boot.sh` fails on it by design, so deploy it with `--no-test`)
came back 5 min 26 s later as `NO-BOOT [watchdog reset: bootstatus=32]` — no
log slot, the log partition being claimed after `/mnt`. Only
firmware → bootloader → the first steps of `init::boot` remain uncovered. The
disarm path (a manual boot, no job) is host-tested but not yet observed on
metal: check `/proc/kdebug` on the next manual boot.

**Host orchestrator: `scripts/metal-run.sh JOB.sh`** (build, deploy only if
the kernel ELF changed, `sync-usb-data.sh`, job + nonce onto the stick,
`BootNext` to the stick's entry found by `boot`'s PARTUUID, reboot), then
`scripts/metal-run.sh --collect` back in Linux: verdict `OK`/`FAIL`/`PANIC`/
`HANG`/`NO-JOB`/`NO-BOOT`, archived with the boot's log under
`target/metal/runs/<nonce>/`; `--abort` undoes a run never booted. This run's
boot is the one numbered above the log's last boot at deploy time that prints
the nonce. `--no-reboot`/`--no-deploy` for dry runs; `--classify` is its
classifier alone, for testing against `usb-log.sh read --all --image` output.

**Resuming the agent after the reboot: `scripts/metal-resume.sh`** (phase 6).
tty1 logs in by itself (a kmscon drop-in, `/etc/systemd/system/
kmsconvt@tty1.service.d/autologin.conf`, outside this repo) and `~/.zlogin`
runs this script there. With a run pending it `--collect`s and then
`claude --resume`s the session that launched it (`session=` in `pending`,
from `CLAUDE_CODE_SESSION_ID`) in the foreground of tty1, with the verdict in
the prompt. Brakes: `target/metal/budget` (automatic resumes left; missing or
0 = collect only) and `target/metal/stop` (collect only). `--dry-run` says
what it would do.

**Testing jobs in QEMU:** put the job into a scratch copy of `disk.img` with
`debugfs -w` (`mkdir /autorun`, `write job /autorun/job`, same for `nonce`)
and boot it with `QEMU_DEBUG_DISK_IMG=<copy> QEMU_DEBUG_EXTRA_ARGS=-no-reboot`,
so the job's final reset ends QEMU instead of re-running it. Keep
`QEMU_DEBUG_STATE_DIR` short: the monitor socket path must be under 108 bytes.
Timing bugs here showed up only under host load — run several in parallel.

## Kernel Log Ring + the No-Input Escape Hatch (`kernel/src/klog.rs`, `/proc/dmesg`)

`klog` keeps every byte `serial_println!`/`serial_println_raw!` emits in a
fixed 64 KiB BSS ring (a boot to the shell prompt measures ~14 KiB, so the
whole boot plus a good deal of runtime fits). `cat /proc/dmesg` reads it
back. **Lock-free on purpose** — one `fetch_add` reserves a byte range and
the writer fills it — because `push` is reachable from the timer ISR, the
page-fault handler, the allocators and the panic handler; interleaving
between concurrent writers is the same trade-off `RawSerialWriter` already
documents, and is worth far more than a log that can deadlock the machine
it is debugging.

**Why it exists, and what `/proc/dmesg` alone does not solve.** On the
physical AM4/Ryzen machine there is no serial capture, so boot messages are
readable only on screen — and they were being destroyed twice over. First,
`FramebufferConsole::new()` cleared the entire screen the first time a
process opened `/dev/fb`, which happens when PID 1's stdout is set up,
*after* every driver has run: any `kalert!` the boot produced was wiped
microseconds before anyone could read it. That is why the USB driver's
on-screen status line was reported as never appearing on real hardware —
it had been drawn and then erased, which is indistinguishable from a driver
that said nothing. `FramebufferConsole::new` now skips that clear when the
kernel has already written to the console (`KERNEL_WROTE`), so the shell's
output scrolls up from the boot log instead of replacing it, and
`draw_boot_screen` parks the console cursor below its own banner
(`reserve_rows_at_top`) so notices don't overprint it.

Second, and the reason a `dmesg` file is only half an answer: **reading it
takes a shell, which takes a keyboard, which is the thing that was
broken.** So `init::boot` also calls `show_boot_log_if_no_keyboard()`,
which renders the USB/PCI-relevant log lines to the framebuffer and holds
them for 30 s when the machine has no keyboard at all. The gate is
deliberately narrow — no USB keyboard enumerated **and**
`hal::i8042::controller_present` says the legacy 8042 does not answer — so
it never fires in QEMU (where the 8042 always answers) and no test flow
pays for it. `klog::dump_to_screen` filters by substring rather than by log
level: a boot is ~300 lines and a screen holds ~80, and threading real
levels through every existing call site is a far bigger change than this
one diagnostic justifies.

**`hal::i8042::controller_present` is read-only by construction.** The
thorough probe (controller self-test `0xAA`, keyboard interface test
`0xAB`) is what Linux does *during its own 8042 init*; issuing either here
— after `init_hardware_interrupts` has set the keyboard up, possibly while
it is in use — can leave the interfaces disabled on real hardware. A probe
whose failure mode is "the keyboard that was working now isn't" is worse
than no probe, so this one only reads the status port and treats the
open-bus `0xFF` as "no controller". It therefore cannot distinguish "8042
present but no keyboard attached"; that case needs the active commands.

**Reproducing the target machine in QEMU:** `QEMU_DEBUG_NO_PS2=1`
(`-machine pc,i8042=off`) removes the legacy controller, so with
`QEMU_USB_KBD=1` the USB driver is the only possible input path — exactly
the bring-up machine's shape. Verified: typing works in that configuration,
`i8042_present=false` is reported, and with the USB keyboard also removed
the no-input hold renders the filtered log and the red summary on screen.

## Per-CPU Init (`kernel/src/cpu/init.rs`, `kernel/src/process/tss.rs`)

Stage 3 of `docs/smp/smp-plan.md`. **`cpu::init_this_cpu(cpu)` is the one
place per-CPU state is set up**, and what an AP will get in stage 4 is
exactly that list: control-register bits copied from the BSP (`CR0.WP/CD/NW`,
`CR4.PGE`, `EFER.NXE` — the bootloader sets WP/NXE on the BSP only), the
shared GDT + this CPU's TSS slot (`0x28 + 16·n`, one TSS and one
double-fault IST stack per CPU), `lidt`, `PerCpu`/`KERNEL_GS_BASE`, the
`syscall` MSRs, `IA32_PAT`, SSE's CR0/CR4 bits, and the local APIC + timer.
Each step has a `verify_*` that reads its register back; the result per CPU
is `cpu_init:` in `/proc/kdebug`. Global *decisions* stay where they were
(`program_pat` picks the PAT, `apic::init` calibrates and routes the I/O
APIC) and every CPU, the BSP included, *applies* them here. **Anything new
that is per CPU gets a step here and a `verify_*`**, or an AP will run with
the firmware's value. `hw_tests::init_this_cpu_restores_what_an_ap_lacks`
resets the BSP's registers to an AP's and checks every step brings its
register back.

## Application Processors (`kernel/src/smp.rs`, `hal/src/smp.rs`)

Stage 4 of `docs/smp/smp-plan.md` starts them; since stage 7 they run
processes (see SMP Scheduling below). Each AP comes up through a four-page
trampoline below 640 KiB (code, then its own PML4/PDPT/PD), **reserved in
`init::memory::init_core` before the Buddy allocator is seeded** — the pages
must never be handed out. The trampoline's PML4 is a *copy* of the kernel's
with entry 0 replaced by a 0–2 MiB identity map, so no live table changes; the
AP loads the kernel's real CR3 first thing in Rust (`ap_entry`), runs
`cpu::init_this_cpu(cpu)`, joins the TLB shootdown (`memory::tlb::this_cpu_ready`),
and loops on `sti; hlt` until `smp::release_aps` (at the first process's
start) sends it into the scheduler on its own idle process. **Its LAPIC
timer stays masked until then** (`interrupts::apic::runs_timer`; unmasked by
`start_timer_on_ap` as it enters) — an AP that took vector 32 with nothing to
schedule would have no process to switch from. `smp::WAKE_VECTOR` (0xF1)
makes the idle loop (`smp::idle_once`) run what `smp::run_on` left in its
mailbox — the TLB self-test's way onto an AP; the scheduler leaves an idle
process running such a job alone (`smp::ap_busy`). APs start one at a
time; every wait is bounded, and an AP that does not answer is logged
(`NO-RESPONSE(stage n)`, the stage the trampoline's progress marker reached),
sent INIT again so it cannot wake late on the next AP's stack, and left out.
The BSP is always CPU 0; the rest follow MADT order up to `MAX_CPUS` (32).
`smp:` in `/proc/kdebug` has the per-CPU outcome and time to come up;
`QEMU_DEBUG_SMP=N` gives QEMU N CPUs, and `boot-matrix.sh` records
`cpus_online=M/N` per boot.

## SMP Scheduling (`kernel/src/process/scheduler.rs`, `kernel/src/process/timer_preempt.rs`, `sched/`)

Stage 7 of `docs/smp/smp-plan.md` (its resolution section has the full
record). Every online CPU runs processes; built with `CONSTANOS_NOSMP=1`
(watched by `build.rs`) only CPU 0 does, the APs still starting for the TLB
self-test.

- **One lock, one core** (`SCHEDULER`), `running[cpu]` + `idle[cpu]`.
  `running_ref`/`running_mut`/`current_pid` mean *this* CPU's. The idle
  processes (one per CPU, all pid 0, as in Linux) are never queued: a CPU
  runs its own when nothing it may take is Ready. `iter_all`/`/proc` skip
  them. `find_process_mut` also finds a process running on another CPU.
- **`LEAVING[cpu]` is `on_cpu`**: the kernel stack a CPU has switched away
  from but still executes on. Set by every switch (`note_leaving`, by RSP),
  cleared by the asm right after `mov rsp, <new frame>`
  (`jump_to_trapframe_raw`, the timer/IPI stubs — which is why
  `jump_to_trapframe` is now a Rust wrapper passing `leaving_slot()`, and the
  stubs' handlers return a `Resume` in RAX:RDX). No other CPU picks that
  process (`eligible`) or frees that stack (`tick`'s
  `pending_stack_frees` drain). **Every kernel stack is freed through
  `pending_stack_frees`** — `waitpid`'s reap too: the zombie's `sys_exit` may
  still be unwinding on another CPU.
- **Never drop the last `Arc<AddressSpace>` of a table some CPU has in CR3.**
  Its PML4 goes back to the Buddy and another CPU can reuse it at once; this
  one then fetches through garbage (a silent triple fault). `sys_exec` drops
  the old space only after `activate()`, and `kill_current` parks a dying
  thread's space in `retiring[cpu]` until `switch_in` has loaded the next.
- **Tick on every scheduling CPU; global work on CPU 0 only**: cursor, USB
  poll, `TICK_COUNT`, hrtimers, the aging clock. Slices are per CPU
  (`SchedCore::start_slice_on`/`consume_quantum_on`). An idle CPU switches
  at its next tick if there is eligible work, or at once on the **reschedule
  IPI** (`RESCHED_VECTOR` 0xF2, `kick_idle`), sent whenever something becomes
  Ready while a CPU idles — to this CPU first, so a keyboard IRQ that wakes
  the shell doesn't wait for a tick.
- **Wakeups across CPUs.** "Register as a waiter, then block" is two steps,
  and another CPU can run a whole wakeup in between. `poll`/`epoll_wait` and
  stdin register *and* block under the scheduler lock (the waker takes its
  registry first, then that lock, so it finds them Blocked), and re-check
  readiness once registered, restarting the syscall (`rip -= 2`) if
  something slipped in. Pipes and sockets can't (they register inside
  `FileHandle::read` with the fd table held; a pipe keeps a FIFO queue of
  waiters per end — it used to keep one, and a second blocked reader
  stranded the first, `userspace/c/pipe_multi_test.c` — and looks up the
  current pid *before* locking its buffer: `sys_fork` holds `SCHEDULER`
  while it `dup`s every pipe, so the reverse nesting deadlocked every
  CPU): their wakers use
  `wake_or_defer`/`deliver_to_waiter`, which leave `Process::wake_pending`
  for `block_current` to consume instead of blocking — **only for waiters
  that register on the way to blocking and whose wait the waker has
  claimed** (see Interruptible Waits), or a stale one leaves a pending
  wakeup for some later block.
  Sockets also close the check→register gap with `unix::WAKE_EPOCH` (read
  before the operation, compared after registering). Counters:
  `early_wakes`, and the `sched:` block of `/proc/kdebug` (per-CPU pid,
  switches, busy/idle ticks, `max_concurrent`, `max_threads_parallel`,
  reschedule IPIs, `leaving_skips`, and `invariants=` —
  `sched::SchedCore::check_invariants_with_running` run on the live
  scheduler).
- Ash's `wait` builtin works since 2026-09-25 (`rt_sigsuspend`, see the
  syscall table); it took three fixes — the syscall itself, the `sigset_t`
  bit layout, and `waitpid`'s `WNOHANG`-before-`ECHILD` order.

## CPU Time Accounting (`sched/src/cputime.rs`, `sched/src/loadavg.rs`, `kernel/src/process/scheduler.rs`)

Tick-sampled, as Linux without `VIRT_CPU_ACCOUNTING`: every tick on every
scheduling CPU charges one tick by what it interrupted — ring 3 → user,
the idle process → idle, anything else in ring 0 → system
(`Scheduler::tick(rsp, user_mode)`, `sched::cputime::classify`). Per CPU
into `CPU_{USER,SYSTEM,IDLE}_TICKS` (`/proc/stat`'s `cpuN` lines; the
other Linux columns are 0), per process into `Process::times`
(`utime`/`stime`; a reap moves the child's time and its `c*time` into the
parent's `cutime`/`cstime` — `credit_reaped`, on every reap path). The
tick is 100 Hz, equal to `USER_HZ`, so nothing is scaled. Separately,
`Process::exec_ns` is **measured**: `switch_in` starts the clock and
`note_leaving` (every switch-out passes through it) stops it — that backs
the CPU-time clocks, so `clock()` has nanoseconds, not 10 ms ticks.
`CLOCK_THREAD_CPUTIME_ID` reads a per-CPU copy `switch_in` keeps
(`scheduler::thread_exec_ns`, IF=0, no lock): through the scheduler lock, a
timing loop on 24 CPUs spent a quarter of its time in the kernel contending
for it. `CLOCK_PROCESS_CPUTIME_ID` still takes the lock (it sums the group).

**A thread group is the processes sharing one `AddressSpace`** (there is no
tgid; a thread is a process). Group time (`times`, `RUSAGE_SELF`,
`CLOCK_PROCESS_CPUTIME_ID`) sums the live members; a dying thread's time is
folded into its leader's `dead_threads`/`dead_threads_ns`, **not** into the
leader's own fields — the first version did, and the main thread's
`CLOCK_THREAD_CPUTIME_ID` gained 200 ms during a `pthread_join`.

The load average is sampled on CPU 0 every `LOAD_FREQ` (501) ticks:
runnable = running on any CPU + ready (Linux also counts `D` sleepers,
which do not exist here). `/proc/loadavg`, `sysinfo`.

Consumers that validate it independently: BusyBox `top` (per-CPU and
per-process `%CPU`), `ps` (`cmdline`), `nproc`, `uptime`, `free`;
`userspace/c/cputime_test.c` (A–I: clock tick and CPU count agree across
`sysconf`/affinity/`/proc/stat`/`/proc/cpuinfo`, the aggregate line is the
column sum, 300 ms spun shows as user time everywhere and 300 ms slept as
none, children and grandchildren reach `cutime` only when waited for,
thread vs process clocks, several CPUs charged in parallel, uptime/btime/
sysinfo/loadavg consistent); and **`cpumon`** (`userspace/src/bin/
cpumon.rs`, disk-resident since its text went proportional through `userspace::text`): a graph per CPU with user and system stacked, total
load, memory, load averages and the busiest processes, from those files
alone — `compositor cpumon`, or on the console.

## Per-Core Frequency (`hal::cpufreq`, `kernel/src/cpu/freq.rs`)

`/proc/cpuinfo`'s `cpu MHz` is each core's real running frequency since
2026-09-26, as on Linux: `base × ΔAPERF / ΔMPERF`, where MPERF ticks at the
TSC's rate and APERF at the core's, both only in C0. **Per-CPU tick work**:
every scheduling CPU reads its own pair in `timer_preempt_handler` (a CPU's
MSRs are only readable by it), before the scheduler lock, into a per-CPU
`hal::cpufreq::Window` (`try_with`, never waits); a result needs 10 ms of
accumulated C0 time (`MIN_ACTIVE_US`), so an idle core keeps reporting the
frequency it last ran at. A result above 4x base is discarded as a reset
counter. Gated on CPUID 6.ECX[0], decided once by the BSP after
`tsc::init` (`cpu::freq::init`, `[cpufreq]` in the boot log) — without it
`rdmsr` would #GP, and `cpu MHz` stays the TSC's. `flags` lists
`aperfmperf` when present (Linux's name), which is how `cpumon` knows the
numbers are measured and draws one per tile plus the range in its header.
**QEMU has none of it**, TCG or KVM (`-cpu host` too): only the fallback
runs there, the measurement only on metal.

## CPU Temperature (`hal::k10temp`, `kernel/src/cpu/temp.rs`, `/proc/sensors`)

AMD family 17h/19h, read as Linux's `k10temp` reads it: SMN registers
through the root complex's index/data pair (00:00.0, config `0x60`/`0x64`,
`pci::smn_read`) — Tctl at `0x59800` (bits 31:21, 1/8 °C, minus 49 °C when
the range bits say so) and one `Tccd` per populated CCD (`0x59800 +
ccd_offset + 4n`, valid bit 11). The model table (CCD offset and count),
the Tctl offset table (`Tdie` only on the early parts that have one) and
the arithmetic are Linux's, pure and host-tested against the values
Linux's k10temp reported on the target machine. Decided once at boot
(`[k10temp] present|absent` in the log): AMD vendor, a covered family,
**and** an AMD root complex (under a hypervisor CPUID can say Zen while
00:00.0 is an emulated Intel bridge); claims 00:18.3 like Linux does.
Read on every open. `/proc/sensors` is one `chip<TAB>type<TAB>label<TAB>
value` line per sensor — hwmon's name, attribute type, `…_label` and
`…_input`: `k10temp temp Tctl 32875` (millidegrees) and, where RAPL exists
(see Idle below), `amd_energy energy Esocket0 <µJ since boot>` as Linux's
`amd_energy` names it — **empty** without either, QEMU always, so only the
Ryzen exercises it. `cpumon` shows Tctl and the package power (the energy
line's difference between two samples) in its header. Family 1Ah (Zen 5) is left out.

**PCI config access is locked** (`pci::CONFIG`, an `IrqLock`) since this:
mechanism #1 is two port accesses and the SMN pair two more, and with
processes on every CPU a `cat /proc/pci` could retarget `0xCF8` under
another CPU's access. `config_write8` (the ACPI reset, also on the panic
path) only *tries* it for a bounded while and then resets regardless.

## Idle, Package Power and C0 Residency (`hal::amd_power`, `kernel/src/cpu/idle.rs`)

Idle CPUs `hlt` (C1). On AMD Zen the kernel also knows ACPI C2 without
AML: a read of `CStateBaseAddr + 1` (MSR `C001_0073`; `0x413` → `0x414`
on the Ryzen, the port its `_CST` gives Linux), switchable live with
`kdebug idle hlt|c2`. Two instruments in `/proc/kdebug`: `rapl:` package
energy since boot (MSR `C001_029B`, accumulated on CPU 0's tick,
`package_uj`, also in `/proc/sensors` for `cpumon`) and `c0_permille:` each CPU's C0 residency over the last
second (ΔMPERF/ΔTSC, per-CPU tick work). Both need a Zen outside a
hypervisor; QEMU shows `absent`/`-`.

**Measured on the Ryzen (boot #56), and why `hlt` stays the default:**
alternating 45 s phases, `hlt` 18.0/19.8 W and C2 17.5/21.4 W package —
the difference is inside the phase-to-phase noise — with every CPU at
0.2-1.4% C0 and Tctl 31 °C in both modes, the same as Linux idle on that
machine. The "constanos idles hotter" reading that prompted this (53 °C)
was taken seconds after boot; Linux reads ~47 °C at that point too.
Read temperatures after the machine has settled, or read `rapl:`.

## Resident Set Size (`hal::paging`, `AddressSpace::mem_stats`, `fs::procfs`)

`/proc/<pid>/stat`'s `rss` and `/proc/<pid>/statm` (`size resident shared
text lib data dt`, pages) are real since 2026-09-26; before, `rss` was 0.
**Walked, not counted:** `mem_stats` runs `hal::paging::count_resident`
(pure, host-tested) over each VMA's range under the address-space lock —
present user leaves, 2 MiB leaves as 512, the shared zero frame excluded, as
Linux excludes its zero page. A counter would need every PTE-changing path
(demand paging, COW, `fork`, `munmap`, `exec`, ELF loader, stack growth,
shm) to remember it; the walk skips absent tables, so its cost is the page
tables that exist. `proc_stat_snapshot` clones the `Arc<AddressSpace>`
under the scheduler lock and walks after releasing it. `shared` is the
resident part of `Shared` VMAs; `text`/`data` are virtual sizes
(`Code` VMAs / the rest), as Linux's. Consumers: `ps -o rss`, `top`,
`cpumon`'s RSS column. Test: `userspace/c/rss_test.c`.

**An anonymous `mmap` of 2 MiB or more is a `Huge2M` VMA** (`sys_mmap_anon`),
backed by whole 2 MiB pages: a one-byte touch, read or write, makes 512 pages
resident — there is no huge zero page. Huge pages are never shared, so
**`fork` copies them** (`AddressSpace::fork_copy_huge`). Until 2026-09-26 it
skipped them — its per-page loop's 4 KiB `translate_page` reports a 2 MiB
leaf as absent — and a child read zeros in every large mapping of its
parent (any big `malloc` included); `rss_test` case F found it. The copy is
2 MiB per present huge page, under the parent's and the scheduler's locks.

## File Timestamps (`vfs::clock`, `vfs/src/ramfs.rs`, `ext2/src/inode.rs`, `kernel/src/fs/ext2.rs`)

`stat()` reports real times since 2026-09-26; before, every file was 1970.
**ext2** reads `i_atime`/`i_ctime`/`i_mtime` from the inode and the adapter
stamps them (`RawInode::stamp_new`/`stamp_modified`/`stamp_changed`, with
`crate::time::now_unix_secs()`): a new inode gets all three, a write or
truncate `mtime`+`ctime`, a `chmod` or a lost link `ctime`, and a directory
that gains or loses an entry `mtime`+`ctime` — through `touch_dir`, read
fresh and written as the **last** step of each operation, because several
operations write a stale `self.raw` copy of the directory for its link
count first. **ramfs** keeps `Times` per node (atomics, shared by a file and
its open handles, like `data`), from `vfs::clock::now()`, which the kernel
registers at boot (`set_clock`, the `set_relax_hook` shape; unset in host
tests, so 0). Neither updates `atime` on a read (Linux's `noatime`).
Filesystems with no times (initramfs, devfs, procfs) report 0 and
`stat`/`fstat` hand that out as the boot time (`fill_missing_times`), not
the epoch. `utimensat` (above) sets them explicitly. `fstat` on an ext2
directory uses the record read at `open`, not the disk: `sys_fstat` calls
`FileHandle::stat` under the scheduler lock. Test: `userspace/c/fstime_test.c`
(ramfs and ext2 stamping, every `utimensat` form, the boot-time fallback,
and the libc fixes that came with it: `ctime()`, `sscanf` widths,
`getpwuid`).

## Interruptible Waits (`kernel/src/process/wait.rs`, `sched/src/wait.rs`)

A signal ends a blocked wait, as on Linux (`wait_intr_test`: 27 cases).
Until 2026-09-25 it only queued, and a `SIGKILL` to `sleep 100` took
100 s — every waker woke a pid blindly, so a registration left behind by
an early wakeup would have acted on the process's *next* wait, and
cleaning registrations up from the signal path is impossible: each
registry lock (pipe buffer, `POLL_WAITERS`, `SOCKETS`, `STDIN_WAITER`)
comes before `SCHEDULER`, and signals are sent holding it.

- **One-shot cell per wait** (`sched::WaitCell`, host-tested with a
  real two-thread race, proven by sabotage). Every registration of a wait
  holds the same `Arc<WaitCell>`. **A waker `claim`s it before touching
  the waiter** — before taking pipe bytes for a reader, writing
  `revents`, consuming a key — and drops the entry if it cannot; a signal
  `cancel`s it (`Scheduler::interrupt_blocked`, which replaced
  `wake_sigsuspended` and runs after every signal is queued). Exactly one
  wins, lock-free; cancelled entries go stale and are dropped when found.
  A claimed wait completes and the handler runs after, as on Linux.
- **`block_current(tf, Wait)`** names the wait: syscall number, return
  `rip` (taken before a socket rewinds it), `RestartPolicy`, and how to
  interrupt it (`Interruptible::Cell`, `WaitPid` — clears `waiting_for` —
  or `No`, the old behaviour). The cell comes from `begin_wait` (under the
  scheduler lock) or `arm_wait` (pipes and sockets: registered under their
  own lock, armed after dropping it); a path that registers and then does
  not sleep calls `abandon_wait`. `block_current` also refuses to sleep
  when an actionable signal is already pending (queued while the process
  was still running) — the check-then-sleep rule for signals.
- **EINTR or restart is decided at delivery** (`signal::deliver_pending`,
  `sched::wait::restarts`): a handler with `SA_RESTART` re-executes
  read/write/futex/waitpid/sockets/stdin (`rip = ret_rip - 2`, `rax = nr`,
  arguments still in the saved registers); nanosleep and poll return
  `EINTR` whenever a handler runs; a stop or no handler re-executes, so
  `SIGSTOP`/`SIGCONT` is invisible to the call. A restarted `nanosleep`
  sleeps its full duration again.
- `sigsuspend`/`pause` keep their own path (`in_sigsuspend`). A child's
  death calls `interrupt_blocked` **after** completing the parent's
  `waitpid`, so SIGCHLD never turns a completed wait into `EINTR`.
- `deliver_one` now discards ignored signals on its way to the first one
  that acts (it stopped at the first ignored one). `/proc/kdebug`:
  `waits_interrupted`.

## TLB Shootdown (`kernel/src/memory/tlb.rs`, `hal/src/tlb.rs`, `kernel/src/tlb_selftest.rs`)

Stage 5 of `docs/smp/smp-plan.md`. After a page-table change, every *other*
CPU that can hold the old entry is sent an IPI (vector 0xF0) and waited for:
for a user mapping, the CPUs whose CR3 is that table right now
(`tlb::invalidate_page(pml4, addr)` — callers pass the table: `OwnedPageTable`
its own, `demand_paging` the current CR3); for a kernel mapping, every CPU in
`READY` (`invalidate_kernel_page`). Who has what loaded is `LOADED[cpu]`,
published before the CR3 write — so **every CR3 load goes through
`tlb::switch_to`** (the one exception, `ap_entry`'s, runs before TR and is
covered by `this_cpu_ready`). One request at a time; the wait is bounded
(1 s, then a panic naming the CPUs). **A CPU spinning with IF=0 cannot take
the IPI**, so such spins call `tlb::service_pending`: the wait for the
sender slot, every `diag::IrqMutex` spin (`IrqControl::relax`), every
kernel lock (`crate::sync::Mutex`/`IrqLock`, whose relax strategy does it —
the scheduler's lock included), `vfs`'s locks (through
`vfs::lock::set_relax_hook`) and the USB transfer waits. `/proc/kdebug`'s `tlb:`
line counts shootdowns, IPIs and wait times. Tested by
`hw_tests::tlb_shootdown_leaves_no_stale_translation` and on demand by
`kdebug tlbtest`, both `tlb_selftest::run`: an AP reads a page in a loop
while the BSP moves it between frames; sabotaged (no IPI) it reports
thousands of stale reads, so QEMU's TLB model does keep old entries.
The COW path swaps frames with `replace_frame` (one PTE store); the
unmap-then-map it replaced left the entry zero in between.

## Interrupt Controllers (`kernel/src/interrupts/`, `hal/src/apic.rs`)

Stage 1 of `docs/smp/smp-plan.md`: the tick is the **LAPIC timer**
(periodic, 100 Hz, calibrated against the already-calibrated TSC — the PIT
is only needed once, to calibrate the TSC) and ISA lines (keyboard IRQ1,
COM1 IRQ4, mouse IRQ12) arrive through the **I/O APIC**, with the MADT's
interrupt source overrides applied. The 8259 stays initialised and remapped
but fully masked, and LAPIC LINT0 (its ExtINT path) is masked too.
`interrupts::apic::init` runs once, IF=0, right after `cpu::tsc::init`, and
is best-effort: any failure (no MADT, no I/O APIC, CPUID without an APIC, a
calibration that doesn't fit) leaves the 8259 + PIT delivering exactly as
before. xAPIC through `memory::mmio::map`, or x2APIC MSRs if the firmware
already turned x2APIC on (it can't be turned back off). Pure parts —
register layout, LVT/divide/redirection encodings, calibration arithmetic,
ISA→GSI routing with polarity/trigger — are host-tested in `hal::apic`.

- **Vectors are unchanged**: timer 32, ISA line n at 32+n, so one IDT serves
  both controllers. LAPIC spurious is `0xFF` (never EOI'd).
- **Drivers call `interrupts::enable_isa_irq(line)` and
  `interrupts::eoi(vector)`, never `pic::*` directly.** The first records the
  line so `apic::init` re-routes whatever was enabled before the switch; the
  second goes to whichever controller is live.
- **Unhandled 34..47 vectors ask the LAPIC's ISR** whether it delivered
  them (I/O APIC pin → LAPIC EOI) or they are 8259 leftovers (no LAPIC EOI —
  it would end whatever else is in service).
- **Edge-triggered lines are drained at the switch** (8042 and 16550 read
  until empty): a device whose line was already high when its pin got
  unmasked produces no edge and would never interrupt again.
- `/proc/kdebug` shows `irq_controller:` (which one is live, the LAPIC
  timer's count/divisor/input clock, the I/O APICs' GSI ranges and every
  routed ISA line, or the fallback reason) and `timer_ticks: N over M ms of
  uptime` — the tick rate check for the serial-less machine; counting
  starts at the first `sti`, so compare two readings. Measured in QEMU:
  input 1000 MHz, 523 ticks in 5.26 s; `-cpu max,-apic` exercises the
  fallback.

## Time Subsystem (`kernel/src/time/`, `kernel/src/rtc.rs`)

Monotonic time (`time::clocksource`, TSC-backed when available, jiffies fallback) is unrelated to wall-clock time, which this kernel gets from a real CMOS/MC146818 RTC (`rtc.rs`, ports `0x70`/`0x71`) read exactly once at boot (`time::init()`, before `fs::init()` mounts ext2 — dtime stamps need it available already). `time::now_unix_secs()` = that one boot-time reading + monotonic uptime since; there's no periodic RTC IRQ and none is needed for this. `rtc::read_unix_time()` handles BCD-vs-binary and 12-vs-24-hour format (Status Register B), the standard double-read-until-stable technique to avoid a snapshot torn across the chip's once-a-second update window, and an exact integer year/month/day → Unix-epoch conversion (Howard Hinnant's `days_from_civil`, correct across the full Gregorian leap-year rule, no floating point). Best-effort like every other optional hardware probe here (mouse, AC97): if the RTC never settles, `now_unix_secs()` just degrades to reporting uptime (boot = epoch), same as before this existed. No century register (unreliable across BIOS/QEMU configs) — assumes 2000-2099.

`/proc` enumerates every live pid for real (`scheduler::all_pids()`, walking `running` + every run queue + the wait queue) — `ls /proc`/`opendir("/proc")` see them all, not just pids looked up by exact name (previously the only way in). Each `/proc/<pid>/stat` renders the classic Linux `stat` format (`fn render_proc_stat`) from a live `Process` snapshot — this is what backs BusyBox `ps`/`top`.

**Real symlinks** (`Inode::readlink()`, `resolve()`/`resolve_no_follow()`, both with an 8-hop `ELOOP` guard, now live in the host-testable `vfs` crate — `vfs::mount::MountTable::resolve`/`resolve_no_follow`, and `normalize_path` in `vfs::path`; `cd vfs && cargo test` runs 167 host tests, no QEMU, covering these plus `RamFs`, `Inode`/`Filesystem`, and the getdents64 helpers. `kernel/src/fs/vfs.rs` is the thin adapter that owns the single `static MOUNTS: MountTable` and re-exposes these as free functions with unchanged signatures, see `vfs/src/lib.rs`'s doc comment). `resolve()` follows a symlink at every path component including the final one (`open`/`stat` semantics); `resolve_no_follow()` leaves the leaf alone (`lstat`/`readlink` semantics). `fs::procfs` produces synthetic symlinks (`/proc/self`, `/proc/<pid>/exe`); ramfs (`/tmp`, `vfs::ramfs::RamFs`) supports creating *real* ones via the `symlink()` syscall (`Inode::symlink`, only writable filesystem that implements it — same `EROFS`-by-default convention as `create`/`mkdir`). This is what backs PID 1's real `busybox --install -s /tmp/bin` at boot (see Userspace Programs below) — no synthetic, kernel-computed symlinks anywhere anymore; `/tmp/bin/<applet>` are indistinguishable from symlinks a real Linux install would create.

**Permission bits** (`fs::types::Stat`): no real per-inode permission model — `regular()` (initramfs/ext2/procfs) hardcodes `0o444`, `regular_writable()` (ramfs only) hardcodes `0o644`. Added because BusyBox `vi`'s readonly check is `access(fn, W_OK) < 0 || !(st_mode & (S_IWUSR|...))` — fixing `access()` alone wasn't enough; every regular file reported zero write bits regardless of which filesystem it actually lived on, so `vi` opened `/tmp/*` files `[Readonly]` too.

The `FileDescriptorTable` per process holds up to 16 open files. FD 0 (stdin) is pre-opened to `/dev/console` (serial — real reads still come from the shared keyboard/UART ring buffer regardless of the handle here); FDs 1/2 (stdout/stderr) are both pre-opened to `/dev/fb` so user-process output and errors are visible on the actual screen, not just in `serial.log` — `FramebufferConsole::write` mirrors every byte it renders out over COM1 too (`[fb] ` prefix), so headless/serial-log debugging still sees everything — and into `klog`, so `/proc/dmesg` and the USB log partition carry it as well.

## Userspace Programs (`kernel/src/process/user_programs.rs`)

Not every userspace program is embedded in the kernel binary. Only what's
needed to reach an interactive shell — `shell` (PID 1), `busybox` (`ash` +
every applet, on the boot path via `busybox --install`), the small Rust
smoke tests (`RUST_PROGRAMS`, all well under 50 KiB but `term`, ~400 KB of font rasters), and `kdebug` (the
live tracing-control tool used in an ongoing debugging investigation
alongside busybox, see Runtime Tracing above) is registered in `PROGRAMS`
and `include_bytes!`'d from `kernel/embedded/`. Everything else runnable-
but-not-boot-critical — `doom`, `quake`, and most of the old C test
programs (`hello`, `pthread_test`, `producer_consumer`,
`mlibc_signal_test`, `stat_test`, `argv_test`, `jobctl_test`,
`ext2_robust_test`, `fpu_test`, `socket_test`, `cputime_test`, `fstime_test`, `pipe_cow_test`, `sigsuspend_test`, `lifecycle_test`, `shm_test`, `pipe_multi_test`, `fb0_test`, `wait_intr_test`, `input_poll_test`, `session_test`, `pty_test`, `rss_test`) — is built straight to
`disk-image-root/bin/` instead and shipped on the ext2 disk image
(`disk.img`, mounted at `/mnt`) rather than baked into the kernel ELF.
This split exists because `kernel/embedded/`'s ELFs (mostly `doom.elf`/
`quake.elf`/the unstripped C test binaries) used to account for the bulk of
the kernel binary's size; moving the non-boot-critical ones off entirely,
plus stripping what's left, took the debug kernel binary from ~37 MB to
~12 MB and `kernel/embedded/` from 27 MB to ~1.6 MB.

`sys_exec` already resolves through the **real VFS** (see below), so a
disk-resident program runs identically to an embedded one once it's on
`$PATH` — no special-casing needed anywhere in the exec path itself, and
disk-resident programs simply don't appear in `PROGRAMS` at all, so they
show up under `/mnt/bin` (`ls /mnt/bin`) instead of initramfs's `/bin`
(`ls /bin`). `userspace/src/bin/shell.rs` sets `PATH=/tmp/bin:/bin:/mnt/bin`
for `ash` — `/mnt/bin` last, so a same-named busybox applet or initramfs
program still wins.

Rebuilt automatically by `kernel/build.rs` (three families, each built
differently, and every resulting ELF — embedded or disk-resident — run
through `strip`/`llvm-strip` afterward, see `strip_tool`/`strip_elf`: this
kernel's ELF loader, `memory/elf_loader.rs`, only ever reads
`Elf64Header`/PT_LOAD program headers, never section headers or a symbol
table, so a stripped binary loads identically to an unstripped one; the
*kernel* binary itself is never stripped — those debug symbols are
actively used, see Build and Run above):

- **Rust** (`RUST_PROGRAMS` in `build.rs`) — built via `cargo build --release` in `userspace/` (separate Cargo workspace), copied from `userspace/target/x86_64-constanos/release/`. **Their target is our own, `userspace/x86_64-constanos.json`** (since 2026-09-26): `x86_64-unknown-none` with hardware SSE2 instead of soft-float (the kernel stays soft-float), built with `-Zjson-target-spec` from `userspace/.cargo/config.toml`. Two things came with it: every Rust program enters through `userspace::entry!` — a plain `extern "C" fn _start` starts with `rsp` 16-aligned instead of 8 mod 16, and with SSE the compiler's `movaps` on stack slots #GPs — and the signal frame carries the FXSAVE image (see FPU/SSE above). `sse_test` (embedded) covers it: IEEE `f64` results, XMM across preemption in 32 processes, a handler clobbering every XMM register and MXCSR, and a handler corrupting the saved MXCSR (proven by sabotage: without the restore C sees 16/16 registers changed; without `sanitize` D gets the reserved bits back — in QEMU; on metal it would be a kernel #GP). All embedded. They have a heap: `userspace::heap` is the `#[global_allocator]` (size classes up to 64 KiB out of 1 MiB `mmap` chunks, one `mmap` per larger block, `munmap` with the exact length — this kernel's `munmap` needs the whole VMA, and a process has 64), and `userspace::syscall` wraps `memfd_create`/`ftruncate`/shared `mmap`/`ioctl`/`epoll_*` and `send_fds`/`recv_fds` (`SCM_RIGHTS`); `userlib_test` covers both. A program that wants argv uses `userspace::entry!(main)` (a naked `_start` handing over the entry `rsp`; `fn main(args: Args) -> i32`). Already built with `strip = true` (`userspace/Cargo.toml`'s release profile), so the build.rs strip pass is a cheap no-op safety net here.
- **C** (`C_PROGRAMS`/`DISK_C_PROGRAMS` in `build.rs`) — `userspace/c/<name>.c`, compiled directly with `clang` against `sysroot/` (built by `scripts/setup-mlibc.sh` if missing), unstripped by clang itself (full `debug_info`, no `-s`) — this is where most of the strip win comes from (e.g. `hello.elf` alone: 2.0 MB unstripped → 0.5 MB stripped). `C_PROGRAMS` (just `kdebug`) builds to `kernel/embedded/`; `DISK_C_PROGRAMS` (the rest) builds straight to `disk-image-root/bin/<name>` (no `.elf` suffix — the output name doubles as the `$PATH`-visible executable name).
- **BusyBox** (`BUSYBOX_ELF` in `build.rs`) — external `make`-based build via `scripts/build-busybox.sh` (git submodule at `busybox/`, config at `busybox-config/minimal.config`), only invoked when `kernel/embedded/busybox.elf` is missing (unlike the two families above, this isn't rebuilt unconditionally — it's slow and `make` already does its own incremental rebuilds); the strip pass, though, runs unconditionally every build (cheap no-op if already stripped), so an old unstripped `busybox.elf` from before this existed still shrinks without forcing a slow external rebuild. Always embedded — **do not change how busybox is loaded**, it's the subject of a live, unresolved debugging investigation (see `busybox_install_fork_flake` below). **Caveat of "only if missing":** after any change to sysroot ABI headers (`mlibc-port/.../abi-bits/*.h`), `rm kernel/embedded/busybox.elf` so the constants don't stay baked into the old static binary — this is exactly how the `SEEK_SET=3` bug survived one rebuild cycle.
- **DOOM** (`DOOM_NAME` in `build.rs`) — doomgeneric (git submodule `doomgeneric/`) + our platform port `doom-port/doomgeneric_constanos.c`, built by `scripts/build-doom.sh [output-path]` (defaults to `kernel/embedded/doom.elf` when invoked by hand; `build.rs` passes `disk-image-root/bin/doom` explicitly — disk-resident, not embedded); rebuilt when that output is missing *or* the port file is newer than it (mtime check — the port file is the only input that changes in practice). The Freedoom IWAD is downloaded by `scripts/fetch-freedoom.sh` into `disk-image-root/`, from where the workspace-root `build.rs` seeds it into `disk.img` (ext2), and DOOM reads it at runtime from `/mnt/freedoom1.wad` — an earlier version routed it through a kernel-embedded `/dev/freedoom1.wad` device instead, worked around what looked like ATA read corruption under DOOM's access pattern that turned out to be the SEEK_SET ABI bug below; gone now that that's fixed. Video: `/dev/fb`'s custom `FBIO_BLIT` ioctl (userspace hands a `0x00RRGGBB` buffer + dims; kernel nearest-neighbor scales and letterboxes it — `Framebuffer::blit_scaled`); raw-blit clients bypass the text console's cursor tracking entirely, so `FBIO_BLIT` flags the framebuffer dirty and the console does one full clear + cursor reset on its next text write (otherwise the next shell prompt draws over DOOM's last frame — see `drivers/framebuffer_console.rs`'s `FB_RAW_DIRTY`). Input: `/dev/input/event0` (keyboard) + `/dev/input/event1` (PS/2 mouse, real evdev wire format, see Device Driver Framework above) — `DG_DrawFrame` accumulates a frame's worth of `EV_REL` deltas and posts one `event_t{type=ev_mouse}` via `D_PostEvent`, giving real mouse-look (turn on X, forward/back on Y, `BT_ATTACK` on left click); PS/2's own sign convention (X+ = right, Y+ = up/away from the user) already matches what `g_game.c`'s mouse handling expects, so deltas are passed through unnegated. Audio: sound effects only (no music — this doomgeneric fork ships no MIDI/OPL synthesis backend at all, unrelated to the driver work) via `doom-port/doomgeneric_sound_constanos.c`'s `sound_module_t DG_sound_module` — decodes DMX sfx lumps (8-bit unsigned PCM, `W_CacheLumpNum`/`W_GetNumForName`, doomgeneric's own portable WAD API), mixes up to 16 channels with 16.16-fixed-point nearest-neighbor resampling up to 48000 Hz stereo, and writes the mixed buffer to `/dev/dsp` once per `Update()` call (~35/sec). `i_sound.c` unconditionally `#include <SDL_mixer.h>` and references `DG_music_module`/`use_libsamplerate`/`libsamplerate_scale` whenever `FEATURE_SOUND` is defined (upstream assumes an SDL_mixer-based backend) — satisfied with an empty `doom-port/stub-include/SDL_mixer.h` (no `Mix_*` symbol is actually used) and a no-op `DG_music_module` in the same sound port file, rather than patching the doomgeneric submodule itself. Run it by typing `doom` in ash.
- **Quake** (`QUAKE_NAME` in `build.rs`) — [`erysdren/quakegeneric`](https://github.com/erysdren/quakegeneric) (git submodule `quakegeneric/`, a doomgeneric-style minimal port of id Software's GPL WinQuake source) + our platform port `quake-port/quakegeneric_constanos.c`, built by `scripts/build-quake.sh [output-path]` (same default-vs-explicit-arg shape as DOOM above; `build.rs` passes `disk-image-root/bin/quake`); same "rebuilt if missing or the port file is newer" staleness check as DOOM. quakegeneric's own README claims "32-bit only" — verified that's overly conservative for a straight compile (built clean at `-m64` on the host with only two harmless warnings, neither a real pointer-width bug) before committing to the port. The shareware `id1/pak0.pak` is downloaded by `scripts/fetch-quake-shareware.sh` (an archive.org mirror of the original `quake_pak.zip`, extracting only the freely-redistributable shareware `pak0.pak`, never the full-game `pak1.pak` also in that archive) into `disk-image-root/id1/`, seeded into `disk.img` (ext2) the same way Freedoom is — `disk.img` is 96MiB, not 48MiB, to fit both IWADs plus headroom (plus the disk-resident program binaries now, ~6-7 MB more). Video: same `FBIO_BLIT` ioctl as DOOM, but quakegeneric hands `QG_DrawFrame` an 8bpp *paletted* 320x240 buffer (not ready-to-blit RGB like doomgeneric), so the port does its own index→RGB conversion via the palette `QG_SetPalette` last supplied. Input: same `/dev/input/event0`+`event1` real evdev devices as DOOM, but pulled (`QG_GetKey`/`QG_GetMouseMove`, called from inside the engine's own frame processing) rather than pushed like DOOM's `D_PostEvent` model — the port drains both fds into small queues/counters once per outer-loop iteration. Audio: sound effects via `quake-port/quakegeneric_sound_constanos.c` (replaces upstream's silent `snd_null.c`), reusing `/dev/dsp`/`ac97.rs` the same way DOOM's sound port does — reimplements the engine's `S_*` API directly (`S_Init`/`S_StartSound`/`S_Update`/channel mixing) rather than hooking a real mixing library, since quakegeneric has no `snd_mixer`-equivalent of its own. Caches decode WAV lumps straight out of `pak0.pak` (`COM_LoadTempFile`) via a small hand-rolled RIFF/WAVE chunk parser — the shareware set is 8-bit mono PCM only, verified directly against the pak, so anything else is treated as "no sound" rather than mixed wrong — then resamples to 48000 Hz stereo s16le with the same 16.16 fixed-point nearest-neighbor technique `doomgeneric_sound_constanos.c` already uses, mixing up to 32 channels. Needed two Quake-side memory fixes once real WAV data started flowing: `quakegeneric.c`'s hardcoded 8 MiB `parms.memsize` (bumped to 64 MiB via an idempotent submodule patch in `scripts/build-quake.sh`, same "patch via build script, never the checkout" convention as mlibc's `do_scanf` patch) and `Z_Malloc`'s separate, much smaller 48 KiB zone heap (`zone.c`'s `DYNAMIC_SIZE`, distinct from the general hunk — raised via `-zone 8192` in the default argv `quakegeneric_constanos.c`'s `main()` injects). The WAV parser also had a real out-of-bounds read: an unrecognized/malformed chunk's declared size went negative once cast to a signed `int` for advancing the parse cursor, sending it wildly out of bounds and reading adjacent heap memory as if it were more WAV chunks — fixed by bounds-checking each chunk's declared size against remaining file length *before* trusting it for anything, not just clamping the final `data` chunk's length after the fact. Verified via `QEMU_AUDIODEV="wav,id=snd0,path=..."` capture + `ffmpeg -af volumedetect` (real signal, not silence). Run it by typing `quake` in ash.

**Getting disk-resident binaries onto an existing `disk.img`:** the workspace-root `build.rs`'s `ensure_ext2_disk_image()` creates `disk.img` **once** and never regenerates it (see that function's doc comment — the point is a persistent image proving the ext2 *write* path survives across `cargo run` invocations), so on a tree that already has a `disk.img`, `mke2fs -d disk-image-root` — which would otherwise pick up `disk-image-root/bin/` automatically — never runs again. `sync_disk_bin_dir()` (also in the root `build.rs`, called unconditionally after `ensure_ext2_disk_image()` on every build) closes that gap without touching the create-once design: it uses `debugfs -w` (ships with the same `e2fsprogs` package already required for `mke2fs`) to `rm`+`write` each `disk-image-root/bin/*` file directly into the existing image's `/bin`, idempotently — safe to run on every build, whether `disk.img` was just created or has been sitting there since before this mechanism existed. `debugfs -f <script>` exits 0 unconditionally regardless of individual command failures (verified directly), so success is checked for real afterward by re-listing `/bin` (a fresh `debugfs -R "ls -l /bin"`) and comparing every synced file's on-disk size against its host-side size — a mismatch panics the build. Note: deleting `disk.img` by hand (the documented reset mechanism) does *not* by itself make a plain `cargo build` regenerate it — Cargo only reruns a build script when one of its *declared* `rerun-if-changed` inputs changes, and `disk.img` is an output, not a watched input, so a build script whose other inputs are all unchanged gets skipped entirely (pre-existing Cargo behavior, not specific to this mechanism); touch `build.rs` (or change any real input) to force a rerun.

**Growable user stack** (`memory::vma::VmaKind::GrowableStack`, `VmaList::grow_stack`, `elf_loader::STACK_PAGES`/`STACK_MAX_PAGES`): every process's stack VMA starts at 64 KiB and the page fault handler (`find_vma_fast_or_grow` in `process::scheduler`, wired into `init::devices::page_fault_handler`'s VMA-lookup step) extends it downward on demand — up to 8 MiB, an `RLIMIT_STACK`-style cap — when a fault lands within a guard gap just below the current low boundary. No program needs its real stack usage known in advance; this replaced an earlier hardcoded per-program override (added for Quake, whose `Host_Init` call chain overflows a small fixed stack) that required guessing every future program's needs by name. **Known flaky pre-existing bug, unrelated to this mechanism:** `busybox --install`'s own `fork()` hangs or double-faults roughly 1 boot in 3-4, reproducible on the unmodified codebase with no Quake/stack changes at all — see the `busybox_install_fork_flake` memory. (An early diagnosis wrongly pinned this on a stack-size change; it isn't — the same failure rate holds with `STACK_PAGES` left completely untouched.)

To add an embedded program:

1. Write the program (Rust in `userspace/src/bin/`, C in `userspace/c/`) or point at an externally-built ELF.
2. Register it in the relevant `build.rs` list (skip this for BusyBox-style external builds).
3. Add `("name", ProgramSource::Elf(include_bytes!("../../embedded/name.elf")))` to `PROGRAMS` in `user_programs.rs` — this alone makes it runnable both via `sys_exec`/the shell (any typed command not matching a shell builtin falls through to `fork()`+`exec_argv()`) and visible under `/bin` in initramfs (`ls`, `opendir`).

To add a disk-resident program instead (the default choice for anything not boot-critical): add it to `DISK_C_PROGRAMS` (C), `DISK_RUST_PROGRAMS` (Rust — every program linking `userspace::text` goes here) or give it the `DOOM_NAME`/`QUAKE_NAME` treatment (external build) in `kernel/build.rs`, targeting `disk-image-root/bin/<name>` — no `PROGRAMS` entry, no `include_bytes!`. It becomes runnable via `$PATH` (`/mnt/bin`) automatically once `sync_disk_bin_dir` has synced it onto `disk.img`.

Only `shell` is spawned automatically at boot (`init/processes.rs`, still looked up by that literal name) — it's PID 1, a minimal init loop, not an interactive shell itself. Before ever touching `ash`, it runs `install_busybox_symlinks()`: `mkdir("/tmp/bin")` then a real `fork()`+`exec()` of `busybox --install -s /tmp/bin` (`waitpid()`-ed to completion) — genuine `symlink(2)` calls, one per applet BusyBox was actually compiled with, using BusyBox's own `--install` machinery (`CONFIG_BUSYBOX` + `CONFIG_FEATURE_INSTALLER`), not anything this kernel computes or hand-maintains. `PATH=/tmp/bin:/bin:/mnt/bin` is then passed to `ash` so plain-name lookups find them (embedded programs via `/bin`, disk-resident ones via `/mnt/bin`). Only after that does the main loop start: `fork()`+exec `busybox ash`, `waitpid(-1)` until it is ash that exited (reaping the orphans the kernel hands PID 1 on the way), and respawn `ash` if it ever exits (its own `exit`, Ctrl-D, or a crash) instead of leaving the system with no way to type anything — see `userspace/src/bin/shell.rs::_start`. Real BusyBox `ash` (job control, line editing, `FEATURE_SH_STANDALONE`+`FEATURE_SH_NOFORK` applet dispatch, see `busybox-config/minimal.config`) is the only interactive shell now; everything else is launched on demand from it.

BusyBox's applet set now covers real day-to-day use, not just a smoke test: `vi` (full-screen editor — needed the framebuffer console's `ESC[J` no-param case, real `TIOCGWINSZ` dimensions instead of a hardcoded 80×25, `CONFIG_FEATURE_VI_WIN_RESIZE` enabled — without it `query_screen_dimensions()` is a compiled-out no-op and `vi` never even calls the ioctl, silently sticking to its built-in 24×80 fallback regardless of how correct `TIOCGWINSZ` is — and the `access()`/`W_OK` + `Stat::regular_writable` fixes below to stop opening every file `[Readonly]`), `grep`/`sed`/`awk`/`find`/`sort`/`diff`/`xargs`, `tar`/`gzip`/`gunzip`, `ps`/`top` (via the real `/proc` pid enumeration above), `df` (via `statvfs`), `du`, `chmod`, `id`/`hostname` (see mlibc sysdeps below), `md5sum`, `od`/`hexdump`, `less`/`more`, `top` with per-CPU and per-process `%CPU` (`FEATURE_TOP_*`), `nproc`, `uptime` and `free`. The last two include `<sys/sysinfo.h>` only under `#ifdef __linux__`, which this target doesn't define — and must not: spoofing it would change behaviour under every *other* `#ifdef __linux__` in BusyBox's ~250K lines — so `scripts/build-busybox.sh`'s compiler wrapper passes `-include sys/sysinfo.h` instead (every BusyBox user of that header is a genuine `struct sysinfo` user). `CONFIG_DESKTOP` is on (2026-09-26): POSIX `ps -o pid,ppid,pgid,stat,etime,time,vsz,args,...` with `FEATURE_PS_TIME`/`PS_ADDITIONAL_COLUMNS` (it replaces `PS_WIDE`), and the extra options of `find`, `ls`, `du`, `dd`, `tar` and friends. With it every `find` predicate but SELinux's `-context` (`-type`, `-exec {} +`, `-newer`, `-mtime`, `-maxdepth`, `!`, `-o`, ...), `ls`'s `-l` dates, user names, `-t`/`-S`, `-R`, `-F`, and `touch -a/-m/-d/-t/-r` (`FEATURE_TOUCH_SUSV3`). `busybox-config/minimal.config` is kept equal to the `.config` `make oldconfig` produces from it — check with a diff after touching it.

`sys_exec` (`process/syscall.rs`) resolves the requested path through the **real VFS**, not a special-cased table lookup: `resolve_exec_path` cwd-normalizes the path, then manually walks symlinks (`fs::vfs::resolve_no_follow` + `Inode::readlink`, up to 8 hops, `ELOOP` beyond that) to a canonical absolute path, which is then `fs::vfs::open()`'d and read fully into an owned buffer for the ELF loader — no more flat `PROGRAMS`-table-only fast path. This is what makes `/mnt/bin/hello` (a real `$PATH` search candidate — see the disk-resident-programs discussion above), `./ls` (explicit relative path), a bare `hello`, and `/proc/self/exe` (a real symlink, see below) all resolve through one uniform mechanism instead of three different ones agreeing by coincidence. The canonical resolved path is recorded as `Process::exe_name` (inherited across `fork()`/`clone()`) — this is what `/proc/<pid>/exe` reports. `Process::name` (Linux's `comm`, 15 bytes + NUL) is the basename of the path **as the caller passed it**, before symlinks — Linux's `kbasename(bprm->filename)` — so ash's standalone applets, re-exec'd through `/proc/self/exe`, are `exe`, exactly as on Linux; `Process::cmdline` is the argv `exec` received (`/proc/<pid>/cmdline`), which is what `ps`/`top` print (`{exe} cat /dev/zero`). `fork`/`clone` children inherit both (until 2026-09-26 every child was named `"child"` and every thread `"thread"`). `Process::name` is display-only — nothing looks a process up by it — but note `set_name` zeroes the whole field: readers stop at the first NUL, so a longer name overwritten by a shorter one would otherwise keep its tail. PID 1 was never exec'd: its `exe_name` is `/bin/shell` and its `cmdline` its name.

The fallback `ProgramSource::RawCode` embeds inline assembly tests from `process/user_test_fileio.rs` and is used for bootstrapping when no ELF exists.

## mlibc Port (`mlibc-port/constanos-sysdeps/`)

`scripts/setup-mlibc.sh` copies this into the `mlibc/` submodule checkout and rebuilds `sysroot/` — it's the only thing that survives a `git submodule update` reset of `mlibc/` itself, so **any fix that needs to live inside the `mlibc/` submodule tree goes through a patch step in `setup-mlibc.sh`, never a direct edit to the checkout** (see the `do_scanf` patch below for the pattern: idempotency-checked via `grep`, then a Python string-replace, with an explicit error if upstream's text ever stops matching).

**ABI-constant hygiene:** the `abi-bits/*.h` headers were originally copied from non-Linux mlibc ports and have repeatedly disagreed with this kernel's Linux-numbered syscall ABI (`MAP_ANONYMOUS`, `O_CREAT`, `POLLOUT`, `F_DUPFD`, `WIFEXITED`, `ENOTEMPTY`, and `SEEK_SET`, which was `3` — `lseek(fd, n, SEEK_SET)` returned EINVAL while SEEK_CUR/SEEK_END coincidentally worked, making files "go empty" after any `fseek(END)` size probe). Two whole headers have since been replaced with mlibc's own `abis/linux/` versions rather than patched constant by constant: `socket.h` (where `sa_family_t` was `unsigned int`, shifting `sun_path` two bytes and breaking every `sockaddr_un`; `AF_UNIX` was 3; `SHUT_RD`/`WR` were swapped; `msghdr`/`cmsghdr` had the wrong field widths for x86-64) and `errno.h` (BSD-numbered, so it agreed with Linux only up to `ERANGE` — `EAGAIN` was 35 against the kernel's 11, making `errno == EAGAIN` a test that could never succeed, and every socket errno past 34 was wrong). `resource.h` followed on 2026-09-26: `RUSAGE_SELF`/`CHILDREN` were 1/2 (Linux: 0/-1) and every `RLIMIT_*` was BSD-numbered. Then `fcntl.h`: its `O_*`/`F_DUPFD..` had been fixed by hand, but every `AT_*` was wrong (`AT_SYMLINK_NOFOLLOW` 4 for 0x100, `AT_REMOVEDIR` 8 for 0x200), as were `F_RDLCK`/`F_WRLCK`, the `F_SEAL_*` and `POSIX_FADV_*`. When touching any of these headers, cross-check values against `mlibc/abis/linux/` and the kernel's own constants, rebuild the sysroot, **and delete `kernel/embedded/busybox.elf` + `doom.elf`** so the "only build if missing" binaries don't keep the old constants baked in.

**More upstream bugs, fixed the same way (2026-09-26):** `asctime_r` formatted `"%.2d:%.2d%.2d"` — no colon before the seconds — so every `ctime()` string was one character short and anything that slices it by offset read garbage (`ls -l` printed the year `970`; a `setup-mlibc.sh` string patch). And `do_scanf` ignored the field width of every integer conversion (`%4u%2u` read all the digits into the first field: `touch -t` said "invalid date"), and read `%x` of a lone `"0"` as a prefix with no digits (a mismatch). That fix is too big for a string replace, so it is a unified diff, `mlibc-port/patches/scanf-int-width.patch`; `setup-mlibc.sh` applies every `mlibc-port/patches/*.patch` after its string patches, skipping one that already reverses cleanly and stopping on one that neither applies nor reverses.

**Real upstream mlibc bug, patched here:** `options/ansi/generic/stdio.cpp`'s `do_scanf` only advanced its internal `count` inside the `if(typed_dest)` branch of the `append_to_buffer` lambda shared by the `%s`/`%c`/`%[` conversions. A *suppressed* conversion (`%*s` — `dest` deliberately null) never touched `count`, so the very next `NOMATCH_CHECK(count == 0)` read "matched nothing" regardless of what was actually consumed, and `do_scanf` returned early right at the first `%*s` in any format string — silently truncating the match count for everything after it. Found via BusyBox `ps`/`top`: `libbb/procps.c`'s `/proc/<pid>/stat` parser skips half its fields with exactly that conversion, so every pid was read correctly but `procps_scan` still reported zero matches (`n=5` instead of the required `11`). Not specific to this port or to BusyBox — any `sscanf`/`fscanf` call with a `%*s` anywhere in it was affected.

**Exit-time flushing needs what `crtbegin.o` would have done.** mlibc's static build leaves calling `__cxa_finalize` to `crtbegin.o`/`crtend.o`, which this port never links (`-nostdlib`, just `crt1.o` + `libc.a`) — so C++ global destructors registered through `__cxa_atexit(..., &__dso_handle)` never ran, among them `stdio_guard`, the one that flushes every `FILE` at `exit()`. A terminal hid it (stdout is line-buffered there); to a file or pipe stdout is fully buffered, and every C program wrote **nothing**: `hello > f` was 0 bytes, `fpu_test | wc -l` printed 0. `generic.cpp` now carries a `[[gnu::destructor]]` next to its `__dso_handle` that calls `__cxa_finalize(&__dso_handle)`, run by `exit()`'s `.fini_array` pass after the plain `atexit` handlers (fixed 2026-09-25). After touching this, rebuild the sysroot and relink everything static: `rm kernel/embedded/busybox.elf`, and `doom`/`quake` only rebuild when missing.

Sysdeps added beyond the original bootstrap set (all in `generic/generic.cpp` unless noted): `sys_memfd_create`/`sys_ftruncate`/`sys_yield` (and a `memfd_create()` wrapper — mlibc's is Linux-option-only; `setup-mlibc.sh` moves its declaration out of that guard in `<sys/mman.h>`), a `sys_vm_map` that passes `flags`/`fd`/`offset` through (it used to `__ensure` anonymous and drop them), `sys_access`, `sys_symlink`/`sys_symlinkat`, `sys_chmod`/`sys_fchmod`/`sys_fchmodat`, `sys_statvfs`/`sys_fstatvfs`, `sys_getgroups`. A few aren't real kernel round-trips at all: `sys_getuid`/`geteuid`/`getgid`/`getegid` return `0` unconditionally (single-user kernel — matches the existing style), `uname()`/`gethostname()`/`sethostname()` are plain userspace stubs (hostname is a per-process static, so `sethostname` in one process is invisible to a process started afterward — nothing here needs cross-process persistence), and `setmntent`/`getmntent`/`endmntent` (`mntent.h`) port the kernel's own fixed, compile-time mount table directly rather than parsing a real (nonexistent) `/etc/mtab` — enough for `df` with no arguments to enumerate mounts. `sysinfo()` is the kernel's `sysinfo` (#99); its header (`include/sys/sysinfo.h`) is a standalone port of mlibc's own Linux-option-only version. Time and CPUs (2026-09-26): `sys_times`, `sys_getrusage`, `sys_clock_getres`, a `sched_getaffinity()` definition (`setup-mlibc.sh` moves its declaration out of the Linux-option guard in `<sched.h>`, like `memfd_create`), and `sys_sysconf` — `_SC_CLK_TCK` 100 (mlibc's default was 1000000), `_SC_NPROCESSORS_ONLN`/`CONF` from the affinity mask (default 1), `_SC_OPEN_MAX` 16, `_SC_PHYS_PAGES`/`AVPHYS_PAGES`; anything else falls back to mlibc's defaults.

## Key Design Invariants

- **Buddy is the only physical frame allocator** after `init_core`. Do not create a second `BootInfoFrameAllocator` over the same memory regions.
- **`memory` module does NOT import `process`**. Demand paging is kept dependency-free from the process layer; the fault handler in `init/devices.rs` bridges them.
- **Interrupt safety:** Always `cli` before acquiring `SCHEDULER` and `sti` after releasing it. The timer ISR acquires the lock; holding it with interrupts enabled causes a deadlock.
- **The same rule covers the allocators, and this one was learned the hard way.** `BUDDY` and `SLAB_ALLOCATOR` (`kernel/src/allocator/mod.rs`) are `diag::IrqMutex<_, KernelIrq>` — not plain `spin::Mutex`es — so the discipline is structural rather than a convention every call site has to remember: `IrqMutex` exposes no `lock()`/`try_lock()` and no public guard type at all, only `with`/`try_with`, which disable interrupts via `x86_64::instructions::interrupts::without_interrupts` *first* and take the real lock only inside that closure. `without_interrupts` (not a bare `cli`/`sti` pair) because it restores the *previous* state and is therefore safe to call from a context that already has interrupts off (the timer ISR itself). Why it matters: ordinary interruptible kernel code allocates all the time — `vfs::resolve_inner` allocates a `Vec<&str>` just to split a path — and if the timer fires while that code holds the allocator lock, `timer_preempt_handler` → `Scheduler::switch_to_next` can itself need to allocate (growing a run queue's `VecDeque`), reentering the same non-reentrant lock on the same CPU and spinning forever at 100% CPU. That was the real cause of a ~1-in-10 debug-boot hang that went unexplained for months while being blamed on `fork`/COW/the physical allocator; it needs no `fork` and no `exec` at all. A `try_lock()` canary for this (`kernel::debug`'s former `SLAB_LOCK_CONTENDED`, `/proc/kdebug`) was later retired — not because the bug is fixed, but because nothing that runs inside the critical section can allocate any more (`mm` links no `alloc`, and every seam it calls out through is either pure address arithmetic or an allocation-free print), so the reentrant acquisition it watched for can no longer happen; see `kernel/src/allocator/mod.rs`'s comment above `SlabGlobalAlloc`'s `GlobalAlloc` impl for the full argument. `IrqMutex` (`diag/src/irqmutex.rs`) is what makes the ordering itself impossible to get wrong now — there is exactly one path to the protected value, and it always disables interrupts before it ever touches the real lock. **Any new global taken on an allocating path needs the same treatment.**
- **Context switches restore all GPRs** via `jump_to_trapframe` (asm `pop` sequence + `iretq`). Never use partial restores that leave callee registers from the killed process.
- **Every kernel entry must clear the direction flag (DF).** Interrupt delivery does not clear it, and `syscall` only clears the RFLAGS bits named in `IA32_FMASK`. `rep movsb` obeys DF, so an entry taken while the interrupted code sat between a `memmove`'s `std` and its `cld` runs the *whole* kernel path — including `*proc.trapframe = *current_tf`, which compiles to `rep movsb` — copying **backward**, writing the 160 bytes *before* the trapframe box instead of into it. That was the single root cause behind three separate long-standing symptoms (a stale-frame resume orphaning an in-flight syscall's locks, jumps into heap data as code, and a box that "ignored" its own memcpy); measured at ~4-8% of debug boots, 48/48 clean after the fix. Two mechanisms cover it, both required: `process/tss.rs` masks DF in `IA32_FMASK` (bit 10, exactly as Linux does), and both hand-written asm entry stubs (`timer_preempt.rs::timer_interrupt_entry`, `syscall/mod.rs::syscall_entry_fast`) emit `cld` as their first instruction. The rustc `x86-interrupt` shims already emit `cld`, so IDT handlers are covered for free — **any new hand-written entry stub is not**. `jump_to_trapframe` is a *resume*, not an entry, and must NOT clear DF (it restores the frame's own RFLAGS). See `docs/hang-hunt-bug2-findings.md`.
- **`sys_exit` must keep IF=0 all the way to the `iretq`.** Its epilogue runs on the kernel stack of the process it just queued for deferred free; re-enabling interrupts before `jump_to_user`'s stack switch lets a timer tick free that stack out from under the running epilogue. `scheduler::tick(interrupted_rsp)` is the complementary half: a queued kstack containing the interrupted RSP stays queued for a later tick. Other CPUs are kept off it by `LEAVING` (SMP Scheduling above).
- **SMP rules** (stage 0 of `docs/smp/smp-plan.md`, written before SMP existed to stop the debt growing; since stage 7 processes run on every CPU and they are simply the rules):
  - **IF=0 is not mutual exclusion.** Shared state takes a real lock; `cli` only prevents reentry on the *same* CPU. `keyboard::DECODER` is the worked example: two ISRs (IRQ1 and the timer's USB poll) wrote it through an `UnsafeCell` justified as "only the keyboard ISR touches it" — never true once USB existed, safe only because `cli` serialized both on one CPU. It is an `IrqMutex` now.
  - **No new `static mut` or global `UnsafeCell` for shared state.** Per-CPU state gets indexed by `cpu::cpu_id()`; existing offenders are the plan's inventory, not precedent.
  - **Every PTE change invalidates through `memory::tlb`** (`invalidate_page(pml4, addr)` for user mappings, `invalidate_kernel_page` for kernel ones) — never `x86_64::instructions::tlb::*` or `MapperFlush::flush()`; consume a `MapperFlush` with `.ignore()` and pass its page. Since stage 5 these shoot down the other CPUs (see TLB Shootdown). **Every CR3 load goes through `tlb::switch_to`**, which records it — a bare `Cr3::write` makes this CPU invisible to user-mapping shootdowns. `grep -rn 'instructions::tlb\|\.flush()' kernel/src/memory` should find only `tlb.rs`.
  - **`gs` is used in exactly one place: `syscall_entry_fast`'s four-instruction `swapgs` window** (stage 2, `kernel/src/cpu/percpu.rs`). It loads this CPU's kernel stack from `PerCpu`, then swaps straight back, so every other path — the timer stub, rustc's `x86-interrupt` shims (which never `swapgs`), `jump_to_trapframe` — runs with user mode's GS_BASE and never reads it. Rust reaches per-CPU data through `cpu::cpu_id()`, which reads the task register (`str`), not `gs:`. **Do not add a `gs:` access anywhere else**; `percpu::check_gs_invariant` (every syscall + every tick) panics if `IA32_KERNEL_GS_BASE` stops pointing at `&PERCPU[cpu]`. Each CPU has its own TSS slot in one GDT at `FIRST_TSS_SELECTOR + 16·n` (stage 3, `process/tss.rs`) — one GDT per CPU with the TSS at the same index would make `cpu_id()` stop telling CPUs apart.
  - **Nothing new hangs off the timer tick** without saying whether it is global work (BSP only: `hrtimer`, the USB poll) or per-CPU work (scheduling).
  - **Kernel locks are `crate::sync::Mutex`, never `spin::Mutex`** (stage 6, `kernel/src/sync.rs`): its relax strategy answers TLB shootdowns, so no CPU can spin on a lock with IF=0 while the holder waits for that CPU's acknowledgement. **A lock an ISR also takes is an `IrqLock` or `diag::IrqMutex`**, or the ISR uses `try_lock` (`FB_STATE`, `FRAMEBUFFER`, `CONTROLLERS`): a plain lock held by a syscall with IF=1 is a deadlock with *one* CPU the moment that ISR lands on it — syscalls enter with IF=0 but most re-enable it at their first guard, and `TERMIOS`, `EPOLL_INSTANCES` and `SERIAL` had exactly that bug. (`serial_println!` with IF=0 only *tries* the lock and falls back to the lock-free writer.) **Any other IF=0 busy-wait calls `memory::tlb::service_pending`** (the USB transfer waits do). Lock order and the full per-lock audit: the stage 6 resolution in `docs/smp/smp-plan.md`.
  - **Check-then-sleep must be one step.** Checking a condition, registering as a waiter and blocking were kept together only by IF=0 on one CPU; with a waker on another CPU, a wakeup in between is lost. `FUTEX_WAIT` holds the scheduler lock and `FUTEX_WAITERS` across all three; `poll`, stdin, pipes and sockets close it as SMP Scheduling above describes. **A new blocking path needs one of those mechanisms.**
  - **A process can migrate at any preemption point.** Code running with IF=1 must not hold on to `cpu::cpu_id()` or anything indexed by it across a point where it can be preempted; the next instruction may run on another CPU.
- **Nothing per-process may live in a per-CPU global across a preemption point.** The current syscall's frame is `syscall::current_tf_ptr()`, computed from the running process's own kernel stack top (this CPU's `PerCpu::kernel_rsp` `- sizeof(TrapFrame)`, where `syscall_entry_fast` always builds it — Linux's `task_pt_regs`). It used to be a global (`CURRENT_SYSCALL_TF`) stored at syscall entry, which went stale whenever a syscall was preempted with IF=1 and another process made syscalls before it resumed: the post-syscall signal check then delivered the parent's SIGCHLD into the dead child's frame and wrote the signal frame over the parent's live stack. Symptom: `ash` dying at its `exit` builtin (`rip` 0, 0x202, or an address in the child's binary) after a short-lived child, under host load only. Found 2026-09-24 by the first autorun job; the user-segfault stack dump (`init::devices::dump_user_stack`) and `ktrace!(PROC)` on signal delivery/`sigreturn` are what cornered it.
