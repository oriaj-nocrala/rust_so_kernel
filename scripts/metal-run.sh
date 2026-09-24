#!/usr/bin/env bash
# scripts/metal-run.sh — one unattended round trip on the bare-metal machine
# (docs/metal/autonomous-loop-plan.md, phase 5).
#
#   scripts/metal-run.sh [--no-deploy] [--no-reboot] JOB.sh
#       build, deploy kernel + data to the stick, drop JOB.sh as the autorun
#       job, BootNext into the stick and reboot. Linux comes back by itself:
#       the job ends in reboot(2), a panic resets (autorun mode), and BootNext
#       is consumed on use.
#   scripts/metal-run.sh --collect
#       after coming back to Linux: read the log partition, classify the run,
#       remove the job from the stick, archive everything under
#       target/metal/runs/<nonce>/. Exit 0 only for OK.
#   scripts/metal-run.sh --abort
#       forget a pending run that was never booted: remove the job from the
#       stick, clear BootNext, drop target/metal/pending.
#
# Verdicts (the nonce, not "newest slot", decides which boot is this run):
#   OK       METAL-DONE <nonce> exit=0
#   FAIL     METAL-DONE <nonce> with any other status
#   PANIC    METAL-BEGIN without DONE, last flush of that boot was a panic
#   HANG     METAL-BEGIN without DONE, no panic (the log ends where it ends —
#            the idle-task flush may have lost the tail). constanos arms the
#            FCH watchdog in autorun mode, so a hang resets after
#            kernel/src/watchdog.rs's TIMEOUT_SECS; the verdict then carries
#            "[watchdog reset: bootstatus=32]" from Linux's sp5100_tco.
#   NO-JOB   a boot newer than the deploy exists but never printed the nonce
#            (died before PID 1 reached the job; its log is saved)
#   NO-BOOT  no boot newer than the deploy on the log partition at all
# READ THE SAVED LOG BEFORE BELIEVING THE VERDICT (boot-matrix.sh's rule).
#
# --no-deploy   skip cargo build + deploy-usb-boot.sh + sync-usb-data.sh
#               (the job still goes onto the stick)
# --no-reboot   do everything except BootNext + reboot (dry run of the
#               host side; follow with --abort)
#
# Deploy is skipped by itself when the kernel ELF hashes the same as at the
# last deploy (each deploy rewrites ~6 MB of the stick's FAT).
#
# Every stick partition is found by GPT label, never by device node (see
# sync-usb-data.sh). The UEFI entry for the stick is dynamic on this ASUS
# board, so it is looked up on every run by the PARTUUID of `boot`.
#
# Needs root for mount/efibootmgr --bootnext/dd; uses sudo non-interactively
# and fails up front if sudo would prompt.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

BOARD="PRIME B450M-A II"   # the Ryzen this loop was measured on
STATE="$REPO_ROOT/target/metal"
PENDING="$STATE/pending"
DATA_DEV="/dev/disk/by-partlabel/constanos-data"
BOOT_DEV="/dev/disk/by-partlabel/boot"
LOG_DEV="/dev/disk/by-partlabel/constanos-log"
KERNEL_ELF="$REPO_ROOT/kernel/target/x86_64-unknown-none/debug/kernel"

die() { echo "metal-run: error: $*" >&2; exit 1; }
say() { echo "metal-run: $*"; }

usage() { sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }

# ── Precondition checks ─────────────────────────────────────────────────
check_machine() {
    local board
    board="$(cat /sys/class/dmi/id/board_name 2>/dev/null || true)"
    [[ "$board" == "$BOARD" ]] || die "board is '$board', not '$BOARD' — this reboots the machine it runs on"
    for dev in "$BOOT_DEV" "$DATA_DEV" "$LOG_DEV"; do
        [[ -e "$dev" ]] || die "$dev missing — is the stick plugged in? (lsblk -o NAME,SIZE,PARTLABEL)"
    done
    command -v efibootmgr >/dev/null || die "efibootmgr not installed"
    sudo -n true 2>/dev/null || die "sudo needs a password; run 'sudo -v' first (or see the plan's Permissions note)"
}

# Highest boot number on the log partition (0 if none yet).
last_boot_seq() {
    { scripts/usb-log.sh list 2>/dev/null || true; } \
        | sed -n 's/^boot #\([0-9]*\) .*/\1/p' | sort -n | tail -1 | grep . || echo 0
}

