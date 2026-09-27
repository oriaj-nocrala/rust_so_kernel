# Phase 5.6 of docs/gpu/gpu-plan.md, on the Ryzen:
#   touch build.rs   # the root build does not watch nvgpu
#   scripts/metal-run.sh --kconf 'gpu=dplink' scripts/metal-jobs/gpu-dplink.sh
# Everything gpu=vpll does, plus /dev/dispctl `train`. This job retrains the
# ASUS's DP link at the same mode (1080p60): detach, train 4 lanes at HBR2
# (nouveau's link; the GOP trains 2x HBR), attach, 15 s; then the same back
# to the GOP's 2x HBR. Fails unless:
# - boot found the output's DP table entry ("dplink: ready");
# - `train` is refused while attached (EAGAIN) and for HBR3 (EINVAL: the
#   ASUS tops at HBR2);
# - each training says OK and the SOR's link reads back as asked, 3 more
#   supervisors per attach, no script error, nothing undone, no CTRL_DISP
#   error;
# - vblank runs at 60.0 +- 0.1 Hz after each attach, with no display
#   interrupt the handler does not service.
# The eyes' part: two blinks of a few seconds, the same image after each
# (the ASUS's menu shows 1920x1080 60 Hz either way). Fire runs 10 s at the end.
cat /proc/gpu
fail=0
sum() { echo "$*"; echo "$*" >> /tmp/gpu-dplink.sum; }
: > /tmp/gpu-dplink.sum
grep '^dplink:' /proc/gpu >> /tmp/gpu-dplink.sum
grep -q '^dplink: ready' /proc/gpu || { sum "gpu-dplink: dplink not set up"; fail=1; }
grep '^super: STOP\|^dplink: STOP' /proc/gpu && fail=1
field() { echo "$1" | tr ' ' '\n' | grep "^$2=" | cut -d= -f2; }
supk() { grep '^gpu_super:' /proc/kdebug; }
dpk() { grep '^gpu_dplink:' /proc/kdebug; }
sum "gpu-dplink: before: $(cat /dev/dispctl)"

rate() {
  s1=$(grep '^gpu_vblank:' /proc/kdebug)
  sleep 10
  s2=$(grep '^gpu_vblank:' /proc/kdebug)
  for f in spurious blocked gone; do
    [ "$(field "$s2" $f)" = 0 ] || { sum "gpu-dplink: $f=$(field "$s2" $f)"; fail=1; }
  done
  [ "$(field "$s2" disp_other)" = 0x0 ] || { sum "gpu-dplink: disp_other=$(field "$s2" disp_other)"; fail=1; }
  r=$(awk -v a="$(field "$s1" seq)" -v b="$(field "$s2" seq)" -v x="$(field "$s1" last_ns)" -v y="$(field "$s2" last_ns)" \
    'BEGIN { if (y > x) printf "%.4f", (b - a) * 1e9 / (y - x); else print "0" }')
}

rate
sum "gpu-dplink: GOP rate: $r Hz"

# Attached: refused.
echo "train 4 0x14" > /dev/dispctl 2>/dev/null && { sum "gpu-dplink: train accepted while attached"; fail=1; }

# $1 lanes, $2 rate, $3 expected link text in the status, $4 attaches so far.
retrain() {
  echo detach > /dev/dispctl || { sum "gpu-dplink: detach refused"; fail=1; return; }
  i=0
  until cat /dev/dispctl | grep -q ' free 1 '; do
    i=$((i + 1)); [ $i -ge 20 ] && { sum "gpu-dplink: SOR never free after detach"; fail=1; return; }
    sleep 1
  done
  if [ "$2" = 0x14 ]; then
    echo "train 4 0x1e" > /dev/dispctl 2>/dev/null && { sum "gpu-dplink: HBR3 accepted"; fail=1; }
  fi
  if echo "train $1 $2" > /dev/dispctl; then
    sum "gpu-dplink: train $1 $2 accepted"
  else
    sum "gpu-dplink: train $1 $2 FAILED"; fail=1
  fi
  grep '^dplink: train' /proc/gpu | tail -1 >> /tmp/gpu-dplink.sum
  st=$(cat /dev/dispctl)
  sum "gpu-dplink: trained: $st"
  echo "$st" | grep -q " link $3 " || { sum "gpu-dplink: SOR link is not $3"; fail=1; }
  i=0
  until echo attach > /dev/dispctl; do
    i=$((i + 1)); [ $i -ge 10 ] && { sum "gpu-dplink: attach refused 10 times"; fail=1; return; }
    sleep 1
  done
  sleep 3
  st=$(cat /dev/dispctl); k=$(supk)
  sum "gpu-dplink: attached: $st"
  sum "gpu-dplink: attached: $k"
  grep '^super: 2 ' /proc/gpu | tail -1 >> /tmp/gpu-dplink.sum
  echo "$st" | grep -q "armed control 0x901 " || { sum "gpu-dplink: not attached back"; fail=1; }
  n=$(( $4 * 2 ))
  [ "$(field "$k" serviced)" = "$n,$n,$n" ] || { sum "gpu-dplink: serviced=$(field "$k" serviced), want $n,$n,$n"; fail=1; }
  rate
  sum "gpu-dplink: after $1 $2: $r Hz"
  awk -v r="$r" 'BEGIN { exit !(r >= 59.9 && r <= 60.1) }' || { sum "gpu-dplink: rate $r outside 60 +- 0.1"; fail=1; }
}

retrain 4 0x14 4x0x14ef 1
echo "gpu-dplink: 4x HBR2 now: 15 s for a look"
sleep 15
retrain 2 0x0a 2x0xaef 2

k=$(supk); p=$(dpk)
sum "gpu-dplink: end: $k"
sum "gpu-dplink: end: $p"
for f in script_errors not_done ctrl_disp_errors; do
  [ "$(field "$k" $f)" = 0 ] || { sum "gpu-dplink: $f=$(field "$k" $f)"; fail=1; }
done
[ "$(field "$p" ok)" = 2 ] || { sum "gpu-dplink: ok=$(field "$p" ok), want 2"; fail=1; }
[ "$(field "$p" failed)" = 0 ] || { sum "gpu-dplink: failed=$(field "$p" failed)"; fail=1; }

echo "gpu-dplink: compositor fire for 10 s"
compositor fire &
sleep 10
kill -TERM $!
wait
grep '^gpu_flip:' /proc/kdebug
grep '^dplink:\|^super:\|^dispctl:' /proc/gpu
cat /tmp/gpu-dplink.sum
echo "gpu-dplink: verdict exit=$fail; 15 s for a look (the console must be back)"
echo "gpu-dplink: console-is-back"
sleep 15
exit $fail
