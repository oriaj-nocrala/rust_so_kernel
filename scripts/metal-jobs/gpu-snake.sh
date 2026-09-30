# snake3d on the Ryzen: the snake in 3D on the GPU (probes/nvk/vk_snake.c), 20 s of the autopilot playing, presented through VK_KHR_swapchain
# (G5 layer 3: Mesa's headless platform is the screen on constanos; the WSI copies each image on the GPU and points the display at it, no CPU copy).
#   probes/nvk/build.py && strip -o disk-image-root/bin/snake3d ~/src/gpu-ref/nvk-probe/vk-snake   # 16 MB: check `dumpe2fs -h disk.img | grep Free`
#   touch build.rs; echo 5 > target/metal/budget
#   scripts/metal-run.sh --kconf 'gpu=uapi' scripts/metal-jobs/gpu-snake.sh
# Passes if the program prints SNAKE3D DONE, at least 50 frames per second reach the screen (the display's 60 Hz), every frame was presented, the
# autopilot scored, and the GPU (gpu_uapi) was never declared dead. The first thing on the screen to look at: depth-tested spheres, the HUD glyphs.
fail=0
sum() { echo "$*"; echo "$*" >> /tmp/gpu-snake.sum; }
: > /tmp/gpu-snake.sum
field() { echo "$1" | tr ' ' '\n' | grep "^$2=" | cut -d= -f2; }
grep -q '^uapi: installed' /proc/gpu || { sum "gpu-snake: /dev/nvgpu is not backed by the hardware (gpu=uapi?)"; fail=1; }
f0=$(grep '^gpu_flip:' /proc/kdebug)
SNAKE3D_AUTOPLAY=1 SNAKE3D_SECONDS=20 NVK_CONSTANOS_DEBUG=1 /mnt/bin/snake3d > /tmp/snake3d.out 2>&1
rc=$?
grep -E 'SNAKE3D|FAIL|ASSERT' /tmp/snake3d.out | while read -r l; do sum "gpu-snake: $l"; done
[ $rc = 0 ] || { sum "gpu-snake: snake3d exit=$rc"; fail=1; tail -n 25 /tmp/snake3d.out >> /tmp/gpu-snake.sum; }
grep -q 'SNAKE3D DONE' /tmp/snake3d.out || { sum "gpu-snake: did not reach SNAKE3D DONE"; fail=1; }
grep -q 'SNAKE3D screen [0-9]*x[0-9]* (the display)' /tmp/snake3d.out || { sum "gpu-snake: the surface had no display behind it"; fail=1; }
grep -q 'SNAKE3D swapchain of [0-9]* images' /tmp/snake3d.out || { sum "gpu-snake: no swapchain was created"; fail=1; }
fps=$(grep -o '([0-9.]* per second' /tmp/snake3d.out | tr -d '(' | cut -d' ' -f1)
awk -v a="$fps" 'BEGIN { exit !(a >= 50 && a <= 61) }' || { sum "gpu-snake: ran at '$fps' frames per second, not at the display's 60"; fail=1; }
score=$(grep -o 'score [0-9]* best' /tmp/snake3d.out | cut -d' ' -f2)
[ "${score:-0}" -gt 0 ] 2>/dev/null || [ "$(grep -o 'best [0-9]*' /tmp/snake3d.out | cut -d' ' -f2)" -gt 0 ] 2>/dev/null || { sum "gpu-snake: the autopilot never scored"; fail=1; }
f1=$(grep '^gpu_flip:' /proc/kdebug)
sum "gpu-snake: flips before: $f0"
sum "gpu-snake: flips after: $f1"
[ "$(field "$f1" refused)" = 0 ] || { sum "gpu-snake: the display refused flips"; fail=1; }
u=$(grep '^gpu_uapi:' /proc/kdebug)
sum "gpu-snake: $u"
[ "$(field "$u" dead)" = 0 ] || { sum "gpu-snake: the GPU was declared dead"; fail=1; }
# the swapchain was destroyed with its last buffer on screen, and the device closed: nothing may stay held (PRESENT's holder is released at close)
sum "gpu-snake: share counters: $(grep '^gpu_share:' /proc/kdebug)"
grep '^gpu_share:' /proc/kdebug | grep -q 'sessions=0 storage_allocs=0 syncs=0' || { sum "gpu-snake: something is still held after snake3d ended"; fail=1; }
echo "---- summary ----"
cat /tmp/gpu-snake.sum
sum "gpu-snake: verdict exit=$fail"
exit $fail
