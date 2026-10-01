# What kills the compositor's GPU channel when there is input (Ryzen #184 and #185: `COMP FAIL present (-4)` = VK_ERROR_DEVICE_LOST seconds after the
# first keys / mouse motion, `[nvgpu] channel 0 (chid 1) is dead: RM reset it (RC_TRIGGERED)`; the runs with COMP_NO_INPUT never failed)?
# Four short runs, every one with the keyboard and mouse open, the person at the machine moving the mouse and tapping keys THE WHOLE TIME:
#   1  vk_comp alone (a background and the cursor: nothing but the renderer, the cursor and the input)
#   2  vk_comp + cpumon (a CPU-drawn window)
#   3  vk_comp + snake3d (a Vulkan client in a window)
#   4  vk_comp + cpumon + snake3d (what failed)
# Each lasts 25 s (COMP_SECONDS) unless the compositor fails first; the summary says, for each, whether the channel died, when (frame, seconds), how
# many key / pointer / button events had arrived, and the kernel's own line about the dead channel.
#   touch build.rs; echo 3 > target/metal/budget
#   scripts/metal-run.sh --kconf 'gpu=uapi' scripts/metal-jobs/gpu-apps-d2.sh
sumfile=/tmp/gpu-apps-d2.sum
fail=0
sum() { echo "$*"; echo "$*" >> $sumfile; }
: > $sumfile
field() { echo "$1" | tr ' ' '\n' | grep "^$2=" | cut -d= -f2; }
up() { cut -d. -f1 /proc/uptime; }
grep -q '^gsp: OK' /proc/gpu || { sum "gpu-apps-d2: gsp did not boot"; fail=1; }
grep -q '^uapi: installed' /proc/gpu || { sum "gpu-apps-d2: the GPU state was not kept for /dev/nvgpu"; fail=1; }
sum "gpu-apps-d2: start at uptime $(up) s; input devices: $(ls /dev/input 2>&1 | tr '\n' ' ')"

run() {
  label=$1; shift
  dead0=$(field "$(grep '^gpu_uapi:' /proc/kdebug)" chans_dead)
  t0=$(up)
  sum "gpu-apps-d2: $label: start at uptime $t0 s (move the mouse, tap keys)"
  COMP_NO_PANEL=1 COMP_SECONDS=25 SNAKE3D_WINDOW=1 NVK_CONSTANOS_DEBUG=1 /mnt/bin/vk_comp "$@" > /tmp/comp.out 2>&1
  rc=$?
  dead1=$(field "$(grep '^gpu_uapi:' /proc/kdebug)" chans_dead)
  sum "gpu-apps-d2: $label: ended after $(( $(up) - t0 )) s, vk_comp exit=$rc, channels lost: $(( dead1 - dead0 ))"
  grep -E '^COMP (FAIL|input|quit|ended)|SNAKE3D [0-9]+ frames|SNAKE3D the compositor' /tmp/comp.out | cut -c1-340 | while read -r l; do sum "gpu-apps-d2: $label: $l"; done
  grep -a '\[nvgpu\] channel .* is dead' /proc/dmesg | tail -n 1 | cut -c1-260 | while read -r l; do sum "gpu-apps-d2: $label: kernel: $l"; done
  [ "$rc" = 0 ] || fail=1
  [ "$dead1" = "$dead0" ] || fail=1
}
run "1 alone"
run "2 cpumon" cpumon
run "3 snake3d" snake3d
run "4 cpumon+snake3d" cpumon snake3d

u=$(grep '^gpu_uapi:' /proc/kdebug)
sum "gpu-apps-d2: end: $(echo "$u" | cut -c1-330)"
sum "gpu-apps-d2: $(grep '^gpu_share:' /proc/kdebug)"
echo "---- summary (the log wraps) ----"
cat $sumfile
sum "gpu-apps-d2: verdict exit=$fail at uptime $(up) s"
exit $fail