# The stick's UEFI entry: the Boot#### whose device path names boot's PARTUUID.
usb_boot_entry() {
    local partuuid
    partuuid="$(lsblk -no PARTUUID "$(readlink -f "$BOOT_DEV")")"
    [[ -n "$partuuid" ]] || die "cannot read the PARTUUID of $BOOT_DEV"
    local entries
    entries="$(efibootmgr | grep -i "GPT,$partuuid," | sed -n 's/^Boot\([0-9A-Fa-f]\{4\}\).*/\1/p')"
    [[ -n "$entries" ]] || die "no UEFI boot entry points at PARTUUID $partuuid (efibootmgr lists none)"
    [[ "$(wc -l <<<"$entries")" -eq 1 ]] || die "more than one UEFI entry points at $partuuid: $entries"
    echo "$entries"
}

# Runs "$@" with the data partition mounted read-write at $MNT.
with_data_mounted() {
    local real
    real="$(readlink -f "$DATA_DEV")"
    if findmnt -n -S "$real" >/dev/null; then die "$real is already mounted — unmount it first"; fi
    MNT="$(mktemp -d)"
    sudo mount -t ext2 "$real" "$MNT"
    local rc=0
    "$@" || rc=$?
    sudo umount "$MNT"
    rmdir "$MNT"
    # Same rule as sync-usb-data.sh: never hand the kernel a filesystem
    # that is already inconsistent.
    local fsck
    fsck="$(sudo e2fsck -fn "$real" 2>&1)" || { echo "$fsck" >&2; die "e2fsck reports $real inconsistent after touching autorun/"; }
    return $rc
}

put_job() {
    [[ ! -e "$MNT/autorun" ]] || { echo "metal-run: error: the stick already carries autorun/ — a run is pending (--collect or --abort)" >&2; return 1; }
    sudo mkdir "$MNT/autorun"
    sudo install -m 0644 "$JOB" "$MNT/autorun/job"
    echo "$NONCE" | sudo tee "$MNT/autorun/nonce" >/dev/null
    sudo sync
}

remove_job() {
    sudo rm -rf "$MNT/autorun"
    sudo sync
}

