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
#   scripts/qemu-debug.sh start [--no-build] [--release]
#   scripts/qemu-debug.sh stop
#   scripts/qemu-debug.sh status
#   scripts/qemu-debug.sh send "text to type"      # maps chars -> sendkey, paced
#   scripts/qemu-debug.sh key ret                  # raw qemu keynames, one per arg
#   scripts/qemu-debug.sh key ctrl-c
#   scripts/qemu-debug.sh enter                    # shortcut for: key ret
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
# Common flow:
#   scripts/qemu-debug.sh start
#   scripts/qemu-debug.sh wait-for "About to start first process"
#   scripts/qemu-debug.sh send "busybox ash"
#   scripts/qemu-debug.sh enter
#   scripts/qemu-debug.sh send "ls"
#   scripts/qemu-debug.sh enter
#   scripts/qemu-debug.sh log 50
#   scripts/qemu-debug.sh stop

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
STATE_DIR="/tmp/qemu-debug-rust_so_kernel"
SOCK="$STATE_DIR/monitor.sock"
SERIAL_LOG="$STATE_DIR/serial.log"
DEBUG_LOG="$STATE_DIR/debug.log"
QEMU_STDOUT="$STATE_DIR/qemu-stdout.log"
PID_FILE="$STATE_DIR/qemu.pid"
KEY_DELAY="${QEMU_KEY_DELAY:-0.15}"

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

cmd_start() {
    if is_running; then
        echo "Already running (pid $(cat "$PID_FILE")). Use 'stop' first." >&2
        exit 1
    fi

    local do_build=1 profile_flag=()
    for arg in "$@"; do
        case "$arg" in
            --no-build) do_build=0 ;;
            --release) profile_flag=(--release) ;;
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

    rm -f "$SOCK"
    : > "$SERIAL_LOG"
    : > "$DEBUG_LOG"

    local qemu_args=(
        -drive "if=pflash,format=raw,readonly=on,file=$ovmf_code"
        -drive "if=pflash,format=raw,file=$ovmf_vars_src"
        -drive "format=raw,file=$uefi_path"
        -m 512M
        -cpu max
        -serial "file:$SERIAL_LOG"
        -monitor "unix:$SOCK,server,nowait"
        -display none
        -d int,guest_errors -D "$DEBUG_LOG"
    )
    if [ -f "$ext2_disk" ]; then
        qemu_args+=(-drive "file=$ext2_disk,format=raw,if=none,id=ext2disk" -device "ide-hd,drive=ext2disk,bus=ide.1")
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

case "${1:-}" in
    start) shift; cmd_start "$@" ;;
    stop) cmd_stop ;;
    status) cmd_status ;;
    send) cmd_send "$2" ;;
    key) shift; cmd_key "$@" ;;
    enter) cmd_key ret ;;
    screendump) cmd_screendump "${2:-}" ;;
    log) cmd_log "${2:-}" ;;
    rawlog) cmd_rawlog "${2:-}" ;;
    dlog) cmd_dlog "${2:-}" ;;
    wait-for) cmd_wait_for "$2" "${3:-}" ;;
    *)
        echo "Usage: $0 {start|stop|status|send TEXT|key KEY...|enter|screendump [out]|log [N]|rawlog [N]|dlog [N]|wait-for PATTERN [TIMEOUT]}" >&2
        exit 1
        ;;
esac
