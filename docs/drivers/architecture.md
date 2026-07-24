# Driver architecture (current)

This describes the driver design **as it exists today**, and the reasoning behind it. For
the imperative "how do I add one" steps, see the `kernel-drivers` skill; for the future
direction, see [roadmap.md](roadmap.md).

## The core principle

Hardware access cannot be unit-tested off the hardware. Pure logic can. So every driver is
split so that **pure logic sits behind a seam** and only real port/MMIO/DMA pokes remain in
the kernel proper. The payoff: parsers and register-sequencing logic run under `cargo test`
in milliseconds, and the hardware-specific glue stays small and obvious.

A closely related principle: the problem was never "globals exist," it was *implicit,
untestable global state*. State is encapsulated in a struct; where a single global instance
is genuinely correct (a parse-once, read-many table), it stays — it's just no longer where
the untestable logic lives.

## Two layers — kept distinct

A device can participate in either or both of these; they are not the same interface.

### 1. `FileHandle` — the VFS/file view

`kernel/src/process/file.rs` defines `trait FileHandle` (`read`/`write`/`stat`/
`getdents64`/`dup`/`seek`/`chmod`). This is what a *process* sees on a file descriptor. A
device only needs it if it is exposed as a file (e.g. `/dev/dsp`). These shims live in
`kernel/src/drivers/` and are typically thin — they delegate to the real hardware driver
(see `drivers/dev_dsp.rs`, which forwards `write()` to `ac97::write_pcm`). Registration is
one line in the `DEVICES` slice in `drivers/mod.rs`.

### 2. The hardware driver — owns the device

This is the layer the seam pattern targets. Not every driver has a file view: ACPI, the
PIT, and the RTC are hardware drivers with no `/dev` node at all. AC97 has both a hardware
driver *and* a `/dev/dsp` `FileHandle`.

## The `hal` crate — pure, host-testable

`hal/` is a **standalone crate** (its own empty `[workspace]` table, `exclude = ["hal"]` in
the root workspace; the kernel depends on it via `hal = { path = "../hal" }`). It is
`#![no_std]` (except under `cfg(test)`) with `extern crate alloc`, and — critically — has
**zero bare-metal dependencies** (no `x86_64` crate). That is what lets it compile for both
the kernel target (`x86_64-unknown-none`) and the host, so its tests run with a plain
`cargo test`.

It contains only:

- **The hardware seams** — `hal::PortIo` (legacy port I/O) and `hal::PhysMem` (physical
  memory reads), plus a `MockIo` for tests.
- **Pure logic** — parsers/decoders/register sequences that read *through* a seam, return
  `Result<_, _>`, and do no logging and touch no globals. They read into local `[u8; N]`
  buffers and decode with `from_le_bytes` (not `#[repr(C, packed)]` + `read_unaligned`).

## The kernel side — `kernel/src/hal.rs`

- **Production seam implementations:** `X86PortIo` (wraps `x86_64::instructions::port::Port`)
  and `KernelPhysMem` (reads through the bootloader's fixed physical-memory offset via
  `memory::physical_memory_offset()`).
- **`Driver` trait:** `fn name(&self) -> &str; fn init(&mut self) -> Result<(), DriverError>`.
  Best-effort: `init()` must never panic; a driver whose hardware is absent returns `Err` and
  the kernel keeps booting.
- **`run_all(&mut [&mut dyn Driver])`:** runs each driver's `init()`, logging
  `[hal] driver '<name>' init: OK/FAILED`. Boot goes through this instead of an ad-hoc list.

## Data flow

```
boot (init/mod.rs)
  └─ hal::run_all([ &mut AcpiDriver, ... ])
       └─ AcpiDriver::init()                        [kernel/src/acpi.rs]
            ├─ hal::acpi::parse(&KernelPhysMem, …)  [pure, hal/src/acpi.rs]
            │     └─ reads physical memory ONLY via the PhysMem seam
            ├─ logs summary + [acpi] SELFTEST …     (kernel-side, not in hal)
            └─ stores result in a spin::Once global; topology() exposes it
```

