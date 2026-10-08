#!/usr/bin/env bash
# scripts/make-release-image.sh [--no-test]
#
# Builds the downloadable image: one disk that boots on a real machine from
# a USB stick and in a VM (QEMU, VirtualBox, VMware) as a disk. Run
# `cargo build` first (and `mesa-port/build.sh` + the Vulkan programs, if
# they should be on it: whatever is in disk-image-root/bin/ goes in).
#
#   target/release-image/constanos.img       raw GPT disk: dd / Etcher / Rufus, or QEMU
#   target/release-image/constanos.vmdk      the same disk for VirtualBox and VMware
#   target/release-image/constanos.img.zst   what gets uploaded, with SHA256SUMS
#
# Layout, the same as the boot stick (scripts/usb-log.sh mkimage):
#   boot            FAT16, efi/boot/bootx64.efi + the stripped kernel
#                   (scripts/deploy-usb-boot.sh --image-only)
#   constanos-data  ext2, a FRESH copy of disk-image-root/ (never the working
#                   disk.img, which carries whatever earlier boots wrote),
#                   plus the GSP firmware and a release etc/kernel.conf
#   constanos-log   the kernel log partition, marked
#
# The kernel finds /mnt in `constanos-data` on a USB stick, or on a GPT disk
# on the secondary IDE channel (block::ata_data_partition), which is where a
# VM must attach it: there is no SATA/AHCI or NVMe driver.
#
# The image is boot-tested in QEMU both ways (USB stick, secondary IDE) with
# -snapshot, so the tests do not write into it. --no-test skips that.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

TEST=1
for arg in "$@"; do
    case "$arg" in
        --no-test) TEST=0 ;;
        *) echo "usage: $0 [--no-test]" >&2; exit 2 ;;
    esac
done

die() { echo "error: $*" >&2; exit 1; }

for tool in mke2fs e2fsck sfdisk qemu-img qemu-system-x86_64 zstd sha256sum rsync; do
    command -v "$tool" >/dev/null || die "'$tool' not found in PATH."
done

OUT_DIR="$REPO_ROOT/target/release-image"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$OUT_DIR"

# ── boot: the FAT with the bootloader and the stripped kernel ─────────────
scripts/deploy-usb-boot.sh --image-only --no-test >/dev/null
BOOT_FAT="$REPO_ROOT/target/usb-boot.img"

# ── constanos-data: a fresh ext2 from disk-image-root/ ─────────────────────
ROOT="$WORK/root"
rsync -a --exclude 'etc/kernel.conf' disk-image-root/ "$ROOT"/

# Release boot options (kernel/src/bootopts.rs). gpu=uapi only acts on the
# RTX 3050 it was written for (10de:2507); anywhere else the driver finds no
# device and the desktop runs on the CPU compositor. nic= defaults to `net`.
mkdir -p "$ROOT/etc"
cat > "$ROOT/etc/kernel.conf" <<'EOF'
# Boot options of the release image (kernel/src/bootopts.rs, docs/reference/gpu.md).
# gpu=uapi drives an NVIDIA RTX 3050 (GA106, 10de:2507) up to Vulkan; on any other
# machine it finds no such device and does nothing. gpu=off disables it.
gpu=uapi
EOF

# The GSP firmware (63 MB) is not in disk-image-root/ (it does not fit the
# working disk.img): the same source as scripts/sync-usb-data.sh.
GSP_REL="lib/firmware/nvidia/ga106/gsp/gsp-570.144.bin"
GSP_ZST="/usr/lib/firmware/nvidia/ga106/gsp/gsp-570.144.bin.zst"
GSP_CACHE="$REPO_ROOT/target/firmware/gsp-570.144.bin"
if [[ ! -f "$GSP_CACHE" && -f "$GSP_ZST" ]]; then
    mkdir -p "$(dirname "$GSP_CACHE")"
    zstd -q -d -f -o "$GSP_CACHE" "$GSP_ZST"
fi
MISSING=()
if [[ -f "$GSP_CACHE" ]]; then
    mkdir -p "$ROOT/$(dirname "$GSP_REL")"
    cp "$GSP_CACHE" "$ROOT/$GSP_REL"
else
    MISSING+=("GSP firmware ($GSP_ZST): the NVIDIA driver stops before GSP-RM")
fi
for f in lib/firmware/nvidia/ga106/gsp/booter_load-570.144.bin lib/firmware/nvidia/LICENCE.nvidia \
         lib/firmware/rtl_nic/rtl8168h-2.fw bin/vk_comp bin/snake3d bin/compositor bin/doom bin/quake \
         freedoom1.wad id1/pak0.pak; do
    [[ -e "$ROOT/$f" ]] || MISSING+=("$f")
done

