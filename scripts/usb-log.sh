#!/usr/bin/env bash
# scripts/usb-log.sh — the kernel log partition (`constanos-log`) on the
# boot pendrive, from the host side.
#
# The physical machine has no serial capture. The kernel copies its log
# ring (`kernel/src/klog.rs`, everything serial_println! prints plus every
# byte user programs write to the screen) onto a raw partition of the stick
# — every 5 s from the idle task, on `kdebug sync`, and from the panic
# handler (`kernel/src/block/logpart.rs`). Reboot into Linux and read it
# here. Format: `hal/src/logpart.rs` (host-tested); the reader below mirrors
# its `Header::decode` and `linearize`.
#
#   scripts/usb-log.sh read [--all | --boot N] [TARGET]   newest boot's log (or all, or boot #N)
#   scripts/usb-log.sh list [TARGET]                      one line per boot kept on the stick
#   scripts/usb-log.sh init [TARGET]                      write the format marker (once)
#   scripts/usb-log.sh mkpart DISK [SIZE]                 append the partition to DISK (default 64MiB)
#   scripts/usb-log.sh mkimage OUT.img                    QEMU test stick: boot + data (disk.img) + log
#
# TARGET is the log partition: by default /dev/disk/by-partlabel/constanos-log
# (found by label, never by device node — see sync-usb-data.sh for why), or
# `--image FILE` for a whole-disk image, whose GPT is searched for the
# partition by name (what QEMU_USB_STORAGE boots from).
#
# THE KERNEL ONLY WRITES A PARTITION THAT `init` HAS MARKED. Being named
# constanos-log is not enough: sector 0 must hold the marker, so a mislabeled
# partition is never scribbled on. `init` itself refuses a partition that
# carries any filesystem signature blkid recognises.
#
# Partition type is "Linux reserved" (8DA63339-0007-60C0-C436-083AC8230908),
# not "Linux filesystem": nothing auto-mounts it, and the kernel's fallback
# lookup for the data partition ("the only Linux filesystem partition") is
# left unambiguous.

set -euo pipefail

PARTLABEL="constanos-log"
DEFAULT_DEV="/dev/disk/by-partlabel/$PARTLABEL"
TYPE_LINUX_RESERVED="8DA63339-0007-60C0-C436-083AC8230908"
TYPE_EFI="C12A7328-F81F-11D2-BA4B-00A0C93EC93B"
TYPE_LINUX_FS="0FC63DAF-8483-4772-8E79-3D69D8477DE4"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Must match hal/src/logpart.rs.
STRIDE=256        # sectors between slots
MAX_SLOTS=16
MARKER='CONSTANOS-KLOG-PARTITION v1\n'
# Everything the reader can ever need: the marker stride plus MAX_SLOTS slots.
SPAN_SECTORS=$(( STRIDE * (MAX_SLOTS + 1) ))

die() { echo "error: $*" >&2; exit 1; }

# ── Target resolution ───────────────────────────────────────────────────────
# Sets FILE (what to dd from/to), OFFSET (sectors into FILE), SECTORS
# (partition size), SUDO (prefix for a block device we can't open directly).
resolve_target() {
    SUDO=""
    if [[ "${1:-}" == "--image" ]]; then
        local img="${2:?--image needs a file}"
        [[ -f "$img" ]] || die "$img: no such file"
        local found
        found="$(sfdisk --json "$img" | python3 -c '
import json, sys
t = json.load(sys.stdin)["partitiontable"]
for p in t.get("partitions", []):
    if p.get("name") == sys.argv[1]:
        print(p["start"], p["size"]); break
' "$PARTLABEL")"
        [[ -n "$found" ]] || die "$img has no partition named '$PARTLABEL'"
        FILE="$img"; OFFSET="${found% *}"; SECTORS="${found#* }"
        return
    fi
    local dev="${1:-$DEFAULT_DEV}"
    [[ -e "$dev" ]] || die "no partition labelled '$PARTLABEL' ($dev missing).
       Plug the stick in, check lsblk -o NAME,SIZE,PARTLABEL, or create it with: $0 mkpart /dev/sdX"
    FILE="$(readlink -f "$dev")"; OFFSET=0
    [[ -r "$FILE" && -w "$FILE" ]] || SUDO="sudo"
    SECTORS=$(( $($SUDO blockdev --getsz "$FILE") ))
}

