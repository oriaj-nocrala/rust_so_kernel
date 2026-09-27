# Phase 3 of docs/gpu/gpu-plan.md, on the Ryzen:
#   scripts/metal-run.sh --kconf 'gpu=vblank' scripts/metal-jobs/gpu-vblank.sh
# Prints /proc/gpu (heads the GOP lit, their timing, the interrupt state
# before arming, the raster-position rate = plan B's instrument) and the
# vblank counters, and fails unless:
# - the MSI is enabled and vblanks of the primary head arrive at
#   60.0 +- 0.1 per second over 30 s (sequence numbers and their ns
#   timestamps, both from the handler: no sleep jitter in the rate);
# - no interrupt was spurious, blocked or found the GPU off the bus.
# Then runs `compositor fire` for 20 s, paced by /dev/vblank, for the
# tearing photo (not part of the verdict).
cat /proc/gpu
fail=0
sample() { grep '^gpu_vblank:' /proc/kdebug; }
field() { echo "$1" | tr ' ' '\n' | grep "^$2=" | cut -d= -f2; }
s1=$(sample)
echo "gpu-vblank: t0 $s1"
sleep 30
s2=$(sample)
echo "gpu-vblank: t1 $s2"
grep '^gpu_vblank' /proc/kdebug
[ "$(field "$s2" enabled)" = 1 ] || { echo "gpu-vblank: MSI not enabled"; fail=1; }
for k in spurious blocked gone; do
  [ "$(field "$s2" $k)" = 0 ] || { echo "gpu-vblank: $k=$(field "$s2" $k)"; fail=1; }
done
q1=$(field "$s1" seq); q2=$(field "$s2" seq)
n1=$(field "$s1" last_ns); n2=$(field "$s2" last_ns)
rate=$(awk -v a="$q1" -v b="$q2" -v x="$n1" -v y="$n2" 'BEGIN { if (y > x) printf "%.4f", (b - a) * 1e9 / (y - x); else print "0" }')
echo "gpu-vblank: $((q2 - q1)) vblanks, rate $rate Hz"
awk -v r="$rate" 'BEGIN { exit !(r >= 59.9 && r <= 60.1) }' || { echo "gpu-vblank: rate $rate outside 60.0 +- 0.1"; fail=1; }
echo "gpu-vblank: verdict so far exit=$fail; compositor fire for 20 s (photo now)"
# Not `timeout`: BusyBox's polls the child with kill(pid, 0), which this
# kernel still answers EINVAL, so it gives up at once.
compositor fire &
sleep 20
kill -TERM $!  # QUIT is ignored in a non-interactive shell's & jobs
wait
sample
exit $fail
