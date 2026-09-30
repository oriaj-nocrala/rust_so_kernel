#!/bin/bash
# One boot, one typed command: run the C test programs inside the guest via /mnt/abi-suite.sh and summarise.
#   scripts/run-abi-suite.sh [--no-build] [--keep-going] [test ...]     (no test names = the standard regression list)
# FAILS FAST: it stops at the first `FAIL` line (unless --keep-going), a kernel panic, a test that runs longer than
# TEST_TIMEOUT seconds (default 90), or no output at all for STALL seconds (default 60) — and names the test that was running.
# Prints every FAIL line and a `name=exit` line per finished test; exit status 1 if anything was not clean.
# Needs the tests on the disk image (a normal `cargo build`; `--no-build` skips that build).
set -u
cd "$(dirname "$0")/.."
Q=scripts/qemu-debug.sh
log=/tmp/qemu-debug-rust_so_kernel/serial.log
build=1; keep_going=0
while [ $# -gt 0 ]; do
    case "$1" in
        --no-build) build=0; shift ;;
        --keep-going) keep_going=1; shift ;;
        *) break ;;
    esac
done
if [ "$build" = 1 ]; then cargo build 2>&1 | grep -E "^error|panicked" -A8 | head -20; fi
free=$(dumpe2fs -h disk.img 2>/dev/null | awk '/^Free blocks/ {print $3}')
[ "${free:-100000}" -lt 2000 ] && echo "warning: disk.img has only $free free blocks; test binaries may be copied truncated" >&2
$Q stop >/dev/null 2>&1
# disk.img only syncs programs from disk-image-root/bin, so put the driver script on it here (QEMU is stopped).
debugfs -w -R "rm /abi-suite.sh" disk.img >/dev/null 2>&1
debugfs -w -R "write disk-image-root/abi-suite.sh /abi-suite.sh" disk.img >/dev/null 2>&1
QEMU_DEBUG_SMP="${QEMU_DEBUG_SMP:-4}" $Q start --no-build >/dev/null 2>&1
$Q wait-for '# ' 90 >/dev/null || { echo "no shell prompt" >&2; $Q stop >/dev/null 2>&1; exit 1; }
sleep 6
$Q send "sh /mnt/abi-suite.sh $*"
sleep 1
$Q enter

test_timeout="${TEST_TIMEOUT:-90}"; stall="${STALL:-60}"
verdict=""
last_size=0; last_change=$SECONDS; cur=""; cur_start=$SECONDS
while :; do
    sleep 2
    grep -q "SUITE_DONE" "$log" 2>/dev/null && break
    size=$(stat -c %s "$log" 2>/dev/null || echo 0)
    if [ "$size" != "$last_size" ]; then last_size=$size; last_change=$SECONDS; fi
    now_cur=$(grep -o "SUITE_START [a-z0-9_]*" "$log" | tail -1 | cut -d' ' -f2)
    if [ "$now_cur" != "$cur" ]; then cur=$now_cur; cur_start=$SECONDS; fi
    if [ "$keep_going" = 0 ] && grep -qE "^\[fb\].*FAIL" "$log"; then verdict="first FAIL in ${cur:-?}"; sleep 3; break; fi
    if grep -qE "KERNEL PANIC|panicked at|DOUBLE FAULT" "$log"; then verdict="kernel panic during ${cur:-?}"; break; fi
    if [ -n "$cur" ] && [ $((SECONDS - cur_start)) -gt "$test_timeout" ]; then verdict="${cur} ran longer than ${test_timeout}s"; break; fi
    if [ $((SECONDS - last_change)) -gt "$stall" ]; then verdict="no output for ${stall}s during ${cur:-boot}"; break; fi
done

grep -E "^\[fb\].*FAIL" "$log"
grep -oE "SUITE_RESULT [a-z0-9_]+=[0-9]+" "$log" | sed 's/SUITE_RESULT //' | sort -u | tee /tmp/abi-suite.results
if [ -n "$verdict" ]; then
    echo "--- last output ---"; grep -E "^\[fb\]" "$log" | tail -8
fi
[ "${KEEP_ALIVE:-0}" = 1 ] || $Q stop >/dev/null 2>&1   # KEEP_ALIVE=1: leave the guest running to inspect a hang
bad=$(grep -vc "=0$" /tmp/abi-suite.results)
# A test that prints FAIL but exits 0 is still a failure.
bad=$((bad + $(grep -cE "^\[fb\].*FAIL" "$log")))
total=$(wc -l < /tmp/abi-suite.results)
[ -n "$verdict" ] && { echo "abi-suite: ABORTED — $verdict ($total tests finished)"; exit 1; }
echo "abi-suite: $total tests, $bad not clean"
[ "$bad" = 0 ]
