# Bare-metal support: kernel log, log partition, unattended runs, watchdog

The target is an AM4/Ryzen machine with **no serial capture**. Every observation there is the screen, the log partition on the USB stick, or `/proc` read from a shell. How to *use* all this (deploy, run a job, read the log): the `metal-run` skill.

## klog (`kernel/src/klog.rs`, `/proc/dmesg`)

- A 64 KiB BSS ring holding every byte of `serial_println!`/`serial_println_raw!` and of the console's serial mirror. A boot to the shell prompt is ~14 KiB.
- **Lock-free** (one `fetch_add` reserves a byte range), because it is written from ISRs, the fault handlers, the allocators and the panic handler. Concurrent writers may interleave.
- **Boot messages stay on screen:** `FramebufferConsole::new` does not clear the screen if the kernel already wrote to it (`KERNEL_WROTE`). `draw_boot_screen` reserves the banner rows.
- **No-keyboard escape hatch:** `show_boot_log_if_no_keyboard()` draws the USB/PCI log lines (filtered by substring) and holds them 30 s. It fires only if no USB keyboard was enumerated **and** no 8042 answers, so never in QEMU unless `QEMU_DEBUG_NO_PS2=1` and no USB keyboard.
- `hal::i8042::controller_present` only reads the status port (0xFF = absent). **Never send 0xAA/0xAB after init**: they can disable a working keyboard.

## Log partition (`kernel/src/block/logpart.rs`, `hal::logpart`)

- The ring is copied onto the stick's raw GPT partition `constanos-log` (no filesystem, so a torn write only spoils the log).
- Flushes happen:
  - every 5 s from the **idle task** (not the ISR, which may interrupt a `SERIAL` holder), if the ring grew. A CPU spinning at 100% starves it. Three failures in a row disable periodic flushing;
  - on `sync(2)` / `kdebug sync`;
  - from the panic handler (`try_lock` everywhere).
- Format:
  - sector 0 is a marker only the host writes;
  - then 16 slots of 128 KiB, **one per boot**, reused oldest first;
  - the ring is stored raw, with `write_pos` in the slot header, so flushes are incremental;
  - a boot claims its slot before writing data.
- Guards against writing the wrong sectors:
  - exact name lookup, with no fallback;
  - the host's marker must be in sector 0;
  - every write goes through `Partition`;
  - the partition type is Linux *reserved*, so it never confuses the data-partition fallback.

## Autorun (`kernel/src/autorun.rs`, `userspace/src/bin/shell.rs`)

- If `/mnt/autorun/job` exists, PID 1 prints `METAL-BEGIN <nonce>`, `sync`s, runs the job with `ash`, prints `METAL-DONE <nonce> exit=N|signal=N`, and `reboot(2)`s.
- In autorun mode a panic resets after the log flush instead of halting (`reboot::restart_from_panic`, lock-free).
- The kernel never deletes the job; the host removes it (`/mnt` is writable now, but a job that the kernel removed itself could be lost on a hang before the verdict).
- On the Ryzen, reset goes through the FADT reset register.

## Watchdog (`kernel/src/watchdog.rs`, `hal::sp5100_tco`)

- The AMD FCH TCO watchdog does not survive a reset, so the kernel arms it itself: **on every boot**, right after framebuffer setup (`watchdog::arm_early`), for 300 s, and never pings it.
- `watchdog::settle` disarms it after `autorun::detect` when there is no job.
- A watchdog reset shows on Linux as `sp5100_tco bootstatus=32`; `metal-run.sh --collect` reports it.
- Not covered: a hang between firmware and the first steps of `init::boot`.
- Build hook to test it: `CONSTANOS_TEST_HANG_BEFORE_FS=1` (spins before `fs::init`).
- `/proc/kdebug` shows the watchdog's state and time left.