The same shape generalizes to any driver: the kernel adapter owns hardware access + logging
+ any global, and calls into pure `hal` logic across the seam.

## The storage stack — a second, related seam

Everything above is about *hardware* drivers (`hal::PortIo`/`hal::PhysMem`) feeding pure
parser/register logic. The storage stack is a sibling seam with the same shape but a
different pair of endpoints: `hal::block::BlockDevice` (`hal/src/block.rs`) sits between a
filesystem and whatever provides its sectors, exactly like `PortIo` sits between a driver and
its registers.

- **`hal::block::BlockDevice`** — `present()`/`read_sectors(lba, count, buf)`/
  `write_sectors(lba, count, buf)`, sector-granular (512 bytes, `hal::block::SECTOR_SIZE`) and
  LBA28-shaped, not filesystem-block-shaped. See that file's module doc comment for why
  sectors and not blocks: a real block layer is sector-granular underneath any filesystem's
  own block size, and keeping the trait's shape identical to the `block::ata` free functions
  it replaces made migrating `fs::ext2` onto it a mechanical rename at each of its 9 call
  sites instead of a structural rewrite of an already invariant-heavy (~2000-line) file.
- **`kernel::block::AtaBlockDevice`** (`kernel/src/block/mod.rs`) — the production
  implementation, a zero-sized wrapper around `block::ata`'s existing free functions. This is
  what `fs::ext2::init()` mounts against at real boot.
- **`hal::block::MemDisk`** — a `Vec<u8>`-backed disk implementing the same trait, host-tested
  in `hal` (7 tests: round-trip, out-of-range/too-small-buffer error paths, `present()`,
  snapshotting) and reused by the kernel's QEMU integration test to mount ext2 without
  touching real hardware or `disk.img` — see Testing below.
- **`fs::ext2::Ext2Fs`** now holds a `device: Box<dyn BlockDevice>` field instead of calling
  `block::ata::*` directly; `fs::ext2::init()` constructs an `AtaBlockDevice` for real boot,
  and a `#[cfg(test)]`-only `fs::ext2::init_with_device()` accepts any `BlockDevice` for the
  integration test.

