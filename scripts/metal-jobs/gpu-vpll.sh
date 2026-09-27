# Phase 5.5 of docs/gpu/gpu-plan.md, on the Ryzen:
#   touch build.rs   # the root build does not watch nvgpu
#   scripts/metal-run.sh --kconf 'gpu=vpll' scripts/metal-jobs/gpu-vpll.sh
# Everything gpu=super does, plus supervisor 2.1 programming the head's
# VPLL. Through /dev/dispctl, this job moves head 0 from the GOP's 148.5 MHz
# to 123.75 MHz on the same raster (2200x1125: 1080p at 50 Hz), holds it
# 20 s, and goes back (`clock 0` = the GOP's clock). Fails unless:
# - boot set the supervisors up and found VPLL0's limits ("vpll: VPLL0");
# - after each request the ARMED pixel clock is what was asked, VPLL0 holds
#   the coefficients for it (0x3b12ab/0xd0001 at 123.75 MHz,
#   0x370000/0xa0001 at 148.5: the GOP's; nouveau's differ, see
#   nvgpu/src/pll.rs), 3 more supervisors
#   were serviced, one more VPLL programmed, and no script error, nothing
#   left undone and no CTRL_DISP error;
# - vblank runs at 50.0 +- 0.1 Hz after the first and 60.0 +- 0.1 Hz after
#   the second, with no display interrupt the handler does not service.
# The eyes' part: a blink, then the same image; during the 20 s at 50 Hz the
# ASUS's menu (information page) should say 50 Hz. Fire runs 10 s at the end.
cat /proc/gpu
fail=0
# The lines that matter, repeated at the end (the log slot wraps).
sum() { echo "$*"; echo "$*" >> /tmp/gpu-vpll.sum; }
: > /tmp/gpu-vpll.sum
grep '^vpll:' /proc/gpu >> /tmp/gpu-vpll.sum
grep -q '^super: ready' /proc/gpu || { sum "gpu-vpll: supervisors not set up"; fail=1; }
grep -q '^vpll: VPLL0 limits' /proc/gpu || { sum "gpu-vpll: no VPLL0 limits"; fail=1; }
grep '^super: STOP' /proc/gpu && fail=1
field() { echo "$1" | tr ' ' '\n' | grep "^$2=" | cut -d= -f2; }
supk() { grep '^gpu_super:' /proc/kdebug; }
sum "gpu-vpll: before: $(cat /dev/dispctl)"
sum "gpu-vpll: before: $(supk)"

# Vblanks per second over 10 s into $r, from /proc/kdebug's counter and
# timestamp (not by $(...): a subshell would lose fail=1).
rate() {
  s1=$(grep '^gpu_vblank:' /proc/kdebug)
  sleep 10
  s2=$(grep '^gpu_vblank:' /proc/kdebug)
  for f in spurious blocked gone; do
    [ "$(field "$s2" $f)" = 0 ] || { sum "gpu-vpll: $f=$(field "$s2" $f)"; fail=1; }
  done
  [ "$(field "$s2" disp_other)" = 0x0 ] || { sum "gpu-vpll: disp_other=$(field "$s2" disp_other)"; fail=1; }
  r=$(awk -v a="$(field "$s1" seq)" -v b="$(field "$s2" seq)" -v x="$(field "$s1" last_ns)" -v y="$(field "$s2" last_ns)" \
    'BEGIN { if (y > x) printf "%.4f", (b - a) * 1e9 / (y - x); else print "0" }')
}

# $1 = kHz to ask for, $2 = ARMED Hz expected, $3/$4 = VPLL0 N/fN and P/M
# words, $5 = requests so far, $6 = refresh expected.
request() {
  i=0
  until echo "clock $1" > /dev/dispctl; do
    i=$((i + 1)); [ $i -ge 10 ] && { sum "gpu-vpll: clock $1 refused 10 times"; fail=1; return; }
    sleep 1
  done
  echo "gpu-vpll: clock $1 requested"
  sleep 3
  st=$(cat /dev/dispctl); k=$(supk)
  sum "gpu-vpll: after clock $1: $st"
  sum "gpu-vpll: after clock $1: $k"
  echo "$st" | grep -q "armed pixclk $2 " || { sum "gpu-vpll: after clock $1 the ARMED pixel clock is not $2"; fail=1; }
  echo "$st" | grep -q "vpll $3 $4 " || { sum "gpu-vpll: after clock $1 VPLL0 is not $3 $4"; fail=1; }
  [ "$(field "$k" serviced)" = "$5,$5,$5" ] || { sum "gpu-vpll: serviced=$(field "$k" serviced), want $5,$5,$5"; fail=1; }
  [ "$(field "$k" clocks_set)" = "$5" ] || { sum "gpu-vpll: clocks_set=$(field "$k" clocks_set), want $5"; fail=1; }
  rate
  sum "gpu-vpll: at clock $1: $r Hz"
  awk -v r="$r" -v w="$6" 'BEGIN { exit !(r >= w - 0.1 && r <= w + 0.1) }' || { sum "gpu-vpll: rate $r outside $6 +- 0.1"; fail=1; }
}

# The GOP's rate first: the same instrument's noise, in the same boot.
rate
sum "gpu-vpll: GOP rate: $r Hz"
request 123750 123750000 0x3b12ab 0xd0001 1 50
echo "gpu-vpll: 50 Hz now: 10 more seconds for a look at the monitor's menu"
sleep 10
request 0 148500000 0x370000 0xa0001 2 60

k=$(supk)
for f in script_errors not_done ctrl_disp_errors; do
  [ "$(field "$k" $f)" = 0 ] || { sum "gpu-vpll: $f=$(field "$k" $f)"; fail=1; }
done
# Out of bounds is refused (EINVAL), not pushed: 1 kHz and 999 MHz.
echo "clock 1" > /dev/dispctl 2>/dev/null && { sum "gpu-vpll: clock 1 accepted"; fail=1; }
echo "clock 999000" > /dev/dispctl 2>/dev/null && { sum "gpu-vpll: clock 999000 accepted"; fail=1; }

echo "gpu-vpll: compositor fire for 10 s"
# Not `timeout`: BusyBox's polls the child with kill(pid, 0) (EINVAL here).
compositor fire &
sleep 10
kill -TERM $!
wait
grep '^gpu_flip:' /proc/kdebug
# Again at the end, in case the log slot wrapped.
grep '^vpll:\|^super:\|^dispctl:' /proc/gpu
cat /tmp/gpu-vpll.sum
echo "gpu-vpll: verdict exit=$fail; 15 s for a look (the console must be back)"
echo "gpu-vpll: console-is-back"
sleep 15
exit $fail
