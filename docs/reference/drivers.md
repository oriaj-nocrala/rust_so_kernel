# Devices, PCI and audio

How to write or port a driver: the `kernel-drivers` skill. Design and direction: `docs/drivers/`.

## Device files (`kernel/src/drivers/`, registry `DEVICES` in `drivers/mod.rs`)

A device is a `FileHandle` (`read`/`write`/`ioctl`/`stat`/`dup`/…). Device state lives in kernel globals, so `dup()` just builds a new instance. `DeviceEntry::open` may fail (`EBUSY`, `ENODEV`).

| Device | What |
|--------|------|
| `/dev/null`, `/dev/zero` | |
| `/dev/urandom`, `/dev/random` | the same non-blocking generator (`kernel/src/random.rs`, `hal::random`); a write mixes in without crediting entropy |
| `/dev/console` | serial; stdin reads come from the keyboard ring |
| `/dev/fb` | text console (`graphics.md`) |
| `/dev/fb0` | exclusive graphics mode (`graphics.md`) |
| `/dev/kbd` | non-blocking keyboard, chars + ANSI sequences |
| `/dev/input/event0` | keyboard, Linux evdev (`struct input_event`, `KEY_*` derived from Set-1, then `SYN_REPORT`). The ring fills from boot, so games drain it at startup |
| `/dev/input/event1` | mouse (PS/2 and USB merged): `REL_X`/`REL_Y` (PS/2 sign: up positive), `REL_WHEEL` (evdev's sign: up/away positive), `BTN_*` |
| `/dev/dsp` | write-only PCM, fixed 48 kHz stereo s16le, AC97 |
| `/dev/ptmx`, `/dev/pts/N`, `/dev/tty` | ptys (`ipc.md`) |

- Keyboard input: the PS/2 ISR and the USB poll both feed `keyboard::process_scancode` (Set-1). The decoder `keyboard::DECODER` is an `IrqMutex`.
- PS/2 mouse (`mouse.rs`): 3-byte packets, or 4 with the wheel: `hal::mouse::enable_aux` knocks (sample rates 200, 100, 80, then "get ID") and an IntelliMouse answers ID 3 (the boot log says `IntelliMouse wheel`); the 4th byte is a signed count, negated into `REL_WHEEL` as Linux's psmouse does. A mouse that ignores or refuses the knock stays 3-byte. QEMU's PS/2 mouse is an IntelliMouse (`qemu-debug.sh mouse-move 0 0 1` is a notch up). A partial packet is dropped after 500 ms of silence (`mouse_resyncs` in `/proc/kdebug`). Test: `qemu-debug.sh mouse-move` + `cat /dev/input/event1 | wc -c`.
- `poll` readiness for input devices: `FileHandle::event_source`.

## PCI (`kernel/src/pci.rs`, `hal::pci`)

- Config access through ports 0xCF8/0xCFC, **behind the lock `pci::CONFIG`** (`IrqLock`; SMN access uses it too). `config_write8` on the reset/panic path only *tries* the lock, for a bounded time.
- Discovery: `find_device` (vendor/device, bus 0) and `for_each_by_class` (all buses, 64-bit BARs).
- **A driver that takes a PCI function must `pci::claim` it** (`claim_matching` for legacy ATA). `/proc/pci` lists every function with its claiming driver or `-`; `unclaimed` is the to-do list for new drivers (on the Ryzen: RTL8111 Ethernet, NVMe, SATA, HDA). The GPU is claimed as `nvgpu` only with `gpu=probe` or higher (`gpu.md`).
- Decoding is host-tested against the Ryzen's real config space (`hal/fixtures/ryzen-pci-config.txt`).
- Full 256-byte space, capabilities, BAR sizing and MSI: `gpu.md`.

## AC97 (`kernel/src/ac97.rs`)

- Intel 82801AA (QEMU `-device AC97`). Cold reset, PCM-out reset, mixer unmute.
- **Polled, not interrupt-driven** (the IDT is built before PCI enumeration): `write_pcm()` spins on the CIV register with no lock held.
- The 32-entry BDL aliases 8 physical buffers (`slot_phys[i % 8]`).
- Fixed format, no negotiation. The Ryzen has HDA instead, which has no driver.
