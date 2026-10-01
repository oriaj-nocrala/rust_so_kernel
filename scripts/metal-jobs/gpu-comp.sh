# G5 layer 4 on the Ryzen: the GPU compositor (vk_comp) owning the screen with two Vulkan clients in windows (vk_window), whose swapchain images
# are GPU buffers it imports where they are and draws with a graphics pipeline, presented through the WSI's direct path.
#   touch build.rs; echo 5 > target/metal/budget
#   scripts/metal-run.sh --kconf 'gpu=uapi' scripts/metal-jobs/gpu-comp.sh
# vk_comp runs up to 40 s with no input devices (COMP_NO_INPUT: nobody is typing), starting two vk_window (900 frames each, a resize halfway: two
# swapchains of three buffers each). Passes if vk_comp and both clients exit cleanly, vk_comp imported and dropped the 12 buffers, composed
# between 600 and 1200 frames (the display's 60 Hz over the windows' life, both clients in each frame), nothing printed COMP FAIL / VK FAIL, the GPU is not dead and nothing is held
# afterwards (gpu_share: 0 sessions, 0 storage allocations, 0 timelines). What it cannot say is what the screen looked like: that is for a person
# (two coloured windows over the blue-grey background, their colours changing every frame, one growing halfway).
cat /proc/gpu
fail=0
sum() { echo "$*"; echo "$*" >> /tmp/gpu-comp.sum; }
: > /tmp/gpu-comp.sum
grep '^fwsec:\|^gsp:\|^vaspace:\|^copy:\|^compute:\|^uapi:' /proc/gpu >> /tmp/gpu-comp.sum
grep -q '^gsp: OK' /proc/gpu || { sum "gpu-comp: gsp did not boot"; fail=1; }
grep -q '^uapi: installed' /proc/gpu || { sum "gpu-comp: the GPU state was not kept for /dev/nvgpu"; fail=1; }
field() { echo "$1" | tr ' ' '\n' | grep "^$2=" | cut -d= -f2; }
sum "gpu-comp: before: $(grep '^gpu_uapi:' /proc/kdebug)"

VK_WINDOW_FRAMES=900 COMP_NO_INPUT=1 COMP_SECONDS=40 NVK_CONSTANOS_DEBUG=1 /mnt/bin/vk_comp /mnt/bin/vk_window /mnt/bin/vk_window > /tmp/comp.out 2>&1
rc=$?
grep -E 'COMP (screen|listening|started|client|quit|FAIL|[0-9]+ frames)|VK window:|VK WINDOW|VK FAIL|COMP ASSERT' /tmp/comp.out | while read -r l; do sum "gpu-comp: $l"; done
[ $rc = 0 ] || { sum "gpu-comp: vk_comp exit=$rc"; fail=1; tail -n 30 /tmp/comp.out >> /tmp/gpu-comp.sum; }
grep -q '^COMP DONE' /tmp/comp.out || { sum "gpu-comp: vk_comp did not reach COMP DONE"; fail=1; }
grep -q 'COMP screen [0-9]*x[0-9]* (the display)' /tmp/comp.out || { sum "gpu-comp: no display behind the surface"; fail=1; }
[ "$(grep -c 'VK WINDOW DONE' /tmp/comp.out)" = 2 ] || { sum "gpu-comp: not both clients finished (VK WINDOW DONE x$(grep -c 'VK WINDOW DONE' /tmp/comp.out))"; fail=1; }
grep -q 'COMP FAIL\|VK FAIL\|COMP ASSERT' /tmp/comp.out && { sum "gpu-comp: a FAIL line"; fail=1; }
q=$(grep 'COMP quit after' /tmp/comp.out)
frames=$(echo "$q" | sed -n 's/COMP quit after \([0-9]*\) frames.*/\1/p')
imports=$(echo "$q" | sed -n 's/.*, \([0-9]*\) imports.*/\1/p')
drops=$(echo "$q" | sed -n 's/.*, \([0-9]*\) drops.*/\1/p')
sum "gpu-comp: frames=$frames imports=$imports drops=$drops"
[ "${frames:-0}" -ge 600 ] 2>/dev/null || { sum "gpu-comp: only ${frames:-0} frames composed"; fail=1; }
# two clients of 900 commits each, paced by the compositor: one composition per vblank serves both, so about 900 frames; far more means each vblank
# served one client (Ryzen #177: 1800 frames, each client at 32 fps)
[ "${frames:-0}" -le 1200 ] 2>/dev/null || { sum "gpu-comp: ${frames:-0} frames composed for 2 x 900 commits: the clients do not share frames"; fail=1; }
[ "${imports:-0}" = 12 ] || { sum "gpu-comp: $imports buffers imported, expected 12"; fail=1; }
[ "${drops:-0}" = 12 ] || { sum "gpu-comp: $drops buffers dropped, expected 12"; fail=1; }

u=$(grep '^gpu_uapi:' /proc/kdebug)
sum "gpu-comp: end: $u"
sum "gpu-comp: slow holds of the GPU lock: $(grep '^gpu_uapi_slow:' /proc/kdebug)"
[ "$(field "$u" dead)" = 0 ] || { sum "gpu-comp: the GPU was declared dead"; fail=1; }
[ "$(field "$u" chans_dead)" = 0 ] || { sum "gpu-comp: a channel was lost"; fail=1; }
s=$(grep '^gpu_share:' /proc/kdebug)
sum "gpu-comp: $s"
echo "$s" | grep -q 'sessions=0 storage_allocs=0 syncs=0' || { sum "gpu-comp: something is still held after every program ended"; fail=1; }
echo 'gsp name' > /dev/dispctl && sum "gpu-comp: RM still answers" || { sum "gpu-comp: RM does not answer"; fail=1; }

echo "---- summary (the log wraps) ----"
cat /tmp/gpu-comp.sum
sum "gpu-comp: verdict exit=$fail"
exit $fail
