# G4c of docs/gpu/g4-nvkmd-plan.md, on the Ryzen (`uapi:` lines, kernel/src/gpu/uapi.rs, nvgpu::hwq):
#   touch build.rs   # the root build does not watch nvgpu
#   echo 5 > target/metal/budget
#   scripts/metal-run.sh --kconf 'gpu=uapi' scripts/metal-jobs/gpu-uapi.sh
# Everything gpu=compute does (job gpu-compute.sh), then /dev/nvgpu on the hardware: the test program nvgpu_hw_test binds buffers into the
# GPU's page tables at run time, launches compute shaders through EXEC and checks each result by a path other than the writer's (the CPU
# for system memory, a second shader for VRAM), rebinding (the TLB flush), 700 submissions through the ring, 29 launches in flight, and the
# device handed back after a close and after a holder that died with work in flight. Fails unless "uapi: installed" is in /proc/gpu,
# /proc/kdebug says gpu_uapi state=ok dead=0, the test printed "hardware" and exited 0 with no FAIL line.
cat /proc/gpu
fail=0
sum() { echo "$*"; echo "$*" >> /tmp/gpu-uapi.sum; }
: > /tmp/gpu-uapi.sum
grep '^fwsec:\|^gsp:\|^vaspace:\|^copy:\|^compute:\|^bench:\|^bar1:\|^intr:' /proc/gpu >> /tmp/gpu-uapi.sum
grep '^chan: STOP\|^super: STOP\|^hdmi: STOP\|^fwsec: STOP\|^gsp: STOP\|^vaspace: STOP\|^copy: STOP\|^compute: STOP\|^bench: STOP' /proc/gpu && fail=1
grep -q '^fwsec: OK' /proc/gpu || { sum "gpu-uapi: FRTS did not finish OK"; fail=1; }
if grep -q '^gsp: OK' /proc/gpu; then sum "gpu-uapi: gsp OK"; else sum "gpu-uapi: gsp did not boot"; fail=1; fi
field() { echo "$1" | tr ' ' '\n' | grep "^$2=" | cut -d= -f2; }
k=$(grep '^gpu_gsp:' /proc/kdebug)
sum "gpu-uapi: $(grep '^gpu_fwsec:' /proc/kdebug)"
sum "gpu-uapi: $k"
v=$(grep '^gpu_vaspace:' /proc/kdebug)
sum "gpu-uapi: $v"
grep -q '^vaspace: OK' /proc/gpu || { sum "gpu-uapi: the virtual address space did not come up"; fail=1; }
c=$(grep '^gpu_copy:' /proc/kdebug)
sum "gpu-uapi: $c"
grep -q '^copy: OK' /proc/gpu || { sum "gpu-uapi: the copy did not complete OK"; fail=1; }
grep -q '^bench: OK' /proc/gpu || { sum "gpu-uapi: the bench did not finish OK"; fail=1; }
sum "gpu-uapi: $(grep '^gpu_bench:' /proc/kdebug)"
sum "gpu-uapi: $(grep '^gpu_intr:' /proc/kdebug)"
[ "$(field "$c" state)" = ok ] || { sum "gpu-uapi: copy state=$(field "$c" state)"; fail=1; }
[ "$(field "$c" mismatch)" = 0 ] || { sum "gpu-uapi: mismatch=$(field "$c" mismatch)"; fail=1; }
for x in up_kbps down_kbps; do
  [ "$(field "$c" $x)" -gt 0 ] 2>/dev/null || { sum "gpu-uapi: $x=$(field "$c" $x)"; fail=1; }
done
[ "$(field "$v" state)" = ok ] || { sum "gpu-uapi: state=$(field "$v" state)"; fail=1; }
[ "$(field "$v" va_base)" = 0x4000000 ] || { sum "gpu-uapi: va_base=$(field "$v" va_base)"; fail=1; }
[ "$(field "$k" stage)" = objects ] || { sum "gpu-uapi: stage=$(field "$k" stage)"; fail=1; }
echo "$k" | grep -q 'name="NVIDIA GeForce RTX 3050"' || { sum "gpu-uapi: the GPU name is not the expected one"; fail=1; }
echo "$k" | grep -q 'rm_name="NVIDIA GeForce RTX 3050"' || { sum "gpu-uapi: no name from our own RM client"; fail=1; }
[ "$(field "$k" booter_mbox0)" = 0x0 ] || { sum "gpu-uapi: booter_mbox0=$(field "$k" booter_mbox0)"; fail=1; }

# Phase 7a: the GR channel.
grep -q '^compute: OK' /proc/gpu || { sum "gpu-uapi: the compute channel did not complete OK"; fail=1; }
cc=$(grep '^gpu_compute:' /proc/kdebug)
sum "gpu-uapi: $cc"
[ "$(field "$cc" state)" = ok ] || { sum "gpu-uapi: compute state=$(field "$cc" state)"; fail=1; }
[ "$(field "$cc" rungs)" -ge 5 ] 2>/dev/null || { sum "gpu-uapi: rungs=$(field "$cc" rungs), wanted at least 5"; fail=1; }
for x in launch_host_us launch_vram_us; do
  [ "$(field "$cc" $x)" -gt 0 ] 2>/dev/null || { sum "gpu-uapi: $x=$(field "$cc" $x)"; fail=1; }
