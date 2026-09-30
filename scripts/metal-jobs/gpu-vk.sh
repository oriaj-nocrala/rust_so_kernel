# G4d of docs/gpu/g4-nvkmd-plan.md, on the Ryzen: a real Vulkan compute dispatch through NVK (Mesa, static musl) on /dev/nvgpu.
#   strip -o disk-image-root/bin/vk_probe ~/src/gpu-ref/nvk-probe/vk-probe   # 15 MB each (and vk_draw); then a normal build
#   touch build.rs; echo 5 > target/metal/budget
#   scripts/metal-run.sh --kconf 'gpu=uapi' scripts/metal-jobs/gpu-vk.sh
# Everything gpu-vk.sh does (G4c: nvgpu_hw_test, now with a copy-engine context as well), then probes/nvk/vk_probe.c with
# VK_PROBE_REQUIRE_EXEC=1: instance, device, a compute pipeline compiled by NAK (its code uploaded by the copy engine through NVK's
# upload queue), a dispatch whose results the CPU must find in the buffer ("EXECUTED"), fences and timeline semaphores. Fails unless the
# probe prints PROBE DONE and EXECUTED and gpu_uapi says dead=0 with ce_execs > 0.
cat /proc/gpu
fail=0
sum() { echo "$*"; echo "$*" >> /tmp/gpu-vk.sum; }
: > /tmp/gpu-vk.sum
grep '^fwsec:\|^gsp:\|^vaspace:\|^copy:\|^compute:\|^bench:\|^bar1:\|^intr:' /proc/gpu >> /tmp/gpu-vk.sum
grep '^chan: STOP\|^super: STOP\|^hdmi: STOP\|^fwsec: STOP\|^gsp: STOP\|^vaspace: STOP\|^copy: STOP\|^compute: STOP\|^bench: STOP' /proc/gpu && fail=1
grep -q '^fwsec: OK' /proc/gpu || { sum "gpu-vk: FRTS did not finish OK"; fail=1; }
if grep -q '^gsp: OK' /proc/gpu; then sum "gpu-vk: gsp OK"; else sum "gpu-vk: gsp did not boot"; fail=1; fi
field() { echo "$1" | tr ' ' '\n' | grep "^$2=" | cut -d= -f2; }
k=$(grep '^gpu_gsp:' /proc/kdebug)
sum "gpu-vk: $(grep '^gpu_fwsec:' /proc/kdebug)"
sum "gpu-vk: $k"
v=$(grep '^gpu_vaspace:' /proc/kdebug)
sum "gpu-vk: $v"
grep -q '^vaspace: OK' /proc/gpu || { sum "gpu-vk: the virtual address space did not come up"; fail=1; }
c=$(grep '^gpu_copy:' /proc/kdebug)
sum "gpu-vk: $c"
grep -q '^copy: OK' /proc/gpu || { sum "gpu-vk: the copy did not complete OK"; fail=1; }
grep -q '^bench: OK' /proc/gpu || { sum "gpu-vk: the bench did not finish OK"; fail=1; }
sum "gpu-vk: $(grep '^gpu_bench:' /proc/kdebug)"
sum "gpu-vk: $(grep '^gpu_intr:' /proc/kdebug)"
[ "$(field "$c" state)" = ok ] || { sum "gpu-vk: copy state=$(field "$c" state)"; fail=1; }
[ "$(field "$c" mismatch)" = 0 ] || { sum "gpu-vk: mismatch=$(field "$c" mismatch)"; fail=1; }
for x in up_kbps down_kbps; do
  [ "$(field "$c" $x)" -gt 0 ] 2>/dev/null || { sum "gpu-vk: $x=$(field "$c" $x)"; fail=1; }
