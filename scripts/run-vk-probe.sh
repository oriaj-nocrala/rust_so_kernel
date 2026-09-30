#!/bin/bash
# Build probes/nvk (a static musl executable with the whole of NVK: docs/gpu/g4-nvkmd-plan.md, mesa-port/README.md), run it in the
# guest against /dev/nvgpu and check it printed `VK PROBE DONE`. Fails fast like run-abi-suite.sh.
#   scripts/run-vk-probe.sh [--no-build]     (the kernel is built unless --no-build; the Mesa side is built by mesa-port/build.sh)
set -u
cd "$(dirname "$0")/.."
Q=scripts/qemu-debug.sh; log=/tmp/qemu-debug-rust_so_kernel/serial.log
[ "${1:-}" = "--no-build" ] || { cargo build 2>&1 | grep -E "^error|panicked" -A8 | head -20; }
probe=$HOME/src/gpu-ref/nvk-probe/vk-probe
[ -x "$probe" ] || { echo "no $probe: run mesa-port/build.sh first"; exit 1; }
strip -o /tmp/vk_probe.stripped "$probe"
$Q stop >/dev/null 2>&1
debugfs -w -R "rm /vk_probe" disk.img >/dev/null 2>&1
debugfs -w -R "write /tmp/vk_probe.stripped /vk_probe" disk.img 2>&1 | grep -i "could not"
QEMU_DEBUG_SMP=4 $Q start --no-build >/dev/null 2>&1
$Q wait-for '# ' 90 >/dev/null || { echo "no prompt"; exit 1; }
sleep 6
$Q send 'NVK_CONSTANOS_DEBUG=1 /mnt/vk_probe; echo VKEND=$?'; sleep 1; $Q enter
why=""; last=0; lc=$SECONDS
while :; do
    sleep 2
    grep -q "VKEND=[0-9]" "$log" && break
    sz=$(stat -c %s "$log"); [ "$sz" != "$last" ] && { last=$sz; lc=$SECONDS; }
    grep -qE "KERNEL PANIC|DOUBLE FAULT" "$log" && { why="kernel panic"; break; }
    [ $((SECONDS - lc)) -gt "${STALL:-60}" ] && { why="no output for ${STALL:-60}s"; break; }
done
out=$(grep -E "^\[fb\]" "$log" | sed -n '/mnt\/vk_probe/,$p')
echo "$out" | grep -E "VK |VKEND|panic|ENOSYS" | cut -c1-200
grep -E "ENOSYS" "$log" | sort -u
[ "${KEEP_ALIVE:-0}" = 1 ] || $Q stop >/dev/null 2>&1
debugfs -w -R "rm /vk_probe" disk.img >/dev/null 2>&1
[ -z "$why" ] && ! echo "$out" | grep -q "VKEND=0" && why="the program failed"
[ -z "$why" ] && echo "$out" | grep -q "VK PROBE DONE" || why="${why:-did not reach VK PROBE DONE}"
[ "$why" = "" ] && { echo "vk-probe: OK"; exit 0; }
echo "vk-probe: FAILED — $why"; exit 1
