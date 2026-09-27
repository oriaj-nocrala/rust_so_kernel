#!/usr/bin/env bash
# scripts/gpu-oracle.sh — phase 0 of docs/gpu/gpu-plan.md: capture, from the
# host Linux of the Ryzen itself, what the GPU driver must reproduce.
#
#   sudo scripts/gpu-oracle.sh static        any boot: VBIOS, EDIDs, lspci, versions
#   sudo scripts/gpu-oracle.sh entry MODE    add a one-shot systemd-boot entry to trace MODE
#   sudo scripts/gpu-oracle.sh trace MODE    in the trace boot: mmiotrace nouveau (MODE nogsp|gsp),
#                                            or gspnotrace: GSP, no mmiotrace, dmesg only (D6)
#   sudo scripts/gpu-oracle.sh install       unattended: a service that runs `auto` in the trace boot
#   sudo scripts/gpu-oracle.sh auto          (the service) trace the boot's MODE, then reboot:
#                                            nogsp → arms gsp; gsp → back to the normal entry
#   sudo scripts/gpu-oracle.sh uninstall     remove the service
#        scripts/gpu-oracle.sh summary       what was captured, and the GSP firmware version (D1)
#
# One reboot per trace, so both start from the firmware's GOP state (the
# same state constanos sees). Unattended:
#   static → install → entry nogsp → reboot   (two more reboots happen alone)
# The entry is one-shot: if a trace boot hangs, a power cycle returns to the
# normal entry.
#
# Output: $ORACLE (default ~/constanos-gpu-oracle of the invoking user),
# outside git: the traces are large and the VBIOS is not ours to publish.
#
# The trace entry: same kernel and initrd as the current entry, with nvidia
# hard-blocked (module_blacklist, which also stops initramfs loads), nouveau
# only kept from auto-loading (modprobe.blacklist, so `modprobe nouveau` by
# name still works), no graphical target, and a 64 MiB printk ring for the
# RPC hex dumps of debug=gsp=trace.
#
# mmiotrace takes all CPUs but one offline while it runs; that is expected.
# Reads in the trace carry their values, so the first read of each register
# in the nogsp trace IS the firmware's state: no separate BAR0 snapshot.
set -euo pipefail

GPU_BDF="0000:09:00.0"
GPU_SYS="/sys/bus/pci/devices/$GPU_BDF"
ESP="${ESP:-/efi}"
ENTRY_ID="constanos-gpu-trace.conf"
TRACEFS="/sys/kernel/tracing"
SETTLE_SECS="${SETTLE_SECS:-25}"

owner="${SUDO_USER:-$USER}"
owner_home="$(getent passwd "$owner" | cut -d: -f6)"
ORACLE="${ORACLE:-$owner_home/constanos-gpu-oracle}"

die() { echo "gpu-oracle: $*" >&2; exit 1; }
need_root() { [[ $EUID -eq 0 ]] || die "run it with sudo"; }
finish() { chown -R "$owner:" "$ORACLE" 2>/dev/null || true; }

read_rom() { # $1 = output file; the PCI ROM BAR, as the firmware left it
    echo 1 > "$GPU_SYS/rom"
    if cat "$GPU_SYS/rom" > "$1" 2>/dev/null && [[ -s $1 ]]; then
        echo "  vbios: $(stat -c %s "$1") bytes -> $1"
    else
        rm -f "$1"; echo "  vbios: the ROM BAR was not readable (driver bound?)"
    fi
    echo 0 > "$GPU_SYS/rom"
}

copy_edid() { # $1 = connector dir, $2 = output; sysfs reports size 0, so test after copying
    cat "$1/edid" > "$2" 2>/dev/null || true
    [[ -s $2 ]] || { rm -f "$2"; return 1; }
}

cmd_static() {
    need_root
    local out="$ORACLE/static"; mkdir -p "$out"
    echo "static -> $out"
    read_rom "$out/vbios-rom.bin"
    lspci -vvv -xxxx -s "${GPU_BDF#0000:}" > "$out/lspci.txt"
    lspci -nn > "$out/lspci-all.txt"
    for c in /sys/class/drm/card*-*; do
        [[ -f $c/edid ]] || continue
        local name="${c##*/}"; name="${name#card*-}"
        copy_edid "$c" "$out/edid-$name.bin" && edid-decode "$out/edid-$name.bin" > "$out/edid-$name.txt" 2>&1 || true
        { echo "status: $(cat "$c/status")"; cat "$c/modes"; } > "$out/modes-$name.txt"
    done
    nvidia-smi -q > "$out/nvidia-smi.txt" 2>&1 || true
    { uname -a; cat /proc/cmdline; modinfo -F version nvidia 2>/dev/null || true;
      pacman -Q linux-firmware-nvidia 2>/dev/null || true; } > "$out/versions.txt"
    ls -l "$out"
    finish
}

