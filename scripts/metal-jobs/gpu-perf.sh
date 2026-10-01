# What P-state and clocks does the GA106 have after our GSP-RM boot? (docs/gpu/g5-graphics-stack-plan.md, "relojes y estado de rendimiento")
#   touch build.rs; echo 5 > target/metal/budget
#   scripts/metal-run.sh --kconf 'gpu=uapi' scripts/metal-jobs/gpu-perf.sh
# `echo 'gsp perf' > /dev/dispctl` asks RM (NV2080_CTRL_CMD_PERF_GET_CURRENT_PSTATE and PERF_GET_LEVEL_INFO_V2 for levels 0..7) and /proc/kdebug's
# gpu_perf: line shows the answer. Reference, from Linux with the proprietary driver on this same card: idle P8 at 210 MHz core / 405 MHz memory,
# maximum 2130 / 7001 MHz (nvidia-smi -q -d CLOCK,PERFORMANCE).
# 1. Idle right after the boot, twice (a reading can be the state the boot left; the second shows whether it moves by itself).
# 2. Under load: snake3d (3D on the GPU, 20 s) in the background, a reading every 4 s while it runs.
# 3. After the load, a reading at once and another 10 s later (does the GPU fall back?).
# This job measures and reports; it fails only if RM does not answer the control or the GPU dies, not for the clocks' values.
cat /proc/gpu
fail=0
sum() { echo "$*"; echo "$*" >> /tmp/gpu-perf.sum; }
: > /tmp/gpu-perf.sum
grep '^gsp:' /proc/gpu >> /tmp/gpu-perf.sum
grep -q '^gsp: OK' /proc/gpu || { sum "gpu-perf: gsp did not boot"; fail=1; }
perf() { # $1 = label
  if echo 'gsp perf' > /dev/dispctl; then sum "gpu-perf: $1: $(grep '^gpu_perf:' /proc/kdebug)"; else sum "gpu-perf: $1: 'gsp perf' FAILED"; fail=1; fi
}

perf "idle 1"
sleep 3
perf "idle 2 (3 s later)"

SNAKE3D_AUTOPLAY=1 SNAKE3D_SECONDS=20 NVK_CONSTANOS_DEBUG=1 /mnt/bin/snake3d > /tmp/snake3d.out 2>&1 &
snake=$!
for t in 4 8 12 16; do
  sleep 4
  perf "load at ${t} s"
done
wait $snake
src=$?
grep -E 'SNAKE3D (DONE|[0-9]+ frames)|FAIL|ASSERT|per second' /tmp/snake3d.out | while read -r l; do sum "gpu-perf: snake: $l"; done
[ $src = 0 ] || { sum "gpu-perf: snake3d exit=$src"; fail=1; tail -n 15 /tmp/snake3d.out >> /tmp/gpu-perf.sum; }
perf "after the load"
sleep 10
perf "10 s after the load"

u=$(grep '^gpu_uapi:' /proc/kdebug)
sum "gpu-perf: end: $u"
echo "$u" | tr ' ' '\n' | grep -q '^dead=0$' || { sum "gpu-perf: the GPU was declared dead"; fail=1; }
echo 'gsp name' > /dev/dispctl && sum "gpu-perf: RM still answers" || { sum "gpu-perf: RM does not answer"; fail=1; }

echo "---- summary (the log wraps) ----"
cat /tmp/gpu-perf.sum
sum "gpu-perf: verdict exit=$fail"
exit $fail
