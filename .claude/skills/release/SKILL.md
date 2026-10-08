---
name: release
description: Playbook for publishing a downloadable constanos release (GitHub release with the disk image): building the image on the developer's machine so it carries the Vulkan programs and the firmware, checking it in QEMU and VirtualBox, and `gh release create`. Use when the user asks for a release, a downloadable image, or to send the image to someone (e.g. a reviewer). Keywords: release, gh release, make-release-image.sh, constanos.img, vmdk, try-it, VirtualBox.
---

# Publishing a release

Nothing has been released yet. The first one is planned for when Gameboy Hub (YouTube; reviews AI-built OSes in
VirtualBox/VMware with a coding agent, then on an HP Pavilion laptop) answers the email of 2026-10-07 about his
VM and laptop: test on what he uses before publishing. **A release is public: ask the user before `gh release create`.**

## Why it is built locally, not by CI

CI (`.github/workflows/ci.yml`) builds the same image as an artifact, but without `vk_comp`, `snake3d` and the
NVIDIA/Realtek firmware: Mesa is built by hand (`mesa-port/build.sh`, needs `~/src/gpu-ref`), and the firmware
comes from the host's `/usr/lib/firmware`. The release must carry them (the NVIDIA driver is what the project
is known for), so it is built on the developer's machine.

## Steps

1. `git status` clean, CI green on the commit being released.
2. Mesa programs current: if `/dev/nvgpu`'s uapi (`nvgpu/uapi/nvgpu.h`) changed since `disk-image-root/bin/vk_comp`
   and `snake3d` were built, rebuild them (`gpu-g5` skill).
3. `cargo build`, then `scripts/make-release-image.sh`. It must end with **no** "not on the image" warning, and
   boot-tests the image in QEMU as a USB stick and as an IDE disk.
4. Check it as the reader will run it, from `target/release-image/`:
   - `scripts/run-release.sh constanos.img` (QEMU): desktop with `compositor`, `doom`.
   - VirtualBox with the exact commands of `docs/try-it.md`, disk on USB (and on IDE). VirtualBox needs `kvm_amd`
     unloaded: `sudo rmmod kvm_amd; sudo modprobe vboxdrv`, and back afterwards with
     `sudo modprobe -r vboxnetflt vboxnetadp vboxdrv; sudo modprobe kvm_amd`. Drive it headless with
     `VBoxManage startvm constanos --type headless`, `controlvm … keyboardputstring`, `controlvm … screenshotpng`,
     and read the serial file the recipe writes.
   - If there is time and the RTX 3050: the image on the stick on the Ryzen (`metal-run` skill), `gpu=uapi` is on in it.
5. Tag and publish (after the user's go-ahead):
   ```bash
   git tag -a v0.1.0 -m "constanos 0.1.0"; git push origin v0.1.0
   gh release create v0.1.0 target/release-image/constanos.img.zst target/release-image/constanos.vmdk \
       target/release-image/SHA256SUMS --title "constanos 0.1.0" --notes-file <notes>
   ```
   Notes: what runs (link `docs/try-it.md`), what was tested where (QEMU, VirtualBox version, the Ryzen), the
   NVIDIA scope (RTX 3050 GA106 `10de:2507` only, one card tested), what does not work (SATA/NVMe, Wi-Fi,
   touchpads, HD Audio), how to report (the boot-report issue template). Only claims the repo can back.
6. `.vmdk` is ~270 MB: if a release asset limit or upload is a problem, ship only `.img.zst` and say in the notes
   how to convert (`qemu-img convert -O vmdk`, or `VBoxManage convertfromraw`).
