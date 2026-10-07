# Trying constanos without building it

One disk image, `constanos.img`, boots on a real machine from a USB stick and in a VM. Download
`constanos.img.zst` (and `constanos.vmdk` for VirtualBox/VMware) from the
[releases](https://github.com/oriaj-nocrala/rust_so_kernel/releases), check it against `SHA256SUMS`, and
unpack it with `zstd -d constanos.img.zst` (on Windows: 7-Zip with zstd support, or use the `.vmdk`).

The image is GPT with three partitions: `boot` (FAT, the UEFI loader and the kernel), `constanos-data`
(ext2, mounted at `/mnt`: the desktop, DOOM, Quake, the Vulkan programs) and `constanos-log` (the kernel
log, see [Reporting a problem](#reporting-a-problem)).

What the machine needs:

- **UEFI** (no legacy BIOS boot) and **Secure Boot off** (the loader is not signed).
- An x86-64 CPU and 2 GiB of RAM or more.
- The disk where the kernel can read it: a **USB stick** (xHCI; in a VM, a disk on a USB controller),
  or the **secondary IDE channel**.
  There is no SATA/AHCI or NVMe driver: a VM's default SATA disk boots the kernel but leaves `/mnt`
  empty, and the screen says so in red.

Once it boots: BusyBox `ash` on the console. `compositor` opens the desktop (Ctrl+Alt+Backspace closes
it), and `doom`, `quake`, `cpumon`, `cmatrix` run from the shell or from the start menu.

## QEMU (Linux)

```bash
scripts/run-release.sh constanos.img       # or: curl -O the script from the repo
```

It boots the image as a USB stick with KVM if available, PS/2 keyboard and mouse, AC97 sound and
virtio-net (DHCP), and writes the kernel log to `constanos-serial.log` next to the image. Needs
`qemu-system-x86_64` and OVMF (Arch: `edk2-ovmf`, Debian/Ubuntu: `ovmf`).

## VirtualBox (7.x)

Tested on VirtualBox 7.2. Run from the directory holding `constanos.vmdk` (Windows:
`"C:\Program Files\Oracle\VirtualBox\VBoxManage.exe"` in PowerShell, same arguments):

```bash
VBoxManage createvm --name constanos --ostype Other_64 --register
VBoxManage modifyvm constanos --firmware efi --memory 4096 --cpus 4 --ioapic on \
  --graphicscontroller vmsvga --mouse ps2 --keyboard ps2 \
  --audio-controller ac97 --audio-enabled on --audio-out on \
  --nic1 none --usb-xhci on \
  --uart1 0x3F8 4 --uart-mode1 file "$PWD/constanos-serial.log"
VBoxManage storagectl constanos --name USB --add usb --controller USB
VBoxManage storageattach constanos --storagectl USB --port 0 --device 0 --type hdd --medium "$PWD/constanos.vmdk"
VBoxManage startvm constanos
```

- **The disk goes on a USB controller** (with xHCI on): the kernel sees it as a USB stick, the way
  the real machine boots. An IDE disk on the secondary master works too (tested in VirtualBox the
  same way, and the only choice in VMware), but USB is the path the real machine uses.
- **PS/2 mouse**, not the USB tablet: the kernel reads relative motion only.
- **AC97** gives DOOM and Quake sound. There is no network in VirtualBox (no e1000 driver).

## VMware Workstation / Player

Put this next to `constanos.vmdk` as `constanos.vmx` and open it:

```
.encoding = "UTF-8"
config.version = "8"
virtualHW.version = "19"
displayName = "constanos"
guestOS = "other-64"
firmware = "efi"
uefi.secureBoot.enabled = "FALSE"
memsize = "4096"
numvcpus = "4"
ide1:0.present = "TRUE"
ide1:0.fileName = "constanos.vmdk"
serial0.present = "TRUE"
serial0.fileType = "file"
serial0.fileName = "constanos-serial.log"
ethernet0.present = "FALSE"
sound.present = "FALSE"
```

`ide1:0` is the secondary master. No sound (VMware has no AC97) and no network. **Not tested in VMware
itself:** VMware cannot present a disk as a USB stick, so this uses the IDE path, which passes the same
load in VirtualBox.

## A real machine (USB stick)

Write the whole image to a USB stick (**this erases the stick**): balenaEtcher or Rufus (DD mode) on
Windows/macOS, or on Linux:

```bash
sudo dd if=constanos.img of=/dev/sdX bs=4M conv=fsync status=progress   # sdX = the stick, not a partition
```

Boot it from the firmware's boot menu with Secure Boot off.

- Works on: USB keyboard and mouse, a laptop's PS/2 keyboard, the UEFI framebuffer at the panel's
  resolution, a Realtek RTL8168 NIC.
- **NVIDIA RTX 3050 (GA106, PCI id `10de:2507`):** the image boots with `gpu=uapi`. The driver brings
  the card up through GSP-RM and Vulkan (Mesa NVK) runs on it: `snake3d`, and `vk_comp`, the GPU
  compositor. It has only been tested on one card, in one machine (a Ryzen 9 5900X): on yours it may
  fail, and the log says where. Every other GPU uses the CPU compositor.
- Not supported: touchpads (I2C-HID), Wi-Fi, HD Audio, SATA/NVMe disks.

## Reporting a problem

Open an [issue](https://github.com/oriaj-nocrala/rust_so_kernel/issues/new/choose) with the machine or
VM, what you did and what you saw (a photo of the screen helps), and the kernel log:

- **VM:** `constanos-serial.log`, from the settings above.
- **USB stick:** the kernel copies its log to the `constanos-log` partition every 5 s and on a panic.
  Plug the stick into Linux and run
  [`scripts/usb-log.sh`](../scripts/usb-log.sh) `read` (needs bash, `dd` and python3, not the rest of the repo):
  `sudo bash usb-log.sh read > constanos.log`.
