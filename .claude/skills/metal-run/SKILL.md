---
name: metal-run
description: Playbook for running rust_so_kernel on the physical AM4/Ryzen target machine (no serial): deploying the kernel and data to the USB stick safely, running an unattended job with scripts/metal-run.sh and collecting the verdict, reading the kernel log from the stick's constanos-log partition, testing a job in QEMU first, and reading the screen via phone photos. Use whenever a change must be verified on real hardware or the user mentions the Ryzen, the stick, metal, or a bare-metal boot. Keywords: metal-run.sh, deploy-usb-boot.sh, sync-usb-data.sh, usb-log.sh, autorun, BootNext, constanos-log, Ryzen.
---

# Running on the Ryzen (bare metal)

The machine boots constanos from a USB stick and also runs Linux, which is where these scripts run. There is no serial port. How autorun, the watchdog and the log partition work inside the kernel: `docs/reference/metal.md`.

**First try to reproduce the problem in QEMU** (`qemu-debug` skill): each metal attempt costs a physical reboot.

## The stick

GPT partitions: `boot` (FAT, 34816 sectors), `constanos-data` (ext2, becomes `/mnt`), `constanos-log` (raw, the kernel log).

- **Never `dd` the UEFI image onto the whole device**: that replaces the GPT and destroys `constanos-data` and `constanos-log`. Also never `dd` the image's own FAT into `boot`: it is too small for the kernel and truncates it.
- **Kernel**: `scripts/deploy-usb-boot.sh`. It builds a fresh FAT16 with `bootx64.efi` and the kernel through `strip --strip-debug`, boot-tests it in QEMU from a `usb-storage` stick, then writes it and reads it back.
  - `--image-only`: build and test without writing.
  - `--no-test`: needed for a kernel built with `CONSTANOS_TEST_HANG_BEFORE_FS=1`, which fails the boot test on purpose.
- **Data** (`/mnt`: programs, WADs, fonts): `scripts/sync-usb-data.sh`.
- **Log partition, once**: `scripts/usb-log.sh mkpart /dev/sdX` (backs up the GPT, appends 64 MiB, asks for `yes`), then `scripts/usb-log.sh init`.

## Unattended run (nobody at the machine)

```bash
scripts/metal-run.sh JOB.sh      # build, deploy if the kernel changed, sync data, job + nonce, BootNext, reboot
# ... the machine boots constanos, runs the job, reboots into Linux ...
scripts/metal-run.sh --collect   # verdict + log archived in target/metal/runs/<nonce>/
```

- A job is an `ash` script, written to `/mnt/autorun/job`. PID 1 prints `METAL-BEGIN <nonce>` … `METAL-DONE <nonce> exit=N|signal=N` and reboots.
- Verdicts: `OK`, `FAIL`, `PANIC`, `HANG`, `NO-JOB`, `NO-BOOT` (plus `watchdog reset` when Linux's `sp5100_tco` reports `bootstatus=32`). The watchdog resets a hung boot after 300 s.
- Boot options for one run: `--kconf 'gpu=probe'` (written to `autorun/kernel.conf`, gone after the run; `docs/reference/gpu.md`). Reusable jobs live in `scripts/metal-jobs/`.
- Other options:
  - `--abort` undoes a run that never booted;
  - `--no-reboot`/`--no-deploy` for dry runs;
  - `--classify` runs the classifier alone.
- **The session resumes by itself** (`scripts/metal-resume.sh`): tty1 autologins and `~/.zlogin` runs it. It `--collect`s, then `claude --resume`s the session that launched the run, with the verdict in the prompt.
  - Brakes: `target/metal/budget` (automatic resumes left; missing or 0 = collect only) and `target/metal/stop`.
  - **Check `cat target/metal/budget` before launching a measurement.** At 0 the machine comes back to Linux, collects, and nobody resumes the session: the result waits until the user reopens it. Tell the user which case applies, and raise the budget (`echo 1 > target/metal/budget`) only if they want the automatic resume.
  - `--dry-run` shows what it would do.

### Test the job in QEMU first

```bash
cp disk.img /tmp/j.img
debugfs -w /tmp/j.img -R 'mkdir /autorun'   # then: write job /autorun/job, write nonce /autorun/nonce
QEMU_DEBUG_DISK_IMG=/tmp/j.img QEMU_DEBUG_EXTRA_ARGS=-no-reboot QEMU_DEBUG_STATE_DIR=/tmp/qj scripts/qemu-debug.sh start
```

`-no-reboot` makes the job's final reset end QEMU instead of running the job again. Timing bugs have shown up only under host load, so run several in parallel.

- **Keep `QEMU_DEBUG_STATE_DIR` short** (`/tmp/qj`, not the session scratchpad): QEMU puts `monitor.sock` there, and a Unix socket path must be under 108 bytes. Too long and QEMU exits at once ("UNIX socket path ... is too long", only in `qemu-stdout.log`) with an empty `serial.log`, which looks like a hang.
- A job run with `--kconf` needs that file in the image too: `write kconf /autorun/kernel.conf`.

## Reading the kernel log

`scripts/usb-log.sh read` (the latest boot), `read --all`, `list` (one slot per boot, 16 kept). In QEMU: `scripts/usb-log.sh mkimage out.img` builds a stick of the real shape, then boot it with `QEMU_USB_STORAGE=out.img QEMU_DEBUG_NO_DISK=1` and read it with `usb-log.sh read --image out.img`.

## When there is no log (early hang, manual boot)

The screen is the only output. Ask the user for a **photo**, not a transcription, and pull it over adb:

```
adb connect <ip>:<port>
adb shell 'ls -t /sdcard/DCIM/Camera/ | head -5'
adb pull /sdcard/DCIM/Camera/<newest>.jpg
```

Then Read the image (upscale first with `ffmpeg -i x.jpg -vf scale=1600:-1 x.png` if needed).

## What only metal shows

Write-combining and VRAM speed, APERF/MPERF, k10temp, RAPL, torn xHCI event TRBs, HDA/NVMe/RTL8111 (no drivers yet), a composite USB mouse, and all RAM above 512 MiB.

## When the user reports a glitch seen on the screen ("a stutter at 2 seconds")

An average hides it (58.6 fps vs 60.1). What worked, in two rounds: (1) make the program log every frame interval over 25 ms with its time since start **and** the absolute `CLOCK_MONOTONIC` of its start; stamp each phase of the job with `cut -d' ' -f1 /proc/uptime` (same clock base); run the program alone first as a baseline; (2) add a kernel-side record of what it suspects (a ring of lock holds over 2 ms: operation, when, how long: `gpu_uapi_slow:` in /proc/kdebug) and print it in the job's summary. Lining the two up located the cause (a global lock held across RM calls) and, after the fix, showed it gone (0 intervals over 25 ms). Pattern: measure first, one round per hypothesis, put every independent diagnostic in the same boot.

Job summary lines are printed twice and the log wraps (64 KiB): print the whole summary at the end of the job (`cat /tmp/<job>.sum`) and read it with `grep -a <job-name> boot.log | awk '!seen[$0]++'`. `scripts/metal-jobs/gpu-multi.sh` is a template for a multi-program job with phases, baselines and a leak check (`gpu_share:` must read 0/0/0 at the end).
