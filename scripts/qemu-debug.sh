#!/usr/bin/env bash
# Headless QEMU debug harness for interactive kernel testing.
#
# Replaces the old per-session pattern of: hand-rolling a giant
# qemu-system-x86_64 command line, then hand-writing a one-off python/socat
# script to send keys character-by-character over the monitor socket. All
# of that is now baked in here — see debugging_technique_qemu_monitor memory
# for *why* this is shaped the way it is (stdin is PS/2, not serial; sendkey
# needs pacing; backgrounding across separate shell calls needs nohup+disown).
#
# Usage:
#   scripts/qemu-debug.sh start [--no-build] [--release] [--gdb] [--gdb-freeze]
#   scripts/qemu-debug.sh stop
#   scripts/qemu-debug.sh status
#   scripts/qemu-debug.sh send "text to type"      # maps chars -> sendkey, paced
#   scripts/qemu-debug.sh key ret                  # raw qemu keynames, one per arg
#   scripts/qemu-debug.sh key ctrl-c
#   scripts/qemu-debug.sh enter                    # shortcut for: key ret
#   scripts/qemu-debug.sh mouse-move dx dy          # relative PS/2 motion (HMP mouse_move)
#   scripts/qemu-debug.sh mouse-button val          # HMP bitmask: 1=left, 2=right, 4=middle, 0=release
#   scripts/qemu-debug.sh screendump [out.png]      # defaults to STATE_DIR/screen.png
#   scripts/qemu-debug.sh log [N]                   # tail -n N serial.log (default 100)
#   scripts/qemu-debug.sh rawlog [N]                # like log, but ANSI/control bytes shown
#                                                    # as ^[ (cat -v) instead of raw — only
#                                                    # needed to tell a *real* escape code
#                                                    # apart from a program that literally
#                                                    # printed the text "[32m". Ordinary `log`
#                                                    # (or `grep`ping serial.log directly) is
#                                                    # usually enough on its own to check
#                                                    # color/SGR output: an ESC byte renders
#                                                    # invisible in a tool-result stream, but
#                                                    # the rest of the code ("[1;32m...[m")
#                                                    # stays as plain, greppable text — no
#                                                    # screendump/pixel-sampling needed just to
#                                                    # confirm what SGR codes got emitted.
#   scripts/qemu-debug.sh dlog [N]                  # tail -n N debug.log (-d int trace)
#   scripts/qemu-debug.sh wait-for PATTERN [TIMEOUT_SECS]   # poll serial.log for a regex
#
# Real-hardware-shaped env overrides (see the block around `local mem=`):
#   QEMU_DEBUG_MEM=8G        RAM size (default 512M) — sizes above 512 MiB
#                            exercise physical addresses a real machine has
#                            and every QEMU default here does not
#   QEMU_DEBUG_NO_DISK=1     omit the ext2 disk (no ATA on the target board)
#   QEMU_DEBUG_NO_AC97=1     omit the AC97 codec (target board has HDA)
#   QEMU_DEBUG_NO_USB=1      omit the xHCI USB controller
#   QEMU_DEBUG_NO_PS2=1      omit the legacy 8042 (PS/2) controller — the
#                            target board has none; combine with QEMU_USB_KBD=1
#                            to make USB the only input path, as on metal
#   QEMU_USB_KBD=1           attach a USB keyboard to it, and route `send`
#                            through it instead of the PS/2 8042
#   QEMU_USB_MOUSE=1         attach a USB mouse to it; `mouse-move`/
#                            `mouse-button` then arrive through xHCI (QEMU
#                            routes them to the newest mouse)
#   QEMU_USB_STORAGE=<img>   attach <img> (raw, or .qcow2) as a USB mass-storage stick —
#                            the boot pendrive's shape; pass a scratch copy,
#                            the kernel mounts it read-write eventually
#   QEMU_DEBUG_SMP=N         N CPUs (-smp N; default 1). The kernel starts
#                            every AP and parks it (stage 4 of the SMP plan);
#                            boot-matrix.sh inherits it from the environment
#   QEMU_DEBUG_EXTRA_ARGS    extra raw qemu args, word-split
#
#   scripts/qemu-debug.sh gdb ["cmd" "cmd" ...]      # batch gdb against a running instance
#                                                    # (needs `start --gdb`/`--gdb-freeze` first)
#                                                    # — see the GDB section below
#
# Common flow:
#   scripts/qemu-debug.sh start
#   scripts/qemu-debug.sh wait-for "About to start first process"
#   scripts/qemu-debug.sh send "busybox ash"
#   scripts/qemu-debug.sh enter
#   scripts/qemu-debug.sh send "ls"
#   scripts/qemu-debug.sh enter
#   scripts/qemu-debug.sh log 50
#   scripts/qemu-debug.sh stop
#
# GDB support:
#
#   `start --gdb` adds QEMU's `-gdb tcp::<port>` (port from $QEMU_GDB_PORT,
#   default 1234) — the guest boots completely normally, no frozen CPU, and
#   the stub just sits there accepting a connection at any later time. This
#   is the flag for the actual motivating use case: a boot that hangs with
#   the CPU spinning at ~1-in-N — start normally, wait for (or detect) the
#   hang, then attach and ask it where it is. `start --gdb-freeze` additionally
#   passes `-S`, halting the CPU at the reset vector until a debugger
#   continues it — useful for single-stepping *early* boot, but deliberately
#   NOT the default (it would hang every `start` call waiting for a debugger
#   that usually isn't there).
#
#   The `gdb` subcommand is the non-interactive half: it runs
#   `gdb -batch -ex ...` (or `rust-gdb`, whichever is on $PATH — see below)
#   against `target remote localhost:<port>` with the kernel's own debug
#   symbols loaded, and prints the result to stdout. Built for an agent
#   without an interactive terminal — no interactive prompt, no TUI, just
#   one-shot commands in, text out:
#
#     scripts/qemu-debug.sh gdb "info registers" "bt" "p \$rip"
#
#   With no arguments it runs a reasonable default hang-diagnosis set
#   (registers, backtrace, symbol-for-RIP). Debugger choice: `rust-gdb`
#   (a thin wrapper around `gdb` that adds Rust pretty-printers) is
#   preferred when present on $PATH, else plain `gdb`; if neither exists
#   the subcommand fails with a clear message instead of half-working.
#   No lldb support — not attempted (this only ports the gdbstub flow).
#
#   Symbol loading needs one extra step because the kernel ELF is a PIE
#   (`bootloader` 0.11 loads it as ET_DYN at a runtime-chosen
#   "virtual_address_offset", not the addresses recorded in the file, and
#   that offset can differ boot to boot depending on the UEFI memory map).
#   The bootloader logs the exact offset it picked to serial at boot
#   (`virtual_address_offset: 0x...`); `gdb` subcommand greps the current
#   $STATE_DIR/serial.log for the most recent one and loads symbols via
#   `add-symbol-file <kernel elf> -o <offset>` so addresses actually line
#   up. The kernel ELF itself is never stripped (see CLAUDE.md) — whichever
#   of kernel/target/x86_64-unknown-none/{debug,release}/kernel was built
#   most recently is used automatically.
#
#   GDB example flow:
#     scripts/qemu-debug.sh start --gdb
#     scripts/qemu-debug.sh wait-for "About to start first process"
#     scripts/qemu-debug.sh gdb "info registers" "bt"
#     scripts/qemu-debug.sh stop

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# Overridable so multiple independent debug sessions (e.g. concurrent
# investigations in the same checkout) don't share — and clobber — the
# same serial.log/monitor.sock/qemu.pid. Default preserved for anyone not
# opting in.
STATE_DIR="${QEMU_DEBUG_STATE_DIR:-/tmp/qemu-debug-rust_so_kernel}"
SOCK="$STATE_DIR/monitor.sock"
SERIAL_LOG="$STATE_DIR/serial.log"
DEBUG_LOG="$STATE_DIR/debug.log"
QEMU_STDOUT="$STATE_DIR/qemu-stdout.log"
PID_FILE="$STATE_DIR/qemu.pid"
KEY_DELAY="${QEMU_KEY_DELAY:-0.15}"
GDB_PORT="${QEMU_GDB_PORT:-1234}"