cmd_entry() {
    need_root
    local mode="${1:-}"
    [[ $mode =~ ^(nogsp|gsp|gspnotrace)$ ]] || die "entry nogsp|gsp|gspnotrace"
    local cur; cur="$(bootctl status 2>/dev/null | sed -n 's/^ *Current Entry: *//p')"
    [[ -n $cur ]] || die "bootctl did not report the current entry"
    [[ $cur == "$ENTRY_ID" ]] && cur="$(cat "$ESP/loader/entries/.constanos-gpu-trace-base")"
    local src="$ESP/loader/entries/$cur"
    [[ -f $src ]] || die "no $src"
    local opts; opts="$(sed -n 's/^options[[:space:]]*//p' "$src")"
    # Drop what hides the GPU from nouveau or hides the log.
    opts="$(tr ' ' '\n' <<<"$opts" | grep -vE \
        '^(quiet|loglevel=.*|nvidia_drm\.modeset=.*|initcall_blacklist=.*|modprobe\.blacklist=.*|module_blacklist=.*)$' \
        | tr '\n' ' ')"
    opts+="module_blacklist=nvidia,nvidia_drm,nvidia_modeset,nvidia_uvm "
    opts+="modprobe.blacklist=nouveau systemd.unit=multi-user.target log_buf_len=64M "
    opts+="constanos.gputrace=$mode"
    {
        echo "title   constanos GPU trace $mode (nouveau, no nvidia)"
        grep -E '^(linux|initrd|machine-id|sort-key)' "$src"
        echo "options $opts"
    } > "$ESP/loader/entries/$ENTRY_ID"
    echo "$cur" > "$ESP/loader/entries/.constanos-gpu-trace-base"
    bootctl set-oneshot "$ENTRY_ID"
    echo "wrote $ESP/loader/entries/$ENTRY_ID (one-shot, next boot only):"
    cat "$ESP/loader/entries/$ENTRY_ID"
    echo; echo "Reboot. With 'install' done the trace runs alone; otherwise: sudo $0 trace $mode"
}

SERVICE=/etc/systemd/system/constanos-gpu-trace.service

cmd_install() {
    need_root
    cat > "$SERVICE" <<UNIT
[Unit]
Description=constanos GPU oracle trace (docs/gpu/gpu-plan.md, phase 0)
ConditionKernelCommandLine=constanos.gputrace
After=local-fs.target systemd-modules-load.service
Wants=local-fs.target

[Service]
Type=oneshot
Environment=ORACLE=$ORACLE SUDO_USER=$owner
ExecStart=$(realpath "$0") auto
TimeoutStartSec=600
StandardOutput=journal+console

[Install]
WantedBy=multi-user.target
UNIT
    systemctl daemon-reload
    systemctl enable constanos-gpu-trace.service
    echo "installed $SERVICE (runs only when the cmdline has constanos.gputrace=)"
}

cmd_uninstall() {
    need_root
    systemctl disable constanos-gpu-trace.service 2>/dev/null || true
    rm -f "$SERVICE"; systemctl daemon-reload
    echo "removed $SERVICE"
}

cmd_auto() {
    need_root
    local mode; mode="$(tr ' ' '\n' < /proc/cmdline | sed -n 's/^constanos.gputrace=//p')"
    [[ -n $mode ]] || die "no constanos.gputrace= on the cmdline"
    mkdir -p "$ORACLE"
    # Whatever happens to the trace, leave this boot: a failure must not
    # strand the machine in the trace entry.
    (cmd_trace "$mode") > "$ORACLE/auto-$mode.log" 2>&1 || echo "trace $mode failed: $?" >> "$ORACLE/auto-$mode.log"
    if [[ $mode == nogsp && ! -e $ORACLE/trace-gsp ]]; then
        cmd_entry gsp >> "$ORACLE/auto-$mode.log" 2>&1
    else
        cmd_uninstall >> "$ORACLE/auto-$mode.log" 2>&1
    fi
    finish
    sync
    systemctl reboot
}

