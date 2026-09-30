#!/bin/bash
# Build probes/tokio (tkprobe: a tokio program: current_thread and multi_thread runtimes, timers, channels, AF_UNIX echo, fs, spawn_blocking,
# process spawn, signals; tkstress: the heavy one: 10k tasks, 1000 timers, fan-in, 100 AF_UNIX clients, 32 MiB through a socket, child
# processes, locks, broadcast, select!, blocking pool, fs storm) for x86_64-unknown-linux-musl, run both in the guest and check every `TK` line. Fails fast like run-abi-suite.sh.
#   scripts/run-tokio-probe.sh [--no-build]     (the kernel is built unless --no-build)
set -u
cd "$(dirname "$0")/.."
Q=scripts/qemu-debug.sh; log=/tmp/qemu-debug-rust_so_kernel/serial.log
[ "${1:-}" = "--no-build" ] || { cargo build 2>&1 | grep -E "^error|panicked" -A8 | head -20; }
tc=$(sed -n 's/^channel *= *"\(.*\)"/\1/p' rust-toolchain.toml)
(cd probes/tokio && RUSTUP_TOOLCHAIN="$tc" cargo build --release --target x86_64-unknown-linux-musl 2>&1 | grep -E "^error" -A8)
rel=probes/tokio/target/x86_64-unknown-linux-musl/release
$Q stop >/dev/null 2>&1
for b in tkprobe tkstress; do
    debugfs -w -R "rm /$b" disk.img >/dev/null 2>&1
    debugfs -w -R "write $rel/$b /$b" disk.img 2>&1 | grep -i "could not"
done
QEMU_DEBUG_SMP=4 $Q start --no-build >/dev/null 2>&1
$Q wait-for '# ' 90 >/dev/null || { echo "no prompt"; exit 1; }
sleep 6
$Q send '/mnt/tkprobe; echo PROBEEND=$?; /mnt/tkstress; echo STRESSEND=$?'; sleep 1; $Q enter
stall="${STALL:-40}"
last=0; lc=$SECONDS; why=""
while :; do
    sleep 2
    grep -q "STRESSEND=[0-9]" "$log" && break
    grep -q "PROBEEND=[1-9]" "$log" && { why="tkprobe failed"; break; }
    sz=$(stat -c %s "$log"); [ "$sz" != "$last" ] && { last=$sz; lc=$SECONDS; }
    grep -qE "KERNEL PANIC|DOUBLE FAULT" "$log" && { why="kernel panic"; break; }
    grep -qE "^\[fb\].*panicked" "$log" && { sleep 3; why="a Rust panic"; break; }
    [ $((SECONDS - lc)) -gt "$stall" ] && { why="no output for ${stall}s"; break; }
done
out=$(grep -E "^\[fb\]" "$log" | sed -n '/mnt\/tkprobe/,$p')
echo "$out" | grep -E "TK |panicked|PROBEEND|STRESSEND" | cut -c1-200
grep -E "ENOSYS" "$log"
[ "${KEEP_ALIVE:-0}" = 1 ] || $Q stop >/dev/null 2>&1
bad=$(echo "$out" | grep -c "ok=false")
[ "$(echo "$out" | grep -c "TK DONE")" = 2 ] || why="${why:-a probe did not reach TK DONE}"
[ "$bad" = 0 ] || why="$bad check(s) reported ok=false"
[ -z "$why" ] && { echo "tokio-probe: OK"; exit 0; }
echo "tokio-probe: FAILED — $why"; exit 1
