# G5 layer 4 on the Ryzen: the GPU compositor (vk_comp) owning the screen with two Vulkan clients in windows (vk_window), whose swapchain images
# are GPU buffers it imports where they are and draws with a graphics pipeline, presented through the WSI's direct path.
#   touch build.rs; echo 5 > target/metal/budget
#   scripts/metal-run.sh --kconf 'gpu=uapi' scripts/metal-jobs/gpu-comp.sh
# vk_comp runs four times (COMP_DELAY_MS 3, 5, 7, 9), with no input devices (COMP_NO_INPUT: nobody is typing), starting two vk_window (300 frames each,
# a resize halfway: two swapchains of three buffers each) and leaving when both are done. The result is the compositor's frame rate per delay (both
# clients are in every frame, so a composition per vblank is 60 fps; 30 means every other vblank is missed: Ryzen #179 at 9 ms).
# Passes if every run is clean (vk_comp and both clients exit 0, 12 buffers imported and dropped, no COMP FAIL / VK FAIL), the best delay reaches 50 fps,
# the GPU is not dead and nothing is held afterwards (gpu_share: 0 sessions, 0 storage allocations, 0 timelines). What it cannot say is what the
# screen looked like: that is for a person (two coloured windows over the blue-grey background, their colours changing every frame, one growing halfway).
cat /proc/gpu
fail=0
sum() { echo "$*"; echo "$*" >> /tmp/gpu-comp.sum; }
: > /tmp/gpu-comp.sum
grep '^fwsec:\|^gsp:\|^vaspace:\|^copy:\|^compute:\|^uapi:' /proc/gpu >> /tmp/gpu-comp.sum
grep -q '^gsp: OK' /proc/gpu || { sum "gpu-comp: gsp did not boot"; fail=1; }
grep -q '^uapi: installed' /proc/gpu || { sum "gpu-comp: the GPU state was not kept for /dev/nvgpu"; fail=1; }
field() { echo "$1" | tr ' ' '\n' | grep "^$2=" | cut -d= -f2; }
sum "gpu-comp: before: $(grep '^gpu_uapi:' /proc/kdebug)"

# one run per COMP_DELAY_MS (how long after a frame is on the screen the next is composed): 300 frames per client, vk_comp leaves when both are done
best=0; best_d=0; runs=0
for d in 3 5 7 9; do
  VK_WINDOW_FRAMES=300 COMP_NO_INPUT=1 COMP_EXIT_WHEN_IDLE=1 COMP_SECONDS=40 COMP_DELAY_MS=$d NVK_CONSTANOS_DEBUG=1 /mnt/bin/vk_comp /mnt/bin/vk_window /mnt/bin/vk_window > /tmp/comp.out 2>&1
  rc=$?
  grep -E 'COMP (screen|quit|FAIL)|VK WINDOW|VK FAIL|COMP ASSERT|VK window: [0-9]+ frames' /tmp/comp.out | while read -r l; do sum "gpu-comp: delay=$d: $l"; done
  [ $rc = 0 ] || { sum "gpu-comp: delay=$d: vk_comp exit=$rc"; fail=1; tail -n 20 /tmp/comp.out >> /tmp/gpu-comp.sum; }
  grep -q '^COMP DONE' /tmp/comp.out || { sum "gpu-comp: delay=$d: vk_comp did not reach COMP DONE"; fail=1; }
  [ "$(grep -c 'VK WINDOW DONE' /tmp/comp.out)" = 2 ] || { sum "gpu-comp: delay=$d: not both clients finished"; fail=1; }
  grep -q 'COMP FAIL\|VK FAIL\|COMP ASSERT' /tmp/comp.out && { sum "gpu-comp: delay=$d: a FAIL line"; fail=1; }
  q=$(grep 'COMP quit after' /tmp/comp.out)
  frames=$(echo "$q" | sed -n 's/COMP quit after \([0-9]*\) frames.*/\1/p')
  ms=$(echo "$q" | sed -n 's/.*clients seen, \([0-9]*\) ms.*/\1/p')
  imports=$(echo "$q" | sed -n 's/.*, \([0-9]*\) imports.*/\1/p')
  drops=$(echo "$q" | sed -n 's/.*, \([0-9]*\) drops.*/\1/p')
  if [ "${ms:-0}" -gt 0 ] 2>/dev/null; then
    fps10=$(( ${frames:-0} * 10000 / ms ))
    sum "gpu-comp: delay=$d: frames=$frames in ${ms} ms = $((fps10 / 10)).$((fps10 % 10)) fps (300 commits per client), imports=$imports drops=$drops"
    [ "$fps10" -gt "$best" ] && { best=$fps10; best_d=$d; }
  else
    sum "gpu-comp: delay=$d: no timing in the quit line"; fail=1
  fi
  [ "${imports:-0}" = 12 ] && [ "${drops:-0}" = 12 ] || { sum "gpu-comp: delay=$d: $imports imports and $drops drops, expected 12 and 12"; fail=1; }
  runs=$((runs + 1))
done
sum "gpu-comp: best: delay=$best_d at $((best / 10)).$((best % 10)) fps"
# the display is 60 Hz and both clients are in every frame: a composition per vblank is ~60 fps; half of that means every other vblank is missed
[ "$best" -ge 500 ] || { sum "gpu-comp: no delay reached 50 fps"; fail=1; }

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
