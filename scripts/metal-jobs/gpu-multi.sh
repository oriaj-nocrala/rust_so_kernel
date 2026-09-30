# G5 layer 1 of docs/gpu/g5-graphics-stack-plan.md, on the Ryzen: several processes on the GPU at once, each with a session of its own.
#   touch build.rs; echo 5 > target/metal/budget
#   scripts/metal-run.sh --kconf 'gpu=uapi' scripts/metal-jobs/gpu-multi.sh
# 1. nvgpu_hw_test in full (its section 6: four processes, four sessions with four different VA ranges, each launching compute 40 times at the
#    same time, every output read back and checked), as gpu-uapi.sh runs it.
# 2. Vulkan: one vk_draw owns the display (VK_DRAW_SCANOUT=12) while three offscreen vk_draw run beside it, each of which must draw and read back
#    its triangle; a second presenter must be refused (PRESENT = EBUSY, "PRESENT failed (-16)"); the owner must still reach the display's 60 Hz.
# 3. Two offscreen vk_draw started at the same instant, five times.
# 5. G5 layer 2, explicit sync: nvgpu_hw_test's section 8 (each process queues GPU work that signals a shared timeline and sits idle on a socket
#    while the other waits on the timeline and reads what that work wrote) and `vk_share` with an exportable timeline semaphore.
#    At the end nothing may be held: /proc/kdebug's gpu_share: line must say no session, no storage allocation, no shared timeline.
# 4. G5 layer 2, buffers shared between processes: nvgpu_hw_test's section 7 (the parent's GPU and a child's GPU write the same system pages
#    and a VRAM page through VAs of their own, the other side reads them) runs in step 1; here `vk_share` does it through Vulkan: exportable
#    memory (VK_KHR_external_memory_fd), the fd sent by SCM_RIGHTS to a child with a device of its own, which imports it; each process's
#    vkCmdFillBuffer writes a third, the child's CPU the last, everything checked through the parent's mapping (VK_PROBE_REQUIRE_EXEC=1).
# Fails unless every program says DRAW DONE / EXECUTED / SHARE DONE, the GPU is not dead and no channel was lost (chans_dead=0).
cat /proc/gpu
fail=0
sum() { echo "$*"; echo "$*" >> /tmp/gpu-multi.sum; }
: > /tmp/gpu-multi.sum
grep '^fwsec:\|^gsp:\|^vaspace:\|^copy:\|^compute:\|^uapi:' /proc/gpu >> /tmp/gpu-multi.sum
grep '^chan: STOP\|^super: STOP\|^hdmi: STOP\|^fwsec: STOP\|^gsp: STOP\|^vaspace: STOP\|^copy: STOP\|^compute: STOP\|^bench: STOP\|^uapi: STOP' /proc/gpu && fail=1
grep -q '^gsp: OK' /proc/gpu || { sum "gpu-multi: gsp did not boot"; fail=1; }
grep -q '^uapi: installed' /proc/gpu || { sum "gpu-multi: the GPU state was not kept for /dev/nvgpu"; fail=1; }
field() { echo "$1" | tr ' ' '\n' | grep "^$2=" | cut -d= -f2; }
sum "gpu-multi: before: $(grep '^gpu_uapi:' /proc/kdebug)"