# Size: the contents plus a quarter for ext2 metadata, plus 64 MiB free.
CONTENT_MIB=$(( ($(du -sb "$ROOT" | cut -f1) + 1048575) / 1048576 ))
DATA_MIB=$(( CONTENT_MIB + CONTENT_MIB / 4 + 64 ))
DATA_IMG="$WORK/data.img"
# Same features as build.rs's disk.img (the kernel's ext2 is minimal).
mke2fs -q -t ext2 -b 1024 -O ^resize_inode,^dir_index -E root_owner=0:0 \
    -d "$ROOT" "$DATA_IMG" "${DATA_MIB}M"
e2fsck -fn "$DATA_IMG" >/dev/null 2>&1 || die "the fresh ext2 does not pass e2fsck"

# ── The whole disk ─────────────────────────────────────────────────────────
IMG="$OUT_DIR/constanos.img"
scripts/usb-log.sh mkimage "$IMG" "$DATA_IMG" >/dev/null
dd if="$BOOT_FAT" of="$IMG" bs=512 seek=34 conv=notrunc status=none

# ── Boot tests ─────────────────────────────────────────────────────────────
BUILD_OUT="$(ls -t target/debug/build/so2-*/output 2>/dev/null | head -1 || true)"
OVMF_CODE="$(grep -oP 'OVMF_CODE=\K.*' "$BUILD_OUT")"
OVMF_VARS="$(grep -oP 'OVMF_VARS=\K.*' "$BUILD_OUT")"
# KVM when there is one, unless QEMU_ACCEL=tcg: under nested virtualization (a CI
# runner) every ATA PIO port access is a costly VM exit, and mounting /mnt from
# the IDE disk took over 100 s with KVM, where TCG pays nothing extra for port I/O.
ACCEL=(-cpu max)
[[ -w /dev/kvm && "${QEMU_ACCEL:-}" != tcg ]] && ACCEL=(-enable-kvm -cpu host)
BOOT_TIMEOUT="${BOOT_TIMEOUT:-120}"

# boot_test NAME DISK-ARGS...: boots the image with -snapshot (nothing is
# written to it) and waits for /mnt and the first process.
boot_test() {
    local name="$1"; shift
    local log="$OUT_DIR/boot-test-$name.log"
    rm -f "$log"
    cp "$OVMF_VARS" "$WORK/vars-$name.fd"
    timeout "$BOOT_TIMEOUT" qemu-system-x86_64 \
        -drive "if=pflash,format=raw,readonly=on,file=$OVMF_CODE" \
        -drive "if=pflash,format=raw,file=$WORK/vars-$name.fd" \
        "$@" -snapshot \
        -m 2G -smp 2 "${ACCEL[@]}" -serial "file:$log" -display none -monitor none \
        >/dev/null 2>&1 &
    local qpid=$! ok=0
    for _ in $(seq 1 "$BOOT_TIMEOUT"); do
        if grep -aq "About to start first process" "$log" 2>/dev/null; then ok=1; break; fi
        if grep -aq "KERNEL PANIC" "$log" 2>/dev/null; then break; fi
        kill -0 "$qpid" 2>/dev/null || break
        sleep 1
    done
    kill "$qpid" 2>/dev/null || true
    wait "$qpid" 2>/dev/null || true
    if [[ $ok -ne 1 ]] || ! grep -aq "ext2: mounted /mnt" "$log" || ! grep -aq "ata: GPT\|usb-storage: GPT" "$log"; then
        echo "error: boot test '$name' failed (no first process, or no /mnt from the GPT). Last serial lines (all: $log):" >&2
        tail -25 "$log" >&2 || true
        exit 1
    fi
    echo "Boot test $name: OK: $(grep -a 'ata: GPT\|usb-storage: GPT' "$log" | head -1 | tr -d '\r')"
}

if [[ $TEST -eq 1 ]]; then
    boot_test usb \
        -device qemu-xhci,id=xhci \
        -drive "if=none,id=stick,format=raw,file=$IMG" \
        -device usb-storage,bus=xhci.0,drive=stick,bootindex=0
    boot_test ide \
        -drive "if=none,id=disk,format=raw,file=$IMG" \
        -device ide-hd,bus=ide.1,drive=disk,bootindex=0
fi

# ── Outputs ────────────────────────────────────────────────────────────────
qemu-img convert -f raw -O vmdk "$IMG" "$OUT_DIR/constanos.vmdk"
zstd -q -19 -T0 -f -o "$OUT_DIR/constanos.img.zst" "$IMG"
(cd "$OUT_DIR" && sha256sum constanos.img constanos.img.zst constanos.vmdk > SHA256SUMS)

echo "Image:   $IMG ($(( $(stat -c %s "$IMG") / 1048576 )) MiB, data partition ${DATA_MIB} MiB)"
echo "VMDK:    $OUT_DIR/constanos.vmdk"
echo "Upload:  $OUT_DIR/constanos.img.zst ($(( $(stat -c %s "$OUT_DIR/constanos.img.zst") / 1048576 )) MiB) + SHA256SUMS"
if [[ ${#MISSING[@]} -gt 0 ]]; then
    echo "warning: not on the image:" >&2
    printf '  %s\n' "${MISSING[@]}" >&2
fi