**Important asymmetry, stated plainly:** unlike the six hardware drivers above,
`kernel/src/block/ata.rs` itself is **not** migrated onto the `hal::PortIo` seam — its port
I/O (`Port<u8>`/`Port<u16>` reads/writes for LBA28 PIO transfers) still lives directly in that
file, untouched by this work. What *is* seamed off is the layer immediately above it: `fs::ext2`
no longer names `block::ata` at all, but `block::ata` itself is exactly as untestable as before.
Migrating `ata.rs` onto `PortIo` (so its command-sequencing logic — drive-select, LBA
programming, DRQ/BSY polling — gets host tests the way ac97's register protocol did) is real,
separate future work, not a gap this refactor silently left.

`fs::ext2` also stays inside the `kernel` crate, not `hal` — it depends on the `Inode`/
`FileHandle` traits that live there, and extracting its ~2000 lines (bitmap/inode/directory
logic, much of it entangled with the `EXT2_LOCK` coarse-locking discipline and the
`reclaim_orphans` mount-time repair pass) into a host-testable pure-logic crate is a
substantial, separate undertaking — noted as future work, not started here. The `BlockDevice`
trait living in `hal` now is what a future extraction would need already in place.

## Testing

- **Host unit tests (fast) — in `hal`.** Build synthetic input (a byte image for a parser, a
  pre-seeded `MockIo` for a port driver), run the logic, assert the `Result`. Always cover
  the failure/guard paths, not just the happy path — malformed input, bad checksums,
  zero/oversized length fields (must terminate, not loop or read out of bounds). Template:
  `hal/src/acpi.rs`'s six tests + its `build_valid_image`/`fix_checksum` helpers.
- **QEMU integration (hardware smoke test) — a real, automated PASS/FAIL, not just a human
  reading serial output.** Two halves:
  - *Guest side* (`kernel/src/test_framework.rs` + `kernel/src/hw_tests.rs`): a
    `#![feature(custom_test_frameworks)]` harness. `kernel_main`'s `#[cfg(test)]` branch
    (`main.rs`) runs `init::test_support::boot_for_tests` — the reusable "how much of
    `init::boot` does a test need" answer (IDT built, physical memory offset + Buddy live,
    zero-frame allocated, then each driver a test needs run through the same `hal::run_all`
    registry real boot uses) — then `test_main()` runs every `#[test_case]`. A failing
    assertion panics straight into a test-mode `#[panic_handler]` that writes the failure code
    to the `isa-debug-exit` I/O port (0xf4); the runner writes the success code once every
    case returns.
  - *Host side* (`qemu-test-runner/`, a standalone crate — see its `Cargo.toml` for why it
    isn't just another root-package binary): boots the built test ELF in QEMU headless with
    `-device isa-debug-exit,iobase=0xf4,iosize=0x04`, enforces a timeout, and maps the exit
    code back to pass/fail.
  - Driven by `scripts/run-kernel-tests.sh`, **not plain `cargo test`** — see that script's
    header comment and `kernel/.cargo/config.toml`'s `runner` key comment for the verified
    reason (`-Z build-std` + `cargo test` building the `kernel` bin twice in one invocation
    produces two incompatible `core` builds; `cargo build --tests` doesn't hit this).
  - `run_selftest` in `kernel/src/acpi.rs` still prints the human-readable
    `[acpi] SELFTEST PASS/FAIL` boot log; `hw_tests.rs`'s `acpi_selftest_passes` is the same
    checks (`acpi::selftest_ok`) as a real assertion. Later hardware-path tests (APIC) add
    more `#[test_case]`s the same way rather than a new mechanism.
  - `hw_tests.rs::ext2_memdisk_roundtrip` is the storage-stack seam's own case: it mounts
    `fs::ext2` on a `hal::block::MemDisk` carrying a small (256 KiB) ext2 image built by hand
    at kernel *runtime* (`fs::ext2::build_minimal_image` — deliberately not `mke2fs`-generated
    and embedded at build time, so the test binary has zero host-tool dependency beyond what
    the root build already needs), then drives create/write/read/mkdir/rename/symlink/
    unlink/rmdir through the real VFS free functions (`fs::vfs::{mkdir,symlink,rename,
    unlink,rmdir,open,stat}`) — the same path every syscall handler uses. `disk.img` is never
    touched. `fs::ext2::EXT2` is a single `spin::Once` global, so this is deliberately one
    large `#[test_case]` scripting the whole scenario rather than several independent ones —
    a second `init_with_device()` call from a separate test case would silently no-op instead
    of mounting a fresh image (see that function's doc comment).

## Current status

All six hardware drivers are migrated onto this pattern: **ACPI** (`PhysMem`, the pilot),
**ac97** (`PortIo` register protocol + the pure `plan_fill` ring state machine), **keyboard**
and **mouse** (pure decoders needing no seam at all — mouse also has a `PortIo` 8042 enable
sequence), and **PIT**/**RTC** (`PortIo`; PIT's divisor arithmetic and RTC's whole CMOS
protocol, including `days_from_civil`, are pure and directly testable). **ATA
(`kernel/src/block/ata.rs`) is the one hardware driver still outside this pattern** — see the
storage stack section above for what *is* seamed (the layer above it, `fs::ext2`) versus what
isn't (the driver itself). 71 host tests in `hal` at last count (`cd hal && cargo test`; 64
from the six hardware drivers + 7 for `hal::block::MemDisk`).

Neither PIT nor RTC joined the `Driver` trait / boot registry: `Driver::init()` takes no
arguments, but PIT's rate is caller-supplied (`init_hardware_interrupts` always passes
100 Hz today) and RTC is a read-once-at-boot probe with no persistent hardware state to own
afterward (no periodic RTC IRQ) — both stay direct calls from `init/mod.rs` /
`time::init()`, same as before. This is a case where forcing the shared lifecycle would add
structure without a real place for it to live, not a gap left to fill in later.

The next planned work (APIC) builds on this now-clean base. See the [roadmap](roadmap.md).