# ---- 1. the test program, sessions in parallel
/mnt/bin/nvgpu_hw_test > /tmp/nvgpu_hw_test.out 2>&1
rc=$?
grep -E 'FAIL|child|clients|four|own client|failure' /tmp/nvgpu_hw_test.out | while read -r l; do sum "gpu-multi: test: $l"; done
[ $rc = 0 ] || { sum "gpu-multi: nvgpu_hw_test exit=$rc"; fail=1; tail -n 12 /tmp/nvgpu_hw_test.out >> /tmp/gpu-multi.sum; tail -n 12 /tmp/nvgpu_hw_test.out; }
grep -q 'hardware' /tmp/nvgpu_hw_test.out || { sum "gpu-multi: the test ran against the software device"; fail=1; }
grep -q 'skip ' /tmp/nvgpu_hw_test.out && { sum "gpu-multi: execution checks were skipped"; fail=1; }
grep -q 'ok   distinct' /tmp/nvgpu_hw_test.out || { sum "gpu-multi: the four sessions were not shown to have four VA ranges"; fail=1; }
grep -q 'ok   all_ok' /tmp/nvgpu_hw_test.out || { sum "gpu-multi: not every client ran its launches right"; fail=1; }
grep -q 'ok   code8 == 0' /tmp/nvgpu_hw_test.out && grep -q 'ok   wait_timeline(t8, 2' /tmp/nvgpu_hw_test.out || { sum "gpu-multi: the shared-timeline section (8) did not pass"; fail=1; }
grep -q 'ok   out_is(&sh8, 1, fillwt_word' /tmp/nvgpu_hw_test.out || { sum "gpu-multi: the child's GPU output was not in the parent's pages after the shared timeline said so"; fail=1; }
grep -q 'ok   code7 == 0' /tmp/nvgpu_hw_test.out || { sum "gpu-multi: the shared-buffer section (7) did not pass"; fail=1; }
grep -q 'ok   out_is(&sh, 0, fillwt_word' /tmp/nvgpu_hw_test.out && grep -q 'ok   out_is(&sh, 1, fill_word' /tmp/nvgpu_hw_test.out || { sum "gpu-multi: the child's GPU results were not in the parent's shared pages"; fail=1; }
grep -q 'FAIL' /tmp/nvgpu_hw_test.out && fail=1
u1=$(grep '^gpu_uapi:' /proc/kdebug)
sum "gpu-multi: after the test: $u1"
[ "$(field "$u1" dead)" = 0 ] || { sum "gpu-multi: the GPU was declared dead"; fail=1; }

# ---- 2. one client owns the display, others draw beside it
stamp() { cut -d' ' -f1 /proc/uptime; }
# 2a. the owner alone first: its frame pacing without anyone else (a baseline for the hitches seen next to other clients)
VK_DRAW_SCANOUT=8 VK_PROBE_REQUIRE_EXEC=1 /mnt/bin/vk_draw > /tmp/solo.out 2>&1
grep -E 'loop starts|hitch|frames shown|FAIL|ASSERT' /tmp/solo.out | while read -r l; do sum "gpu-multi: solo owner: $l"; done
sum "gpu-multi: solo owner done at uptime $(stamp)"
sum "gpu-multi: owner starts at uptime $(stamp)"
VK_DRAW_SCANOUT=12 VK_PROBE_REQUIRE_EXEC=1 /mnt/bin/vk_draw > /tmp/own.out 2>&1 &
owner=$!
sleep 5   # the owner is presenting by now (its start-up took about as long in the single-client runs)
sum "gpu-multi: side draws start at uptime $(stamp)"
n=0
for i in 1 2 3; do
  VK_PROBE_REQUIRE_EXEC=1 /mnt/bin/vk_draw > /tmp/side$i.out 2>&1 &
  eval "side$i=\$!"
done
for i in 1 2 3; do
  eval "wait \$side$i"
  src=$?
  grep -q 'DRAW DONE' /tmp/side$i.out && grep -q 'draw result: EXECUTED' /tmp/side$i.out && [ $src = 0 ] && sum "gpu-multi: side draw $i beside the presenter: ok" || { sum "gpu-multi: side draw $i failed (exit=$src)"; fail=1; tail -n 15 /tmp/side$i.out >> /tmp/gpu-multi.sum; }
done
sum "gpu-multi: side draws done, the second presenter starts at uptime $(stamp)"
VK_DRAW_SCANOUT=2 VK_PROBE_REQUIRE_EXEC=1 /mnt/bin/vk_draw > /tmp/second.out 2>&1
src=$?
sum "gpu-multi: the second presenter done at uptime $(stamp)"
grep -q 'PRESENT failed (-16)' /tmp/second.out && [ $src != 0 ] && sum "gpu-multi: the second presenter was refused with EBUSY (exit=$src)" || { sum "gpu-multi: the second presenter was not refused (exit=$src)"; fail=1; tail -n 10 /tmp/second.out >> /tmp/gpu-multi.sum; }
wait $owner
orc=$?
sum "gpu-multi: owner done at uptime $(stamp)"
grep -E 'VK scanout|FAIL|ASSERT|DRAW DONE' /tmp/own.out | while read -r l; do sum "gpu-multi: owner: $l"; done
[ $orc = 0 ] || { sum "gpu-multi: the owner exit=$orc"; fail=1; tail -n 20 /tmp/own.out >> /tmp/gpu-multi.sum; }
grep -q 'VK scanout: [0-9]* frames shown' /tmp/own.out || { sum "gpu-multi: the owner showed no frames"; fail=1; }
# the presenter shared the GPU with three other clients, so its rate is reported; it must still keep up with the display
awk -v a="$(grep -o 'frames shown in [0-9.]* s ([0-9.]*' /tmp/own.out | sed 's/.*(//')" 'BEGIN { exit !(a >= 40 && a <= 61) }' || { sum "gpu-multi: the owner ran at $(grep -o '([0-9.]* per second' /tmp/own.out) frames"; fail=1; }
sum "gpu-multi: after the presenter: $(grep '^gpu_uapi:' /proc/kdebug)"
sum "gpu-multi: slow holds of the GPU lock so far: $(grep '^gpu_uapi_slow:' /proc/kdebug)"

