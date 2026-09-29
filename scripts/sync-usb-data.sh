#!/usr/bin/env bash
# scripts/sync-usb-data.sh [--dry-run]
#
# Refreshes the ext2 data partition on the bare-metal USB stick from this
# tree's `disk-image-root/`, the same directory `build.rs` seeds `disk.img`
# from. Run it after any build that rewrote `disk-image-root/bin/`
# (kernel/build.rs rebuilds DISK_C_PROGRAMS, doom and quake on every
# build) so the physical machine runs the binaries you just compiled.
#
# WHY A SCRIPT AND NOT build.rs: `disk.img`'s sync happens inside the
# build (`sync_disk_bin_dir`, using `debugfs -w` on a plain file that
# needs no privileges). This one writes to a real block device, needs
# `sudo mount`, and depends on a particular USB stick being plugged in —
# none of which belongs in a build script that must work unattended and
# on machines where the stick isn't present. Keeping it manual also means
# the stick is only ever written when you mean to write it.
#
# THE PARTITION IS FOUND BY LABEL, NOT BY DEVICE NODE. The stick showed up
# as /dev/sdb on the machine that created it, but a device node is a
# function of enumeration order — plug in a second disk, or boot with the
# stick already inserted, and today's /dev/sdb is tomorrow's /dev/sdc.
# Writing 54 MB over the wrong device node is not a recoverable mistake,
# so the lookup is by the GPT partition label `constanos-data`, which
# travels with the partition itself. A missing label is a hard error, not
# a fallback to guessing.
#
# The partition was created (once) with the same parameters build.rs uses
# for disk.img, so the kernel's minimal ext2 reader accepts it unchanged:
#
#   mke2fs -q -t ext2 -b 1024 -O ^resize_inode,^dir_index -L constanos \
#          -d disk-image-root /dev/disk/by-partlabel/constanos-data
#
# (-O ^resize_inode,^dir_index keeps s_feature_incompat at FILETYPE alone,
# which is the only incompat bit ext2/src/superblock.rs accepts.)

set -euo pipefail

PARTLABEL="${CONSTANOS_USB_PARTLABEL:-constanos-data}"
DEV="/dev/disk/by-partlabel/$PARTLABEL"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="$REPO_ROOT/disk-image-root"

DRY_RUN=0
[[ "${1:-}" == "--dry-run" ]] && DRY_RUN=1

if [[ ! -d "$SRC" ]]; then
    echo "error: $SRC does not exist — run a build first." >&2
    exit 1
fi

if [[ ! -e "$DEV" ]]; then
    echo "error: no partition labelled '$PARTLABEL' is present." >&2
    echo "       Plug the USB stick in, or check: lsblk -o NAME,SIZE,PARTLABEL" >&2
    exit 1
fi

REAL_DEV="$(readlink -f "$DEV")"

# Refuse to touch a partition that is already mounted somewhere else —
# rsync --delete into a mountpoint someone is using is exactly the
# accident this check exists to prevent.
if findmnt -n -S "$REAL_DEV" >/dev/null 2>&1; then
    echo "error: $REAL_DEV is already mounted at $(findmnt -n -o TARGET -S "$REAL_DEV")." >&2
    echo "       Unmount it first." >&2
    exit 1
fi

FSTYPE="$(lsblk -no FSTYPE "$REAL_DEV")"
if [[ "$FSTYPE" != "ext2" ]]; then
    echo "error: $REAL_DEV is '$FSTYPE', not ext2 — refusing to write." >&2
    exit 1
fi

echo "Source:    $SRC ($(du -sh "$SRC" | cut -f1))"
echo "Target:    $REAL_DEV (label '$PARTLABEL')"

MNT="$(mktemp -d)"
cleanup() {
    if mountpoint -q "$MNT"; then
        sudo umount "$MNT" || true
    fi
    rmdir "$MNT" 2>/dev/null || true
}
trap cleanup EXIT

sudo mount -t ext2 "$REAL_DEV" "$MNT"

# --delete so a binary removed from disk-image-root/ stops being on the
# stick too; --exclude lost+found because it is the filesystem's, not the
# source tree's, and deleting it makes e2fsck recreate it later anyway.
# --itemize-changes: one line per file that differs, so --dry-run actually
# says what it would write (with only --info=stats1 it printed byte totals
# and no file names, whatever was about to change).
# The GSP firmware (63 MB) does not fit disk.img, so it is not in
# disk-image-root/: it is added below from the host's copy and protected here
# from --delete (`P` = protect).
GSP_REL="lib/firmware/nvidia/ga106/gsp/gsp-570.144.bin"
GSP_ZST="/usr/lib/firmware/nvidia/ga106/gsp/gsp-570.144.bin.zst"
GSP_CACHE="$REPO_ROOT/target/firmware/gsp-570.144.bin"
RSYNC_ARGS=(-a --delete --exclude 'lost+found' --filter "P /$GSP_REL" --info=stats1 --itemize-changes)
[[ $DRY_RUN -eq 1 ]] && RSYNC_ARGS+=(--dry-run)

sudo rsync "${RSYNC_ARGS[@]}" "$SRC"/ "$MNT"/

if [[ $DRY_RUN -eq 1 ]]; then
    echo "(dry run — nothing written)"
    exit 0
fi

# GPU phase 4: gsp-570.144.bin (decompressed once into target/firmware/).
# rsync compares size and mtime, so an unchanged file is not rewritten.
if [[ -f "$GSP_ZST" ]]; then
    if [[ ! -f "$GSP_CACHE" ]]; then
        mkdir -p "$(dirname "$GSP_CACHE")"
        zstd -q -d -f -o "$GSP_CACHE" "$GSP_ZST"
    fi
    sudo mkdir -p "$MNT/$(dirname "$GSP_REL")"
    sudo rsync -a --inplace --itemize-changes "$GSP_CACHE" "$MNT/$GSP_REL"
else
    echo "note: $GSP_ZST not on the host: gpu=gsp will find no gsp-570.144.bin on the stick" >&2
fi

sudo sync
echo "Contents:"
sudo ls -1 "$MNT" | sed 's/^/  /'
echo "Free:      $(df -h "$MNT" | tail -1 | awk '{print $4" of "$2}')"

cleanup
trap - EXIT

# The kernel mounts this read-write and runs its own repair passes
# (reconcile_free_counts / reclaim_orphans) at every boot. Handing it a
# filesystem that is already inconsistent would make any later problem
# impossible to attribute, so the sync ends by proving this one is clean.
echo "Checking filesystem..."
sudo e2fsck -fn "$REAL_DEV"
