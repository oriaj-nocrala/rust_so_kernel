# The SM clock of the GA106 under our GSP-RM boot, measured from inside the GPU (docs/gpu/g5-graphics-stack-plan.md, "relojes y estado de rendimiento").
#   touch build.rs; echo 5 > target/metal/budget
#   scripts/metal-run.sh --kconf 'gpu=uapi' scripts/metal-jobs/gpu-clock.sh
# `nvgpu_hw_test clock` (nvgpu/gen/shader/clock.cu) divides SM cycles by the GPU's global timer across a chain of FFMAs: MHz per launch, no help from RM.
# `echo 'gsp perf' > /dev/dispctl` gives the P-state next to it (gpu_perf:). Reference (Linux, proprietary driver, same card): idle P8 at 210 MHz, maximum 2130 MHz.
# 1. After 20 s of idle (the GPU settles into its idle state): the P-state, then 12 s of back-to-back launches: the clock of the FIRST launch after an idle and how
#    long until it reaches its top (what a client that wakes the GPU gets).
# 2. The same again after the load has stopped for 12 s: does the GPU fall back, and does it ramp the same way?
# 3. A light duty cycle (a launch of ~2-15 ms every 8 ms, 10 s): where does a light, steady load (a compositor) sit?
# 4. The same probe beside snake3d (3D at 60 fps for 14 s): the clock a real client sees.
# It measures and reports. It fails only when a launch does not run or the numbers cannot be a clock, or the GPU dies.
cat /proc/gpu
fail=0
sum() { echo "$*"; echo "$*" >> /tmp/gpu-clock.sum; }
: > /tmp/gpu-clock.sum
grep '^gsp:' /proc/gpu >> /tmp/gpu-clock.sum
grep -q '^gsp: OK' /proc/gpu || { sum "gpu-clock: gsp did not boot"; fail=1; }
perf() { # $1 = label
  if echo 'gsp perf' > /dev/dispctl; then sum "gpu-clock: $1: $(grep '^gpu_perf:' /proc/kdebug | sed 's/ L0=.*//')"; else sum "gpu-clock: $1: 'gsp perf' FAILED"; fail=1; fi
}
probe() { # $1 = label, then the arguments of `nvgpu_hw_test clock`
  label=$1; shift
  /mnt/bin/nvgpu_hw_test clock "$@" > /tmp/clock.out 2>&1
  rc=$?
  grep 'CLOCK t=' /tmp/clock.out | awk 'NR<=6 || NR%4==0' | while read -r l; do sum "gpu-clock: $label: $l"; done
  grep 'CLOCK SUMMARY' /tmp/clock.out | while read -r l; do sum "gpu-clock: $label: $l"; done
  grep 'FAIL\|skip' /tmp/clock.out | while read -r l; do sum "gpu-clock: $label: $l"; done
  [ $rc = 0 ] || { sum "gpu-clock: $label: nvgpu_hw_test clock exit=$rc"; fail=1; }
  grep -q 'CLOCK SUMMARY' /tmp/clock.out || { sum "gpu-clock: $label: no summary"; fail=1; }
}

sleep 20
perf "idle before the first run"
probe "cold run" 12 0
perf "after the first run"
sleep 12
perf "12 s after the first run"
probe "second cold run" 12 0
sleep 12
perf "12 s idle again"
probe "light duty" 10 8
perf "after the light duty"

SNAKE3D_AUTOPLAY=1 SNAKE3D_SECONDS=14 NVK_CONSTANOS_DEBUG=1 /mnt/bin/snake3d > /tmp/snake3d.out 2>&1 &
snake=$!
sleep 2
probe "beside snake3d" 10 50
perf "beside snake3d"
wait $snake
grep -E 'SNAKE3D DONE|per second' /tmp/snake3d.out | while read -r l; do sum "gpu-clock: snake: $l"; done
grep -q 'SNAKE3D DONE' /tmp/snake3d.out || { sum "gpu-clock: snake3d did not finish"; fail=1; }

u=$(grep '^gpu_uapi:' /proc/kdebug)
sum "gpu-clock: end: $u"
echo "$u" | tr ' ' '\n' | grep -q '^dead=0$' || { sum "gpu-clock: the GPU was declared dead"; fail=1; }
echo 'gsp name' > /dev/dispctl && sum "gpu-clock: RM still answers" || { sum "gpu-clock: RM does not answer"; fail=1; }

echo "---- summary (the log wraps) ----"
cat /tmp/gpu-clock.sum
sum "gpu-clock: verdict exit=$fail"
exit $fail
