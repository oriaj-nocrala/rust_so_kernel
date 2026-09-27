# GPU (NVIDIA GA106) and its kernel prerequisites

Plan and decisions: `docs/gpu/gpu-plan.md`. This page is the current state. Pure logic: crate `nvgpu` (GPU) and `hal::pcicfg`/`hal::dma`/`hal::bootopts` (generic); kernel adapter: `kernel/src/gpu/`.

## Boot options (`kernel/src/bootopts.rs`, `hal::bootopts`)

- UEFI gives no command line. `key=value` words are read once after `fs::init` from `/mnt/etc/kernel.conf`, then `/mnt/autorun/kernel.conf` (later wins; `#` comments). Before that, everything is at its default.
- `/mnt/autorun/kernel.conf` is written by `scripts/metal-run.sh --kconf '...'` and removed with `autorun/`, so an option applies to one unattended run only.
- `disk-image-root/etc/kernel.conf` is gitignored: a checkout's own options.
- `gpu=` (`hal::bootopts::GpuLevel`): `off` (default: the GPU is not touched), `probe`, `disp`, `vblank`. Each level does everything the previous ones do. An unknown value is logged and read as `off`.
- `disk-image-root/etc/` is not synced to `disk.img` (only `etc/gui` is). To try a level in QEMU, write the file into the image: `debugfs -w -R "write <file> /etc/kernel.conf" disk.img` (and `rm` it after).

## `gpu=probe` (phase 1)

