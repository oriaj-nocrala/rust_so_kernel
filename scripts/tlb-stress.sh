#!/bin/bash
# Reproduces the TLB-shootdown ack timeout ("TLB shootdown of ... (kernel): cpus 0x.. never acknowledged") in QEMU, on 4 CPUs, in minutes:
# a 4 MiB block cache (ext2cache=4) so every read misses, six readers cycling over six ~1.5 MB programs, and twelve fork+exec of `hello` per round,
# as an autorun job (so the panic shows in serial.log and QEMU exits).
#   scripts/tlb-stress.sh [N]     N parallel QEMUs (default 4); prints PANIC / DONE / TIMEOUT per run
# Measured 2026-10-01: with the kernel as it was, 6 of 6 panic (several CPUs unacknowledged); the same panic also shows in `scripts/run-abi-suite.sh
# gui_comp_test` about one run in three. See docs/reference/cpu.md "Known issue".
set -u
cd "$(dirname "$0")/.."
n=${1:-4}
run() {
    id=$1; d=/tmp/qs$id
    rm -rf $d; mkdir -p $d; cp disk.img $d/j.img
    cat > $d/job.sh <<'JOB'
echo STRESS-BEGIN
r=0
while [ $r -lt 4 ]; do
  for f in cpumon textdemo panel compositor tkstress tkprobe; do (cat /mnt/bin/$f > /dev/null; cat /mnt/bin/$f > /dev/null) & done
  i=0; while [ $i -lt 12 ]; do (/mnt/bin/hello > /dev/null) & i=$((i+1)); done
  wait
  r=$((r+1))
done
echo STRESS-OK
JOB
    printf 'ext2cache=4\n' > $d/kernel.conf; echo "stress-$id" > $d/nonce
    debugfs -w $d/j.img -R 'mkdir /autorun' >/dev/null 2>&1
    for f in job.sh:job nonce:nonce kernel.conf:kernel.conf; do debugfs -w $d/j.img -R "write $d/${f%%:*} /autorun/${f##*:}" >/dev/null 2>&1; done
    QEMU_DEBUG_SMP=4 QEMU_DEBUG_DISK_IMG=$d/j.img QEMU_DEBUG_EXTRA_ARGS=-no-reboot QEMU_DEBUG_STATE_DIR=$d scripts/qemu-debug.sh start --no-build >/dev/null 2>&1
    v=TIMEOUT
    for _ in $(seq 1 120); do
        if grep -a -q "KERNEL PANIC" $d/serial.log 2>/dev/null; then v=PANIC; break; fi
        if grep -a -q "METAL-DONE" $d/serial.log 2>/dev/null; then v=DONE; break; fi
        sleep 5
    done
    echo "run $id: $v $(grep -a -A2 'KERNEL PANIC' $d/serial.log | tail -1)"
    QEMU_DEBUG_STATE_DIR=$d scripts/qemu-debug.sh stop >/dev/null 2>&1
}
for i in $(seq 1 "$n"); do run "$i" & done
wait
