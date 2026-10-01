# Phase 0 of docs/gpu/hw-cursor-plan.md: what the GOP left for a hardware cursor (read-only), and the baseline of the cursor as it is today.
#   touch build.rs; echo 3 > target/metal/budget
#   scripts/metal-run.sh --kconf 'gpu=uapi' scripts/metal-jobs/gpu-cursor-probe.sh        (any level >= super: dispctl must open)
# Part 1 (nothing is written to the display): `dispctl peek` reads, on the real machine, the registers whose values the design depends on:
#   - the core's ARMED and ASSEMBLY HEAD_SET_HEAD_USAGE_BOUNDS (the cursor field is bits 2:0: 0 none, 1 32x32, 2 64, 3 128, 4 256x256),
#     HEAD_SET_CONTROL_CURSOR (enable bit 31, format, size), _COMPOSITION, CONTEXT_DMA_CURSOR, OFFSET_CURSOR, for heads 0 and 1;
#   - the cursor channel of head 0 (chid 73): control 0x610604, status 0x610784 (idle = bits 19:16 == 4), the interrupt mask 0x611dac, its exception
#     slot (0x611020 + 73*12) and the first words of its user region (0x6d8000: PUT/GET, FREE at +8);
#   - window 0 for comparison (control 0x6104e4, status 0x610664).
# Part 2: the cursor path as it is (a quad in the compositor's frame), three loads of 15-20 s, the person moving the mouse THE WHOLE TIME in each:
#   1 vk_comp alone (a background and the pointer: compositions happen only because the pointer moves)
#   2 + cpumon + snake3d (autoplay) in a 960x540 window
#   3 + cpumon + snake3d (autoplay) in a 1880x1000 window (about what the user maximized in Ryzen #188)
# For each: the compositor's `pace (5 s)` lines (flips < 20 ms = 60 fps, 20-37 ms = 30 fps), its `input` lines (pointer motions, REL records) and
# the kernel's USB mouse report counter before and after: pointer motions per second against compositions per second is the baseline a hardware
# cursor has to beat. Measurement only: passes unless vk_comp fails or a channel dies.
sumfile=/tmp/gpu-cursor-probe.sum
fail=0
sum() { echo "$*"; echo "$*" >> $sumfile; }
: > $sumfile
field() { echo "$1" | tr ' ' '\n' | grep "^$2=" | cut -d= -f2; }
up() { cut -d. -f1 /proc/uptime; }
sum "gpu-cursor-probe: start at uptime $(up) s"
sum "gpu-cursor-probe: gpu: $(grep -E '^(gsp|uapi|scanout|chan):' /proc/gpu | cut -c1-120 | tr '\n' '|')"

peek() { # peek <label> <offset>...: one `peek:` line per call
  label=$1; shift
  for o in "$@"; do echo "peek $o" > /dev/dispctl 2>/dev/null || sum "gpu-cursor-probe: $label: peek $o refused"; done
  l=$(grep '^peek:' /dev/dispctl 2>/dev/null)
  sum "gpu-cursor-probe: $label: $l"
}
if [ -e /dev/dispctl ] && grep -q '^dispctl' /dev/dispctl 2>/dev/null; then
  sum "gpu-cursor-probe: $(grep '^dispctl' /dev/dispctl | cut -c1-200)"
  # core ARMED base 0x688000, ASSEMBLY base 0x680000; head h adds h*0x400
  peek "head0 ARMED usage bounds, cursor dma/offset/control/composition" 0x68a030 0x68a088 0x68a090 0x68a09c 0x68a0a0
  peek "head0 ASSEMBLY the same" 0x682030 0x682088 0x682090 0x68209c 0x6820a0
  peek "head1 ARMED the same" 0x68a430 0x68a488 0x68a490 0x68a49c 0x68a4a0
  peek "cursor channel head0: control, status, intr mask, exception slot x3" 0x610604 0x610784 0x611dac 0x61138c 0x611390 0x611394
  peek "cursor channel head0 user region: +0 +4 +8(FREE) +0x200 +0x208" 0x6d8000 0x6d8004 0x6d8008 0x6d8200 0x6d8208
  peek "window0 for comparison: control, status; core control/status" 0x6104e4 0x610664 0x6104e0 0x610630
else
  sum "gpu-cursor-probe: /dev/dispctl does not open (needs gpu=super or higher): no registers read"
fi

usbm() { grep -E '^usb_mouse_reports' /proc/kdebug | cut -d' ' -f2; }
run() {
  label=$1; secs=$2; shift 2
  dead0=$(field "$(grep '^gpu_uapi:' /proc/kdebug)" chans_dead)
  m0=$(usbm); t0=$(up)
  sum "gpu-cursor-probe: $label: start at uptime $t0 s for $secs s (move the mouse all the time)"
  env COMP_NO_PANEL=1 COMP_SECONDS=$secs SNAKE3D_WINDOW=1 SNAKE3D_AUTOPLAY=1 NVK_CONSTANOS_DEBUG=1 "$@" > /tmp/comp.out 2>&1
  rc=$?
  m1=$(usbm); dead1=$(field "$(grep '^gpu_uapi:' /proc/kdebug)" chans_dead)
  sum "gpu-cursor-probe: $label: ended after $(( $(up) - t0 )) s, vk_comp exit=$rc, channels lost: $(( dead1 - dead0 )), USB mouse reports: $(( ${m1:-0} - ${m0:-0} ))"
  grep -E '^COMP (input|pace|[0-9]+ frames|quit|FAIL|ended)|SNAKE3D [0-9]+ frames|SNAKE3D (resized|window)' /tmp/comp.out | cut -c1-330 | while read -r l; do sum "gpu-cursor-probe: $label: $l"; done
  [ "$rc" = 0 ] || fail=1
  [ "$dead1" = "$dead0" ] || fail=1
}
run "1 alone" 15 /mnt/bin/vk_comp
run "2 cpumon+snake3d 960x540" 20 /mnt/bin/vk_comp cpumon snake3d
run "3 cpumon+snake3d 1880x1000" 20 SNAKE3D_W=1880 SNAKE3D_H=1000 /mnt/bin/vk_comp cpumon snake3d

sum "gpu-cursor-probe: end: $(grep '^gpu_uapi:' /proc/kdebug | cut -c1-260)"
sum "gpu-cursor-probe: $(grep '^gpu_share:' /proc/kdebug)"
sum "gpu-cursor-probe: $(grep '^gpu_flip:' /proc/kdebug | cut -c1-260)"
grep -a -E '\[nvgpu\] channel .* is dead|\[gsp\] event' /proc/dmesg | tail -n 6 | cut -c1-300 | while read -r l; do sum "gpu-cursor-probe: kernel: $l"; done
echo "---- summary (the log wraps) ----"
cat $sumfile
sum "gpu-cursor-probe: verdict exit=$fail at uptime $(up) s"
exit $fail
