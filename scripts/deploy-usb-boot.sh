#!/usr/bin/env bash
# scripts/deploy-usb-boot.sh [--image-only] [--no-test]
#
# Writes the kernel you just built to the `boot` partition of the
# bare-metal USB stick. Run `cargo build` first; for the data partition
# (doom, quake, /mnt/bin) use scripts/sync-usb-data.sh.
#
# WHY NOT JUST `dd` THE UEFI IMAGE'S FAT: the `dev` kernel outgrew the
# stick's 17 MiB `boot` partition (18 MB on 2026-09-23), and `bootloader`
# sizes its own FAT to fit, at 18 MiB — so `dd ... count=34816` of it
# silently truncates the kernel. This builds a fresh FAT16 of exactly the
# partition's size holding the same two files:
#
#   efi/boot/bootx64.efi   copied from the UEFI image
#   kernel-x86_64          kernel/target/.../kernel through `strip --strip-debug`
#
# Stripping only drops DWARF: the PT_LOAD segments are byte-identical, and
# nothing on the target machine reads debug info (gdb runs against the
# unstripped ELF on the host). ~18 MB -> ~6 MB.
#
# Before writing, the image is boot-tested in QEMU as a `usb-storage`
# stick next to the real stick's shape (`scripts/usb-log.sh mkimage`),
# and must reach "About to start first process" (--no-test skips this).
# After writing, the partition is read back and compared byte for byte.
#
# The partition is found by GPT label `boot`, never by device node — see
# scripts/sync-usb-data.sh for why — and its size must match the image.
#
#   --image-only   build target/usb-boot.img (and test it), write nothing
#   --no-test      skip the QEMU boot test

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

IMAGE_ONLY=0
TEST=1
for arg in "$@"; do
    case "$arg" in
        --image-only) IMAGE_ONLY=1 ;;
        --no-test) TEST=0 ;;
        *) echo "usage: $0 [--image-only] [--no-test]" >&2; exit 2 ;;
    esac
done

LABEL=boot
DEV="/dev/disk/by-partlabel/$LABEL"
OUT="$REPO_ROOT/target/usb-boot.img"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

for tool in mkfs.fat mcopy mmd fsck.fat strip qemu-system-x86_64; do
    command -v "$tool" >/dev/null || { echo "error: '$tool' not found in PATH." >&2; exit 1; }
done

# ── Inputs: the newest build ────────────────────────────────────────────
BUILD_OUT="$(ls -t target/debug/build/so2-*/output 2>/dev/null | head -1 || true)"
[[ -n "$BUILD_OUT" ]] || { echo "error: no build output under target/ — run 'cargo build' first." >&2; exit 1; }
UEFI_IMG="$(grep -oP 'UEFI_PATH=\K.*' "$BUILD_OUT")"
KERNEL_ELF="$REPO_ROOT/kernel/target/x86_64-unknown-none/debug/kernel"
[[ -f "$UEFI_IMG" && -f "$KERNEL_ELF" ]] || { echo "error: missing $UEFI_IMG or $KERNEL_ELF." >&2; exit 1; }

# Partition size: from the stick when writing, else the known 34816.
SECTORS=34816
if [[ $IMAGE_ONLY -eq 0 ]]; then
    [[ -e "$DEV" ]] || { echo "error: no partition labelled '$LABEL' ($DEV). Is the stick plugged in?" >&2; exit 1; }
    REAL_DEV="$(readlink -f "$DEV")"
    SECTORS="$(sudo blockdev --getsz "$REAL_DEV")"
    if findmnt -n -S "$REAL_DEV" >/dev/null; then
        echo "error: $REAL_DEV is mounted at $(findmnt -n -o TARGET -S "$REAL_DEV"). Unmount it first." >&2
        exit 1
    fi
fi

# ── Build the FAT ────────────────────────────────────────────────────────
strip --strip-debug -o "$WORK/kernel" "$KERNEL_ELF"
mcopy -i "$UEFI_IMG@@$((34 * 512))" ::/efi/boot/bootx64.efi "$WORK/bootx64.efi"
rm -f "$OUT"
truncate -s $((SECTORS * 512)) "$OUT"
mkfs.fat -F 16 -s 1 -n KERNEL "$OUT" >/dev/null
mmd -i "$OUT" ::/efi ::/efi/boot
mcopy -i "$OUT" "$WORK/bootx64.efi" ::/efi/boot/bootx64.efi
if ! mcopy -i "$OUT" "$WORK/kernel" ::/kernel-x86_64; then
    echo "error: the stripped kernel ($(stat -c %s "$WORK/kernel") bytes) does not fit a $SECTORS-sector FAT." >&2
    exit 1
fi
fsck.fat -n "$OUT" >/dev/null
echo "Image:  $OUT ($SECTORS sectors, kernel $(stat -c %s "$WORK/kernel") bytes stripped)"

# ── Boot test in QEMU, from a stick with the real shape ──────────────────
if [[ $TEST -eq 1 ]]; then
    OVMF_CODE="$(grep -oP 'OVMF_CODE=\K.*' "$BUILD_OUT")"
    OVMF_VARS="$(grep -oP 'OVMF_VARS=\K.*' "$BUILD_OUT")"
    cp "$OVMF_VARS" "$WORK/vars.fd"
    scripts/usb-log.sh mkimage "$WORK/stick.img" >/dev/null
    dd if="$OUT" of="$WORK/stick.img" bs=512 seek=34 conv=notrunc status=none
    echo "Boot-testing in QEMU (up to 90 s)..."
    timeout 90 qemu-system-x86_64 \
        -drive "if=pflash,format=raw,readonly=on,file=$OVMF_CODE" \
        -drive "if=pflash,format=raw,file=$WORK/vars.fd" \
        -device qemu-xhci,id=xhci \
        -drive "if=none,id=stick,format=raw,file=$WORK/stick.img" \
        -device usb-storage,bus=xhci.0,drive=stick,bootindex=0 \
        -m 2G -cpu max -serial "file:$WORK/serial.log" -display none -monitor none \
        >/dev/null 2>&1 &
    QPID=$!
    ok=0
    for _ in $(seq 1 90); do
        if grep -aq "About to start first process" "$WORK/serial.log" 2>/dev/null; then ok=1; break; fi
        if grep -aq "KERNEL PANIC" "$WORK/serial.log" 2>/dev/null; then break; fi
        kill -0 "$QPID" 2>/dev/null || break
        sleep 1
    done
    kill "$QPID" 2>/dev/null || true
    wait "$QPID" 2>/dev/null || true
    if [[ $ok -ne 1 ]]; then
        echo "error: the image did not boot in QEMU. Last serial lines:" >&2
        tail -20 "$WORK/serial.log" >&2 || true
        exit 1
    fi
    echo "Boot test: OK"
fi

[[ $IMAGE_ONLY -eq 1 ]] && exit 0

# ── Write and verify ─────────────────────────────────────────────────────
echo "Writing to $REAL_DEV (label '$LABEL')..."
sudo dd if="$OUT" of="$REAL_DEV" bs=1M conv=fsync status=none
sudo sync
if sudo cmp "$OUT" "$REAL_DEV"; then
    echo "Written and verified."
else
    echo "error: read-back of $REAL_DEV differs from the image." >&2
    exit 1
fi