done
[ "$(field "$v" state)" = ok ] || { sum "gpu-vk: state=$(field "$v" state)"; fail=1; }
[ "$(field "$v" va_base)" = 0x4000000 ] || { sum "gpu-vk: va_base=$(field "$v" va_base)"; fail=1; }
[ "$(field "$k" stage)" = objects ] || { sum "gpu-vk: stage=$(field "$k" stage)"; fail=1; }
echo "$k" | grep -q 'name="NVIDIA GeForce RTX 3050"' || { sum "gpu-vk: the GPU name is not the expected one"; fail=1; }
echo "$k" | grep -q 'rm_name="NVIDIA GeForce RTX 3050"' || { sum "gpu-vk: no name from our own RM client"; fail=1; }
[ "$(field "$k" booter_mbox0)" = 0x0 ] || { sum "gpu-vk: booter_mbox0=$(field "$k" booter_mbox0)"; fail=1; }

# Phase 7a: the GR channel.
grep -q '^compute: OK' /proc/gpu || { sum "gpu-vk: the compute channel did not complete OK"; fail=1; }
cc=$(grep '^gpu_compute:' /proc/kdebug)
sum "gpu-vk: $cc"
[ "$(field "$cc" state)" = ok ] || { sum "gpu-vk: compute state=$(field "$cc" state)"; fail=1; }
[ "$(field "$cc" rungs)" -ge 5 ] 2>/dev/null || { sum "gpu-vk: rungs=$(field "$cc" rungs), wanted at least 5"; fail=1; }
for x in launch_host_us launch_vram_us; do
  [ "$(field "$cc" $x)" -gt 0 ] 2>/dev/null || { sum "gpu-vk: $x=$(field "$cc" $x)"; fail=1; }
done
sum "gpu-vk: the grid's own release semaphore: grid_release=$(field "$cc" grid_release) (1 = seen); vram_cpu_view=$(field "$cc" vram_cpu_view)"
[ "$(field "$cc" golden_ms)" -gt 0 ] 2>/dev/null || { sum "gpu-vk: golden_ms=$(field "$cc" golden_ms)"; fail=1; }

# G4c: /dev/nvgpu on the hardware.
grep '^uapi:\|^compute: floorsweeping\|^compute: .* GPCs' /proc/gpu >> /tmp/gpu-vk.sum
grep -q '^uapi: installed' /proc/gpu || { sum "gpu-vk: the GPU state was not kept for /dev/nvgpu"; fail=1; }
grep '^uapi: STOP' /proc/gpu && fail=1
grep 'DOES NOT REACH VRAM' /proc/gpu && sum "gpu-vk: a BAR1 span does not reach VRAM (PRAMIN serves it: slower, not wrong)"
u0=$(grep '^gpu_uapi:' /proc/kdebug)
sum "gpu-vk: before the test: $u0"
[ "$(field "$u0" state)" = ok ] || { sum "gpu-vk: uapi state=$(field "$u0" state)"; fail=1; }
/mnt/bin/nvgpu_hw_test > /tmp/nvgpu_hw_test.out 2>&1
rc=$?
grep -E 'FAIL|device:|EXEC to fence|empty EXECs|in flight|failure' /tmp/nvgpu_hw_test.out | while read -r l; do sum "gpu-vk: test: $l"; done
[ $rc = 0 ] || { sum "gpu-vk: nvgpu_hw_test exit=$rc"; fail=1; tail -n 12 /tmp/nvgpu_hw_test.out >> /tmp/gpu-vk.sum; tail -n 12 /tmp/nvgpu_hw_test.out; }
grep -q 'hardware' /tmp/nvgpu_hw_test.out || { sum "gpu-vk: the test ran against the software device"; fail=1; }
grep -q 'skip ' /tmp/nvgpu_hw_test.out && { sum "gpu-vk: execution checks were skipped"; fail=1; }
grep -q 'FAIL' /tmp/nvgpu_hw_test.out && fail=1
u1=$(grep '^gpu_uapi:' /proc/kdebug)
sum "gpu-vk: after the test: $u1"
[ "$(field "$u1" dead)" = 0 ] || { sum "gpu-vk: the GPU was declared dead"; fail=1; }
for x in binds unbinds tlb_flushes execs fences; do
  [ "$(field "$u1" $x)" -gt 0 ] 2>/dev/null || { sum "gpu-vk: $x=$(field "$u1" $x)"; fail=1; }
