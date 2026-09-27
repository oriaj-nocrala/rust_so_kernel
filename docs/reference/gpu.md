# GPU (NVIDIA GA106) and its kernel prerequisites

Plan and decisions: `docs/gpu/gpu-plan.md`. This page is the current state. Pure logic: crate `nvgpu` (GPU) and `hal::pcicfg`/`hal::dma`/`hal::bootopts` (generic); kernel adapter: `kernel/src/gpu/`.

## Boot options (`kernel/src/bootopts.rs`, `hal::bootopts`)

- UEFI gives no command line. `key=value` words are read once after `fs::init` from `/mnt/etc/kernel.conf`, then `/mnt/autorun/kernel.conf` (later wins; `#` comments). Before that, everything is at its default.
- `/mnt/autorun/kernel.conf` is written by `scripts/metal-run.sh --kconf '...'` and removed with `autorun/`, so an option applies to one unattended run only.
- `disk-image-root/etc/kernel.conf` is gitignored: a checkout's own options.
- `gpu=` (`hal::bootopts::GpuLevel`): `off` (default: the GPU is not touched), `probe`. An unknown value is logged and read as `off`.

## `gpu=probe` (phase 1)

`gpu::probe()` runs in `init::boot` after `logpart::init`, before the APs are released. Output goes to the log (`gpu: ...`) and `/proc/gpu`:
- the firmware files (size, first word, FNV-1a 64 to compare with the host's copy);
- D4: each IOMMU from the IVRS (`hal::acpi::parse_ivrs`), its control register and `IommuEn`;
- the GA106 (`10de:2507`): capabilities, MSI/MSI-X, BARs with sizes; BAR0 mapped UC and `PMC_BOOT_0` decoded (`nvgpu::id`); the first 16 MiB of BAR1 mapped WC (not accessed); `pci::claim(..., "nvgpu")`.
- **No GPU register is written.** The only writes are the BAR sizing protocol's, in configuration space, with decoding off for microseconds (no CPU may draw on the GOP framebuffer then: hence before the APs).
- Metal job: `scripts/metal-run.sh --kconf 'gpu=probe' scripts/metal-jobs/gpu-probe.sh`.

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
- QEMU: `hw_tests::edu_mmio_dma_msi` (`scripts/run-kernel-tests.sh`; the runner adds `-device edu,dma_mask=0xffffffffffff`): MMIO, MSI to CPU 1 (the test boot keeps IF=0 on CPU 0), DMA both ways, mask refusal. The `edu` driver (`kernel/src/edu.rs`) is test-only.
