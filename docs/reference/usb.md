# USB (xHCI): keyboard, mouse, mass storage

Code: pure logic in `hal/src/{xhci,usb,hid,msc,gpt}.rs` (host-tested, including fixtures from the real stick in `hal/fixtures/`); hardware half in `kernel/src/usb/` (`xhci.rs`, `xhci/msc.rs`), `kernel/src/block/usb.rs`, `kernel/src/memory/mmio.rs`. Plan/history: `docs/storage/usb-msc-plan.md`.

Why it exists: the Ryzen target has no PS/2 and no IDE. Keyboard, mouse and `/mnt` all come over USB there.

## Design

- **Input reuses the PS/2 pipeline.** USB key presses are translated to Set-1 scancodes (`hal::hid::usage_to_set1`) and fed to `keyboard::process_scancode`, so every keyboard consumer works unchanged and both keyboards merge into one stream. Boot-mouse reports go into the PS/2 mouse queue (`mouse::push_usb_event`) with **HID Y negated** (PS/2 convention: up is positive). Byte 3, when the report has one, is the wheel (HID's sign is `REL_WHEEL`'s), as Linux's boot-protocol `usbmouse` reads it; the endpoint's full max packet is requested, so it arrives.
- **Polled, not interrupt-driven.** `usb::poll()` runs on CPU 0's tick, before the scheduler lock, with `try_lock` (skips if busy). It *returns* scancodes instead of dispatching them, so the driver lock is released before `process_scancode` (which can take the scheduler lock for SIGINT). Reason: the IDT is filled before PCI enumeration.
- **MMIO** (`memory::mmio::map`): 4 KiB pages mapped uncached (PWT|PCD) in an unused higher-half PML4 slot, so every address space inherits them. DMA buffers use the normal cacheable physmap (x86 DMA is coherent). BARs may sit above 4 GiB.
- PCI discovery by class over all buses: `pci::for_each_by_class`, 64-bit BARs.
- **One reader of the event ring**: `Xhci::service_events`. Keyboard events are decoded into a pending buffer and re-armed by whoever is draining; the caller gets its own event; everything else is logged. `usb_keys_dropped` in `/proc/kdebug` should stay 0.

## xHCI rules (each one cost a bare-metal debugging round)

- **Read dword 3 (the cycle bit) of an event TRB before its payload.** The controller writes the cycle bit last. Reading the payload first gives torn events (completion code 0, TRB pointer 0) and loses the real one. QEMU writes TRBs atomically and never shows this.
- **Match transfer events by slot + endpoint, not by TRB pointer alone.** Errors can be reported against the Setup-stage TRB.
- Log unmatched events (bounded); never drop them silently.
- Recovery, every retry logged:
  - a STALL → Reset Endpoint + Set TR Dequeue, then retry once;
  - port reset: up to 3 tries;
  - Address Device: retried once.
- Keyboard and mouse are looked up per interface, and both endpoints go into one Configure Endpoint. An interface that refuses `SET_PROTOCOL` is dropped alone. (The Ryzen's HyperX mouse also declares a boot keyboard.)
- Out of scope: hot-plug (ports are enumerated once at boot), external hubs, anything but boot keyboards, boot mice and storage.

## Mass storage (`/mnt` from the pendrive)

- The stick has GPT partitions `boot` (FAT), `constanos-data` (ext2) and `constanos-log` (raw, see `metal.md`).
- `hal::gpt` checks both CRCs and falls back to the backup. It looks partitions up by name, else takes the *only* Linux-filesystem partition. `hal::block::Partition` refuses out-of-range requests.
- `fs::ext2::init` tries USB first, then ATA.
- **The USB mount is read-write** since 2026-09-30, with the same mount-time repair passes as ATA (no journal: a hang mid-write can leave the data partition inconsistent; `scripts/sync-usb-data.sh` rebuilds it, and the boot partition is separate). `sync(2)` and the reboot path send SCSI SYNCHRONIZE CACHE (`block::usb::sync_stick`) so the stick's own cache reaches the medium; a stick that rejects it is only logged. `sync-usb-data.sh` recreates the filesystem, so files a metal run leaves on `/mnt` are gone at the next sync.
- **Every transfer runs with IF=0 and `CONTROLLERS` held** (`usb::storage_read`/`storage_write`, ≤64 KiB each, lock released between transfers), so the transfer is the ring's only reader and can't be preempted. Cost: ~1 ms of interrupt latency per 64 KiB.
- BOT recovery:
  - STALL → clear halt on both sides;
  - bad CSW or Phase Error → Reset Recovery;
  - UNIT ATTENTION → retry.

  QEMU never exercises any of this.

## Observing and testing

- `/proc/kdebug`: `usb_keyboards`, `usb_key_reports` (0 = the controller delivers nothing; >0 = the fault is in decoding), `usb_mice`, `usb_mouse_reports`, `usb_keys_dropped`.
- The on-screen boot summary shows counts (controllers, ports, connected, addressed, errors, keyboards), not a verdict.
- QEMU: `qemu-xhci` is always attached (`QEMU_DEBUG_NO_USB=1` removes it).
  - `QEMU_USB_KBD=1`: `sendkey` goes through xHCI instead of the 8042.
  - `QEMU_USB_MOUSE=1`: USB mouse.
  - `QEMU_DEBUG_NO_PS2=1`: no 8042, the Ryzen's shape.
  - `QEMU_USB_STORAGE=<img>` + `QEMU_DEBUG_NO_DISK=1`: `/mnt` from a USB stick.
  - `-device qemu-xhci,p2=4,p3=0`: USB 2 ports.
- Deploying to the real stick: `metal-run` skill.
