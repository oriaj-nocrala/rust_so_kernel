---
name: Boot report
about: constanos did not boot, or something broke, on your machine or VM
labels: boot-report
---

**Machine or VM:** (e.g. VirtualBox 7.1 on Windows 11; or HP Pavilion 15-xx, Intel i5-1135G7, Iris Xe)

**How you ran it:** (QEMU / VirtualBox / VMware / USB stick; release version or commit)

**What happened:** (where it stopped, what the screen says — a photo helps)

**Kernel log:** attach `constanos-serial.log` (VM) or the output of `sudo bash usb-log.sh read` (USB stick).
See [docs/try-it.md](../../docs/try-it.md#reporting-a-problem).