mkdir -p "$STATE_DIR"

mon() {
    # Send one monitor command. Separate socat invocation per call — the
    # monitor socket needs to be freshly connected each time, and pacing
    # between keystrokes matters (see module docstring).
    echo "$1" | socat - "UNIX-CONNECT:$SOCK" >/dev/null 2>&1
}

is_running() {
    [ -f "$PID_FILE" ] && kill -0 "$(cat "$PID_FILE")" 2>/dev/null
}

find_output_file() {
    # Newest so2-* build-script output file across debug/release profiles —
    # this is where UEFI_PATH/OVMF_CODE/OVMF_VARS/EXT2_DISK_PATH live (see
    # build.rs's cargo:rustc-env lines).
    find "$REPO_ROOT/target" -maxdepth 4 -path "*/build/so2-*/output" -printf '%T@ %p\n' 2>/dev/null \
        | sort -rn | head -1 | cut -d' ' -f2-
}

find_kernel_elf() {
    # Newest kernel ELF across debug/release profiles — same "newest wins"
    # convention as find_output_file(). Per CLAUDE.md the kernel binary is
    # never stripped, so this always has full debug symbols for gdb.
    find "$REPO_ROOT/kernel/target/x86_64-unknown-none" -maxdepth 2 -type f -name kernel -printf '%T@ %p\n' 2>/dev/null \
        | sort -rn | head -1 | cut -d' ' -f2-
}