read_span() {
    local n=$(( SECTORS < SPAN_SECTORS ? SECTORS : SPAN_SECTORS ))
    $SUDO dd if="$FILE" bs=512 skip="$OFFSET" count="$n" status=none
}

# ── read / list ─────────────────────────────────────────────────────────────
cmd_read() {
    local mode="newest" boot=""
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --all) mode="all"; shift ;;
            --boot) mode="boot"; boot="${2:?--boot needs a number}"; shift 2 ;;
            --list) mode="list"; shift ;;
            *) break ;;
        esac
    done
    resolve_target "$@"
    read_span | python3 -c '
import sys, zlib, struct, datetime

STRIDE, MAX_SLOTS = 256, 16
MARKER = b"CONSTANOS-KLOG-PARTITION v1\n"
REASONS = {1: "periodic", 2: "sync", 3: "PANIC"}
mode, want = sys.argv[1], sys.argv[2]
data = sys.stdin.buffer.read()

if data[:512] != MARKER + bytes(512 - len(MARKER)):
    sys.exit("error: partition is not formatted (run: scripts/usb-log.sh init)")

def decode(s):  # mirrors hal::logpart::Header::decode
    if len(s) < 512 or s[0:8] != b"KLOGSLOT":
        return None
    if struct.unpack_from("<I", s, 56)[0] != zlib.crc32(s[:56]) or struct.unpack_from("<I", s, 8)[0] != 1:
        return None
    ring, seq, pos, up, unix, flushes = struct.unpack_from("<IQQQQI", s, 12)
    if ring == 0 or ring > (STRIDE - 1) * 512 or ring % 512 or s[52] not in REASONS:
        return None
    return dict(ring=ring, seq=seq, pos=pos, up=up, unix=unix, flushes=flushes, reason=REASONS[s[52]])

def linearize(ring, pos):  # mirrors hal::logpart::linearize
    if pos <= len(ring):
        return ring[:pos]
    k = pos % len(ring)
    return ring[k:] + ring[:k]

boots = []
for i in range(MAX_SLOTS):
    base = (i + 1) * STRIDE * 512
    h = decode(data[base:base + 512])
    if h:
        h["slot"] = i
        h["log"] = linearize(data[base + 512:base + 512 + h["ring"]], h["pos"])
        boots.append(h)
boots.sort(key=lambda h: h["seq"])
if not boots:
    sys.exit("no boots logged yet")

def title(h):
    when = datetime.datetime.fromtimestamp(h["unix"]).strftime("%Y-%m-%d %H:%M:%S") if h["unix"] > 10**9 else "?"
    wrapped = ", wrapped: oldest lines lost" if h["pos"] > h["ring"] else ""
    return "boot #%d (slot %d): %d flush(es), last %s at uptime %.1fs, RTC %s, %d bytes%s" % (
        h["seq"], h["slot"], h["flushes"], h["reason"], h["up"] / 1e9, when, len(h["log"]), wrapped)

if mode == "list":
    for h in boots:
        print(title(h))
    sys.exit(0)
if mode == "newest":
    boots = boots[-1:]
elif mode == "boot":
    boots = [h for h in boots if h["seq"] == int(want)]
    if not boots:
        sys.exit("no boot #%s on the stick (try: scripts/usb-log.sh list)" % want)
out = sys.stdout
for h in boots:
    out.write("===== %s =====\n" % title(h))
    out.write(h["log"].decode("utf-8", "replace"))
    if not h["log"].endswith(b"\n"):
        out.write("\n")
' "$mode" "$boot"
}

# ── init ────────────────────────────────────────────────────────────────────
cmd_init() {
    resolve_target "$@"
    (( SECTORS >= STRIDE * 2 )) || die "partition too small: $SECTORS sectors, need at least $(( STRIDE * 2 ))"
    if [[ "$OFFSET" == 0 ]]; then
        local sig
        sig="$($SUDO blkid -p -o value -s TYPE "$FILE" 2>/dev/null || true)"
        [[ -z "$sig" ]] || die "$FILE carries a '$sig' signature — not touching it"
        if findmnt -rn -S "$FILE" >/dev/null; then die "$FILE is mounted"; fi
    fi
    local n=$(( SECTORS < SPAN_SECTORS ? SECTORS : SPAN_SECTORS ))
    echo "Marking $FILE (offset $OFFSET, $SECTORS sectors) as the kernel log partition; clearing $n sectors."
    # Zero the marker stride and every slot header first, so no stale bytes
    # can ever decode as a boot, then write the marker.
    $SUDO dd if=/dev/zero of="$FILE" bs=512 seek="$OFFSET" count="$n" conv=notrunc status=none
    printf "$MARKER" | $SUDO dd of="$FILE" bs=512 seek="$OFFSET" conv=notrunc,sync status=none
    sync
    echo "Done. The next boot of the kernel from this stick logs to it."
}

