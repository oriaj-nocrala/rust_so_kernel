#!/usr/bin/env bash
# scripts/run-release.sh [constanos.img]
#
# Boots the release image (docs/try-it.md) in QEMU, with nothing built: the
# disk as a USB stick (where the real machine has it), PS/2 keyboard and
# mouse, AC97 sound, virtio-net, and the kernel log in constanos-serial.log
# next to the image. Needs qemu-system-x86_64 and OVMF (Arch: edk2-ovmf,
# Debian/Ubuntu: ovmf). The image is used with -snapshot: nothing written in
# the guest is kept; set KEEP=1 to keep it.

set -euo pipefail

IMG="${1:-constanos.img}"
[[ -f "$IMG" ]] || { echo "usage: $0 constanos.img (unpack constanos.img.zst with: zstd -d constanos.img.zst)" >&2; exit 2; }
command -v qemu-system-x86_64 >/dev/null || { echo "error: qemu-system-x86_64 not found" >&2; exit 1; }

OVMF_CODE="" OVMF_VARS=""
for pair in \
    /usr/share/edk2/x64/OVMF_CODE.4m.fd:/usr/share/edk2/x64/OVMF_VARS.4m.fd \
    /usr/share/OVMF/OVMF_CODE_4M.fd:/usr/share/OVMF/OVMF_VARS_4M.fd \
    /usr/share/OVMF/OVMF_CODE.fd:/usr/share/OVMF/OVMF_VARS.fd \
    /usr/share/ovmf/x64/OVMF_CODE.fd:/usr/share/ovmf/x64/OVMF_VARS.fd \
    /usr/share/ovmf/OVMF_CODE.fd:/usr/share/ovmf/OVMF_VARS.fd \
    /usr/share/edk2-ovmf/x64/OVMF_CODE.fd:/usr/share/edk2-ovmf/x64/OVMF_VARS.fd; do
    if [[ -f "${pair%%:*}" && -f "${pair##*:}" ]]; then
        OVMF_CODE="${pair%%:*}" OVMF_VARS="${pair##*:}"
        break
    fi
done
[[ -n "$OVMF_CODE" ]] || { echo "error: no OVMF found (Arch: pacman -S edk2-ovmf; Debian/Ubuntu: apt install ovmf)" >&2; exit 1; }

VARS="$(mktemp)"
trap 'rm -f "$VARS"' EXIT
cp "$OVMF_VARS" "$VARS"

ACCEL=(-cpu max)
[[ -w /dev/kvm ]] && ACCEL=(-enable-kvm -cpu host)
SNAPSHOT=(-snapshot)
[[ "${KEEP:-0}" == 1 ]] && SNAPSHOT=()
LOG="$(dirname "$IMG")/constanos-serial.log"
AUDIO=none
for a in pipewire pa sdl; do
    if qemu-system-x86_64 -audiodev help 2>/dev/null | grep -qx "$a"; then AUDIO=$a; break; fi
done

echo "Kernel log: $LOG"
# QEMU_ARGS: extra options, e.g. QEMU_ARGS="-display none".
qemu-system-x86_64 \
    -drive "if=pflash,format=raw,readonly=on,file=$OVMF_CODE" \
    -drive "if=pflash,format=raw,file=$VARS" \
    -device qemu-xhci,id=xhci \
    -drive "if=none,id=stick,format=raw,file=$IMG" \
    -device usb-storage,bus=xhci.0,drive=stick,bootindex=0 \
    "${SNAPSHOT[@]}" \
    -m 4G -smp 4 "${ACCEL[@]}" \
    -audiodev $AUDIO,id=snd0 -device AC97,audiodev=snd0 \
    -netdev user,id=n0 -device virtio-net-pci,netdev=n0,disable-legacy=on \
    -serial "file:$LOG" ${QEMU_ARGS:-}