find_debugger() {
    # rust-gdb is a thin wrapper around gdb that also loads Rust's
    # pretty-printers — strictly better than plain gdb when present, same
    # -batch/-ex CLI otherwise. No lldb support here.
    if command -v rust-gdb >/dev/null 2>&1; then
        echo rust-gdb
    elif command -v gdb >/dev/null 2>&1; then
        echo gdb
    else
        echo ""
    fi
}

cmd_start() {
    if is_running; then
        echo "Already running (pid $(cat "$PID_FILE")). Use 'stop' first." >&2
        exit 1
    fi

    local do_build=1 profile_flag=() enable_gdb=0 freeze_gdb=0
    for arg in "$@"; do
        case "$arg" in
            --no-build) do_build=0 ;;
            --release) profile_flag=(--release) ;;
            --gdb) enable_gdb=1 ;;
            --gdb-freeze) enable_gdb=1; freeze_gdb=1 ;;
            *) echo "Unknown start arg: $arg" >&2; exit 1 ;;
        esac
    done

    if [ "$do_build" = 1 ]; then
        echo "Building (cargo build ${profile_flag[*]:-})..." >&2
        (cd "$REPO_ROOT" && cargo build "${profile_flag[@]}")
    fi

    local out_file
    out_file="$(find_output_file)"
    if [ -z "$out_file" ]; then
        echo "No build output found under target/. Run without --no-build first." >&2
        exit 1
    fi

    local uefi_path ovmf_code ovmf_vars ext2_disk
    uefi_path="$(grep -oP 'UEFI_PATH=\K.*' "$out_file")"
    ovmf_code="$(grep -oP 'OVMF_CODE=\K.*' "$out_file")"
    ovmf_vars_src="$(grep -oP 'OVMF_VARS=\K.*' "$out_file")"
    ext2_disk="$(grep -oP 'EXT2_DISK_PATH=\K.*' "$out_file")"
    # Overridable: point this at a scratch copy (`cp disk.img /tmp/foo.img`)
    # for any session that shouldn't write to the shared disk.img — two
    # QEMUs writing the same ext2 image concurrently can corrupt it.
    ext2_disk="${QEMU_DEBUG_DISK_IMG:-$ext2_disk}"

    rm -f "$SOCK"
    : > "$SERIAL_LOG"
    : > "$DEBUG_LOG"

    # AC97 audiodev backend: "none" by default (no host audio needed,
    # works in any headless/sandboxed environment) — override with
    # QEMU_AUDIODEV="wav,id=snd0,path=/some/file.wav" to capture real PCM
    # output to a host .wav file for verification.
    local audiodev="${QEMU_AUDIODEV:-none,id=snd0}"

    # ── Real-hardware-shaped overrides ──────────────────────────────
    # This kernel is also brought up on a physical AM4/Ryzen machine that
    # has none of QEMU's default legacy devices (no ATA/IDE, no AC97, no
    # PS/2) and far more RAM than 512 MiB. Reproducing a real-hardware-only
    # hang needs those same conditions here, where serial.log and gdb still
    # work — hence these knobs rather than a hand-rolled qemu command line
    # (which is exactly what this script exists to replace):
    #   QEMU_DEBUG_MEM=32G        RAM size (default 512M)
    #   QEMU_DEBUG_NO_DISK=1      omit the ext2 disk (machine has no ATA)
    #   QEMU_DEBUG_NO_AC97=1      omit the AC97 codec (machine has HDA)
    #   QEMU_DEBUG_EXTRA_ARGS=".." extra raw args, word-split (e.g. "-smp 8")
    local mem="${QEMU_DEBUG_MEM:-512M}"

    local qemu_args=(
        -drive "if=pflash,format=raw,readonly=on,file=$ovmf_code"
        # file.locking=off on the VARS pflash and the UEFI boot image: both
        # live under the shared build output dir (not overridable the way
        # the ext2 disk is, since they're build.rs's own outputs, not a
        # standalone file a caller can just point elsewhere), so a second
        # concurrent qemu-debug.sh session against the same build — a
        # different STATE_DIR, same $REPO_ROOT/target — would otherwise
        # fail to launch at all with QEMU's "Failed to get write lock"
        # (verified: this is exactly what happens without it). Both are
        # effectively read-only in practice for this kernel (no meaningful
        # UEFI NVRAM writes, boot image content never changes at runtime),
        # so disabling QEMU's advisory lock here doesn't add real risk.
        -drive "if=pflash,format=raw,file=$ovmf_vars_src,file.locking=off"
        -drive "format=raw,file=$uefi_path,file.locking=off"
        -m "$mem"
        -cpu max
        -smp "${QEMU_DEBUG_SMP:-1}"
        -serial "file:$SERIAL_LOG"
        -monitor "unix:$SOCK,server,nowait"
        -display none
        -d int,guest_errors -D "$DEBUG_LOG"
    )
    if [ -z "${QEMU_DEBUG_NO_AC97:-}" ]; then
        qemu_args+=(-audiodev "$audiodev" -device "AC97,audiodev=snd0")
    fi
    # xHCI controller (kernel/src/usb/) — present by default so the USB
    # driver's bring-up runs on every boot; QEMU_DEBUG_NO_USB=1 omits it
    # for a machine-shape sweep (see the NO_DISK/NO_AC97 knobs above).
    #
    # The keyboard behind it is opt-in: QEMU sends monitor `sendkey` events
    # to whichever keyboard it considers current, so attaching a usb-kbd
    # unconditionally would silently move `send`/`enter` below — and every
    # script built on them — onto the USB path. QEMU_USB_KBD=1 makes that
    # rerouting deliberate, which is how the USB driver is tested end to
    # end: same `send "text"`, arriving through xHCI instead of the 8042.
    # No legacy PS/2 controller — what a modern board with the 8042 fused
    # out looks like, and the single most faithful knob for reproducing the
    # USB-keyboard-only bring-up machine: with it set, the USB driver is the
    # only possible source of input, exactly as on real hardware.
    if [ -n "${QEMU_DEBUG_NO_PS2:-}" ]; then
        qemu_args+=(-machine "pc,i8042=off")
    fi
    if [ -z "${QEMU_DEBUG_NO_USB:-}" ]; then
        qemu_args+=(-device "qemu-xhci,id=xhci")
        if [ -n "${QEMU_USB_KBD:-}" ]; then
            qemu_args+=(-device "usb-kbd,bus=xhci.0")
        fi
        if [ -n "${QEMU_USB_MOUSE:-}" ]; then
            qemu_args+=(-device "usb-mouse,bus=xhci.0")
        fi
        if [ -n "${QEMU_USB_STORAGE:-}" ]; then
            local stick_fmt="raw"
            case "$QEMU_USB_STORAGE" in *.qcow2) stick_fmt="qcow2" ;; esac
            qemu_args+=(-drive "if=none,id=usbstick,format=${stick_fmt},file=${QEMU_USB_STORAGE}")
            qemu_args+=(-device "usb-storage,bus=xhci.0,drive=usbstick")
        fi
    fi
    if [ -f "$ext2_disk" ] && [ -z "${QEMU_DEBUG_NO_DISK:-}" ]; then
        # QEMU_DEBUG_DISK_IMG can point at a qcow2 overlay instead of the
        # real raw disk.img (see boot-matrix.sh: `qemu-img create -f qcow2
        # -b disk.img -F raw overlay.qcow2` — kilobytes instead of copying
        # the whole 96 MiB image, one overlay per parallel instance since
        # the kernel mounts /mnt read-write). Detect by extension rather
        # than probing the file (`-f raw` would otherwise misread a qcow2
        # overlay's header as ext2 filesystem garbage).
        local ext2_fmt="raw"
        case "$ext2_disk" in
            *.qcow2) ext2_fmt="qcow2" ;;
        esac
        qemu_args+=(-drive "file=$ext2_disk,format=$ext2_fmt,if=none,id=ext2disk" -device "ide-hd,drive=ext2disk,bus=ide.1")
    fi

    if [ -n "${QEMU_DEBUG_EXTRA_ARGS:-}" ]; then
        # Deliberately word-split (unquoted): the point is to pass several
        # raw qemu args from one env var.
        # shellcheck disable=SC2206
        qemu_args+=(${QEMU_DEBUG_EXTRA_ARGS})
    fi

    if [ "$enable_gdb" = 1 ]; then
        # Equivalent to -s (which is hardcoded to port 1234) but with a
        # configurable port so multiple sessions don't collide.
        qemu_args+=(-gdb "tcp::$GDB_PORT")
        echo "gdbstub listening on tcp::$GDB_PORT (attach with '$0 gdb ...')" >&2
    fi
    if [ "$freeze_gdb" = 1 ]; then
        qemu_args+=(-S)
        echo "CPU frozen at reset (-S) — nothing boots until a debugger connects and continues it" >&2
    fi

    echo "Launching qemu (state dir: $STATE_DIR)..." >&2
    nohup qemu-system-x86_64 "${qemu_args[@]}" > "$QEMU_STDOUT" 2>&1 < /dev/null &
    disown
    echo $! > "$PID_FILE"

    # Wait for the monitor socket to come up rather than a blind sleep.
    for _ in $(seq 1 50); do
        [ -S "$SOCK" ] && break
        sleep 0.1
    done
    if [ ! -S "$SOCK" ]; then
        echo "qemu started (pid $(cat "$PID_FILE")) but monitor socket never appeared — check $QEMU_STDOUT" >&2
        exit 1
    fi
    echo "Running (pid $(cat "$PID_FILE")). serial: $SERIAL_LOG  monitor: $SOCK" >&2
}

