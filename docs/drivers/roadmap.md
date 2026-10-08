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

## Phase 4 — Drivers out of the kernel *(rewritten 2026-10-08)*

The previous phase 4 aimed at a stable **in-kernel** ABI so foreign drivers could load into the
kernel. It is replaced, for one reason with evidence: third-party kernel drivers caused ~70% of
Windows crashes in Microsoft's own analysis (data to 2004; 85% on Windows XP per Microsoft
Research), and CrowdStrike 2024 was an out-of-bounds read in a kernel driver. A driver we did
not write, or one an LLM writes on demand (`docs/ai/software-on-demand.md`), must not share the
kernel's address space. Principles: P6.2 "risk follows reach, not author", P5.3 "class drivers
first" (`docs/ux/principles.md`).

**Rule:** hand-written, reviewed drivers may stay in the kernel (today: storage, USB host,
network, `nvgpu`). Foreign and generated drivers run in **userland driver hosts**.

What a userland driver host needs from the kernel, in order:

1. **Capabilities** (`docs/ai/capabilities-plan.md`, the cornerstone): a driver host starts with
   nothing and is handed one device.
2. **Device fds.** A capability-scoped handle to one device: for USB, control/bulk/interrupt
   transfers to one device (the xHCI stack stays in the kernel); for PCI, its BARs mapped into
   the host and its MSI/MSI-X vectors delivered as fd events (MSI vectors already exist,
   `kernel/src/interrupts/msi.rs`).
3. **IOMMU** (AMD-Vi on the Ryzen; QEMU emulates one): a device can DMA only into buffers its
   host was given. **Without it, no userland driver may program DMA**: a host that can aim a
   device's DMA owns all of physical memory.
4. **Supervision:** a host that crashes is restarted and the device reset, without taking
   anything else down (`docs/userland/init-plan.md`), and the reason is recorded (P1.2).

**The ladder** (from `docs/ai/software-on-demand.md`):

| Level | What | Where | Written by |
|---|---|---|---|
| 0 | Class drivers: USB Audio Class 2, HID, Mass Storage, IPP (network printing) | kernel or a trusted host | hand-written, tested on metal |
| 1 | Vendor control over USB control transfers (e.g. a Focusrite Scarlett's mixer and routing, on top of the class audio driver) | userland host, no DMA, one device | may be generated on demand |
| 2 | PCI device with DMA | userland host + IOMMU domain | hand-written or generated, with replay tests |
| 3 | Kernel code | kernel | hand-written only, never generated on demand |

**Trust comes from tests, not from the code's author.** A driver at level 1-2 carries recorded
device traffic (fixtures) and replay tests, as the GPU display work does (`gpu-display` skill:
oracle → fixture → pure code → replay test → sabotage → adapter) and as the `hal` seams allow.
A repo entry is request + code + tests + capability manifest.

**Foreign source drivers (the BSD precedents), now in userland:** LinuxKPI (FreeBSD builds Linux
DRM drivers against a shim of the Linux kernel API) remains the template for porting a Linux
driver's *source*, but the shim lives inside a userland driver host, not in the kernel; NetBSD's
rump kernels and Genode's DDE are the prior art for running another kernel's drivers as
processes. Binary foreign drivers (FreeBSD's NDISulator) are out of scope.

**First steps:** (a) a USB device fd with control transfers + a level-1 driver for a real device
(the Scarlett's mixer, which Geoffrey Bennett's Linux work documents); (b) IOMMU bring-up in QEMU,
then the Ryzen; (c) a level-2 host for a simple PCI device already supported in the kernel, so the
two can be compared (the virtio-net or RTL8168 driver is the natural candidate).

## Phase 5 — NVIDIA: done our own way, and what is left *(rewritten 2026-10-08)*

The old phase 5 planned to run NVIDIA's proprietary blob or open kernel modules through a
LinuxKPI-scale layer. That is not what happened: constanos has **its own driver** for the GA106
(`nvgpu` crate + `kernel/src/gpu/`): display modeset, GSP-RM boot and RPCs, GPU page tables, GPFIFO
channels and the copy engine, and a `/dev/nvgpu` device that Mesa's NVK drives from userland, up
to a Vulkan compositor at 60 fps on the Ryzen (`docs/gpu/gpu-plan.md`, `docs/reference/gpu.md`).
The firmware (GSP-RM) carries most of the hardware's complexity, which is what made a
from-scratch driver tractable.

What is left belongs to other plans: the cursor channel (`docs/gpu/hw-cursor-plan.md`), the
graphics stack (`docs/gpu/g5-graphics-stack-plan.md`). Open question for later, once phase 4's
IOMMU exists: whether parts of `nvgpu` that only talk to GSP-RM over RPC could move into a
userland host (the firmware does the privileged work; the kernel would keep BAR mapping,
interrupts and the IOMMU domain).

---

## Summary table

| Phase | Trait layer becomes… | Buys | Status |
|-------|----------------------|------|--------|
| 0 | (none) — ad-hoc modules | Fast early progress | done |
| 1 | `Driver` + `PortIo`/`PhysMem` seams | Host-testable logic, encapsulation | done (ACPI pilot) |
| 2 | Same, applied to every driver + QEMU test runner | Tests everywhere; pays down test debt | **done** |
| 3 | `Bus`/`Device`/`probe` device model | Enumeration, resource ownership, lifecycle | next (the APIC migration it waited for is done: `apic::init`, `docs/reference/cpu.md`) |
| 4 | Userland driver hosts: capabilities, device fds, IOMMU, supervision; the risk ladder | Foreign and generated drivers that can't take the system down | direction (needs capabilities) |
| 5 | Own NVIDIA driver (GSP-RM + NVK) | Vulkan on the GA106 | done; cursor and graphics stack in their own plans |

Each row is the previous row's discipline at larger scope. That is the whole plan.
