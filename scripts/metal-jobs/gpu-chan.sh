# Phase 5.2 of docs/gpu/gpu-plan.md, on the Ryzen:
#   scripts/metal-run.sh --kconf 'gpu=chan' scripts/metal-jobs/gpu-chan.sh
# At boot the kernel brought up the display's instance memory, the core
# channel and window 0, and pushed one UPDATE on each that repeats what the
# GOP left (`chan:` lines of /proc/gpu). This prints them and fails unless:
# - the sequence reached its end ("chan: OK: ...": both gates passed, no
#   exception or supervisor, both UPDATEs latched only the pushed methods);
# - the head still scans out: vblanks at 60.0 +- 0.1 per second over 10 s,
#   and no display interrupt the vblank handler does not service.
# The image must not change at any point: that part is the user's eyes
# (the job waits 20 s at the end with the console on screen).
cat /proc/gpu
fail=0
grep -q '^chan: OK: ' /proc/gpu || { echo "gpu-chan: the channel sequence did not finish OK"; fail=1; }
grep '^chan: STOP' /proc/gpu && fail=1
sample() { grep '^gpu_vblank:' /proc/kdebug; }
field() { echo "$1" | tr ' ' '\n' | grep "^$2=" | cut -d= -f2; }
s1=$(sample)
sleep 10
s2=$(sample)
echo "gpu-chan: t0 $s1"
echo "gpu-chan: t1 $s2"
[ "$(field "$s2" enabled)" = 1 ] || { echo "gpu-chan: vblank MSI not enabled"; fail=1; }
for k in spurious blocked gone; do
  [ "$(field "$s2" $k)" = 0 ] || { echo "gpu-chan: $k=$(field "$s2" $k)"; fail=1; }
done
[ "$(field "$s2" disp_other)" = 0x0 ] || { echo "gpu-chan: disp_other=$(field "$s2" disp_other)"; fail=1; }
q1=$(field "$s1" seq); q2=$(field "$s2" seq)
n1=$(field "$s1" last_ns); n2=$(field "$s2" last_ns)
rate=$(awk -v a="$q1" -v b="$q2" -v x="$n1" -v y="$n2" 'BEGIN { if (y > x) printf "%.4f", (b - a) * 1e9 / (y - x); else print "0" }')
echo "gpu-chan: $((q2 - q1)) vblanks, rate $rate Hz"
awk -v r="$rate" 'BEGIN { exit !(r >= 59.9 && r <= 60.1) }' || { echo "gpu-chan: rate $rate outside 60.0 +- 0.1"; fail=1; }
echo "gpu-chan: verdict exit=$fail; 20 s for a look at the screen (it must be the usual console)"
sleep 20
exit $fail
