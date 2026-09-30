#!/bin/bash
# Build probes/tokio (a tokio program: current_thread and multi_thread runtimes, timers, channels, AF_UNIX echo, fs, spawn_blocking,
# process spawn, signals) for x86_64-unknown-linux-musl, run it in the guest and check every `TK` line. Fails fast like run-abi-suite.sh.
#   scripts/run-tokio-probe.sh [--no-build]     (the kernel is built unless --no-build)
set -u
cd "$(dirname "$0")/.."
Q=scripts/qemu-debug.sh; log=/tmp/qemu-debug-rust_so_kernel/serial.log
[ "${1:-}" = "--no-build" ] || { cargo build 2>&1 | grep -E "^error|panicked" -A8 | head -20; }
tc=$(sed -n 's/^channel *= *"\(.*\)"/\1/p' rust-toolchain.toml)
(cd probes/tokio && RUSTUP_TOOLCHAIN="$tc" cargo build --release --target x86_64-unknown-linux-musl 2>&1 | grep -E "^error" -A8)
bin=probes/tokio/target/x86_64-unknown-linux-musl/release/tkprobe
$Q stop >/dev/null 2>&1
debugfs -w -R "rm /tkprobe" disk.img >/dev/null 2>&1
debugfs -w -R "write $bin /tkprobe" disk.img 2>&1 | grep -i "could not"
QEMU_DEBUG_SMP=4 $Q start --no-build >/dev/null 2>&1
$Q wait-for '# ' 90 >/dev/null || { echo "no prompt"; exit 1; }
sleep 6
$Q send '/mnt/tkprobe; echo PROBEEND=$?'; sleep 1; $Q enter
last=0; lc=$SECONDS; why=""
while :; do
    sleep 2
    grep -q "PROBEEND=[0-9]" "$log" && break
    sz=$(stat -c %s "$log"); [ "$sz" != "$last" ] && { last=$sz; lc=$SECONDS; }
    grep -qE "KERNEL PANIC|DOUBLE FAULT" "$log" && { why="kernel panic"; break; }
    [ $((SECONDS - lc)) -gt 30 ] && { why="no output for 30s"; break; }
done
out=$(grep -E "^\[fb\]" "$log" | sed -n '/mnt\/tkprobe/,$p')
echo "$out" | grep -E "TK |panicked|PROBEEND" | cut -c1-200
grep -E "ENOSYS" "$log"
$Q stop >/dev/null 2>&1
bad=$(echo "$out" | grep -c "ok=false")
echo "$out" | grep -q "TK DONE" || why="${why:-the probe did not reach TK DONE}"
echo "$out" | grep -q "PROBEEND=0" || why="${why:-non-zero exit}"
[ "$bad" = 0 ] || why="$bad check(s) reported ok=false"
[ -z "$why" ] && { echo "tokio-probe: OK"; exit 0; }
echo "tokio-probe: FAILED — $why"; exit 1
