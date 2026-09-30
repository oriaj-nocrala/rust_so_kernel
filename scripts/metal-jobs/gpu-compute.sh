# Phase 7a of docs/gpu/gpu-plan.md, on the Ryzen (`compute:` lines, kernel/src/gpu/compute.rs):
#   touch build.rs   # the root build does not watch nvgpu
#   scripts/metal-run.sh --kconf 'gpu=compute' scripts/metal-jobs/gpu-compute.sh
# Everything gpu=copy does (job gpu-copy.sh), plus the GR channel: nouveau's golden context
# (a channel on GR0, PROMOTE_CTX with every context buffer, a 3D object 0xc797 so RM builds the
# golden image, all freed), then the real channel (ALLOC, BIND, GPFIFO_SCHEDULE, PROMOTE_CTX with
# its own buffers, the compute object 0xc7c0) and three pushes through it: a semaphore release, an
# inline write of 64 bytes into host memory and one of 4 KiB into VRAM, each verified by a path other
# than the writer's; then (phase 7b) a compute shader, launched through a QMD, that writes 256 distinct words:
# once into host memory (read by the CPU, the other 768 words of the page must stay the scribble it started with) and
# once into VRAM (verified by the GPU: a second shader reads the page back into host memory; rung 6 measures which ways of reading it through PRAMIN show the stores to the CPU). Fails unless "compute: OK" is in /proc/gpu, /proc/kdebug says gpu_compute
# state=ok rungs>=5, and everything gpu-copy.sh checks. grid_release (the QMD's own semaphore) is reported, not required.
cat /proc/gpu
fail=0
sum() { echo "$*"; echo "$*" >> /tmp/gpu-compute.sum; }
: > /tmp/gpu-compute.sum
grep '^fwsec:\|^gsp:\|^vaspace:\|^copy:\|^compute:\|^bench:\|^bar1:\|^intr:' /proc/gpu >> /tmp/gpu-compute.sum
grep '^chan: STOP\|^super: STOP\|^hdmi: STOP\|^fwsec: STOP\|^gsp: STOP\|^vaspace: STOP\|^copy: STOP\|^compute: STOP\|^bench: STOP' /proc/gpu && fail=1
grep -q '^fwsec: OK' /proc/gpu || { sum "gpu-compute: FRTS did not finish OK"; fail=1; }
if grep -q '^gsp: OK' /proc/gpu; then sum "gpu-compute: gsp OK"; else sum "gpu-compute: gsp did not boot"; fail=1; fi
field() { echo "$1" | tr ' ' '\n' | grep "^$2=" | cut -d= -f2; }
k=$(grep '^gpu_gsp:' /proc/kdebug)
sum "gpu-compute: $(grep '^gpu_fwsec:' /proc/kdebug)"
sum "gpu-compute: $k"
v=$(grep '^gpu_vaspace:' /proc/kdebug)
sum "gpu-compute: $v"
grep -q '^vaspace: OK' /proc/gpu || { sum "gpu-compute: the virtual address space did not come up"; fail=1; }
c=$(grep '^gpu_copy:' /proc/kdebug)
sum "gpu-compute: $c"
grep -q '^copy: OK' /proc/gpu || { sum "gpu-compute: the copy did not complete OK"; fail=1; }
grep -q '^bench: OK' /proc/gpu || { sum "gpu-compute: the bench did not finish OK"; fail=1; }
sum "gpu-compute: $(grep '^gpu_bench:' /proc/kdebug)"
sum "gpu-compute: $(grep '^gpu_intr:' /proc/kdebug)"
[ "$(field "$c" state)" = ok ] || { sum "gpu-compute: copy state=$(field "$c" state)"; fail=1; }
[ "$(field "$c" mismatch)" = 0 ] || { sum "gpu-compute: mismatch=$(field "$c" mismatch)"; fail=1; }
for x in up_kbps down_kbps; do
  [ "$(field "$c" $x)" -gt 0 ] 2>/dev/null || { sum "gpu-compute: $x=$(field "$c" $x)"; fail=1; }
