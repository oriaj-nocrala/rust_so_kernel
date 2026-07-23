---
name: kernel-drivers
description: Playbook for writing, porting, or adapting a hardware driver in this kernel (rust_so_kernel). Use whenever adding or modifying a device driver, hardware probe, or bus/table parser. Covers the two-layer architecture (FileHandle vs hardware driver), the hal crate with PortIo/PhysMem hardware seams, the Driver trait and boot registry, exposing a /dev device, and testing (host cargo test with mocks plus QEMU selftest). Related keywords include add a driver, port a driver, new device, test a driver, PortIo, PhysMem, hal crate.
---

# Writing / porting / adapting drivers

This kernel separates **hardware access** (untestable off-hardware) from **pure logic**
(host-testable). The goal of every driver you write or migrate: put pure logic behind a
**seam** so it runs under `cargo test` in milliseconds, and keep only real port/MMIO/DMA
pokes in the kernel. The enemy is not globals — it's *implicit, untestable global state*.
Encapsulate state in a struct, generic over the seam.

The **ACPI driver is the worked reference** for everything below — read it before starting:
`hal/src/acpi.rs` (pure), `kernel/src/acpi.rs` (adapter), `kernel/src/hal.rs` (seams + registry).

Design rationale and where this is all heading (Linux-class device model → BSD-style compat →
proprietary drivers) live in `docs/drivers/` (`architecture.md`, `roadmap.md`). This skill is
the *how-to*; those docs are the *why* and the *direction*.

## Two layers — don't confuse them

1. **`FileHandle`** (`kernel/src/process/file.rs`) — the *file/VFS* interface a process sees on
   an fd (`read`/`write`/`stat`/`getdents64`/`dup`/`seek`). Only needed if the driver is
   exposed as a file (e.g. `/dev/foo`). Thin driver shims live in `kernel/src/drivers/` and
   usually just delegate to the real hardware driver (see `drivers/dev_dsp.rs` →
   `ac97::write_pcm`).
2. **The hardware driver** — owns hardware + state. This is where the seam pattern applies.
   A driver may have both layers (ac97 has a `/dev/dsp` FileHandle + the ac97 hardware driver)
   or only layer 2 (acpi, pit, rtc have no device file).

## The `hal` crate (pure, host-testable)

`hal/` is a **standalone crate** (its own empty `[workspace]` table; excluded from the root
workspace via `exclude = ["hal"]`; kernel depends via `hal = { path = "../hal" }`). It is
`#![cfg_attr(not(test), no_std)]` + `extern crate alloc` and has **zero bare-metal deps** (no
`x86_64` crate) so it builds for both `x86_64-unknown-none` and the host.

Put in `hal`:
- **Seam traits** — `PortIo` (legacy port I/O) and `PhysMem` (physical-memory reads) in
  `hal/src/lib.rs`, plus `MockIo` for tests.
- **Pure logic** — parsers/decoders/register-sequencing that read *through* a seam, return
  `Result<_, SomeError>`, and do **no logging and touch no globals**. Read via the seam into
  local `[u8; N]` buffers and decode with `from_le_bytes` — never `#[repr(C, packed)]` +
  `read_unaligned` (the ACPI parser was rewritten this way: simpler and host-safe).

Do NOT put in `hal`: anything that touches real ports/memory, `serial_println!`, or a global.

**Not every driver needs a seam.** Some driver logic is a pure `(state, input) -> actions`
state machine where the input already arrives from an ISR — no hardware access at all. The
PS/2 keyboard decoder (`hal::keyboard::KeyDecoder::process(scancode) -> KeyOutput`) and the
mouse packet decoder (`hal::mouse::PacketDecoder::push_byte(byte) -> Option<MouseEvent>`) are
exactly this: they need **no `PortIo`, no `PhysMem`, no mock** — host-test them directly by
feeding inputs and asserting outputs. When a driver *does* mix pure decode with hardware
setup (mouse: pure packet decode + a `PortIo` 8042 enable sequence), split them — the decode
needs nothing, only the setup takes a seam.

**The load-bearing pattern is "decide, don't do":** the pure function *returns what should
happen* (a `FillPlan`, a `KeyOutput`, an `Option<Event>`); the kernel adapter *executes the
effects* (DMA copy, ring push, `tty::feed_input`). That split is what makes the logic pure —
`ac97::plan_fill` and `keyboard::KeyDecoder::process` are the reference examples. Keep any
ISR-path return type allocation-free (a fixed inline array + count, not `Vec` — see
`KeyOutput`). The adapter holds the decoder's state in an ISR-safe `static UnsafeCell`
(single-writer trust model), since a `spin::Mutex` in an ISR risks deadlock.

Reusable test double: **`ScriptedIo`** (in `hal/src/lib.rs`, `#[cfg(test)]`) — per-port FIFO
read queues (sticky last value) + a recorded write log — for any `PortIo` sequence test
(codec reset, 8042 enable). Use it instead of hand-rolling a mock per driver.

## The kernel side (`kernel/src/hal.rs`)

- **Production seam impls**: `X86PortIo` (wraps `x86_64::instructions::port::Port`),
  `KernelPhysMem` (reads via `crate::memory::physical_memory_offset()` — the same idiom every
  driver uses for physical access). Reuse these; don't reinvent.
- **`Driver` trait**: `fn name(&self) -> &str; fn init(&mut self) -> Result<(), DriverError>;`
  Best-effort — `init()` must **never panic**; return `Err` and let boot continue (same
  convention as mouse/ac97/rtc).