cmd_trace() {
    need_root
    local mode="${1:-}" gsprm mmio=1
    case "$mode" in
        nogsp) gsprm=0 ;;
        gsp) gsprm=1 ;;
        # D6: is the GSP display failure nouveau's, or mmiotrace's (one CPU, slow MMIO)?
        gspnotrace) gsprm=1; mmio=0 ;;
        *) die "trace nogsp|gsp|gspnotrace" ;;
    esac
    grep -q "module_blacklist=nvidia" /proc/cmdline || die "not in the trace boot (run 'entry' and reboot)"
    lsmod | grep -qE '^(nvidia|nouveau) ' && die "nvidia or nouveau already loaded: reboot into the trace entry"
    [[ -d $TRACEFS ]] || mount -t tracefs nodev "$TRACEFS"

    local out="$ORACLE/trace-$mode"
    [[ -e $out ]] && die "$out exists; move it away to trace again"
    mkdir -p "$out"
    echo "trace $mode -> $out"

    read_rom "$out/vbios-rom-pre.bin"
    dmesg > "$out/dmesg-before.txt"
    dmesg -C

    local reader=
    if (( mmio )); then
        echo nop > "$TRACEFS/current_tracer"
        # Per CPU (all possible CPUs, even the ones mmiotrace offlines); trace_pipe
        # drains it continuously, so 32 MiB each is plenty.
        echo 32768 > "$TRACEFS/buffer_size_kb"
        echo mmiotrace > "$TRACEFS/current_tracer"
        cat "$TRACEFS/trace_pipe" > "$out/mmiotrace.txt" &
        reader=$!
        sleep 1
    fi

    echo "  modprobe nouveau config=NvGspRm=$gsprm (settling ${SETTLE_SECS}s)"
    # modeset=1 overrides the host's /etc/modprobe.d/blacklist.conf
    # (`options nouveau modeset=0`, which loads nouveau without binding the
    # GPU): modprobe puts config options first, and the last value wins.
    modprobe nouveau modeset=1 config="NvGspRm=$gsprm" \
        debug="gsp=trace,disp=debug,i2c=debug,bios=debug,devinit=debug" \
        || echo "  modprobe failed (kept going to save the trace)"
    sleep "$SETTLE_SECS"

    if [[ -e /sys/bus/pci/drivers/nouveau/$GPU_BDF ]]; then
        echo "  nouveau is bound to $GPU_BDF"
    else
        echo "  NOUVEAU DID NOT BIND $GPU_BDF: the trace is empty"
    fi

    if (( mmio )); then
        echo "marker: settled" > "$TRACEFS/trace_marker" 2>/dev/null || true
        # The tracer can't be changed while trace_pipe is open (EBUSY): stop
        # recording, let the reader drain, close it, then switch to nop.
        echo 0 > "$TRACEFS/tracing_on"
        sleep 3; kill "$reader" 2>/dev/null || true; wait "$reader" 2>/dev/null || true
        echo nop > "$TRACEFS/current_tracer"
        echo 1 > "$TRACEFS/tracing_on"
    fi

    dmesg > "$out/dmesg.txt"
    for c in /sys/class/drm/card*-*; do
        [[ -f $c/status ]] || continue
        local name="${c##*/}"; name="${name#card*-}"
        copy_edid "$c" "$out/edid-$name.bin" || true
        { echo "status: $(cat "$c/status")"; cat "$c/modes"; } > "$out/modes-$name.txt"
    done
    # The VBIOS image nouveau actually parsed (PROM/PRAMIN), which may differ
    # from the PCI ROM BAR.
    local dbg; dbg="$(ls -d /sys/kernel/debug/dri/*/vbios.rom 2>/dev/null | head -1 || true)"
    [[ -n $dbg ]] && cp "$dbg" "$out/vbios-nouveau.bin"
    echo "  $(wc -l < "$out/mmiotrace.txt" 2>/dev/null || echo 0) trace lines, $(wc -l < "$out/dmesg.txt") dmesg lines"
    grep -m3 -iE "gsp.*(570|535)|firmware" "$out/dmesg.txt" || true
    finish
    echo "Done. Next: run 'entry' + reboot for the other mode, or reboot normally."
}

cmd_summary() {
    [[ -d $ORACLE ]] || die "nothing captured in $ORACLE"
    echo "$ORACLE:"
    du -sh "$ORACLE"/* 2>/dev/null
    for m in nogsp gsp gspnotrace; do
        local d="$ORACLE/trace-$m"
        [[ -d $d ]] || { echo "trace-$m: missing"; continue; }
        echo "== trace-$m"
        echo "  writes $(grep -c '^W ' "$d/mmiotrace.txt" || true), reads $(grep -c '^R ' "$d/mmiotrace.txt" || true)"
        grep -hoE "nvidia/ga106/gsp/[a-z_]+-[0-9.]+\.bin|gsp-[0-9.]+" "$d/dmesg.txt" | sort -u | sed 's/^/  fw: /' || true
        grep -cE "msg fn:|rpc fn:" "$d/dmesg.txt" | sed 's/^/  rpc lines: /' || true
        ls "$d"/edid-*.bin 2>/dev/null | sed 's/^/  /'
    done
}

case "${1:-}" in
    static)  cmd_static ;;
    entry)   shift; cmd_entry "$@" ;;
    install) cmd_install ;;
    uninstall) cmd_uninstall ;;
    auto)    cmd_auto ;;
    trace)   shift; cmd_trace "$@" ;;
    summary) cmd_summary ;;
    *) sed -n '2,15p' "$0"; exit 1 ;;
esac