cmd_stop() {
    if ! is_running; then
        echo "Not running." >&2
        rm -f "$PID_FILE" "$SOCK"
        return
    fi
    kill "$(cat "$PID_FILE")" 2>/dev/null || true
    sleep 0.3
    kill -9 "$(cat "$PID_FILE")" 2>/dev/null || true
    rm -f "$PID_FILE" "$SOCK"
    echo "Stopped." >&2
}

cmd_status() {
    if is_running; then
        echo "Running (pid $(cat "$PID_FILE"))"
    else
        echo "Not running"
    fi
}

char_to_key() {
    local c="$1"
    case "$c" in
        [a-z0-9]) echo "$c" ;;
        [A-Z]) echo "shift-${c,,}" ;;
        ' ') echo "spc" ;;
        $'\t') echo "tab" ;;
        '-') echo "minus" ;;
        '=') echo "equal" ;;
        '[') echo "bracket_left" ;;
        ']') echo "bracket_right" ;;
        '\') echo "backslash" ;;
        ';') echo "semicolon" ;;
        "'") echo "apostrophe" ;;
        '`') echo "grave_accent" ;;
        ',') echo "comma" ;;
        '.') echo "dot" ;;
        '/') echo "slash" ;;
        '!') echo "shift-1" ;;
        '@') echo "shift-2" ;;
        '#') echo "shift-3" ;;
        '$') echo "shift-4" ;;
        '%') echo "shift-5" ;;
        '^') echo "shift-6" ;;
        '&') echo "shift-7" ;;
        '*') echo "shift-8" ;;
        '(') echo "shift-9" ;;
        ')') echo "shift-0" ;;
        '_') echo "shift-minus" ;;
        '+') echo "shift-equal" ;;
        '{') echo "shift-bracket_left" ;;
        '}') echo "shift-bracket_right" ;;
        '|') echo "shift-backslash" ;;
        ':') echo "shift-semicolon" ;;
        '"') echo "shift-apostrophe" ;;
        '~') echo "shift-grave_accent" ;;
        '<') echo "shift-comma" ;;
        '>') echo "shift-dot" ;;
        '?') echo "shift-slash" ;;
        *) echo "" ;;
    esac
}

