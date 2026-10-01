# Only phase D of gpu-apps.sh, to chase what killed the compositor's channel in Ryzen #184 (COMP FAIL present (-4) = VK_ERROR_DEVICE_LOST at 19 s,
# chans_dead=1, no reason kept in the log): vk_comp + cpumon + snake3d with the keyboard and mouse, for 100 s, nobody scripting anything.
#   touch build.rs; echo 3 > target/metal/budget
#   scripts/metal-run.sh --kconf 'gpu=uapi' scripts/metal-jobs/gpu-apps-d.sh
# Play with WASD/arrows (click the snake window first), move the mouse, drag a window, press F11. Q quits the snake, Ctrl+Alt+Backspace the compositor.
# What it adds over D in gpu-apps.sh: vk_comp counts the key / pointer / button events it received (`COMP input` lines: is the mouse alive?), and the
# summary ends with the kernel's own account (/proc/dmesg lines about nvgpu, which names the reason a channel died).
sumfile=/tmp/gpu-apps-d.sum
fail=0
sum() { echo "$*"; echo "$*" >> $sumfile; }
: > $sumfile
field() { echo "$1" | tr ' ' '\n' | grep "^$2=" | cut -d= -f2; }
up() { cut -d. -f1 /proc/uptime; }
grep -q '^gsp: OK' /proc/gpu || { sum "gpu-apps-d: gsp did not boot"; fail=1; }
grep -q '^uapi: installed' /proc/gpu || { sum "gpu-apps-d: the GPU state was not kept for /dev/nvgpu"; fail=1; }
sum "gpu-apps-d: start at uptime $(up) s"
sum "gpu-apps-d: before: $(grep '^gpu_uapi:' /proc/kdebug | cut -c1-200)"
sum "gpu-apps-d: input devices: $(ls /dev/input 2>&1 | tr '\n' ' ')"
t0=$(up)
COMP_SECONDS=100 SNAKE3D_WINDOW=1 NVK_CONSTANOS_DEBUG=1 /mnt/bin/vk_comp cpumon snake3d > /tmp/comp.out 2>&1
rc=$?
sum "gpu-apps-d: ran uptime $t0 .. $(up) s, vk_comp exit=$rc"
grep -E '^COMP (started|cannot|program|ended|F11|client [0-9]+ (is not|disconnected|connected|gone)|input|pace \(5 s\)|pace \(all\)|quit|FAIL|ASSERT|DONE|FAILED)|SNAKE3D (window|resized|[0-9]+ frames|FAIL|DONE|using|the compositor)' /tmp/comp.out | cut -c1-330 | head -n 70 | while read -r l; do sum "gpu-apps-d: $l"; done
[ "$rc" = 0 ] || { sum "gpu-apps-d: vk_comp exit=$rc"; fail=1; }
grep -q 'COMP FAIL' /tmp/comp.out && { sum "gpu-apps-d: a COMP FAIL line"; fail=1; }
u=$(grep '^gpu_uapi:' /proc/kdebug)
sum "gpu-apps-d: end: $u"
sum "gpu-apps-d: slow holds of the GPU lock: $(grep '^gpu_uapi_slow:' /proc/kdebug | cut -c1-400)"
[ "$(field "$u" chans_dead)" = 0 ] || { sum "gpu-apps-d: a channel was lost"; fail=1; }
sum "gpu-apps-d: $(grep '^gpu_share:' /proc/kdebug)"
sum "gpu-apps-d: end: $(grep '^gpu_flip:' /proc/kdebug)"
# the kernel's account: why a channel died, any RC / fault / alert in the last stretch
sum "gpu-apps-d: dmesg (nvgpu, RC, fault, alert):"
grep -a -i -E 'nvgpu|RC_TRIGGERED|rc event|fault|alert|channel' /proc/dmesg | grep -a -v 'gpu-apps-d' | tail -n 30 | cut -c1-300 | while read -r l; do sum "gpu-apps-d:   $l"; done
echo "---- summary (the log wraps) ----"
cat $sumfile
sum "gpu-apps-d: verdict exit=$fail at uptime $(up) s"
exit $fail
