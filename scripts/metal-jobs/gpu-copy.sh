# Phase 6c of docs/gpu/gpu-plan.md, on the Ryzen:
#   touch build.rs   # the root build does not watch nvgpu
#   scripts/metal-run.sh --kconf 'gpu=copy' scripts/metal-jobs/gpu-copy.sh
# Everything gpu=vaspace does (job gpu-vaspace.sh), plus a GPFIFO channel on the
# Ampere copy engine through RM (ALLOC 0xc56f, BIND, GPFIFO_SCHEDULE, the copy
# object 0xc7b5, the work-submit token) and two copies of 4 MiB through it:
# system pages -> VRAM and VRAM -> other system pages, each waited on through a
# semaphore the copy engine releases (`copy:` lines of /proc/gpu, `gpu_copy:` in
# /proc/kdebug). Fails unless:
# - "copy: OK" is in /proc/gpu and /proc/kdebug says gpu_copy state=ok with
#   mismatch=0 (the data came back identical) and non-zero up_kbps/down_kbps;
# - everything gpu-vaspace.sh checks.
cat /proc/gpu
fail=0
sum() { echo "$*"; echo "$*" >> /tmp/gpu-copy.sum; }
: > /tmp/gpu-copy.sum
grep '^fwsec:\|^gsp:\|^vaspace:\|^copy:' /proc/gpu >> /tmp/gpu-copy.sum
grep '^chan: STOP\|^super: STOP\|^hdmi: STOP\|^fwsec: STOP\|^gsp: STOP\|^vaspace: STOP\|^copy: STOP' /proc/gpu && fail=1
grep -q '^fwsec: OK' /proc/gpu || { sum "gpu-copy: FRTS did not finish OK"; fail=1; }
if grep -q '^gsp: OK' /proc/gpu; then sum "gpu-copy: gsp OK"; else sum "gpu-copy: gsp did not boot"; fail=1; fi
field() { echo "$1" | tr ' ' '\n' | grep "^$2=" | cut -d= -f2; }
k=$(grep '^gpu_gsp:' /proc/kdebug)
sum "gpu-copy: $(grep '^gpu_fwsec:' /proc/kdebug)"
sum "gpu-copy: $k"
v=$(grep '^gpu_vaspace:' /proc/kdebug)
sum "gpu-copy: $v"
grep -q '^vaspace: OK' /proc/gpu || { sum "gpu-copy: the virtual address space did not come up"; fail=1; }
c=$(grep '^gpu_copy:' /proc/kdebug)
sum "gpu-copy: $c"
grep -q '^copy: OK' /proc/gpu || { sum "gpu-copy: the copy did not complete OK"; fail=1; }
[ "$(field "$c" state)" = ok ] || { sum "gpu-copy: copy state=$(field "$c" state)"; fail=1; }
[ "$(field "$c" mismatch)" = 0 ] || { sum "gpu-copy: mismatch=$(field "$c" mismatch)"; fail=1; }
for x in up_kbps down_kbps; do
  [ "$(field "$c" $x)" -gt 0 ] 2>/dev/null || { sum "gpu-copy: $x=$(field "$c" $x)"; fail=1; }
done
[ "$(field "$v" state)" = ok ] || { sum "gpu-copy: state=$(field "$v" state)"; fail=1; }
[ "$(field "$v" va_base)" = 0x4000000 ] || { sum "gpu-copy: va_base=$(field "$v" va_base)"; fail=1; }
[ "$(field "$k" stage)" = objects ] || { sum "gpu-copy: stage=$(field "$k" stage)"; fail=1; }
echo "$k" | grep -q 'name="NVIDIA GeForce RTX 3050"' || { sum "gpu-copy: the GPU name is not the expected one"; fail=1; }
echo "$k" | grep -q 'rm_name="NVIDIA GeForce RTX 3050"' || { sum "gpu-copy: no name from our own RM client"; fail=1; }
[ "$(field "$k" booter_mbox0)" = 0x0 ] || { sum "gpu-copy: booter_mbox0=$(field "$k" booter_mbox0)"; fail=1; }

sample() { grep '^gpu_vblank:' /proc/kdebug; }
s1=$(sample)
sleep 10
s2=$(sample)
sum "gpu-copy: t0 $s1"
sum "gpu-copy: t1 $s2"
[ "$(field "$s2" enabled)" = 1 ] || { sum "gpu-copy: vblank MSI not enabled"; fail=1; }
for x in spurious blocked gone; do
  [ "$(field "$s2" $x)" = 0 ] || { sum "gpu-copy: $x=$(field "$s2" $x)"; fail=1; }
done
q1=$(field "$s1" seq); q2=$(field "$s2" seq)
n1=$(field "$s1" last_ns); n2=$(field "$s2" last_ns)
rate=$(awk -v a="$q1" -v b="$q2" -v x="$n1" -v y="$n2" 'BEGIN { if (y > x) printf "%.4f", (b - a) * 1e9 / (y - x); else print "0" }')
sum "gpu-copy: ASUS rate $rate Hz"
awk -v r="$rate" 'BEGIN { exit !(r >= 59.9 && r <= 60.1) }' || { sum "gpu-copy: rate $rate outside 60.0 +- 0.1"; fail=1; }
# The logs again, after the 10 s: has GSP-RM written more?
sum "gpu-copy: (log put pointers at boot) $(grep '^gpu_gsp:' /proc/kdebug | tr ' ' '\n' | grep '^log_pp=')"

echo "---- summary (the log wraps) ----"
cat /tmp/gpu-copy.sum
sum "gpu-copy: verdict exit=$fail"
exit $fail