done
[ "$(field "$u1" again)" -gt 0 ] 2>/dev/null && sum "gpu-vk: the ring filled $(field "$u1" again) times (EAGAIN handled)" || { sum "gpu-vk: the ring never filled (again=0): the EAGAIN path was not exercised"; fail=1; }

# G4d: Vulkan on the hardware.
VK_PROBE_REQUIRE_EXEC=1 NVK_CONSTANOS_DEBUG=1 /mnt/bin/vk_probe > /tmp/vk_probe.out 2>&1
vrc=$?
grep -E 'VK (queue family|using|device|pipeline|dispatch)|PROBE DONE|FAIL' /tmp/vk_probe.out | while read -r l; do sum "gpu-vk: $l"; done
[ $vrc = 0 ] || { sum "gpu-vk: vk_probe exit=$vrc"; fail=1; tail -n 25 /tmp/vk_probe.out >> /tmp/gpu-vk.sum; tail -n 25 /tmp/vk_probe.out; }
grep -q 'PROBE DONE' /tmp/vk_probe.out || { sum "gpu-vk: the probe did not reach PROBE DONE"; fail=1; }
grep -q 'EXECUTED' /tmp/vk_probe.out || { sum "gpu-vk: the dispatch did not execute"; fail=1; }
grep -q 'FAIL' /tmp/vk_probe.out && fail=1
u2=$(grep '^gpu_uapi:' /proc/kdebug)
sum "gpu-vk: after the probe: $u2"
[ "$(field "$u2" dead)" = 0 ] || { sum "gpu-vk: the GPU was declared dead during the probe"; fail=1; }
[ "$(field "$u2" ce_execs)" -gt 0 ] 2>/dev/null || { sum "gpu-vk: ce_execs=$(field "$u2" ce_execs): the copy channel was never used"; fail=1; }

# G4e experiment, last because a copy push the GR channel cannot take faults it for good (dead=1 afterwards): does a copy work on the GR
# channel with nothing binding the copy class, as NVK's graphics queue pushes it? (`grcopy-bind` = with a SET_OBJECT first.)
grep '^compute: copy object\|^compute: the copy object' /proc/gpu | while read -r l; do sum "gpu-vk: $l"; done
/mnt/bin/nvgpu_hw_test ${GRCOPY_VARIANT:-grcopy} > /tmp/grcopy.out 2>&1
grc=$?
grep -E 'FAIL|copy ran|failure' /tmp/grcopy.out | while read -r l; do sum "gpu-vk: grcopy: $l"; done
[ $grc = 0 ] || { sum "gpu-vk: the copy on the GR channel did not work (exit=$grc)"; fail=1; }
sum "gpu-vk: after grcopy: $(grep '^gpu_uapi:' /proc/kdebug)"

# G4e: the first draw. vk_draw renders one triangle over a 64x64 RGBA8 image in VRAM with NVK (dynamic rendering: the 3D engine) and copies
# it to a host buffer with vkCmdCopyImageToBuffer (the copy object on the GR channel); the CPU checks every pixel. Last of all: if the 3D
# path faults the GR channel, nothing after it is lost but the verdict.
VK_PROBE_REQUIRE_EXEC=1 NVK_CONSTANOS_DEBUG=1 /mnt/bin/vk_draw > /tmp/vk_draw.out 2>&1
drc=$?
grep -E 'VK (using|image|draw)|DRAW DONE|FAIL|ASSERT' /tmp/vk_draw.out | while read -r l; do sum "gpu-vk: draw: $l"; done
[ $drc = 0 ] || { sum "gpu-vk: vk_draw exit=$drc"; fail=1; tail -n 25 /tmp/vk_draw.out >> /tmp/gpu-vk.sum; tail -n 25 /tmp/vk_draw.out; }
grep -q 'DRAW DONE' /tmp/vk_draw.out || { sum "gpu-vk: the draw program did not reach DRAW DONE"; fail=1; }
grep -q 'draw result: EXECUTED' /tmp/vk_draw.out || { sum "gpu-vk: the draw did not execute correctly"; fail=1; }
sum "gpu-vk: after the draw: $(grep '^gpu_uapi:' /proc/kdebug)"

