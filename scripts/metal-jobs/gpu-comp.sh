# G5 layer 4 on the Ryzen: the GPU compositor (vk_comp) owning the screen with Vulkan clients in windows (vk_window), whose swapchain images
# are GPU buffers it imports where they are and draws with a graphics pipeline, presented through the WSI's direct path.
#   touch build.rs; echo 5 > target/metal/budget
#   scripts/metal-run.sh --kconf 'gpu=uapi' scripts/metal-jobs/gpu-comp.sh
# vk_comp runs once per COMP_DELAY_MS (2 = the default, 0, 4, 6) with two vk_window, then once at 2 ms with a single vk_window (pacing without
# sharing the frame), with no input devices (COMP_NO_INPUT: nobody is typing). Each vk_window presents 300 frames with a resize halfway (two swapchains of
# three buffers each); vk_comp leaves once every program it started has exited and every client has gone (COMP_EXIT_WHEN_IDLE).
# For each run the summary keeps vk_comp's and the clients' own lines (start times, how each program ended, `COMP pace (all)`: where a
# composition's time goes, how far apart its flips land, and the latest PRESENT that made its vblank) and the kernel's flip counters before and after (`gpu_flip:`: latency from
# PRESENT to the flip seen done, vblanks per flip). Both clients are in every frame, so a composition per vblank is 60 fps; ~30 means every
# other vblank is missed (Ryzen #179-#181 at 5-9 ms, before the clients were answered at repaint).
# Passes if every run is clean (vk_comp and the clients exit 0, 12 buffers imported and dropped, 6 for the single client, no FAIL line), the
# best two-client delay reaches 50 fps, the GPU is not dead and nothing is held afterwards (gpu_share: 0 sessions, 0 storage allocations,
# 0 timelines). What it cannot say is what the screen looked like: that is for a person (coloured windows over the blue-grey background,
# their colours changing every frame, each growing halfway).
cat /proc/gpu
fail=0
sum() { echo "$*"; echo "$*" >> /tmp/gpu-comp.sum; }
: > /tmp/gpu-comp.sum
grep '^fwsec:\|^gsp:\|^vaspace:\|^copy:\|^compute:\|^uapi:' /proc/gpu >> /tmp/gpu-comp.sum
grep -q '^gsp: OK' /proc/gpu || { sum "gpu-comp: gsp did not boot"; fail=1; }
grep -q '^uapi: installed' /proc/gpu || { sum "gpu-comp: the GPU state was not kept for /dev/nvgpu"; fail=1; }
field() { echo "$1" | tr ' ' '\n' | grep "^$2=" | cut -d= -f2; }
sum "gpu-comp: before: $(grep '^gpu_uapi:' /proc/kdebug)"
sum "gpu-comp: $(grep '^gpu_vblank:' /proc/kdebug | cut -c1-120)"

# run <label> <delay> <clients> <expected imports>
best=0; best_d=0
run() {
  label=$1; d=$2; n=$3; want=$4
  progs=""; i=0; while [ $i -lt $n ]; do progs="$progs /mnt/bin/vk_window"; i=$((i + 1)); done
  sum "gpu-comp: $label: flips before: $(grep '^gpu_flip:' /proc/kdebug | cut -d' ' -f2-)"
  t0=$(cut -d' ' -f1 /proc/uptime)
  VK_WINDOW_FRAMES=300 COMP_NO_INPUT=1 COMP_EXIT_WHEN_IDLE=1 COMP_SECONDS=60 COMP_DELAY_MS=$d NVK_CONSTANOS_DEBUG=1 /mnt/bin/vk_comp $progs > /tmp/comp.out 2>&1
  rc=$?
  sum "gpu-comp: $label: ran $t0 .. $(cut -d' ' -f1 /proc/uptime) s, vk_comp exit=$rc"
  sum "gpu-comp: $label: flips after: $(grep '^gpu_flip:' /proc/kdebug | cut -d' ' -f2-)"
  grep -E '^COMP (started|cannot|program|client [0-9]+ (is not|disconnected)|pace \(all\)|quit|FAIL|ASSERT)|VK WINDOW|VK FAIL|VK ASSERT|VK window: [0-9]+ frames' /tmp/comp.out | head -n 30 | while read -r l; do sum "gpu-comp: $label: $l"; done
  [ $rc = 0 ] || { fail=1; tail -n 20 /tmp/comp.out >> /tmp/gpu-comp.sum; }
  grep -q '^COMP DONE' /tmp/comp.out || { sum "gpu-comp: $label: vk_comp did not reach COMP DONE"; fail=1; }
  [ "$(grep -c 'VK WINDOW DONE' /tmp/comp.out)" = "$n" ] || { sum "gpu-comp: $label: not every client finished"; fail=1; }
  grep -q 'COMP FAIL\|VK FAIL\|COMP ASSERT\|VK ASSERT' /tmp/comp.out && { sum "gpu-comp: $label: a FAIL line"; fail=1; }
  q=$(grep 'COMP quit after' /tmp/comp.out)
  # the rate while clients were connected (the quit line's total includes loading the programs: 15 MB each)
  frames=$(echo "$q" | sed -n 's/.*with clients: \([0-9]*\) frames.*/\1/p')
  ms=$(echo "$q" | sed -n 's/.*with clients: [0-9]* frames in \([0-9]*\) ms.*/\1/p')
  imports=$(echo "$q" | sed -n 's/.*, \([0-9]*\) imports.*/\1/p')
  drops=$(echo "$q" | sed -n 's/.*, \([0-9]*\) drops.*/\1/p')
  fps10=0
  if [ "${ms:-0}" -gt 0 ] 2>/dev/null; then
    fps10=$(( ${frames:-0} * 10000 / ms ))
    sum "gpu-comp: $label: $frames frames in ${ms} ms with clients = $((fps10 / 10)).$((fps10 % 10)) fps (300 commits per client), imports=$imports drops=$drops"
  else
    sum "gpu-comp: $label: no timing in the quit line"; fail=1
  fi
  [ "${imports:-0}" = "$want" ] && [ "${drops:-0}" = "$want" ] || { sum "gpu-comp: $label: $imports imports and $drops drops, expected $want and $want"; fail=1; }
  if [ "$n" = 2 ] && [ "$fps10" -gt "$best" ]; then best=$fps10; best_d=$d; fi
}

for d in 2 0 4 6; do run "delay=$d" $d 2 12; done
run "one-client delay=2" 2 1 6
sum "gpu-comp: best two-client delay: $best_d ms at $((best / 10)).$((best % 10)) fps"
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
