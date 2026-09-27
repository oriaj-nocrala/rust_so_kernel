# Phase 5.7 of docs/gpu/gpu-plan.md, on the Ryzen:
#   touch build.rs   # the root build does not watch nvgpu
#   scripts/metal-run.sh --kconf 'gpu=modes' scripts/metal-jobs/gpu-modes.sh
# Everything gpu=dplink does, plus /dev/dispctl `mode WxH@Hz`. This job sets
# the ASUS to 1920x1080 at 180 Hz (its EDID's DTD 4, 420.78 MHz: needs the
# link retrained to 4x HBR2), 30 s for a look at its menu and 10 s of
# `compositor fire`; then 120 Hz (CVT-RB2, generated: no EDID timing) and
# back to 60 Hz (EDID DTD 1, the GOP's mode), without rebooting. Fails
# unless:
# - boot listed the modes ("modes: ready");
# - requests that cannot work are refused before anything is touched
#   (EINVAL: 200 Hz, another size, 586 MHz, bad syntax);
# - each mode says OK, the head's ARMED raster/clock is the mode's, the
#   180 Hz one retrained the link (4x0x14, detach + attach) and the others
#   did not (one UPDATE with the SOR attached: Ryzen #83 showed a detach at
#   180 Hz stops vblank and delays the supervisors ~1 s);
# - vblank runs at the mode's rate +- 0.1 Hz (179.82, 120.00, 60.00), with
#   no display interrupt the handler does not service;
# - no script error, nothing undone, no CTRL_DISP error, no failed set.
# The eyes' part: three blinks; the ASUS's menu shows 180 Hz for 30 s, then
# 120, then 60 (it rounds to whole hertz).
cat /proc/gpu
fail=0
sum() { echo "$*"; echo "$*" >> /tmp/gpu-modes.sum; }
: > /tmp/gpu-modes.sum
grep '^modes:' /proc/gpu >> /tmp/gpu-modes.sum
grep -q '^modes: ready' /proc/gpu || { sum "gpu-modes: modes not set up"; fail=1; }
grep '^super: STOP\|^dplink: STOP\|^modes: STOP' /proc/gpu && fail=1
field() { echo "$1" | tr ' ' '\n' | grep "^$2=" | cut -d= -f2; }
supk() { grep '^gpu_super:' /proc/kdebug; }
modek() { grep '^gpu_mode:' /proc/kdebug; }
sum "gpu-modes: before: $(cat /dev/dispctl | tr '\n' ' ')"

rate() {
  s1=$(grep '^gpu_vblank:' /proc/kdebug)
  sleep 10
  s2=$(grep '^gpu_vblank:' /proc/kdebug)
  for f in spurious blocked gone; do
    [ "$(field "$s2" $f)" = 0 ] || { sum "gpu-modes: $f=$(field "$s2" $f)"; fail=1; }
  done
  [ "$(field "$s2" disp_other)" = 0x0 ] || { sum "gpu-modes: disp_other=$(field "$s2" disp_other)"; fail=1; }
  r=$(awk -v a="$(field "$s1" seq)" -v b="$(field "$s2" seq)" -v x="$(field "$s1" last_ns)" -v y="$(field "$s2" last_ns)" \
    'BEGIN { if (y > x) printf "%.4f", (b - a) * 1e9 / (y - x); else print "0" }')
}

rate
sum "gpu-modes: GOP rate: $r Hz"

# Refused before touching anything.
for bad in 1920x1080@200 1280x720@60 2560x1440@144 1920x1080 foo; do
  echo "mode $bad" > /dev/dispctl 2>/dev/null && { sum "gpu-modes: mode $bad accepted"; fail=1; }
done
k=$(modek)
[ "$(field "$k" refused)" = 5 ] || { sum "gpu-modes: refused=$(field "$k" refused), want 5"; fail=1; }
[ "$(field "$k" sets)" = 0 ] || { sum "gpu-modes: a refused request started a set: $k"; fail=1; }

# $1 request, $2 source in the status, $3 link, $4 min rate, $5 max rate,
# $6 retrains so far.
setmode() {
  if echo "mode $1" > /dev/dispctl; then
    sum "gpu-modes: mode $1 accepted"
  else
    sum "gpu-modes: mode $1 FAILED"; fail=1
  fi
  grep '^mode: ' /proc/gpu | tail -2 >> /tmp/gpu-modes.sum
  grep '^super: 2 ' /proc/gpu | tail -1 >> /tmp/gpu-modes.sum
  st=$(cat /dev/dispctl | tr '\n' ' ')
  sum "gpu-modes: $1: $st"
  echo "$st" | grep -q "armed control 0x901 " || { sum "gpu-modes: not attached"; fail=1; }
  echo "$st" | grep -q " link $3 " || { sum "gpu-modes: link is not $3"; fail=1; }
  echo "$st" | grep -q " $2 clock" || { sum "gpu-modes: mode is not $2"; fail=1; }
  k=$(modek)
  sum "gpu-modes: $1: $k"
  [ "$(field "$k" retrains)" = "$6" ] || { sum "gpu-modes: retrains=$(field "$k" retrains), want $6"; fail=1; }
  rate
  sum "gpu-modes: $1: $r Hz"
  awk -v r="$r" -v lo="$4" -v hi="$5" 'BEGIN { exit !(r >= lo && r <= hi) }' || { sum "gpu-modes: rate $r outside $4..$5"; fail=1; }
}

setmode 1920x1080@180 'Edid(3)' 4x0x14ef 179.72 179.92 1
echo "gpu-modes: 180 Hz now: 30 s to photograph the ASUS's menu"
sleep 30
f1=$(grep '^gpu_flip:' /proc/kdebug)
echo "gpu-modes: compositor fire at 180 Hz for 10 s"
compositor fire &
sleep 10
kill -TERM $!
wait
f2=$(grep '^gpu_flip:' /proc/kdebug)
sum "gpu-modes: flips before fire: $f1"
sum "gpu-modes: flips after fire: $f2"

setmode 1920x1080@120 'Cvt' 4x0x14ef 119.9 120.1 1
setmode 1920x1080@60 'Edid(0)' 4x0x14ef 59.9 60.1 1

k=$(supk); m=$(modek)
sum "gpu-modes: end: $k"
sum "gpu-modes: end: $m"
sum "gpu-modes: end: $(grep '^gpu_dplink:' /proc/kdebug)"
for f in script_errors not_done ctrl_disp_errors; do
  [ "$(field "$k" $f)" = 0 ] || { sum "gpu-modes: $f=$(field "$k" $f)"; fail=1; }
done
[ "$(field "$m" ok)" = 3 ] || { sum "gpu-modes: ok=$(field "$m" ok), want 3"; fail=1; }
[ "$(field "$m" failed)" = 0 ] || { sum "gpu-modes: failed=$(field "$m" failed)"; fail=1; }
# 180: detach + attach (2 rounds); 120 and 60: one UPDATE each, attached.
[ "$(field "$k" serviced)" = "4,4,4" ] || { sum "gpu-modes: serviced=$(field "$k" serviced), want 4,4,4"; fail=1; }

grep '^modes:\|^mode:\|^dplink:\|^super:\|^dispctl:' /proc/gpu
cat /tmp/gpu-modes.sum
echo "gpu-modes: verdict exit=$fail; 15 s for a look (the console must be back, 60 Hz)"
echo "gpu-modes: console-is-back"
sleep 15
exit $fail
