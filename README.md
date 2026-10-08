# 🦀 constanos — an x86-64 operating system in Rust

*English · [Español](README.es.md)*

[![CI](https://github.com/oriaj-nocrala/rust_so_kernel/actions/workflows/ci.yml/badge.svg)](https://github.com/oriaj-nocrala/rust_so_kernel/actions/workflows/ci.yml)

> **Status: experimental.** A personal project, developed by one person with a lot
> of help from Claude Code. Everything listed below runs today, in QEMU and on one
> real machine; the gaps are listed under [What's missing](#-whats-missing).

A kernel written from scratch in Rust (`no_std`, UEFI, SMP) with a Linux-numbered
syscall ABI, [mlibc](https://github.com/managarm/mlibc) as its libc, BusyBox as its
userland, a desktop with its own compositor, and a driver for an NVIDIA RTX 3050
(GA106) that goes all the way up to Vulkan through Mesa's NVK. It runs in QEMU and
on a physical machine (AM4 / Ryzen 9 5900X) that has no serial port, where
everything — keyboard, mouse, disk — comes in over USB.

![The constanos desktop with the Luna theme: a terminal and cpumon with four CPUs in windows, taskbar with the Apps button](docs/screenshots/compositor-luna.png)

*The desktop with the **Luna 2026** theme: `term` (BusyBox `ash`) and `cpumon` in
windows, a taskbar with buttons for the open windows and the clock. A real QEMU
screenshot with 4 CPUs.*

<table>
<tr>
<td><img src="docs/screenshots/compositor-luna-menu.png" alt="Luna theme start menu, with the applications and the theme picker"></td>
<td><img src="docs/screenshots/compositor-9x-menu.png" alt="The same desktop with the modern 9x theme and its start menu with the vertical strip"></td>
</tr>
<tr>
<td><em>Start menu (Luna): the applications from <code>/mnt/etc/gui/apps</code> and the theme picker.</em></td>
<td><em>The same desktop in <strong>modern 9x</strong>, switched from the menu (or with F12).</em></td>
</tr>
</table>

![A frame from the GPU renderer: glass taskbar and menu with the blurred background behind them, translucent windows](docs/screenshots/vk-comp-glass.png)

*Glass in the GPU compositor (`vk_comp`): the taskbar and the menu copy what is
behind them, blur it with a Gaussian blur in a compute shader and draw on top. This
frame comes from the `probes/nvk/host-comp.sh` harness, which runs the real renderer
on lavapipe and compares it pixel by pixel with the CPU rasterizer.*

<table>
<tr>
<td><img src="docs/doom-screenshot.png" alt="DOOM running on constanos"></td>
<td><img src="docs/quake-screenshot.png" alt="Quake running on constanos"></td>
<td><img src="docs/cmatrix-screenshot.png" alt="cmatrix on the framebuffer console"></td>
</tr>
<tr>
<td><em>DOOM, with mouse and sound.</em></td>
<td><em>Quake, a real game with QuakeC.</em></td>
<td><em><code>cmatrix</code> on a port of ncurses.</em></td>
</tr>
</table>

## 🚀 What it has

### Kernel

- **UEFI boot** (the `bootloader` crate), GOP framebuffer, kernel log on serial, in
  `/proc/dmesg` and — on the real machine — in a partition of the USB stick.
- **SMP**: up to 32 CPUs (APs via a trampoline, one-shot LAPIC timer, I/O APIC,
  MSI/MSI-X), a preemptive multilevel priority scheduler shared by all CPUs
  (processes migrate between them), cross-CPU TLB shootdown. The lock and interrupt
  rules that make it work are in `CLAUDE.md`.
- **Memory**: the buddy allocator as the only frame allocator, slab for the heap,
  per-process page tables, VMAs, demand paging, copy-on-write, a stack that grows on
  demand, shared memory (`memfd_create`, `MAP_SHARED`), DMA for drivers.
- **Processes and threads**: `fork` with COW, `execve` of static ELF64 binaries
  (static-pie included), `clone` with real threads, futex, POSIX signals with
  `sigaltstack` and `ucontext`, job control, `wait4`, `pidfd_open`, credentials, CPU
  time accounting.
- **IPC**: pipes, AF_UNIX sockets (stream, dgram, seqpacket, `SCM_RIGHTS`), ptys,
  `poll`/`epoll`/`eventfd`.
- **File systems**: a VFS with mounts, ramfs on `/tmp`, devfs, procfs (BusyBox's `ps`
  and `top` read it), and **read-write ext2** on `/mnt` (ATA or the USB stick), with
  a block cache and repair at mount time.

### Linux compatibility

- The syscall numbers and mlibc's `abi-bits` headers are Linux's, so what compiles
  against Linux usually runs unchanged.
- **C**: a port of mlibc (`mlibc-port/`), with a few upstream bugs patched (one fix
  sent upstream: [managarm/mlibc#1925](https://github.com/managarm/mlibc/pull/1925)).
  Unmodified BusyBox 1.36.1: `ash` is the shell, with ~60 applets (`vi`, `less`,
  `grep`, `awk`, `tar`, `ps`, `top`, `wget`, `nc`, `ping`, `httpd`...).
- **Rust std on musl**: ordinary `x86_64-unknown-linux-musl` programs run as they
  are, **tokio** included (multi-threaded runtime, timers, sockets, `tokio::fs`,
  processes); tested with loads of 10,000 tasks (`scripts/run-tokio-probe.sh`).

### Networking

- **virtio-net** (QEMU) and **Realtek RTL8168** (the Ryzen's board, gigabit
  verified), with MSI-X interrupts.
- The [smoltcp](https://github.com/smoltcp-rs/smoltcp) stack: DHCP, AF_INET **TCP**
  (client and server), **UDP** and **raw ICMP** sockets; BusyBox's `wget`, `nc` and
  `ping` work.

### Desktop

- **Its own Wayland-style protocol** (the `gui` crate): shared or GPU buffers,
  `commit`, frame callbacks, popups, panel roles. The window manager (move, resize,
  maximize, F11 fullscreen, focus) is pure logic with ~90 host tests.
- **Two compositors with the same display list**:
  - `compositor`: paints on the CPU into `/dev/fb0` — works anywhere (QEMU included);
  - `vk_comp`: composites on the GPU with Vulkan on NVK on the RTX 3050 — rounded-box
    SDFs, gradients, borders, shadows, premultiplied transparency and blurred glass.
    The same frame is rasterized on the CPU and compared pixel by pixel in the tests.
- **Themes** (`gui::theme`): **Luna 2026** and **modern 9x**, switched live from the
  start menu or with F12.
- **Programs**: `panel` (taskbar and start menu), `term` (a terminal emulator with
  its own VT parser, the `vt` crate), `cpumon` (per-CPU graphs, memory, processes),
  `textdemo` (antialiased TrueType text, the `text` crate), `imgview` (PNG), `snake`,
  `fire`, and DOOM and Quake in windows.
- `scripts/gui-e2e.sh` drives the desktop in QEMU with keyboard and mouse and checks
  the screenshots pixel by pixel.

### GPU: NVIDIA RTX 3050 (GA106)

Behind the `gpu=` boot option (off by default), in steps that are tested one at a
time on the real machine (`docs/reference/gpu.md`, plan in `docs/gpu/gpu-plan.md`):

- VBIOS, DCB, EDID over AUX/I2C, modes, DisplayPort and HDMI link training, its own
  scanout;
- booting **GSP-RM** (the NVIDIA firmware that runs the GPU from the inside), GPU page
  tables, GPFIFO channels, the copy engine and compute;
- **`/dev/nvgpu`**: the interface for a port of Mesa's NVK driver (`mesa-port/`).
  Vulkan works: compute, 3D, a swapchain on the display, buffers and timelines shared
  between processes. `snake3d` (Vulkan, 60 fps) and `vk_comp` run on it.

### Real hardware

The Ryzen has no serial port, no PS/2 and no IDE. That is why there are:

- its own **xHCI** driver: USB keyboard, mouse and mass storage (the ext2 for `/mnt`
  lives in a partition of the boot stick);
- a **log partition** on the stick (the kernel ring is copied every 5 s and on a
  panic; `scripts/usb-log.sh read` reads it from Linux);
- **unattended runs** (`scripts/metal-run.sh`): a job on the stick, a watchdog, the
  verdict in the log;
- a framebuffer console with a shadow buffer and write-combining (`seq 1 400`: from
  875 s down to 0.75 s on metal; `docs/fb/console-perf.md`);
- CPU sensors (frequency, temperature, energy) shown by `cpumon`.

### Games

- **DOOM** ([doomgeneric](https://github.com/ozkl/doomgeneric) + `doom-port/`): the
  Freedoom IWAD from `/mnt`, mouse-look and sound effects through its own AC97 driver.
- **Quake** ([quakegeneric](https://github.com/erysdren/quakegeneric) +
  `quake-port/`): the shareware `pak0.pak`, a new game with QuakeC and sound.

## 🧪 Tests

The kernel can't run `cargo test`, so all the logic that can be written against plain
types lives in separate crates with host tests (`hal`, `mm`, `vfs`, `ext2`, `sched`,
`usock`, `tty`, `net`, `nvgpu`, `gui`, `vt`, `text`, ...). The rest is tested in QEMU:

| What | How |
|------|-----|
| Pure crates | `cd <crate> && cargo test` |
| Kernel in QEMU | `scripts/run-kernel-tests.sh` |
| Linux ABI (raw C tests) | `scripts/run-abi-suite.sh` |
| Rust std / tokio | `scripts/run-std-probe.sh`, `scripts/run-tokio-probe.sh` |
| Desktop | `scripts/gui-e2e.sh [term\|wm\|text]` |
| GPU renderer (lavapipe) | `probes/nvk/host-comp.sh` |
| Networking | `scripts/net-e2e.sh` |

A test counts only once it fails when the code it covers is sabotaged; several
subsystems have mutation scripts (`scripts/gpu-mutate*.py`, `nvgpu/mutations/`).

## 🏗️ Layout

```
kernel/            the kernel (no_std): init, memory, processes, syscalls, fs, drivers,
                   usb, networking, gpu, cpu/smp, interrupts, time
hal/ mm/ sched/    pure logic with host tests: hardware (xHCI, virtio, APIC, ACPI,
vfs/ ext2/ usock/  GPT...), memory, scheduler, VFS, ext2, AF_UNIX, ptys,
tty/ net/ diag/    networking, lock diagnostics
nvgpu/             the GA106 driver (pure logic) + the /dev/nvgpu uapi
gui/ gui-capi/     protocol, window manager and themes; its C API
draw/ text/ img/   drawing primitives, TrueType text, PNG
vt/                terminal parser
vk-comp/           the GPU compositor (Rust std on musl)
probes/            test programs: NVK/Vulkan, compositor renderer, std, tokio
userspace/         programs in Rust (shell, compositor, panel, term, cpumon...) and C
mlibc-port/ mesa-port/ doom-port/ quake-port/   the ports
mlibc/ busybox/ doomgeneric/ quakegeneric/      upstream submodules
disk-image-root/   contents of /mnt (disk.img)
docs/              per-subsystem reference (docs/reference/) and plans
scripts/           QEMU, tests, deploying to the stick, metal runs
build.rs src/      host side: builds the UEFI image and disk.img, launches QEMU
```

The per-subsystem reference (`docs/reference/`) is in English; many of the design
plans elsewhere in `docs/` are still in Spanish.

## ▶️ Running it

**Without building anything:** download the image from the
[releases](https://github.com/oriaj-nocrala/rust_so_kernel/releases) and follow
[docs/try-it.md](docs/try-it.md): QEMU, VirtualBox, VMware or a USB stick, and how to
send the kernel log with a bug report.

### Building it

Requirements: Rust **nightly** (pinned in `rust-toolchain.toml`),
`qemu-system-x86_64`, OVMF, `clang`/`llvm`/`lld`, `meson`, `ninja`, `make`,
`e2fsprogs`, and `curl`/`unzip` to download Freedoom and the Quake shareware.

On Arch:
```bash
sudo pacman -S qemu-system-x86 qemu-img qemu-ui-gtk edk2-ovmf clang llvm meson ninja lld e2fsprogs
```

```bash
cargo run       # builds everything (mlibc, BusyBox, programs, kernel) and boots in QEMU
cargo build     # just the UEFI image and disk.img
```

From the shell, `compositor` opens the desktop (Ctrl+Alt+Backspace closes it), and
`doom`, `quake`, `cmatrix`, `cpumon`... run from there or in a window.
`scripts/qemu-debug.sh` boots QEMU headless for debugging (keyboard, mouse,
screenshots, gdb).

### On the real machine

The USB stick has three GPT partitions: `boot` (FAT, the kernel), `constanos-data`
(ext2, `/mnt`) and `constanos-log` (the log).

```bash
scripts/deploy-usb-boot.sh   # kernel → boot
scripts/sync-usb-data.sh     # disk-image-root/ → constanos-data
scripts/usb-log.sh read      # afterwards: the log of that boot
```

Never `dd` the whole image onto the stick: it replaces the GPT and takes the other
two partitions with it. Boot options (`gpu=`, `nic=`) go in `/mnt/etc/kernel.conf`.

## 🎯 What's missing

- **Dynamic linking**: the loader rejects `PT_INTERP`; file-backed `mmap` and `mmap`
  address hints are missing. It comes next after an HTTPS client
  (`docs/userland/roadmap.md`).
- **Isolation between processes on the GPU**: all `/dev/nvgpu` sessions share page
  tables.
- **A single compositor**: `vk_comp` with a software backend instead of two programs
  (`docs/gui/compositor-visual-plan.md`, step 2e); icons.
- Networking: IPv6, loopback, TLS.

## 📜 License

The code in this repository is free software, licensed under your choice of
[MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE) (`MIT OR Apache-2.0`, like the
Rust ecosystem). Unless you say otherwise, any contribution you submit is licensed
the same way.

Submodules and third-party files keep their own licenses: mlibc (MIT), BusyBox
(GPLv2), doomgeneric and quakegeneric (GPLv2), ncurses (MIT-X11), cmatrix (GPLv3),
Mesa (MIT) and Freedoom (`disk-image-root/freedoom-COPYING.txt`). Binaries that link
GPL code (BusyBox, DOOM, Quake, cmatrix) are distributed under the corresponding
GPL.

---

*A personal project to learn how to build an operating system in Rust, with plenty
of help from Claude Code along the way.*
