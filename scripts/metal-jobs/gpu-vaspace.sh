# Phase 6b of docs/gpu/gpu-plan.md, on the Ryzen:
#   touch build.rs   # the root build does not watch nvgpu
#   scripts/metal-run.sh --kconf 'gpu=vaspace' scripts/metal-jobs/gpu-vaspace.sh
# Everything gpu=gsp does (job gpu-gsp.sh), plus a GPU virtual address space for
# our RM client: FERMI_VASPACE_A (externally owned), page tables built by
# nvgpu::mmu, written to VRAM 64 MiB through PRAMIN and read back, and RM
# accepting their root with NV0080_CTRL_CMD_DMA_SET_PAGE_DIRECTORY
# (`vaspace:` lines of /proc/gpu, `gpu_vaspace:` in /proc/kdebug). Fails unless:
# - "vaspace: OK" is in /proc/gpu and /proc/kdebug says gpu_vaspace state=ok
#   with tables=5 and the VA range RM reported (va_base=0x4000000);
# - everything gpu-gsp.sh checks (GSP boot, RM name, ASUS at 60 Hz).
cat /proc/gpu
fail=0
sum() { echo "$*"; echo "$*" >> /tmp/gpu-vaspace.sum; }
: > /tmp/gpu-vaspace.sum
grep '^fwsec:\|^gsp:\|^vaspace:' /proc/gpu >> /tmp/gpu-vaspace.sum
grep '^chan: STOP\|^super: STOP\|^hdmi: STOP\|^fwsec: STOP\|^gsp: STOP\|^vaspace: STOP' /proc/gpu && fail=1
grep -q '^fwsec: OK' /proc/gpu || { sum "gpu-vaspace: FRTS did not finish OK"; fail=1; }
if grep -q '^gsp: OK' /proc/gpu; then sum "gpu-vaspace: gsp OK"; else sum "gpu-vaspace: gsp did not boot"; fail=1; fi
field() { echo "$1" | tr ' ' '\n' | grep "^$2=" | cut -d= -f2; }
k=$(grep '^gpu_gsp:' /proc/kdebug)
sum "gpu-vaspace: $(grep '^gpu_fwsec:' /proc/kdebug)"
sum "gpu-vaspace: $k"
v=$(grep '^gpu_vaspace:' /proc/kdebug)
sum "gpu-vaspace: $v"
grep -q '^vaspace: OK' /proc/gpu || { sum "gpu-vaspace: the virtual address space did not come up"; fail=1; }
[ "$(field "$v" state)" = ok ] || { sum "gpu-vaspace: state=$(field "$v" state)"; fail=1; }
[ "$(field "$v" tables)" = 5 ] || { sum "gpu-vaspace: tables=$(field "$v" tables)"; fail=1; }
[ "$(field "$v" va_base)" = 0x4000000 ] || { sum "gpu-vaspace: va_base=$(field "$v" va_base)"; fail=1; }
[ "$(field "$k" stage)" = objects ] || { sum "gpu-vaspace: stage=$(field "$k" stage)"; fail=1; }
echo "$k" | grep -q 'name="NVIDIA GeForce RTX 3050"' || { sum "gpu-vaspace: the GPU name is not the expected one"; fail=1; }
echo "$k" | grep -q 'rm_name="NVIDIA GeForce RTX 3050"' || { sum "gpu-vaspace: no name from our own RM client"; fail=1; }
[ "$(field "$k" booter_mbox0)" = 0x0 ] || { sum "gpu-vaspace: booter_mbox0=$(field "$k" booter_mbox0)"; fail=1; }

sample() { grep '^gpu_vblank:' /proc/kdebug; }
s1=$(sample)
sleep 10
s2=$(sample)
sum "gpu-vaspace: t0 $s1"
sum "gpu-vaspace: t1 $s2"
[ "$(field "$s2" enabled)" = 1 ] || { sum "gpu-vaspace: vblank MSI not enabled"; fail=1; }
for x in spurious blocked gone; do
  [ "$(field "$s2" $x)" = 0 ] || { sum "gpu-vaspace: $x=$(field "$s2" $x)"; fail=1; }
done
q1=$(field "$s1" seq); q2=$(field "$s2" seq)
n1=$(field "$s1" last_ns); n2=$(field "$s2" last_ns)
rate=$(awk -v a="$q1" -v b="$q2" -v x="$n1" -v y="$n2" 'BEGIN { if (y > x) printf "%.4f", (b - a) * 1e9 / (y - x); else print "0" }')
sum "gpu-vaspace: ASUS rate $rate Hz"
awk -v r="$rate" 'BEGIN { exit !(r >= 59.9 && r <= 60.1) }' || { sum "gpu-vaspace: rate $rate outside 60.0 +- 0.1"; fail=1; }
# The logs again, after the 10 s: has GSP-RM written more?
sum "gpu-vaspace: (log put pointers at boot) $(grep '^gpu_gsp:' /proc/kdebug | tr ' ' '\n' | grep '^log_pp=')"

echo "---- summary (the log wraps) ----"
cat /tmp/gpu-vaspace.sum
sum "gpu-vaspace: verdict exit=$fail"
exit $fail
