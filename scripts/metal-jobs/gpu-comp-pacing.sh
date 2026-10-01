# Where does a compositor frame's time go, and why is it 9 ms at the GPU's lowest P-state (Ryzen #190: `present` 1.8 / 4.2 / 9.1 ms at P0 / P5 / P8)?
# docs/gpu/hw-cursor-plan.md section 12. A modern GPU should not need ms to draw a background and three rectangles: this separates the GPU executing
# slowly, the work waiting behind another channel's, and the wait for the previous flip. Measures only; changes no clock and nothing else.
#   touch build.rs; echo 3 > target/metal/budget
#   scripts/metal-run.sh --kconf 'gpu=uapi' scripts/metal-jobs/gpu-comp-pacing.sh
# The kernel's pacing instrument (`/proc/kdebug` `gpu_pacing:` and `gpu_pacing_trace:`, `dispctl trace reset`; nvgpu::pacing) records, per channel, the time
# from a submission to the first look that found it done, how long after the previous vblank each PRESENT arrives (buckets of 2 ms: the deadline is
# ~9 ms) and how long the PRESENT ioctl takes, and the latest 60 events: S<chan>.<seq> submitted, D<chan>.<seq> seen done, P<us after the vblank> a
# PRESENT, E<us> it returned, V<seq> a vblank. No mouse is read (nobody has to do anything): vk_comp runs on its own with cpumon + snake3d autoplay.
#   L  960x540 snake window, 30 s: the P-state decays P0 -> P5 -> P8 under this light load, so one run shows all three regimes
#   H  1880x1000 snake window, 15 s: the heavier load that keeps the GPU at a high P-state
# The statistics are read and reset every 5 s together with the P-state, so each window says what the clocks were while it was measured; chosen windows
# also print the event trace (read before the reset).
#   C  cpumon only, 20 s: the control, no other GPU channel (no context switch between channels) Passes unless vk_comp fails or a channel dies.
sumfile=/tmp/gpu-comp-pacing.sum
fail=0
sum() { echo "$*"; echo "$*" >> $sumfile; }
: > $sumfile
field() { echo "$1" | tr ' ' '\n' | grep "^$2=" | cut -d= -f2; }
upi() { cut -d. -f1 /proc/uptime; }
sum "gpu-comp-pacing: start at uptime $(cut -d' ' -f1 /proc/uptime) s"
grep -q '^gsp: OK' /proc/gpu || { sum "gpu-comp-pacing: gsp did not boot"; fail=1; }
pst() { echo 'gsp pstate' > /dev/dispctl 2>/dev/null && grep '^gpu_perf:' /proc/kdebug | sed 's/gpu_perf: pstate=//; s/ .*//' || echo '?'; }
window() { # window <label> <show trace 0|1>: the P-state and the pacing since the last reset, then reset
  p=$(pst)
  sum "gpu-comp-pacing: $1 t=$(upi) P-state $p: $(grep '^gpu_pacing:' /proc/kdebug | cut -c13-420)"
  [ "$2" = 1 ] && sum "gpu-comp-pacing: $1 trace: $(grep '^gpu_pacing_trace:' /proc/kdebug | cut -c19-900)"
  echo 'trace reset' > /dev/dispctl 2>/dev/null
}
sampler() { # sampler <label> <seconds>
  n=0
  while [ $n -lt $(( $2 + 25 )) ] && ! grep -q '^COMP \(DONE\|FAILED\)' /tmp/comp.out 2>/dev/null; do
    sleep 1
    n=$((n + 1))
    if [ $((n % 5)) = 0 ]; then
      k=$((n / 5))
      case " $TRACE_WINDOWS " in *" $k "*) window "$1 window $k" 1 ;; *) window "$1 window $k" 0 ;; esac
    fi
  done
}
run() { # run <label> <secs> env... command...
  label=$1; secs=$2; shift 2
  dead0=$(field "$(grep '^gpu_uapi:' /proc/kdebug)" chans_dead)
  sum "gpu-comp-pacing: $label: start at uptime $(upi) s for $secs s"
  echo 'trace reset' > /dev/dispctl 2>/dev/null
  env COMP_NO_PANEL=1 COMP_NO_INPUT=1 COMP_SECONDS=$secs SNAKE3D_WINDOW=1 SNAKE3D_AUTOPLAY=1 NVK_CONSTANOS_DEBUG=1 "$@" > /tmp/comp.out 2>&1 &
  pid=$!
  sampler "$label" $secs
  wait $pid
  rc=$?
  dead1=$(field "$(grep '^gpu_uapi:' /proc/kdebug)" chans_dead)
  sum "gpu-comp-pacing: $label: ended, vk_comp exit=$rc, channels lost: $(( dead1 - dead0 ))"
  grep -E '^COMP (pace|quit|FAIL)|SNAKE3D [0-9]+ frames' /tmp/comp.out | cut -c1-300 | while read -r l; do sum "gpu-comp-pacing: $label: $l"; done
  [ "$rc" = 0 ] || fail=1
  [ "$dead1" = "$dead0" ] || fail=1
}
# Ryzen #192 found, per frame: the compositor's render submission takes 1.8 ms at P0, 3.4 at P5, 6-7 at P8 (6.0-6.5 with only cpumon: not waiting for another
# channel), the blit 0.13 / 1.0 / 2.3 ms (memory-clock bound), PRESENT itself ~5 us. The suspect for the render floor: the CPU-drawn windows (cpumon's 4.7 MB)
# read from system memory by the shader every frame. So, in this order (cpumon first, right after the boot, while the GPU is still at P0):
#   C  cpumon only (a CPU window, no other GPU channel), 20 s   S  snake3d only (a GPU window in VRAM, no CPU window), 20 s   L  both, 15 s
TRACE_WINDOWS="1 2 3"
run "C cpumon only" 20 /mnt/bin/vk_comp cpumon
TRACE_WINDOWS="2 3"
run "S snake3d only" 20 /mnt/bin/vk_comp snake3d
TRACE_WINDOWS="3"
run "L 960x540 both" 15 /mnt/bin/vk_comp cpumon snake3d

sum "gpu-comp-pacing: end: $(grep '^gpu_uapi:' /proc/kdebug | cut -c1-200)"
sum "gpu-comp-pacing: $(grep '^gpu_share:' /proc/kdebug)"
grep -a -E '\[nvgpu\] channel .* is dead|\[gsp\] event' /proc/dmesg | tail -n 4 | cut -c1-300 | while read -r l; do sum "gpu-comp-pacing: kernel: $l"; done
echo "---- summary (the log wraps) ----"
cat $sumfile
sum "gpu-comp-pacing: verdict exit=$fail at uptime $(cut -d' ' -f1 /proc/uptime) s"
exit $fail