done
sum "gpu-uapi: the grid's own release semaphore: grid_release=$(field "$cc" grid_release) (1 = seen); vram_cpu_view=$(field "$cc" vram_cpu_view)"
[ "$(field "$cc" golden_ms)" -gt 0 ] 2>/dev/null || { sum "gpu-uapi: golden_ms=$(field "$cc" golden_ms)"; fail=1; }

# G4c: /dev/nvgpu on the hardware.
grep '^uapi:' /proc/gpu >> /tmp/gpu-uapi.sum
grep -q '^uapi: installed' /proc/gpu || { sum "gpu-uapi: the GPU state was not kept for /dev/nvgpu"; fail=1; }
grep '^uapi: STOP' /proc/gpu && fail=1
grep 'DOES NOT REACH VRAM' /proc/gpu && sum "gpu-uapi: a BAR1 span does not reach VRAM (PRAMIN serves it: slower, not wrong)"
u0=$(grep '^gpu_uapi:' /proc/kdebug)
sum "gpu-uapi: before the test: $u0"
[ "$(field "$u0" state)" = ok ] || { sum "gpu-uapi: uapi state=$(field "$u0" state)"; fail=1; }
/mnt/bin/nvgpu_hw_test > /tmp/nvgpu_hw_test.out 2>&1
rc=$?
grep -E 'FAIL|device:|EXEC to fence|empty EXECs|in flight|failure' /tmp/nvgpu_hw_test.out | while read -r l; do sum "gpu-uapi: test: $l"; done
[ $rc = 0 ] || { sum "gpu-uapi: nvgpu_hw_test exit=$rc"; fail=1; tail -n 12 /tmp/nvgpu_hw_test.out >> /tmp/gpu-uapi.sum; tail -n 12 /tmp/nvgpu_hw_test.out; }
grep -q 'hardware' /tmp/nvgpu_hw_test.out || { sum "gpu-uapi: the test ran against the software device"; fail=1; }
grep -q 'skip ' /tmp/nvgpu_hw_test.out && { sum "gpu-uapi: execution checks were skipped"; fail=1; }
grep -q 'FAIL' /tmp/nvgpu_hw_test.out && fail=1
u1=$(grep '^gpu_uapi:' /proc/kdebug)
sum "gpu-uapi: after the test: $u1"
[ "$(field "$u1" dead)" = 0 ] || { sum "gpu-uapi: the GPU was declared dead"; fail=1; }
for x in binds unbinds tlb_flushes execs fences; do
  [ "$(field "$u1" $x)" -gt 0 ] 2>/dev/null || { sum "gpu-uapi: $x=$(field "$u1" $x)"; fail=1; }
done
[ "$(field "$u1" again)" -gt 0 ] 2>/dev/null && sum "gpu-uapi: the ring filled $(field "$u1" again) times (EAGAIN handled)" || sum "gpu-uapi: the ring never filled (again=0): the EAGAIN path was not exercised"

# RM still answers after all this, and the display is unharmed.
echo 'gsp name' > /dev/dispctl && sum "gpu-uapi: RM still answers" || { sum "gpu-uapi: RM does not answer"; fail=1; }
sum "gpu-uapi: $(grep '^gpu_gsprt:' /proc/kdebug)"

sample() { grep '^gpu_vblank:' /proc/kdebug; }
s1=$(sample)
sleep 10
s2=$(sample)
sum "gpu-uapi: t0 $s1"
sum "gpu-uapi: t1 $s2"
[ "$(field "$s2" enabled)" = 1 ] || { sum "gpu-uapi: vblank MSI not enabled"; fail=1; }
for x in spurious blocked gone; do
  [ "$(field "$s2" $x)" = 0 ] || { sum "gpu-uapi: $x=$(field "$s2" $x)"; fail=1; }
done
q1=$(field "$s1" seq); q2=$(field "$s2" seq)
n1=$(field "$s1" last_ns); n2=$(field "$s2" last_ns)
rate=$(awk -v a="$q1" -v b="$q2" -v x="$n1" -v y="$n2" 'BEGIN { if (y > x) printf "%.4f", (b - a) * 1e9 / (y - x); else print "0" }')
sum "gpu-uapi: ASUS rate $rate Hz"
awk -v r="$rate" 'BEGIN { exit !(r >= 59.9 && r <= 60.1) }' || { sum "gpu-uapi: rate $rate outside 60.0 +- 0.1"; fail=1; }
# The logs again, after the 10 s: has GSP-RM written more?
sum "gpu-uapi: (log put pointers at boot) $(grep '^gpu_gsp:' /proc/kdebug | tr ' ' '\n' | grep '^log_pp=')"

echo "---- summary (the log wraps) ----"
cat /tmp/gpu-uapi.sum
sum "gpu-uapi: verdict exit=$fail"
exit $fail