done
[ "$(field "$v" state)" = ok ] || { sum "gpu-compute: state=$(field "$v" state)"; fail=1; }
[ "$(field "$v" va_base)" = 0x4000000 ] || { sum "gpu-compute: va_base=$(field "$v" va_base)"; fail=1; }
[ "$(field "$k" stage)" = objects ] || { sum "gpu-compute: stage=$(field "$k" stage)"; fail=1; }
echo "$k" | grep -q 'name="NVIDIA GeForce RTX 3050"' || { sum "gpu-compute: the GPU name is not the expected one"; fail=1; }
echo "$k" | grep -q 'rm_name="NVIDIA GeForce RTX 3050"' || { sum "gpu-compute: no name from our own RM client"; fail=1; }
[ "$(field "$k" booter_mbox0)" = 0x0 ] || { sum "gpu-compute: booter_mbox0=$(field "$k" booter_mbox0)"; fail=1; }

# Phase 7a: the GR channel.
grep -q '^compute: OK' /proc/gpu || { sum "gpu-compute: the compute channel did not complete OK"; fail=1; }
cc=$(grep '^gpu_compute:' /proc/kdebug)
sum "gpu-compute: $cc"
[ "$(field "$cc" state)" = ok ] || { sum "gpu-compute: compute state=$(field "$cc" state)"; fail=1; }
[ "$(field "$cc" rungs)" -ge 5 ] 2>/dev/null || { sum "gpu-compute: rungs=$(field "$cc" rungs), wanted at least 5"; fail=1; }
for x in launch_host_us launch_vram_us; do
  [ "$(field "$cc" $x)" -gt 0 ] 2>/dev/null || { sum "gpu-compute: $x=$(field "$cc" $x)"; fail=1; }
done
sum "gpu-compute: the grid's own release semaphore: grid_release=$(field "$cc" grid_release) (1 = seen); vram_cpu_view=$(field "$cc" vram_cpu_view)"
[ "$(field "$cc" golden_ms)" -gt 0 ] 2>/dev/null || { sum "gpu-compute: golden_ms=$(field "$cc" golden_ms)"; fail=1; }

# RM still answers after all this, and the display is unharmed.
echo 'gsp name' > /dev/dispctl && sum "gpu-compute: RM still answers" || { sum "gpu-compute: RM does not answer"; fail=1; }
sum "gpu-compute: $(grep '^gpu_gsprt:' /proc/kdebug)"

sample() { grep '^gpu_vblank:' /proc/kdebug; }
s1=$(sample)
sleep 10
s2=$(sample)
sum "gpu-compute: t0 $s1"
sum "gpu-compute: t1 $s2"
[ "$(field "$s2" enabled)" = 1 ] || { sum "gpu-compute: vblank MSI not enabled"; fail=1; }
for x in spurious blocked gone; do
  [ "$(field "$s2" $x)" = 0 ] || { sum "gpu-compute: $x=$(field "$s2" $x)"; fail=1; }
done
q1=$(field "$s1" seq); q2=$(field "$s2" seq)
n1=$(field "$s1" last_ns); n2=$(field "$s2" last_ns)
rate=$(awk -v a="$q1" -v b="$q2" -v x="$n1" -v y="$n2" 'BEGIN { if (y > x) printf "%.4f", (b - a) * 1e9 / (y - x); else print "0" }')
sum "gpu-compute: ASUS rate $rate Hz"
awk -v r="$rate" 'BEGIN { exit !(r >= 59.9 && r <= 60.1) }' || { sum "gpu-compute: rate $rate outside 60.0 +- 0.1"; fail=1; }
# The logs again, after the 10 s: has GSP-RM written more?
sum "gpu-compute: (log put pointers at boot) $(grep '^gpu_gsp:' /proc/kdebug | tr ' ' '\n' | grep '^log_pp=')"

echo "---- summary (the log wraps) ----"
cat /tmp/gpu-compute.sum
sum "gpu-compute: verdict exit=$fail"
exit $fail