# G4e presentation: the same program, then 20 s of a spinning triangle rendered by the GPU (640x400, B8G8R8A8), copied to a host buffer and
# from there into /dev/fb0 frame by frame: what the monitor shows is the GPU's picture (the CPU carries it to the screen for now).
VK_DRAW_PRESENT=20 VK_PROBE_REQUIRE_EXEC=1 NVK_CONSTANOS_DEBUG=1 /mnt/bin/vk_draw > /tmp/vk_present.out 2>&1
prc=$?
grep -E 'VK present|FAIL|ASSERT|DRAW DONE' /tmp/vk_present.out | while read -r l; do sum "gpu-vk: present: $l"; done
[ $prc = 0 ] || { sum "gpu-vk: the presentation run exit=$prc"; fail=1; tail -n 20 /tmp/vk_present.out >> /tmp/gpu-vk.sum; }
grep -q 'VK present: [0-9]* frames' /tmp/vk_present.out || { sum "gpu-vk: no frames were presented"; fail=1; }
sum "gpu-vk: after the presentation: $(grep '^gpu_uapi:' /proc/kdebug)"

# G4e robustness, the very last thing (it kills the GPU channel on purpose): a launch whose program is an unbound VA. RM must tell us (RC_TRIGGERED)
# and the kernel must declare the device dead within tens of ms, wake the waiter and answer the next EXEC with EIO; RM itself must live on.
/mnt/bin/nvgpu_hw_test rc > /tmp/rc.out 2>&1
rrc=$?
grep -E 'FAIL|woke|failure' /tmp/rc.out | while read -r l; do sum "gpu-vk: rc: $l"; done
[ $rrc = 0 ] || { sum "gpu-vk: the RC test exit=$rrc"; fail=1; tail -n 12 /tmp/rc.out >> /tmp/gpu-vk.sum; }
sum "gpu-vk: after the fault: $(grep '^gpu_uapi:' /proc/kdebug)"
[ "$(field "$(grep '^gpu_uapi:' /proc/kdebug)" dead)" = 1 ] || { sum "gpu-vk: the device was not declared dead after the fault"; fail=1; }
sum "gpu-vk: $(grep '^gpu_gsprt:' /proc/kdebug)"

# RM still answers after all this, and the display is unharmed.
echo 'gsp name' > /dev/dispctl && sum "gpu-vk: RM still answers" || { sum "gpu-vk: RM does not answer"; fail=1; }
sum "gpu-vk: $(grep '^gpu_gsprt:' /proc/kdebug)"

sample() { grep '^gpu_vblank:' /proc/kdebug; }
s1=$(sample)
sleep 10
s2=$(sample)
sum "gpu-vk: t0 $s1"
sum "gpu-vk: t1 $s2"
[ "$(field "$s2" enabled)" = 1 ] || { sum "gpu-vk: vblank MSI not enabled"; fail=1; }
for x in spurious blocked gone; do
  [ "$(field "$s2" $x)" = 0 ] || { sum "gpu-vk: $x=$(field "$s2" $x)"; fail=1; }
done
q1=$(field "$s1" seq); q2=$(field "$s2" seq)
n1=$(field "$s1" last_ns); n2=$(field "$s2" last_ns)
rate=$(awk -v a="$q1" -v b="$q2" -v x="$n1" -v y="$n2" 'BEGIN { if (y > x) printf "%.4f", (b - a) * 1e9 / (y - x); else print "0" }')
sum "gpu-vk: ASUS rate $rate Hz"
awk -v r="$rate" 'BEGIN { exit !(r >= 59.9 && r <= 60.1) }' || { sum "gpu-vk: rate $rate outside 60.0 +- 0.1"; fail=1; }
# The logs again, after the 10 s: has GSP-RM written more?
sum "gpu-vk: (log put pointers at boot) $(grep '^gpu_gsp:' /proc/kdebug | tr ' ' '\n' | grep '^log_pp=')"

echo "---- summary (the log wraps) ----"
cat /tmp/gpu-vk.sum
sum "gpu-vk: verdict exit=$fail"
exit $fail
