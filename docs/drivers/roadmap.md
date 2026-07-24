# Driver architecture — roadmap

Where the driver layer is heading, and what each step buys. This is a *direction*, not a
schedule: phases are ordered by dependency, not dated. The guiding rule throughout is
**pull the contract apart from the implementation, one honest step at a time** — that is the
single thread connecting today's tiny `Driver` trait to the most ambitious end state.

For where we are right now, see [architecture.md](architecture.md).

---

## The through-line

Everything below is the same move, applied at growing scope: define an **interface** (a
trait, a seam, an ABI) and put the messy, hardware-specific, or foreign-OS-specific detail on
the far side of it. Testability, portability, and eventually foreign-driver compatibility are
all *consequences* of that one discipline — not separate projects. So even the wildly
ambitious end states are reached by continuing exactly what we're doing now, not by a
rewrite.

A useful honesty check at every phase: **does this step pay for itself with current drivers,
or is it speculative structure?** We build the model by extracting it from real drivers, never
by designing it in the abstract ahead of need. The kernel is already large; speculative
frameworks are how it would drown.

---

## Phase 0 — Ad-hoc drivers *(where we came from)*

Each driver a free-standing module: global state (`static AC97: Mutex<Option<Ac97>>`,
atomics, `spin::Once`), hardware access hardwired to `x86_64::Port`, and initialization a
hand-maintained list of `crate::x::init()` calls in `init/mod.rs`. No common interface, no
way to exercise the logic without the real hardware.

This got the kernel a long way — cheaply — and there is no shame in it. It stopped scaling
once the driver count and the cost of untested changes grew.

## Phase 1 — The seam + host tests *(done)*

Introduced the `hal` crate, the `PortIo`/`PhysMem` hardware seams, the `Driver` trait, and a
best-effort `run_all` registry. **ACPI is the pilot.** See [architecture.md](architecture.md).

**What the trait layer is, at this phase:** deliberately minimal — `name()` + `init()`. It is
lifecycle sugar plus, far more importantly, the *seam* that makes logic host-testable. The
value delivered now is concrete: the ACPI parser has six host tests (including malformed-table
guards) that run in under a second with no QEMU.

## Phase 2 — Roll out the seam + a real test harness *(done)*

Migrate the remaining drivers onto the seam, one at a time, each gaining host tests:

1. **ac97** — done. First, because it exercises `PortIo` + `MockIo` in a real
   register-sequencing driver (the ACPI pilot only exercised `PhysMem`). Proves the port-I/O
   half of the seam.
2. **mouse**, **keyboard** — done. PS/2 packet/scancode decode is pure logic ripe for host
   tests.