- **`run_all(&mut [&mut dyn Driver])`**: best-effort registry runner; logs `[hal] driver
  'name' init: OK/FAILED`. Boot wiring goes through this.

## Step-by-step: a new hardware driver

1. **Pure logic → `hal`.** Add `hal/src/<name>.rs` (`pub mod <name>;` in `hal/src/lib.rs`).
   Take `&dyn PortIo` / `&dyn PhysMem` as input. Return `Result`. Keep any variable-length
   parse **bounds-checked before trusting a length field** (the anti-OOB discipline — this
   kernel already shipped one OOB bug from skipping it; the ACPI malformed-table tests exist
   to guard exactly this).
2. **Encapsulate state in a struct, generic over the seam** when the driver holds hardware
   state: `struct Foo<IO: PortIo> { io: IO, ... }`. The kernel global becomes
   `static FOO: Mutex<Option<Foo<X86PortIo>>>` — one instance, but the *type* is testable as
   `Foo<MockIo>`. (A read-once table like ACPI topology needs no generic struct — a
   `spin::Once<Topology>` in the adapter is fine; that's not the anti-pattern.)
3. **Kernel adapter** (`kernel/src/<name>.rs`): owns hardware access (construct `X86PortIo` /
   `KernelPhysMem`), calls the `hal` logic, does logging + holds the global, and implements
   `crate::hal::Driver`. Re-export any `hal` types the rest of the kernel names
   (`pub use hal::<name>::{...}`) so callers don't reach into `hal` directly.
4. **Boot wiring** (`kernel/src/init/mod.rs`): add the driver to a `run_all([...])` call at the
   right point in the sequence. Physical-memory drivers must run **after** `memory::init_core`
   (line ~46). Interrupt-related setup must run before `init_hardware_interrupts`.
5. **Expose as `/dev/<name>` (only if it's a file device)**: add `kernel/src/drivers/<name>.rs`
   implementing `FileHandle` (delegating to the hardware driver), then one line in the
   `DEVICES` slice in `kernel/src/drivers/mod.rs`. See `drivers/dev_dsp.rs` for the minimal
   shape.
6. **Introspection (optional, encouraged)**: expose state via a synthetic `/proc/<name>` by
   mirroring `KdebugInode`/`render_kdebug` in `kernel/src/fs/procfs.rs` (a `render_<name>()` +
   an inode + a `match name` arm + a `readdir` entry). `cat` output is mirrored to serial, so
   it doubles as a debug read. This kernel favors `/proc` introspection.

## Testing

**Host unit tests (fast, the ones that matter) — in `hal`:**
- Put `#[cfg(test)] mod tests` next to the pure logic. Build synthetic input (a byte image for
  a parser; a `MockIo` pre-seeded with register values for a port driver), run the logic, and
  assert the `Result`.
- **Always test the failure/guard paths**, not just the happy path: malformed input, bad
  checksums, zero/oversized length fields (must terminate, not loop or read OOB). See
  `hal/src/acpi.rs`'s 6 tests + the `fix_checksum`/`build_valid_image` helpers as the template.
- Run: `cd hal && cargo test`. Must run in <1s, no QEMU.

**QEMU integration (hardware smoke test):**
- Add a boot-time self-check in the kernel adapter that validates against known QEMU values and
  prints a stable, greppable marker (`[<name>] SELFTEST PASS/FAIL`) — never panic on FAIL. See
  `run_selftest` in `kernel/src/acpi.rs`.
- Verify with `scripts/qemu-debug.sh start` → `wait-for "SELFTEST"` → `log`. (A formal
  `#![test_runner]` harness over the already-configured `isa-debug-exit` device in
  `kernel/Cargo.toml` is the pending next step for real `cargo test`-style integration.)

## Verify a change end-to-end

```bash
cd hal && cargo test                                    # host logic tests
cd kernel && cargo build --target x86_64-unknown-none   # kernel builds
scripts/qemu-debug.sh start && scripts/qemu-debug.sh wait-for "SELFTEST"
scripts/qemu-debug.sh log 40                            # check summary + SELFTEST PASS
scripts/qemu-debug.sh stop
```

## Gotchas (learned the hard way)

- **`panic = "abort"`**: the root `Cargo.toml` sets it for dev+release; a normal workspace
  member inherits it and `cargo test`'s unwinding harness breaks. `hal` sidesteps this by being
  its own workspace (empty `[workspace]` table) — keep it that way; don't add `hal` to the root
  `members`.
- **No bare-metal deps in `hal`**: adding `x86_64` (or anything hardware-specific) breaks the
  host build. Production seam impls stay in `kernel/src/hal.rs`.
- **`build-std`**: the kernel builds `core`/`alloc`/`compiler_builtins` via
  `kernel/.cargo/config.toml`. `hal`'s host `cargo test` deliberately does NOT inherit that
  (config is cwd-scoped and `hal/` is outside `kernel/`), which is what makes host tests work.
- **Best-effort, never hang boot**: bounded polls, `Err` not panic, on missing/failed hardware
  (mouse, ac97, rtc, acpi all follow this).

## Migration status / order

Migrated: **ACPI** (`PhysMem`), **ac97** (`PortIo` + `ScriptedIo`), **keyboard** + **mouse**
(pure decoders, no seam; mouse also has a `PortIo` 8042 enable), and **pit**/**rtc**
(`PortIo`; pit's divisor math and rtc's whole CMOS protocol + `days_from_civil` are pure and
directly testable — neither joined the `Driver` registry, see `docs/drivers/architecture.md`'s
"Current status" for why). All planned drivers are migrated; the APIC work is next, on top of
this clean base. 64 host tests in `hal` at last count.