# ── mkpart ──────────────────────────────────────────────────────────────────
cmd_mkpart() {
    local disk="${1:?usage: $0 mkpart DISK [SIZE]}" size="${2:-64MiB}"
    [[ -e "$disk" ]] || die "$disk: no such device or file"
    local sudo=""
    [[ -w "$disk" ]] || sudo="sudo"
    if $sudo sfdisk --json "$disk" | grep -q "\"name\":\"$PARTLABEL\""; then
        die "$disk already has a '$PARTLABEL' partition"
    fi
    if [[ -b "$disk" ]]; then
        [[ "$(lsblk -dno TYPE "$disk")" == "disk" ]] || die "$disk is not a whole disk (give /dev/sdX, not a partition)"
        if lsblk -no MOUNTPOINTS "$disk" | grep -q .; then die "a partition of $disk is mounted — unmount it first"; fi
    fi
    local backup="$HOME/constanos-gpt-backup-$(date +%Y%m%d-%H%M%S).sfdisk"
    $sudo sfdisk --dump "$disk" > "$backup"
    echo "Current table of $disk (backed up to $backup — restore with: sfdisk $disk < $backup):"
    $sudo sfdisk --list "$disk" | sed 's/^/    /'
    echo
    echo "Will APPEND one partition: size=$size, type=Linux reserved, name=$PARTLABEL."
    echo "Existing partitions are not moved or resized."
    if [[ -b "$disk" ]]; then
        read -r -p "Type 'yes' to write the new table to $disk: " answer
        [[ "$answer" == "yes" ]] || die "aborted, nothing written"
    fi
    echo "size=$size, type=$TYPE_LINUX_RESERVED, name=$PARTLABEL" | $sudo sfdisk --append --no-reread "$disk"
    if [[ -b "$disk" ]]; then
        $sudo partprobe "$disk" 2>/dev/null || $sudo blockdev --rereadpt "$disk" || true
        $sudo udevadm settle || true
        echo "Now run: $0 init"
    fi
}

# ── mkimage ─────────────────────────────────────────────────────────────────
# A stick image for QEMU (QEMU_USB_STORAGE=OUT QEMU_DEBUG_NO_DISK=1): the real
# stick's three-partition shape, with this tree's disk.img as the data
# partition and a formatted log partition.
cmd_mkimage() {
    local out="${1:?usage: $0 mkimage OUT.img}"
    local data="$REPO_ROOT/disk.img"
    [[ -f "$data" ]] || die "$data missing — run a build first"
    local data_sectors=$(( $(stat -c %s "$data") / 512 ))
    local log_sectors=$(( 64 * 2048 ))
    local data_start=36864   # boot: 34 + 34816 sectors, like the real stick; then 1 MiB-aligned
    local log_start=$(( (data_start + data_sectors + 2047) / 2048 * 2048 ))
    local total=$(( log_start + log_sectors + 2048 ))
    rm -f "$out"
    truncate -s $(( total * 512 )) "$out"
    sfdisk --quiet "$out" <<EOF
label: gpt
first-lba: 34
start=34, size=34816, type=$TYPE_EFI, name=boot
start=$data_start, size=$data_sectors, type=$TYPE_LINUX_FS, name=constanos-data
start=$log_start, size=$log_sectors, type=$TYPE_LINUX_RESERVED, name=$PARTLABEL
EOF
    dd if="$data" of="$out" bs=1M seek=$(( data_start * 512 )) oflag=seek_bytes conv=notrunc,sparse status=none
    cmd_init --image "$out" >/dev/null
    echo "$out: boot + constanos-data (disk.img) + $PARTLABEL (formatted)"
}

case "${1:-}" in
    read) shift; cmd_read "$@" ;;
    list) shift; cmd_read --list "$@" ;;
    init) shift; cmd_init "$@" ;;
    mkpart) shift; cmd_mkpart "$@" ;;
    mkimage) shift; cmd_mkimage "$@" ;;
    *) sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; exit 1 ;;
esac