# ---- 3. two at the same instant
for round in 1 2 3 4 5; do
  VK_PROBE_REQUIRE_EXEC=1 /mnt/bin/vk_draw > /tmp/pa.out 2>&1 &
  pa=$!
  VK_PROBE_REQUIRE_EXEC=1 /mnt/bin/vk_draw > /tmp/pb.out 2>&1 &
  pb=$!
  wait $pa; ra=$?
  wait $pb; rb=$?
  if [ $ra = 0 ] && [ $rb = 0 ] && grep -q 'draw result: EXECUTED' /tmp/pa.out && grep -q 'draw result: EXECUTED' /tmp/pb.out; then
    sum "gpu-multi: pair $round: both drew"
  else
    sum "gpu-multi: pair $round failed (exits $ra $rb)"; fail=1; tail -n 12 /tmp/pa.out >> /tmp/gpu-multi.sum; tail -n 12 /tmp/pb.out >> /tmp/gpu-multi.sum
  fi
done

# ---- 4. memory shared through Vulkan between two processes
VK_PROBE_REQUIRE_EXEC=1 NVK_CONSTANOS_DEBUG=1 /mnt/bin/vk_share > /tmp/vk_share.out 2>&1
shrc=$?
grep -E 'VK (parent|child)|FAIL|ASSERT|SHARE' /tmp/vk_share.out | while read -r l; do sum "gpu-multi: share: $l"; done
[ $shrc = 0 ] || { sum "gpu-multi: vk_share exit=$shrc"; fail=1; tail -n 25 /tmp/vk_share.out >> /tmp/gpu-multi.sum; }
grep -q 'VK SHARE DONE' /tmp/vk_share.out || { sum "gpu-multi: vk_share did not reach SHARE DONE"; fail=1; }
grep -q 'VK skip' /tmp/vk_share.out && { sum "gpu-multi: vk_share skipped its GPU checks"; fail=1; }
sum "gpu-multi: after the share: $(grep '^gpu_uapi:' /proc/kdebug)"
sum "gpu-multi: share counters: $(grep '^gpu_share:' /proc/kdebug)"
echo "$(grep '^gpu_share:' /proc/kdebug)" | grep -q 'sessions=0 storage_allocs=0 syncs=0' || { sum "gpu-multi: something is still held after every program ended"; fail=1; }

u2=$(grep '^gpu_uapi:' /proc/kdebug)
sum "gpu-multi: end: $u2"
sum "gpu-multi: slow holds of the GPU lock at the end: $(grep '^gpu_uapi_slow:' /proc/kdebug)"
[ "$(field "$u2" dead)" = 0 ] || { sum "gpu-multi: the GPU was declared dead"; fail=1; }
[ "$(field "$u2" chans_dead)" = 0 ] || { sum "gpu-multi: a channel was lost (chans_dead=$(field "$u2" chans_dead))"; fail=1; }
[ "$(field "$u2" chans_made)" -gt 0 ] 2>/dev/null || { sum "gpu-multi: no run-time channel was ever made"; fail=1; }
echo 'gsp name' > /dev/dispctl && sum "gpu-multi: RM still answers" || { sum "gpu-multi: RM does not answer"; fail=1; }

echo "---- summary (the log wraps) ----"
cat /tmp/gpu-multi.sum
sum "gpu-multi: verdict exit=$fail"
exit $fail