`gpu::probe()` runs in `init::boot` after `logpart::init`, before the APs are released. Output goes to the log (`gpu: ...`) and `/proc/gpu`:
- the firmware files (size, first word, FNV-1a 64 to compare with the host's copy);
- D4: each IOMMU from the IVRS (`hal::acpi::parse_ivrs`), its control register and `IommuEn`;
- the GA106 (`10de:2507`): capabilities, MSI/MSI-X, BARs with sizes; BAR0 mapped UC and `PMC_BOOT_0` decoded (`nvgpu::id`); the first 16 MiB of BAR1 mapped WC (not accessed); `pci::claim(..., "nvgpu")`.
- **No GPU register is written.** The only writes are the BAR sizing protocol's, in configuration space, with decoding off for microseconds (no CPU may draw on the GOP framebuffer then: hence before the APs).
- Metal job: `scripts/metal-run.sh --kconf 'gpu=probe' scripts/metal-jobs/gpu-probe.sh`.

## `gpu=disp` (phase 2)

Everything `probe` does, then (`gpu::probe_displays`, only if `PMC_BOOT_0` says GA106):
- **VBIOS** from the PROM (`nvgpu::vbios::read_prom`: BAR0 `0x300000`, image chain by PCIR/NPDE like nouveau's `shadow.c`). The ROM-shadow bit (`0x088050` bit 0) is cleared only if set and restored; on the target it is already clear, so no write. `/proc/gpu` shows size, time, FNV-1a of the first 148 992 bytes (= the phase 0 dump), images and the BIT version.
- **DCB** (`nvgpu::dcb`): outputs (walk of `engine/disp/nv50.c`), CCB 4.1, connector table. Logged as nouveau prints them.
- **Connectors** (`nvgpu::display::connectors`): the connector-table entries some output uses. Name = DRM type + count per type in table order (nouveau's naming): `DP-1`, `DP-2`, `DP-3` (the ASUS, AUX 3), `HDMI-A-1` (the HP, I2C port 5). The proprietary driver calls the ASUS `DP-1`; these names are not Linux's `nvidia` ones.
- **Probe** by polling, no interrupts (`nvgpu::display::probe`):
  - DP: DPCD caps (+ the 0x2200 extended copy) and EDID blocks 0-1 by I2C-over-AUX (`nvgpu::aux`: `auxgm200.c` transaction + DRM's retry policy). No sink (AUX status bit 28) = disconnected.
  - HDMI: EDID by bit-banged I2C (`nvgpu::i2c`: nouveau's `bit.c` on `busgf119.c`'s port register). NACK at 0x50 = disconnected.
  - Pads (`nvgpu::pad`) switched to AUX/I2C and put back; the AUX auto-DPCD bit (nouveau leaves it cleared) is restored too.
  - More than one EDID extension is not read (needs the E-DDC segment pointer, never used in the trace): shown as a `note:`.
- `/proc/displays`: one line per connector (`name status conn N aux|i2c N  MFG name`), then `dpcd:`, `preferred:`, `range:`, `modes:` (every DTD), `edid-fnv1a64:` and `edid-hex:`.
- Runs before the APs are released; delays are TSC busy-waits that service TLB shootdowns.
- Metal job: `scripts/metal-run.sh --kconf 'gpu=disp' scripts/metal-jobs/gpu-disp.sh`.

## `gpu=vblank` (phase 3)

Everything `disp` does, then `gpu::vblank::setup` (still at boot, IF=0, before the APs):
- **Heads** (`nvgpu::vblank`): which exist (`0x610060`) and what each scans out, from its ARM state (`0x68a000 + head*0x400`: totals, blanking, pixel clock → refresh). A head with a clock and a raster is "lit"; the lowest lit one is the **primary**.
- Read-only evidence, logged before arming: the interrupt tree and display interrupt registers as the firmware left them; the primary head's raster position sampled for 200 ms (frames counted by vline wrapping: plan B's instrument); whether its vblank status bit latches (cleared, read 40 ms later).
- **Arming** (`nvgpu::vblank::arm`, nouveau's order): unarm the VFN tree (`0xb81610`), block every leaf, reset + allow only the display bit (leaf 4, `0x04000000`), MSK=vblank for every head (`0x611cc0`), EN bit 2 for the lit ones (`0x611d80`), MSI rearm (`0x088704`), rearm (`0xb81608`). Then PCI bus mastering on (MSI is a memory write; the GOP leaves it off) and MSI to CPU 0.
- **Handler** (`on_msi`, ISR on CPU 0, global work): `nvgpu::vblank::service`, the trace's handler register for register (unarm, MSI rearm, leaves, `PMC_BOOT_0` check, reset, `0x611ec0`, per-head ack of LOADV and VBLANK, rearm). Nothing else touches BAR0 after boot, and the GPU sends no second MSI before the rearm, so no lock. Leaf bits with no handler are blocked (nouveau's storm guard).
- Counters in `/proc/kdebug`: `gpu_vblank: enabled primary_head seq last_ns msi vector spurious blocked gone disp_other head_other`, plus `gpu_vblank_headN` per lit head. `seq`/`last_ns` are the primary head's vblank count and the `ktime_get` of the last one, so a rate needs no sleep precision.
- **`/dev/vblank`** (`drivers/dev_vblank.rs`): `ENODEV` unless armed. Readable (poll/epoll) once a vblank happened after the one the handle last read; `read` never blocks and returns 16 bytes (sequence number, ns). Blocking reuses poll's input-queue machinery (`evdev::QUEUE_VBLANK`; `EventSource::seen` carries the handle's number and the re-check compares it with the live one).
- `PollSource` is pinned at 16 bytes (`const` assert in `poll.rs`): `poll_wake_where` keeps 8 waiters with a 16-entry map each on the ISR's stack, and 24 bytes overflowed it.
- Metal job: `scripts/metal-run.sh --kconf 'gpu=vblank' scripts/metal-jobs/gpu-vblank.sh` (rate over 30 s must be 60.0 ± 0.1, then `compositor fire` for 20 s for a tearing photo).

## Oracle tools (`scripts/gpu-trace.py`)

- `aux DIR CH` lists AUX transactions on a channel; `aux DIR CH SEL OUT` writes them as a `ReplayMmio` fixture (`nvgpu/fixtures/aux-ch3-dpcd-edid.txt`).
- `i2c DIR DRIVE` decodes bit-banged I2C from a trace (proved the port-register bits: it yields the HP's EDID byte for byte).

## PCI configuration space (`kernel/src/pci.rs`, `hal::pcicfg`)

- `config_space` reads all 256 bytes (mechanism #1; no ECAM, so no extended capabilities). `hal::pcicfg::capabilities` walks the list, bounded.
- `size_bars`: Linux's sizing protocol, decode off, under one hold of `CONFIG`.
- `enable_msi(bdf, dest_apic_id, vector)`: single-vector MSI, INTx disabled; refuses without a LAPIC (8259 fallback). `hal::pcicfg::MsiCap::enable_writes` decides the writes, host-tested against the GA106's real config space (`hal/fixtures/ga106-config.txt`: MSI at 0x68, 64-bit, no masking, no MSI-X).

## MSI vectors (`kernel/src/interrupts/msi.rs`)

- 0x50–0x5F have stubs in the IDT from the start (it is a `Once`); `msi::alloc(handler)` hands one out, `free` returns it. Dispatch is a table of atomics, no lock; EOI after the handler.
- Handlers run in ISR context on the CPU the message targets: ISR rules apply, and the driver states whether its work is global or per-CPU.
- `msi::count(vector)`, `msi::stray()` for tests and diagnostics.

## DMA (`kernel/src/memory/dma.rs`, `hal::dma`)

- `DmaBuf::alloc(len, mask)`: zeroed, contiguous, power-of-two pages from the buddy; `bus_addr()` = physical (no IOMMU, D4); fails with `AboveMask` if the block is not under the device's mask (the buddy cannot allocate below a limit).
- `DmaPages::alloc(count, mask)`: non-contiguous 4 KiB pages (for radix3 tables).
- **Release is explicit** (`free`); there is no `Drop`, so a forgotten buffer leaks rather than being handed back while a device may still write it.
- Cacheable through the physical window (x86 DMA snoops).

## Firmware (`kernel/src/firmware.rs`)

- `firmware::load("nvidia/ga106/gsp/...")` reads `/mnt/lib/firmware/<rel>` whole (max 96 MiB).
- The root `build.rs` (`ensure_firmware`) decompresses the files from the host's `/usr/lib/firmware` into `disk-image-root/lib/firmware/` (gitignored) with `LICENCE.nvidia`, and syncs them to `disk.img`; `sync-usb-data.sh` takes them to the stick.
- Staged today: `bootloader`, `booter_load`, `booter_unload` of 570.144. **`gsp-570.144.bin` (63 MB) is not**: `disk.img` is 96 MiB with ~20 MB free; phase 4 decides.
- Their first word is `0x000010de`, the `nvfw_bin_hdr.bin_magic` (`nouveau/include/nvfw/fw.h:8`).

## Tests

- Host: `cd hal && cargo test` (pcicfg, dma, bootopts, acpi IVRS), `cd nvgpu && cargo test`.
- `nvgpu` fixtures: the two EDIDs + their `edid-decode` output, an AUX trace extract, and one vblank interrupt as nouveau serviced it (`vblank-service.txt`) (committed); the VBIOS is read from `$GPU_ORACLE/static/vbios-rom.bin` (default `~/constanos-gpu-oracle`, not in git, D3) and those tests print `SKIP` without it.
- `nvgpu` mocks: `TableMmio` (fixed values), `ReplayMmio` (per-register read queues from a trace extract + write log to compare), `i2c::tests::DdcSim` (open-drain bus with a DDC EEPROM).
- QEMU: `hw_tests::edu_mmio_dma_msi` (`scripts/run-kernel-tests.sh`; the runner adds `-device edu,dma_mask=0xffffffffffff`): MMIO, MSI to CPU 1 (the test boot keeps IF=0 on CPU 0), DMA both ways, mask refusal. The `edu` driver (`kernel/src/edu.rs`) is test-only.
