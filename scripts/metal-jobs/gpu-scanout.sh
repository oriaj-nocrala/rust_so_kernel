# Phase 5.3 of docs/gpu/gpu-plan.md, on the Ryzen:
#   touch build.rs   # the root build does not watch nvgpu
#   scripts/metal-run.sh --kconf 'gpu=scanout' scripts/metal-jobs/gpu-scanout.sh
# At boot, after the channels came up (5.2), window 0 moved from the GOP
# framebuffer to this kernel's two VRAM buffers: three flips, each timed
# against the raster and the head's LOADV/VBLANK bits, and the GOP
# framebuffer painted dark red (`scanout:` lines of /proc/gpu). This prints
# them and fails unless:
# - the sequence reached its end ("scanout: OK: ...");
# - the head still runs at 60.0 +- 0.1 vblanks per second over 10 s, with no
#   display interrupt the handler does not service;
# - `compositor fire` for 20 s presents by page flips: at least 600 flips
#   latched (30 per second), none refused.
# The eyes' part: the screen is never red, and fire does not tear.
cat /proc/gpu
fail=0
grep -q '^scanout: OK: ' /proc/gpu || { echo "gpu-scanout: the scanout sequence did not finish OK"; fail=1; }
grep '^scanout: STOP' /proc/gpu && fail=1
sample() { grep '^gpu_vblank:' /proc/kdebug; }
flips() { grep '^gpu_flip:' /proc/kdebug; }
field() { echo "$1" | tr ' ' '\n' | grep "^$2=" | cut -d= -f2; }
s1=$(sample)
sleep 10
s2=$(sample)
echo "gpu-scanout: t0 $s1"
echo "gpu-scanout: t1 $s2"
[ "$(field "$s2" enabled)" = 1 ] || { echo "gpu-scanout: vblank MSI not enabled"; fail=1; }
for k in spurious blocked gone; do
  [ "$(field "$s2" $k)" = 0 ] || { echo "gpu-scanout: $k=$(field "$s2" $k)"; fail=1; }
done
[ "$(field "$s2" disp_other)" = 0x0 ] || { echo "gpu-scanout: disp_other=$(field "$s2" disp_other)"; fail=1; }
q1=$(field "$s1" seq); q2=$(field "$s2" seq)
n1=$(field "$s1" last_ns); n2=$(field "$s2" last_ns)
rate=$(awk -v a="$q1" -v b="$q2" -v x="$n1" -v y="$n2" 'BEGIN { if (y > x) printf "%.4f", (b - a) * 1e9 / (y - x); else print "0" }')
echo "gpu-scanout: $((q2 - q1)) vblanks, rate $rate Hz"
awk -v r="$rate" 'BEGIN { exit !(r >= 59.9 && r <= 60.1) }' || { echo "gpu-scanout: rate $rate outside 60.0 +- 0.1"; fail=1; }

f1=$(flips)
echo "gpu-scanout: before fire $f1"
echo "gpu-scanout: compositor fire for 20 s (tearing photo now)"
# Not `timeout`: BusyBox's polls the child with kill(pid, 0), which this
# kernel still answers EINVAL.
compositor fire &
sleep 20
kill -TERM $!
wait
f2=$(flips)
echo "gpu-scanout: after fire  $f2"
grep '^gpu_vblank' /proc/kdebug
l1=$(field "$f1" latched); l2=$(field "$f2" latched)
echo "gpu-scanout: $((l2 - l1)) flips latched during fire"
[ $((l2 - l1)) -ge 600 ] || { echo "gpu-scanout: fewer than 600 flips in 20 s"; fail=1; }
[ "$(field "$f2" refused)" = 0 ] || { echo "gpu-scanout: refused=$(field "$f2" refused)"; fail=1; }
# Again at the end: fire's traffic wraps the 64 KiB log slot (boot #75
# lost the boot-time flip timings printed at the top).
grep '^scanout:' /proc/gpu
echo "gpu-scanout: verdict exit=$fail; 15 s for a look (the console must be back)"
echo "gpu-scanout: console-is-back"
sleep 15
exit $fail