3. **pit**, **rtc** — done. Small, well-understood; good practice targets. Neither joined the
   `Driver` registry (see [architecture.md](architecture.md)'s "Current status" for why) —
   they stay direct calls, which turned out to be the honest shape rather than a gap to close.

Each migrated driver became a `struct Foo<IO: PortIo> { io: IO, … }`, testable as
`Foo<MockIo>`/`Foo<ScriptedIo>`, with any global reduced to a single
`static FOO: Mutex<Option<Foo<X86PortIo>>>` where one is even needed (pit/rtc need none — see
above). All six drivers are migrated; 64 host tests in `hal` at last count.

The **QEMU integration test framework** is done too: `kernel/src/test_framework.rs`
(`#![feature(custom_test_frameworks)]` + `#[test_case]`, guest side) boots the kernel for
real in QEMU with `-device isa-debug-exit,iobase=0xf4,iosize=0x04` and reports PASS/FAIL as a
real process exit code — `qemu-test-runner/` (host side) launches QEMU headless, enforces a
timeout, and translates the exit code. `[acpi] SELFTEST` is its first case
(`kernel/src/hw_tests.rs::acpi_selftest_passes`), asserting the same checks the boot-time log
already printed instead of relying on a human reading them.

Corrects a stale claim that used to live in this section: `isa-debug-exit` was **not**
"already configured in `kernel/Cargo.toml`" — that file's `[package.metadata.bootimage]`
block was dead config for the `bootimage` tool (`bootloader` 0.9-era), which was never
installed and never read by anything in this repo (which uses `bootloader` 0.11 with a
hand-written `build.rs`/`src/main.rs` launcher). That block has been removed from both
`Cargo.toml`s; QEMU is only given `-device isa-debug-exit` by `qemu-test-runner/` now.

**Also verified, not assumed: plain `cargo test --target x86_64-unknown-none` does not work**
on this crate. It makes cargo build the `kernel` bin target twice in one invocation (once
normally, once under `--cfg test`), and with `-Z build-std` active that produces two
independently-built `core` crates that collide (`error[E0152]: duplicate lang item in crate
'core': 'sized'`) the moment a shared dependency needs both — the same class of `-Z
build-std` limitation as the `bindeps`/artifact-dependency panic already documented in the
root `build.rs`. `cargo build --target x86_64-unknown-none --tests` (no implicit "also build
it normally" step) doesn't hit this, so `scripts/run-kernel-tests.sh` drives that instead of
`cargo test` itself — see that script's header and `kernel/.cargo/config.toml`'s `runner` key
comment for the full diagnosis. Functionally equivalent either way: real QEMU boot, real
PASS/FAIL exit code.

**End state of Phase 2 (reached):** every driver has pure logic behind a seam, host tests for
that logic, and a repeatable integration test for the hardware path. This is the phase that
pays down the "no tests" debt.

**A related seam, added after the six drivers above: the storage stack.** `fs::ext2` (the
read-write ext2 filesystem, mounted at `/mnt`) used to call `block::ata::{read_sectors,
write_sectors,present}` directly at 9 call sites. `hal::block::BlockDevice` (`hal/src/
block.rs`) now sits between them — the same seam shape as `PortIo`/`PhysMem`, sector-granular
rather than filesystem-block-granular (see that file's doc comment for why). `kernel::block::
AtaBlockDevice` is the production implementation `fs::ext2::init()` mounts against at real
boot; `hal::block::MemDisk` (`Vec<u8>`-backed, 7 host tests) is what the QEMU integration test
(`hw_tests.rs::ext2_memdisk_roundtrip`) mounts instead, exercising ext2's full read-write path
— create/mkdir/rename/symlink/unlink/rmdir through the real VFS — with zero risk to the real
`disk.img`.

This is explicitly a **partial** migration, stated honestly: `block::ata.rs` itself is not
seamed onto `PortIo` the way the six drivers above are — its LBA28 PIO command sequencing is
just as untestable on the host today as before this work. Only the layer *above* it moved.
Migrating `ata.rs` itself, and extracting `fs::ext2`'s ~2000 lines of pure bitmap/inode/
directory logic into something host-testable (it currently lives in the `kernel` crate because
it depends on `Inode`/`FileHandle`, which live there too), are both real future work — see
`docs/drivers/architecture.md`'s storage-stack section for the reasoning on why neither was
folded into this pass.

## Phase 3 — A Linux-class device model *(medium-term)*

(This phase is about the `Bus`/`Device`/`probe` model for *hardware* enumeration — PCI/APIC.
The storage stack's own seam, `hal::block::BlockDevice`, is a separate, already-done track
described at the end of Phase 2 above; finishing it — migrating `block::ata.rs` itself onto
`PortIo`, and any future filesystem beyond ext2 — doesn't depend on this phase landing first.)

This is where the trait layer grows from "an init registry" into a real **device model**. The
concepts, roughly mirroring Linux's `struct device`/`struct driver`/`struct bus_type` (and
BSD's newbus):

- **`Bus`** — an enumeration + matching mechanism. We already have the seed: `pci.rs`'s
  `find_device(vendor, device)`. Generalize it into a `Bus` that enumerates devices and
  advertises their resources. (Legacy ISA devices become a trivial "platform" bus of
  fixed-address entries.)
- **`Device`** — a discovered piece of hardware with its resources: I/O port ranges, MMIO
  regions, IRQ line(s), DMA capability. ACPI/MADT (already parsed) and PCI config space are
  the resource sources.
- **`Driver::probe(&Device) -> Result<Box<dyn Driver>>`** — match + bind. The registry stops
  being a hardcoded list and becomes "for each device, find a driver that claims it." This
  replaces the `run_all([...])` explicit list.
- **Resource ownership** — IRQ/MMIO/port-range allocation with conflict detection, so two
  drivers can't silently fight over the same region. (Requires the APIC/interrupt work — see
  below — to be meaningful for IRQs.)
- **Lifecycle** — `probe`/`remove`, and eventually `suspend`/`resume`.

**Prerequisite/companion work:** the **APIC migration** (LAPIC/IOAPIC, replacing the 8259
PIC) lands around here, because a real interrupt model — routing GSIs to handlers, per-device
IRQ ownership — is what makes the device model's resource management worth having. The MADT
topology parsed in Phase 1 exists precisely to feed this. This is deliberately *after* the
seam and tests, so APIC is built on a tested base with its own tests.

**The over-engineering guardrail is sharpest here.** A full device model for ~10 fixed devices
on a QEMU i440fx machine can easily cost more than it returns. Build only the parts that at
least two real drivers demand, and let PCI + APIC be the forcing functions.

## Phase 4 — A stable driver contract + foreign-driver compatibility *(long-term, ambitious)*

The ambition the project is ultimately curious about: running drivers *not written for this
kernel*. Two distinct sub-goals, often conflated:

### 4a. A stable in-kernel driver API/ABI

Today the "contract" a driver codes against is our own moving traits. To host third-party
drivers, that contract has to become **stable and documented** — a Kernel Programming
Interface. Linux famously keeps its internal API *unstable* (drivers live in-tree and are
rebuilt); the BSDs keep theirs comparatively stable (KPIs), which is part of why BSD is the
proven ground for compat layers. Whichever we choose, the prerequisite is: the seams and
traits stop changing casually and gain versioned guarantees.

### 4b. Compatibility shims for foreign drivers — the BSD precedents

BSD is the honest model for "run another OS's drivers on a non-Linux kernel," via two real
mechanisms worth naming precisely:

- **LinuxKPI (FreeBSD, `linuxkpi`)** — a shim that *implements enough of the Linux kernel API*
  (memory, DMA, PCI, workqueues, locks, and most consequentially the **DRM** graphics
  subsystem) that lightly-patched Linux drivers compile and run on FreeBSD. This is how
  FreeBSD ships modern `amdgpu`/`i915` graphics: the drivers are Linux source, built against
  LinuxKPI rather than Linux. This is the real template for us — a `linuxkpi`-equivalent is a
  large but *bounded, incremental* surface: implement each Linux API a target driver calls, one
  at a time, driven by an actual driver you're trying to bring up.
- **NDISulator / "Project Evil" (FreeBSD)** — a wrapper that loaded *Windows* NDIS network
  driver **binaries**. This is the precedent for running a closed-source binary blob from
  another OS: emulate the ABI it was linked against, thunk its calls into native services. Far
  hairier (binary, not source; another OS entirely), but it demonstrates the ceiling of what a
  compat layer can do.

The trait/seam direction we're on now is *literally the first rung of this ladder*: separating
"what a driver needs from the kernel" (a contract) from "how this kernel provides it" (the
implementation) is exactly the seam that a compat layer plugs a foreign contract into.

## Phase 5 — Proprietary / Nvidia *(north star, framed honestly)*

The most ambitious end state, stated without hype so expectations are set correctly.

"Running the Nvidia driver" is not one thing:

- The **proprietary Linux blob** — a large closed kernel module built against Linux's kernel
  ABI, plus userspace blobs. Running it would require a LinuxKPI-scale compat layer complete
  enough that the `.ko` binds and runs: memory management, DMA, PCI, interrupts (needs the
  APIC/IRQ model from Phase 3), the DRM/KMS subsystem, workqueues, mutexes, firmware loading,
  and more. This is a *multi-year, enormous-surface* effort — the honest ceiling, not a
  near-term target.
- The **Nvidia open kernel modules** (open-source since 2022, for Turing and newer GPUs) —
  meaningfully more tractable: open source, so buildable against a compat KPI the way FreeBSD
  builds `amdgpu` against LinuxKPI, rather than reverse-engineering a binary. If Nvidia ever
  becomes realistic here, this is the door, not the closed blob.

Why keep this on the map at all, given the distance? Because it clarifies the *direction* of
every earlier phase. The device model (Phase 3), the stable contract (4a), and the compat shim
(4b) are each independently worthwhile — and they happen to be, in order, exactly the
prerequisites for this north star. We are not detouring toward it; we are building the things
worth building anyway, in the order that also happens to point at it.

---

## Summary table

| Phase | Trait layer becomes… | Buys | Status |
|-------|----------------------|------|--------|
| 0 | (none) — ad-hoc modules | Fast early progress | done |
| 1 | `Driver` + `PortIo`/`PhysMem` seams | Host-testable logic, encapsulation | done (ACPI pilot) |
| 2 | Same, applied to every driver + QEMU test runner | Tests everywhere; pays down test debt | **done** |
| 3 | `Bus`/`Device`/`probe` device model | Enumeration, resource ownership, lifecycle | next (after APIC migration) |
| 4 | Stable KPI + LinuxKPI-style compat shim | Run foreign (Linux) drivers | long-term |
| 5 | Compat surface complete enough to bind Nvidia | Proprietary/GPU drivers | north star |

Each row is the previous row's discipline at larger scope. That is the whole plan.