cmd_send() {
    is_running || { echo "Not running." >&2; exit 1; }
    local text="$1"
    local i c key
    for (( i=0; i<${#text}; i++ )); do
        c="${text:$i:1}"
        key="$(char_to_key "$c")"
        if [ -z "$key" ]; then
            echo "warning: no keymap for char '$c', skipping" >&2
            continue
        fi
        mon "sendkey $key"
        sleep "$KEY_DELAY"
    done
}

cmd_key() {
    is_running || { echo "Not running." >&2; exit 1; }
    for k in "$@"; do
        mon "sendkey $k"
        sleep "$KEY_DELAY"
    done
}

cmd_mouse_move() {
    is_running || { echo "Not running." >&2; exit 1; }
    # QEMU HMP mouse_move dx dy [dz] — relative deltas by default (no
    # absolute pointing device, e.g. usb-tablet, is attached in cmd_start).
    mon "mouse_move $1 $2"
}

cmd_mouse_button() {
    is_running || { echo "Not running." >&2; exit 1; }
    # QEMU HMP bitmask: 1=left, 2=right, 4=middle (measured: 2 -> BTN_RIGHT,
    # 4 -> BTN_MIDDLE on both PS/2 and USB). 0 releases all buttons.
    mon "mouse_button $1"
}

cmd_screendump() {
    is_running || { echo "Not running." >&2; exit 1; }
    local out="${1:-$STATE_DIR/screen.png}"
    local ppm="$STATE_DIR/screen.ppm"
    mon "screendump $ppm"
    sleep 0.3
    python3 -c "from PIL import Image; Image.open('$ppm').save('$out')" 2>/dev/null \
        && echo "$out" \
        || { echo "PIL unavailable or conversion failed, raw ppm at $ppm" >&2; echo "$ppm"; }
}

cmd_log() {
    tail -n "${1:-100}" "$SERIAL_LOG"
}

cmd_rawlog() {
    # `cat -v` renders control bytes visibly (ESC -> "^[", so a color code
    # like "\x1b[0;32m" reads as "^[[0;32m") instead of the terminal
    # swallowing/misrendering them or `log`'s plain tail hiding them
    # entirely. Greppable — e.g. `rawlog 200 | grep -o '\^\[\[[0-9;]*m'`
    # pulls out every SGR code emitted, no screendump/pixel-sampling needed.
    tail -n "${1:-100}" "$SERIAL_LOG" | cat -v
}

cmd_dlog() {
    tail -n "${1:-100}" "$DEBUG_LOG"
}

cmd_wait_for() {
    local pattern="$1"
    local timeout="${2:-15}"
    local waited=0
    while ! grep -qE "$pattern" "$SERIAL_LOG" 2>/dev/null; do
        sleep 0.2
        waited=$(echo "$waited + 0.2" | bc)
        if (( $(echo "$waited >= $timeout" | bc -l) )); then
            echo "timeout waiting for pattern: $pattern" >&2
            exit 1
        fi
    done
    echo "matched: $pattern" >&2
}

cmd_gdb() {
    is_running || { echo "Not running. Start with '$0 start --gdb' first." >&2; exit 1; }

    local dbg
    dbg="$(find_debugger)"
    if [ -z "$dbg" ]; then
        echo "No debugger found on host (checked: rust-gdb, gdb). Install gdb to use this subcommand." >&2
        exit 1
    fi

    local kernel_elf
    kernel_elf="$(find_kernel_elf)"
    if [ -z "$kernel_elf" ] || [ ! -f "$kernel_elf" ]; then
        echo "No kernel ELF found under $REPO_ROOT/kernel/target/x86_64-unknown-none/{debug,release}/kernel — build first (plain 'start' builds it)." >&2
        exit 1
    fi

    # bootloader 0.11 loads the kernel as a PIE (ET_DYN) at a runtime-chosen
    # virtual_address_offset (bootloader-x86_64-common's load_kernel.rs),
    # NOT the addresses recorded in the ELF, and it isn't necessarily the
    # same across boots — it depends on which regions the UEFI memory map
    # leaves free. The bootloader logs the exact offset it picked to serial
    # at boot ("virtual_address_offset: 0x..."); pull the most recent one
    # out of this session's serial.log so gdb shifts the whole symbol table
    # to match where the kernel actually landed in guest memory this run.
    local offset
    offset="$(grep -oP 'virtual_address_offset: \K0x[0-9a-fA-F]+' "$SERIAL_LOG" 2>/dev/null | tail -1)"
    if [ -z "$offset" ]; then
        echo "warning: 'virtual_address_offset' not found yet in $SERIAL_LOG (kernel may not have reached that boot log line) — loading symbols unshifted; addresses/backtraces will likely be wrong" >&2
        offset="0x0"
    fi

    local commands=("$@")
    if [ ${#commands[@]} -eq 0 ]; then
        # Reasonable default for "the kernel is hung, where is it": full
        # register dump, backtrace, and which function RIP currently falls
        # inside of.
        commands=("info registers" "bt" "info symbol \$pc")
    fi

    local gdb_args=(
        -batch -nx
        -ex "set pagination off"
        -ex "set confirm off"
        -ex "target remote localhost:$GDB_PORT"
        -ex "add-symbol-file $kernel_elf -o $offset"
    )
    local c
    for c in "${commands[@]}"; do
        gdb_args+=(-ex "$c")
    done
    # Detach rather than let batch-mode exit implicitly kill/disconnect the
    # target ungracefully — the whole point is to inspect a still-running
    # (possibly hung) QEMU and leave it running afterward.
    gdb_args+=(-ex "detach")

    echo "debugger: $dbg" >&2
    echo "kernel ELF: $kernel_elf" >&2
    echo "virtual_address_offset: $offset" >&2
    "$dbg" "${gdb_args[@]}"
}

case "${1:-}" in
    start) shift; cmd_start "$@" ;;
    stop) cmd_stop ;;
    status) cmd_status ;;
    send) cmd_send "$2" ;;
    key) shift; cmd_key "$@" ;;
    enter) cmd_key ret ;;
    mouse-move) cmd_mouse_move "${2:-0}" "${3:-0}" ;;
    mouse-button) cmd_mouse_button "${2:-0}" ;;
    screendump) cmd_screendump "${2:-}" ;;
    log) cmd_log "${2:-}" ;;
    rawlog) cmd_rawlog "${2:-}" ;;
    dlog) cmd_dlog "${2:-}" ;;
    wait-for) cmd_wait_for "$2" "${3:-}" ;;
    gdb) shift; cmd_gdb "$@" ;;
    *)
        echo "Usage: $0 {start [--gdb|--gdb-freeze]|stop|status|send TEXT|key KEY...|enter|screendump [out]|log [N]|rawlog [N]|dlog [N]|wait-for PATTERN [TIMEOUT]|gdb [\"cmd\" ...]}" >&2
        exit 1
        ;;
esac
