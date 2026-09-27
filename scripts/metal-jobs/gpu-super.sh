# Phase 5.4 of docs/gpu/gpu-plan.md, on the Ryzen:
#   touch build.rs   # the root build does not watch nvgpu
#   scripts/metal-run.sh --kconf 'gpu=super' scripts/metal-jobs/gpu-super.sh
# Everything gpu=scanout does, plus the display's supervisor interrupts
# serviced from the vblank MSI handler. This job asks, through
# /dev/dispctl, for SOR-1 to be detached from head 0 (the screen goes dark:
# the monitor may show "no signal") and, 5 s later, attached back with the
# GOP's control value, the GOP's link and clock. Each request runs
# supervisors 1, 2, 3 (`super:` lines in /proc/gpu). Fails unless:
# - boot set the supervisors up ("super: ready");
# - after each request the SOR's ARMED control is what was asked (0, then
#   the GOP's), and 3 more supervisors were serviced, with no script error,
#   nothing left undone and no CTRL_DISP error;
# - the head still runs at 60.0 +- 0.1 vblanks per second afterwards, with
#   no display interrupt the handler does not service.
# The eyes' part: dark for ~5 s, then the image comes back (fire runs 10 s
# at the end to show the flips still work).
cat /proc/gpu
fail=0
grep -q '^super: ready' /proc/gpu || { echo "gpu-super: supervisors not set up"; fail=1; }
grep '^super: STOP' /proc/gpu && fail=1
field() { echo "$1" | tr ' ' '\n' | grep "^$2=" | cut -d= -f2; }
supk() { grep '^gpu_super:' /proc/kdebug; }
gop=$(cat /dev/dispctl | sed -n 's/.*(GOP \(0x[0-9a-f]*\)).*/\1/p')
echo "gpu-super: before: $(cat /dev/dispctl)"
echo "gpu-super: before: $(supk)"

# $1 = detach|attach, $2 = the ARMED control expected, $3 = serviced count.
request() {
  i=0
  until echo "$1" > /dev/dispctl; do
    i=$((i + 1)); [ $i -ge 10 ] && { echo "gpu-super: $1 refused 10 times"; fail=1; return; }
    sleep 1
  done
  echo "gpu-super: $1 requested"
  sleep 5
  st=$(cat /dev/dispctl); k=$(supk)
  echo "gpu-super: after $1: $st"
  echo "gpu-super: after $1: $k"
  echo "$st" | grep -q "armed control $2 " || { echo "gpu-super: after $1 the ARMED control is not $2"; fail=1; }
  [ "$(field "$k" serviced)" = "$3,$3,$3" ] || { echo "gpu-super: after $1 serviced=$(field "$k" serviced), want $3,$3,$3"; fail=1; }
}
request detach 0x0 1
request attach "$gop" 2

k=$(supk)
for f in script_errors not_done ctrl_disp_errors; do
  [ "$(field "$k" $f)" = 0 ] || { echo "gpu-super: $f=$(field "$k" $f)"; fail=1; }
done
grep '^super: [123] \|^dispctl:' /proc/gpu

sample() { grep '^gpu_vblank:' /proc/kdebug; }
s1=$(sample)
sleep 10
s2=$(sample)
echo "gpu-super: t0 $s1"
echo "gpu-super: t1 $s2"
for f in spurious blocked gone; do
  [ "$(field "$s2" $f)" = 0 ] || { echo "gpu-super: $f=$(field "$s2" $f)"; fail=1; }
done
[ "$(field "$s2" disp_other)" = 0x0 ] || { echo "gpu-super: disp_other=$(field "$s2" disp_other)"; fail=1; }
q1=$(field "$s1" seq); q2=$(field "$s2" seq)
n1=$(field "$s1" last_ns); n2=$(field "$s2" last_ns)
rate=$(awk -v a="$q1" -v b="$q2" -v x="$n1" -v y="$n2" 'BEGIN { if (y > x) printf "%.4f", (b - a) * 1e9 / (y - x); else print "0" }')
echo "gpu-super: $((q2 - q1)) vblanks, rate $rate Hz"
awk -v r="$rate" 'BEGIN { exit !(r >= 59.9 && r <= 60.1) }' || { echo "gpu-super: rate $rate outside 60.0 +- 0.1"; fail=1; }

echo "gpu-super: compositor fire for 10 s"
# Not `timeout`: BusyBox's polls the child with kill(pid, 0) (EINVAL here).
compositor fire &
sleep 10
kill -TERM $!
wait
grep '^gpu_flip:' /proc/kdebug
# Again at the end, in case the log slot wrapped.
grep '^super:\|^dispctl:' /proc/gpu
echo "gpu-super: verdict exit=$fail; 15 s for a look (the console must be back)"
echo "gpu-super: console-is-back"
sleep 15
exit $fail
