# Is the 30 fps lock of Ryzen #189 (load 2: cpumon + snake3d in a 960x540 window, `present` a constant 9.1 ms) about the GPU's clocks, or about the
# mouse? Findings so far (docs/gpu/hw-cursor-plan.md section 11): with the mouse moving, 960x540 -> 30 fps lock; 1880x1000 -> 59 fps; in Ryzen #188
# phase A (960x540, NO mouse read) the same load ran at ~51 fps. Two suspects: the GPU's P-state (a light load keeps it low) and the pointer's
# extra compositions. This job measures; it does NOT change any clock (and pinning the GPU high for a compositor's whole life would not be a design:
# peak watts for an idle desktop; if clocks are the cause, the answer is to make `present` cheap or to boost for a burst, not to pin).
#   touch build.rs; echo 3 > target/metal/budget
#   scripts/metal-run.sh --kconf 'gpu=uapi' scripts/metal-jobs/gpu-cursor-pstate.sh
# `echo 'gsp pstate' > /dev/dispctl` is one RM control (the current P-state, `gpu_perf: pstate=Pn`), sampled once a second by a loop beside the load.
# Phases (15 s each; cpumon + snake3d autoplay in a window, unless said):
#   P1  960x540, vk_comp does NOT read the mouse (COMP_NO_INPUT): the person leaves the mouse alone
#   P2  960x540, the mouse read, the person moving it ALL THE TIME           (the 30 fps lock of #189?)
#   P3  960x540, the mouse read and moved, NO sampler (does asking RM for the P-state change the result?)
#   P4  1880x1000, the mouse read and moved                                  (60 fps in #189)
#   P5  vk_comp alone, the mouse read and moved
# For each: the compositor's `pace (5 s)` lines (flips < 20 ms = 60 fps, 20-37 ms = 30 fps, and `present`), its `input` and quit lines, the snake's
# frame rate, and the P-state trace `t=<uptime s>:Pn` so the P-state can be set against the 5 s windows. Passes unless vk_comp fails or a channel dies.
sumfile=/tmp/gpu-cursor-pstate.sum
fail=0
sum() { echo "$*"; echo "$*" >> $sumfile; }
: > $sumfile
field() { echo "$1" | tr ' ' '\n' | grep "^$2=" | cut -d= -f2; }
up() { cut -d' ' -f1 /proc/uptime; }
upi() { cut -d. -f1 /proc/uptime; }
sum "gpu-cursor-pstate: start at uptime $(up) s"
grep -q '^gsp: OK' /proc/gpu || { sum "gpu-cursor-pstate: gsp did not boot"; fail=1; }
pst() { echo 'gsp pstate' > /dev/dispctl 2>/dev/null && grep '^gpu_perf:' /proc/kdebug | sed 's/gpu_perf: pstate=//; s/ .*//' || echo '?'; }
# sampler <max seconds>: the P-state once a second until vk_comp prints its last line (`kill -0` is no use: a child that has exited stays a zombie
# until `wait`), as `t:Pn` pairs in $trace
sampler() {
  trace=""
  n=0
  while [ $n -lt $1 ] && ! grep -q '^COMP \(DONE\|FAILED\)' /tmp/comp.out 2>/dev/null; do
    trace="$trace $(upi):$(pst)"
    sleep 1
    n=$((n + 1))
  done
}

sum "gpu-cursor-pstate: idle: $(pst) $(sleep 1; pst) $(sleep 1; pst) (the P-state at rest before any load)"

run() { # run <label> <sampler 0|1> <secs> env... command...
  label=$1; samp=$2; secs=$3; shift 3
  dead0=$(field "$(grep '^gpu_uapi:' /proc/kdebug)" chans_dead)
  t0=$(upi)
  sum "gpu-cursor-pstate: $label: start at uptime $t0 s for $secs s"
  env COMP_NO_PANEL=1 COMP_SECONDS=$secs SNAKE3D_WINDOW=1 SNAKE3D_AUTOPLAY=1 NVK_CONSTANOS_DEBUG=1 "$@" > /tmp/comp.out 2>&1 &
  pid=$!
  trace=""
  if [ "$samp" = 1 ]; then sampler $((secs + 25)); fi
  wait $pid
  rc=$?
  dead1=$(field "$(grep '^gpu_uapi:' /proc/kdebug)" chans_dead)
  sum "gpu-cursor-pstate: $label: ended after $(( $(upi) - t0 )) s, vk_comp exit=$rc, channels lost: $(( dead1 - dead0 ))"
  [ -n "$trace" ] && sum "gpu-cursor-pstate: $label: P-state t:Pn$trace"
  grep -E '^COMP (input|pace|quit|FAIL|ended)|SNAKE3D [0-9]+ frames|SNAKE3D (resized|window)' /tmp/comp.out | cut -c1-330 | while read -r l; do sum "gpu-cursor-pstate: $label: $l"; done
  [ "$rc" = 0 ] || fail=1
  [ "$dead1" = "$dead0" ] || fail=1
}
run "P1 960x540 mouse NOT read" 1 15 COMP_NO_INPUT=1 /mnt/bin/vk_comp cpumon snake3d
run "P2 960x540 mouse read+moved" 1 15 /mnt/bin/vk_comp cpumon snake3d
run "P3 960x540 mouse read+moved, no sampler" 0 15 /mnt/bin/vk_comp cpumon snake3d
run "P4 1880x1000 mouse read+moved" 1 15 SNAKE3D_W=1880 SNAKE3D_H=1000 /mnt/bin/vk_comp cpumon snake3d
run "P5 alone mouse read+moved" 1 10 /mnt/bin/vk_comp

sum "gpu-cursor-pstate: after: $(pst) (the P-state right after the loads)"
sum "gpu-cursor-pstate: end: $(grep '^gpu_uapi:' /proc/kdebug | cut -c1-260)"
sum "gpu-cursor-pstate: $(grep '^gpu_share:' /proc/kdebug)"
grep -a -E '\[nvgpu\] channel .* is dead|\[gsp\] event' /proc/dmesg | tail -n 6 | cut -c1-300 | while read -r l; do sum "gpu-cursor-pstate: kernel: $l"; done
echo "---- summary (the log wraps) ----"
cat $sumfile
sum "gpu-cursor-pstate: verdict exit=$fail at uptime $(up) s"
exit $fail
