# Phase 5.8 of docs/gpu/gpu-plan.md, on the Ryzen:
#   touch build.rs   # the root build does not watch nvgpu
#   scripts/metal-run.sh --kconf 'gpu=hdmi' scripts/metal-jobs/gpu-hdmi.sh
# Everything gpu=modes does, plus /dev/dispctl `hdmi on` / `hdmi off`: the HP
# (HDMI-A-1) is lit at its preferred 1920x1080 60 Hz on head 1 / SOR-0 /
# window 2 with the kernel's own picture (colour bars, ramp, "constanos"
# panel, white border), while the ASUS keeps running. This job switches it
# on, leaves it 40 s for a look, checks it, switches it off, and on again.
# Fails unless:
# - boot said "hdmi: ready";
# - `hdmi on` succeeds and `hdmi off` succeeds, twice (the second `on` reuses
#   the channel);
# - each `on` served supervisors 1-3 once (serviced grows by 1,1,1), the
#   head's ARMED raster and clock are the mode's, and head 1's raster
#   refresh is 60 +- 0.5 Hz;
# - no script error, no CTRL_DISP error, and the ASUS's vblank stays 60 Hz
#   (or the mode it had) throughout.
# The eyes' part: the HP shows the picture for 40 s (border visible on all
# four edges, seven bars, the text), goes dark on `off`, lights again.
cat /proc/gpu
fail=0
sum() { echo "$*"; echo "$*" >> /tmp/gpu-hdmi.sum; }
: > /tmp/gpu-hdmi.sum
grep '^hdmi:' /proc/gpu >> /tmp/gpu-hdmi.sum
grep -q '^hdmi: ready' /proc/gpu || { sum "gpu-hdmi: hdmi not set up"; fail=1; }
grep '^super: STOP\|^hdmi: STOP\|^chan: STOP' /proc/gpu && fail=1
field() { echo "$1" | tr ' ' '\n' | grep "^$2=" | cut -d= -f2; }
supk() { grep '^gpu_super:' /proc/kdebug; }
hk() { grep '^gpu_hdmi:' /proc/kdebug; }
sum "gpu-hdmi: before: $(cat /dev/dispctl | tr '\n' ' ')"

rate() {
  s1=$(grep '^gpu_vblank:' /proc/kdebug)
  sleep 5
  s2=$(grep '^gpu_vblank:' /proc/kdebug)
  r=$(awk -v a="$(field "$s1" seq)" -v b="$(field "$s2" seq)" -v x="$(field "$s1" last_ns)" -v y="$(field "$s2" last_ns)" \
    'BEGIN { if (y > x) printf "%.4f", (b - a) * 1e9 / (y - x); else print "0" }')
}
rate
sum "gpu-hdmi: ASUS rate before: $r Hz"
asus0=$r

# $1 = expected serviced total after this `on` ("a,b,c").
turn_on() {
  if echo "hdmi on" > /dev/dispctl; then sum "gpu-hdmi: hdmi on accepted"; else sum "gpu-hdmi: hdmi on FAILED"; fail=1; fi
  grep '^hdmi:\|^super: \|^dispctl:' /proc/gpu | tail -40 >> /tmp/gpu-hdmi.sum
  k=$(hk); sum "gpu-hdmi: $k"
  echo "$k" | grep -q ' on=1 ' || { sum "gpu-hdmi: not on"; fail=1; }
  mhz=$(field "$k" refresh_mhz)
  awk -v m="$mhz" 'BEGIN { exit !(m >= 59500 && m <= 60500) }' || { sum "gpu-hdmi: head 1 refresh $mhz mHz outside 59.5..60.5 Hz"; fail=1; }
  sk=$(supk); sum "gpu-hdmi: on: $sk"
  [ "$(field "$sk" serviced)" = "$1" ] || { sum "gpu-hdmi: serviced=$(field "$sk" serviced), want $1"; fail=1; }
  sum "gpu-hdmi: status: $(cat /dev/dispctl | tr '\n' ' ')"
}
turn_off() {
  if echo "hdmi off" > /dev/dispctl; then sum "gpu-hdmi: hdmi off accepted"; else sum "gpu-hdmi: hdmi off FAILED"; fail=1; fi
  k=$(hk); sum "gpu-hdmi: $k"
  echo "$k" | grep -q ' on=0 ' || { sum "gpu-hdmi: not off"; fail=1; }
  sk=$(supk); sum "gpu-hdmi: off: $sk"
  [ "$(field "$sk" serviced)" = "$1" ] || { sum "gpu-hdmi: serviced=$(field "$sk" serviced), want $1"; fail=1; }
}

s0=$(supk); sum "gpu-hdmi: start: $s0"
base=$(field "$s0" serviced)   # a,b,c before anything
a=${base%%,*}
turn_on "$((a+1)),$((a+1)),$((a+1))"
echo "gpu-hdmi: the HP shows the picture: 40 s for a look (photo)"
sleep 40
rate
sum "gpu-hdmi: ASUS rate with the HP on: $r Hz"
awk -v r="$r" -v r0="$asus0" 'BEGIN { d = r - r0; if (d < 0) d = -d; exit !(d <= 0.3) }' || { sum "gpu-hdmi: the ASUS's rate moved ($asus0 -> $r)"; fail=1; }
turn_off "$((a+2)),$((a+2)),$((a+2))"
sleep 5
turn_on "$((a+3)),$((a+3)),$((a+3))"
sleep 15
turn_off "$((a+4)),$((a+4)),$((a+4))"

k=$(supk); h=$(hk)
sum "gpu-hdmi: end: $k"
sum "gpu-hdmi: end: $h"
for f in script_errors ctrl_disp_errors; do
  [ "$(field "$k" $f)" = 0 ] || { sum "gpu-hdmi: $f=$(field "$k" $f)"; fail=1; }
done
[ "$(field "$h" ok)" = 2 ] || { sum "gpu-hdmi: ok=$(field "$h" ok), want 2"; fail=1; }
[ "$(field "$h" failed)" = 0 ] || { sum "gpu-hdmi: failed=$(field "$h" failed)"; fail=1; }
f1=$(grep '^gpu_vblank:' /proc/kdebug)
for f in spurious blocked gone; do
  [ "$(field "$f1" $f)" = 0 ] || { sum "gpu-hdmi: $f=$(field "$f1" $f)"; fail=1; }
done

grep '^hdmi:\|^super:\|^dispctl:' /proc/gpu
cat /tmp/gpu-hdmi.sum
echo "gpu-hdmi: verdict exit=$fail; 15 s for a look (the console must be back)"
echo "gpu-hdmi: console-is-back"
sleep 15
exit $fail