# ── JOB.sh ──────────────────────────────────────────────────────────────
cmd_run() {
    local deploy=1 reboot=1
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --no-deploy) deploy=0; shift ;;
            --no-reboot) reboot=0; shift ;;
            -*) usage ;;
            *) break ;;
        esac
    done
    [[ $# -eq 1 ]] || usage
    JOB="$(readlink -f "$1")"
    [[ -f "$JOB" ]] || die "$1: no such file"

    check_machine
    [[ ! -e "$PENDING" ]] || die "a run is already pending ($(grep ^nonce= "$PENDING")) — --collect or --abort it first"
    local entry
    entry="$(usb_boot_entry)"
    mkdir -p "$STATE"

    if [[ $deploy -eq 1 ]]; then
        say "building..."
        cargo build 2>&1 | tail -3
        local hash
        hash="$(sha256sum "$KERNEL_ELF" | cut -d' ' -f1)"
        if [[ "$hash" == "$(cat "$STATE/deployed-kernel.sha256" 2>/dev/null || true)" ]]; then
            say "kernel unchanged since the last deploy — not rewriting the boot partition"
        else
            scripts/deploy-usb-boot.sh
            echo "$hash" > "$STATE/deployed-kernel.sha256"
        fi
        # rsync --delete: must run before the job goes on, or it removes it.
        scripts/sync-usb-data.sh >/dev/null
        say "data partition synced"
    fi

    NONCE="$(date +%Y%m%d-%H%M%S)-$(od -An -N3 -tx1 /dev/urandom | tr -d ' \n')"
    local prev_seq
    prev_seq="$(last_boot_seq)"
    with_data_mounted put_job
    {
        echo "nonce=$NONCE"
        echo "created=$(date -Is)"
        echo "commit=$(git rev-parse --short HEAD)$(git diff --quiet HEAD -- kernel userspace hal ext2 mm vfs diag sched usock || echo -dirty)"
        echo "job=$JOB"
        echo "prev_boot_seq=$prev_seq"
        echo "usb_entry=$entry"
    } > "$PENDING"
    cp "$JOB" "$STATE/pending.job"
    say "job on the stick, nonce $NONCE (log partition's last boot: #$prev_seq)"

    if [[ $reboot -eq 0 ]]; then
        say "--no-reboot: stopping here (undo with --abort)"
        return
    fi
    sudo efibootmgr --bootnext "$entry" >/dev/null
    say "BootNext=$entry; rebooting. Afterwards: scripts/metal-run.sh --collect"
    sync
    sudo systemctl reboot
}

# Splits $1/all-boots.log per boot, picks this run's by nonce ($2) among the
# boots numbered above $3, and writes $1/boot.log + $1/verdict.
classify() {
    python3 - "$1" "$2" "$3" <<'EOF'
import re, sys
run, nonce, prev = sys.argv[1], sys.argv[2], int(sys.argv[3])
text = open(run + "/all-boots.log", encoding="utf-8", errors="replace").read()
head = re.compile(r"^===== boot #(\d+) \(slot \d+\): \d+ flush\(es\), last (\S+) .*=====$", re.M)
boots, marks = [], list(head.finditer(text))
for i, m in enumerate(marks):
    end = marks[i + 1].start() if i + 1 < len(marks) else len(text)
    boots.append((int(m.group(1)), m.group(2), text[m.start():end]))
new = [b for b in boots if b[0] > prev]
mine = [b for b in new if ("METAL-BEGIN " + nonce) in b[2]]
detail = ""
if mine:
    seq, reason, log = mine[-1]
    done = re.search(r"METAL-DONE " + re.escape(nonce) + r" (\S+)", log)
    if done:
        verdict = "OK" if done.group(1) == "exit=0" else "FAIL"
        detail = done.group(1)
    elif reason == "PANIC" or "KERNEL PANIC" in log:
        verdict = "PANIC"
    else:
        verdict = "HANG"
        detail = "last flush: " + reason
elif new:
    seq, reason, log = new[-1]
    verdict, detail = "NO-JOB", "boot #%d never printed the nonce; last flush: %s" % (seq, reason)
else:
    seq, log = None, ""
    verdict = "NO-BOOT"
    detail = "no boot newer than #%d on the log partition" % prev
if log:
    open(run + "/boot.log", "w").write(log)
line = verdict + (" " + detail if detail else "") + ("" if seq is None else " (boot #%d)" % seq)
if len(new) > 1:
    line += " [%d new boots since deploy: %s]" % (len(new), ", ".join("#%d" % b[0] for b in new))
open(run + "/verdict", "w").write(line + "\n")
EOF
}

# ── --collect ───────────────────────────────────────────────────────────
cmd_collect() {
    check_machine
    [[ -e "$PENDING" ]] || die "no pending run (target/metal/pending)"
    local nonce prev_seq
    nonce="$(sed -n 's/^nonce=//p' "$PENDING")"
    prev_seq="$(sed -n 's/^prev_boot_seq=//p' "$PENDING")"
    local run="$STATE/runs/$nonce"
    mkdir -p "$run"

    scripts/usb-log.sh read --all > "$run/all-boots.log" || die "reading the log partition failed"
    classify "$run" "$nonce" "$prev_seq"
    # Linux's sp5100_tco driver reads the TCO's WatchDogFired bit at load:
    # bootstatus 32 (WDIOF_CARDRESET) means the reset that brought us back
    # was the watchdog constanos armed (kernel/src/watchdog.rs).
    local bs
    bs="$(cat /sys/class/watchdog/watchdog0/bootstatus 2>/dev/null || echo "?")"
    echo "$bs" > "$run/watchdog-bootstatus"
    if [[ "$bs" != "0" && "$bs" != "?" ]]; then
        sed -i "1s/\$/ [watchdog reset: bootstatus=$bs]/" "$run/verdict"
    fi

    # Remove the job whatever the verdict, so the next manual boot of the
    # stick is not an unattended run.
    with_data_mounted remove_job
    [[ "$(efibootmgr | sed -n 's/^BootNext: //p')" == "" ]] || sudo efibootmgr --delete-bootnext >/dev/null
    mv "$PENDING" "$run/pending"
    mv "$STATE/pending.job" "$run/job.sh"

    local verdict
    verdict="$(cat "$run/verdict")"
    say "$verdict"
    say "saved in ${run#$REPO_ROOT/} (boot.log, all-boots.log, job.sh, pending)"
    if [[ -f "$run/boot.log" ]]; then
        # Only what user programs wrote to the console ([fb] lines); the
        # kernel's own traces stay in boot.log.
        echo "---- job output (console only; kernel traces in boot.log) ----"
        sed -n "/METAL-BEGIN $nonce/,\$p" "$run/boot.log" | grep -a '^\[fb\] ' | sed 's/^\[fb\] //' | tail -40
    fi
    [[ "$verdict" == OK* ]]
}

# ── --abort ─────────────────────────────────────────────────────────────
cmd_abort() {
    check_machine
    with_data_mounted remove_job
    [[ "$(efibootmgr | sed -n 's/^BootNext: //p')" == "" ]] || sudo efibootmgr --delete-bootnext >/dev/null
    rm -f "$PENDING" "$STATE/pending.job"
    say "pending run discarded; stick has no autorun/, BootNext cleared"
}

case "${1:-}" in
    --collect) shift; cmd_collect "$@" ;;
    --abort) shift; cmd_abort "$@" ;;
    --classify) shift; classify "$@"; cat "$1/verdict" ;;   # testing: RUN_DIR NONCE PREV_SEQ
    ""|-h|--help) usage ;;
    *) cmd_run "$@" ;;
esac
