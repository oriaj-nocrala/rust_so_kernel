#!/bin/bash
# Build probes/std/g1_std.rs (plain Rust std for x86_64-unknown-linux-musl: hard links, statx, Command with uid/gid over a
# SOCK_SEQPACKET error pipe), run it in the guest and check it reached `G1 DONE`. Fails fast like run-abi-suite.sh.
#   scripts/run-std-probe.sh [--no-build]     (the kernel is built unless --no-build)
set -u
cd "$(dirname "$0")/.."
Q=scripts/qemu-debug.sh; log=/tmp/qemu-debug-rust_so_kernel/serial.log
[ "${1:-}" = "--no-build" ] || { cargo build 2>&1 | grep -E "^error|panicked" -A8 | head -20; }
# rustc must run from inside the repo: only the pinned nightly has the musl std.
bin=$(mktemp -d)/g1_std
rustc --target x86_64-unknown-linux-musl -O -C strip=symbols probes/std/g1_std.rs -o "$bin" 2>&1 | grep -E "^error" -A8
$Q stop >/dev/null 2>&1
debugfs -w -R "rm /g1_std" disk.img >/dev/null 2>&1
debugfs -w -R "write $bin /g1_std" disk.img 2>&1 | grep -i "could not"
QEMU_DEBUG_SMP=4 $Q start --no-build >/dev/null 2>&1
$Q wait-for '# ' 90 >/dev/null || { echo "no prompt"; exit 1; }
sleep 6
$Q send '/mnt/g1_std; echo STDEND=$?'; sleep 1; $Q enter
why=""; last=0; lc=$SECONDS
while :; do
    sleep 2
    grep -q "STDEND=[0-9]" "$log" && break
    sz=$(stat -c %s "$log"); [ "$sz" != "$last" ] && { last=$sz; lc=$SECONDS; }
    grep -qE "KERNEL PANIC|DOUBLE FAULT" "$log" && { why="kernel panic"; break; }
    [ $((SECONDS - lc)) -gt "${STALL:-40}" ] && { why="no output for ${STALL:-40}s"; break; }
done
out=$(grep -E "^\[fb\]" "$log" | sed -n '/mnt\/g1_std/,$p')
echo "$out" | grep -E "G1 |panicked|STDEND" | cut -c1-200
grep -E "ENOSYS" "$log"
$Q stop >/dev/null 2>&1
debugfs -w -R "rm /g1_std" disk.img >/dev/null 2>&1
[ -z "$why" ] && ! echo "$out" | grep -q "STDEND=0" && why="the program failed"
[ -z "$why" ] && echo "$out" | grep -q "G1 DONE" || why="${why:-did not reach G1 DONE}"
[ "$why" = "" ] && { echo "std-probe: OK"; exit 0; }
echo "std-probe: FAILED — $why"; exit 1
