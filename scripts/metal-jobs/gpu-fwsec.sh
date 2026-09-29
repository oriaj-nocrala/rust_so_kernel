# Phase 4c of docs/gpu/gpu-plan.md, on the Ryzen:
#   touch build.rs   # the root build does not watch nvgpu
#   scripts/metal-run.sh --kconf 'gpu=fwsec' scripts/metal-jobs/gpu-fwsec.sh
# Everything gpu=hdmi does, plus, at boot before vblank is armed, FWSEC-FRTS on
# the GSP falcon (`fwsec:` lines of /proc/gpu; `gpu_fwsec:` in /proc/kdebug):
# the VBIOS's signed microcode creates the protected region (WPR2) the GSP's
# own boot needs. Fails unless:
# - "fwsec: OK" is in /proc/gpu (the falcon halted with mailbox 0 = 0 and
#   0x1438 reported no error) and /proc/kdebug says state=ok;
# - WPR2 low is 0x01ffe000 (nouveau's, trace-gsp; the region is the VRAM's
#   last MiB minus the VGA workspace) and high is not 0;
# - no chan/super/hdmi STOP, and the ASUS still gets vblanks at 60 Hz (or the
#   mode it had): FRTS must not disturb the display.
# The log wraps and loses the first lines: the summary is repeated at the end.
cat /proc/gpu
fail=0
sum() { echo "$*"; echo "$*" >> /tmp/gpu-fwsec.sum; }
: > /tmp/gpu-fwsec.sum
grep '^fwsec:' /proc/gpu >> /tmp/gpu-fwsec.sum
grep '^chan: STOP\|^super: STOP\|^hdmi: STOP' /proc/gpu && fail=1
if grep -q '^fwsec: OK' /proc/gpu; then sum "gpu-fwsec: fwsec OK"; else sum "gpu-fwsec: fwsec did not finish OK"; fail=1; fi
grep '^fwsec: STOP' /proc/gpu && fail=1
field() { echo "$1" | tr ' ' '\n' | grep "^$2=" | cut -d= -f2; }
k=$(grep '^gpu_fwsec:' /proc/kdebug)
sum "gpu-fwsec: $k"
[ "$(field "$k" state)" = ok ] || { sum "gpu-fwsec: state=$(field "$k" state)"; fail=1; }
lo=$(field "$k" wpr2_lo); hi=$(field "$k" wpr2_hi)
[ "$lo" = 0x1ffe000 ] || { sum "gpu-fwsec: wpr2_lo=$lo, want 0x1ffe000"; fail=1; }
[ "$hi" != 0x0 ] && [ -n "$hi" ] || { sum "gpu-fwsec: wpr2_hi=$hi"; fail=1; }

sample() { grep '^gpu_vblank:' /proc/kdebug; }
s1=$(sample)
sleep 10
s2=$(sample)
sum "gpu-fwsec: t0 $s1"
sum "gpu-fwsec: t1 $s2"
[ "$(field "$s2" enabled)" = 1 ] || { sum "gpu-fwsec: vblank MSI not enabled"; fail=1; }
for x in spurious blocked gone; do
  [ "$(field "$s2" $x)" = 0 ] || { sum "gpu-fwsec: $x=$(field "$s2" $x)"; fail=1; }
done
q1=$(field "$s1" seq); q2=$(field "$s2" seq)
n1=$(field "$s1" last_ns); n2=$(field "$s2" last_ns)
rate=$(awk -v a="$q1" -v b="$q2" -v x="$n1" -v y="$n2" 'BEGIN { if (y > x) printf "%.4f", (b - a) * 1e9 / (y - x); else print "0" }')
sum "gpu-fwsec: ASUS rate $rate Hz"
awk -v r="$rate" 'BEGIN { exit !(r >= 59.9 && r <= 60.1) }' || { sum "gpu-fwsec: rate $rate outside 60.0 +- 0.1"; fail=1; }

echo "---- summary (the log wraps) ----"
cat /tmp/gpu-fwsec.sum
sum "gpu-fwsec: verdict exit=$fail"
exit $fail
