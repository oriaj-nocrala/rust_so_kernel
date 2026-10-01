# Does a USB mouse event kill the compositor's GPU channel because it ARRIVES (the kernel's USB path, whatever reads it) or because vk_comp READS it
# (Ryzen #184-#186: with the keyboard and mouse open the channel dies within seconds of the first motion, at the first frame in #186; with
# COMP_NO_INPUT it never did)? The same load twice, 20 s each, the person moving the mouse and tapping keys THE WHOLE TIME in both:
#   1  vk_comp with COMP_NO_INPUT=1 (it opens neither device) + cpumon + snake3d (autoplay)
#   2  the same with the keyboard and mouse open
# The kernel's USB counters (`usb_*` lines of /proc/kdebug) before and after each run show whether events arrived; `[gsp] event ...` lines are RM's own
# account of an error (the words of an RC / MMU fault, the text of an error log; the first 32 only) and `[nvgpu] channel .. is dead` names the victim.
# If 1 survives with events arriving and 2 dies, reading is the trigger (the evdev path, the cursor, the pointer); if both die, arrival is (USB).
#   touch build.rs; echo 3 > target/metal/budget
#   scripts/metal-run.sh --kconf 'gpu=uapi' scripts/metal-jobs/gpu-apps-d3.sh
sumfile=/tmp/gpu-apps-d3.sum
fail=0
sum() { echo "$*"; echo "$*" >> $sumfile; }
: > $sumfile
field() { echo "$1" | tr ' ' '\n' | grep "^$2=" | cut -d= -f2; }
up() { cut -d. -f1 /proc/uptime; }
grep -q '^gsp: OK' /proc/gpu || { sum "gpu-apps-d3: gsp did not boot"; fail=1; }
grep -q '^uapi: installed' /proc/gpu || { sum "gpu-apps-d3: the GPU state was not kept for /dev/nvgpu"; fail=1; }
sum "gpu-apps-d3: start at uptime $(up) s"
usb() { grep -E '^(usb_|mouse_resyncs)' /proc/kdebug | cut -c1-200 | tr '\n' '|'; }

run() {
  label=$1; shift
  dead0=$(field "$(grep '^gpu_uapi:' /proc/kdebug)" chans_dead)
  sum "gpu-apps-d3: $label: usb before: $(usb)"
  t0=$(up)
  sum "gpu-apps-d3: $label: start at uptime $t0 s (move the mouse, tap keys)"
  env "$@" > /tmp/comp.out 2>&1
  rc=$?
  dead1=$(field "$(grep '^gpu_uapi:' /proc/kdebug)" chans_dead)
  sum "gpu-apps-d3: $label: ended after $(( $(up) - t0 )) s, vk_comp exit=$rc, channels lost: $(( dead1 - dead0 ))"
  sum "gpu-apps-d3: $label: usb after: $(usb)"
  grep -E '^COMP (FAIL|input|quit|ended)|SNAKE3D [0-9]+ frames|SNAKE3D the compositor' /tmp/comp.out | cut -c1-340 | while read -r l; do sum "gpu-apps-d3: $label: $l"; done
  grep -a -E '\[nvgpu\] channel .* is dead|\[gsp\] event' /proc/dmesg | tail -n 12 | cut -c1-420 | while read -r l; do sum "gpu-apps-d3: $label: kernel: $l"; done
  [ "$rc" = 0 ] || fail=1
  [ "$dead1" = "$dead0" ] || fail=1
}
BASE="COMP_NO_PANEL=1 COMP_SECONDS=20 SNAKE3D_WINDOW=1 SNAKE3D_AUTOPLAY=1 NVK_CONSTANOS_DEBUG=1"
run "1 no input read" $BASE COMP_NO_INPUT=1 /mnt/bin/vk_comp cpumon snake3d
run "2 input read" $BASE /mnt/bin/vk_comp cpumon snake3d

sum "gpu-apps-d3: end: $(grep '^gpu_uapi:' /proc/kdebug | cut -c1-330)"
sum "gpu-apps-d3: $(grep '^gpu_share:' /proc/kdebug)"
echo "---- summary (the log wraps) ----"
cat $sumfile
sum "gpu-apps-d3: verdict exit=$fail at uptime $(up) s"
exit $fail
