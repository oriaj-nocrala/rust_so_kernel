# Phases 4d + 4e + 4f of docs/gpu/gpu-plan.md, on the Ryzen:
#   touch build.rs   # the root build does not watch nvgpu
#   scripts/metal-run.sh --kconf 'gpu=gsp' scripts/metal-jobs/gpu-gsp.sh
# (metal-run syncs the data partition, which puts gsp-570.144.bin, 63 MB, on the
# stick: scripts/sync-usb-data.sh.)
# Everything gpu=fwsec does, plus, at boot before vblank is armed: the GSP-RM
# memory (firmware behind radix3, bootloader, WPR meta, LibOS arguments, three
# log buffers, RM arguments, queues), FRTS, a GSP reset into RISC-V mode, the
# LibOS address in the GSP mailboxes, the booter on SEC2 and a check that the
# GSP's RISC-V core is active; then the RPC channel (4f): SET_SYSTEM_INFO and
# SET_REGISTRY queued before boot, the sequencer commands GSP-RM sends while
# it boots run on the host, INIT_DONE, and GET_GSP_STATIC_INFO whose reply
# carries the GPU's name (`gsp:` lines of /proc/gpu, `gpu_gsp:` in
# /proc/kdebug). Fails unless:
# - "gsp: OK" is in /proc/gpu and /proc/kdebug says stage=name with
#   name="NVIDIA GeForce RTX 3050" (RM's own answer);
# - the booter left mailbox 0 = 0 (booter_mbox0=0x0);
# - "fwsec: OK" (FRTS) as in gpu-fwsec.sh, and no STOP line anywhere;
# - the ASUS still gets vblanks at 60 Hz (GSP-RM must not disturb the display).
# The log wraps and loses the first lines: the summary is repeated at the end.
cat /proc/gpu
fail=0
sum() { echo "$*"; echo "$*" >> /tmp/gpu-gsp.sum; }
: > /tmp/gpu-gsp.sum
grep '^fwsec:\|^gsp:' /proc/gpu >> /tmp/gpu-gsp.sum
grep '^chan: STOP\|^super: STOP\|^hdmi: STOP\|^fwsec: STOP\|^gsp: STOP' /proc/gpu && fail=1
grep -q '^fwsec: OK' /proc/gpu || { sum "gpu-gsp: FRTS did not finish OK"; fail=1; }
if grep -q '^gsp: OK' /proc/gpu; then sum "gpu-gsp: gsp OK"; else sum "gpu-gsp: gsp did not boot"; fail=1; fi
field() { echo "$1" | tr ' ' '\n' | grep "^$2=" | cut -d= -f2; }
k=$(grep '^gpu_gsp:' /proc/kdebug)
sum "gpu-gsp: $(grep '^gpu_fwsec:' /proc/kdebug)"
sum "gpu-gsp: $k"
[ "$(field "$k" stage)" = name ] || { sum "gpu-gsp: stage=$(field "$k" stage)"; fail=1; }
echo "$k" | grep -q 'name="NVIDIA GeForce RTX 3050"' || { sum "gpu-gsp: the GPU name is not the expected one"; fail=1; }
[ "$(field "$k" booter_mbox0)" = 0x0 ] || { sum "gpu-gsp: booter_mbox0=$(field "$k" booter_mbox0)"; fail=1; }

sample() { grep '^gpu_vblank:' /proc/kdebug; }
s1=$(sample)
sleep 10
s2=$(sample)
sum "gpu-gsp: t0 $s1"
sum "gpu-gsp: t1 $s2"
[ "$(field "$s2" enabled)" = 1 ] || { sum "gpu-gsp: vblank MSI not enabled"; fail=1; }
for x in spurious blocked gone; do
  [ "$(field "$s2" $x)" = 0 ] || { sum "gpu-gsp: $x=$(field "$s2" $x)"; fail=1; }
done
q1=$(field "$s1" seq); q2=$(field "$s2" seq)
n1=$(field "$s1" last_ns); n2=$(field "$s2" last_ns)
rate=$(awk -v a="$q1" -v b="$q2" -v x="$n1" -v y="$n2" 'BEGIN { if (y > x) printf "%.4f", (b - a) * 1e9 / (y - x); else print "0" }')
sum "gpu-gsp: ASUS rate $rate Hz"
awk -v r="$rate" 'BEGIN { exit !(r >= 59.9 && r <= 60.1) }' || { sum "gpu-gsp: rate $rate outside 60.0 +- 0.1"; fail=1; }
# The logs again, after the 10 s: has GSP-RM written more?
sum "gpu-gsp: (log put pointers at boot) $(grep '^gpu_gsp:' /proc/kdebug | tr ' ' '\n' | grep '^log_pp=')"

echo "---- summary (the log wraps) ----"
cat /tmp/gpu-gsp.sum
sum "gpu-gsp: verdict exit=$fail"
exit $fail
