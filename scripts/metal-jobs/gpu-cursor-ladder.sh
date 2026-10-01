# The hardware cursor's core methods one at a time (Ryzen #196/#197: pushing all five left the core not idle, its exception slot naming method 0x2088 =
# HEAD_SET_CONTEXT_DMA_CURSOR with type 0, put == get). Each step is ONE method + UPDATE through `cursor raw`, waits for the core to go idle and logs the
# core's status, put/get and exception slot; the ladder stops at the first step that leaves the core busy. Least suspect first:
#   1 composition 0x72ff   2 offset 0x50000 (80 MiB >> 8)   3 control 0xcf (format only, disabled)   4 usage bounds 0x1114
#   5 context DMA 0xf0000001 (the core's LUT context DMA, flags 0x45; Ryzen #198: our own 0xfb000100, flags 0x05, left the core in CTX_DMA_LOOKUP)
#   6 control 0x800000cf (enable, 32x32)
# then, if the cursor is on: the arrow sweeps the screen and sits at the middle for 6 s (the person's eyes), and the cursor is turned off again
# (control 0xcf, context DMA 0, usage bounds 0x1110: the GOP's value).
#   touch build.rs; echo 3 > target/metal/budget; scripts/metal-run.sh --kconf 'gpu=uapi' scripts/metal-jobs/gpu-cursor-ladder.sh
sumfile=/tmp/gpu-cursor-ladder.sum
fail=0
sum() { echo "$*"; echo "$*" >> $sumfile; }
: > $sumfile
up() { cut -d' ' -f1 /proc/uptime; }
sum "gpu-cursor-ladder: start at uptime $(up) s"
st() { grep -a '^cursor:' /dev/dispctl 2>/dev/null | head -n 1 | cut -c1-400; }
st2() { grep -a '^cursor:' /dev/dispctl 2>/dev/null | sed -n 2p | cut -c1-300; }
rawlog() { grep -a 'cursor: raw' /proc/gpu | tail -n 1 | cut -c1-400; }
if [ -e /dev/dispctl ] && grep -q '^dispctl' /dev/dispctl 2>/dev/null; then :; else
  sum "gpu-cursor-ladder: /dev/dispctl does not open (needs gpu=super or higher)"
  # on a machine with the GPU a missing dispctl is a failure to bring up (chan: STOP ...), not a pass
  grep -a '^chan:\|^super:\|STOP' /proc/gpu 2>/dev/null | head -n 6 | cut -c1-300 | while read -r l; do sum "gpu-cursor-ladder: /proc/gpu: $l"; done
  if grep -aq 'STOP' /proc/gpu 2>/dev/null; then fail=1; fi
  echo "---- summary (the log wraps) ----"; cat $sumfile; exit $fail
fi
echo 'cursor image 32' > /dev/dispctl && sum "gpu-cursor-ladder: image written" || { sum "gpu-cursor-ladder: image FAILED"; fail=1; }
stopped=0
step() { # step <n> <label> <method> <value>
  [ $stopped = 1 ] && return
  if echo "cursor raw $3 $4" > /dev/dispctl; then sum "gpu-cursor-ladder: $1 $2: idle again"
  else sum "gpu-cursor-ladder: $1 $2: CORE NOT IDLE (or refused): the ladder stops here"; stopped=1; fail=1; fi
  sum "gpu-cursor-ladder: $1 $2: $(rawlog)"
  sum "gpu-cursor-ladder: $1 $2: $(st)"
  sum "gpu-cursor-ladder: $1 $2: $(st2)"
}
step 1 composition 0x20a0 0x72ff
step 2 offset 0x2090 0x50000
step 3 control-disabled 0x209c 0xcf
step 4 usage-bounds 0x2030 0x1114
# 5: the cursor's context DMA = the core's LUT one (nouveau's NV50_DISP_HANDLE_VRAM, 0xf0000001, serves both). Ryzen #198-#202: after the GSP boot every core context
# DMA lookup hung because the instance memory was in GSP's reserved top of VRAM; it now lives at 256 MiB (`evo::INST_VRAM`).
step 5 cursor-context-dma 0x2088 0xf0000001
echo 'cursor probe' > /dev/dispctl && sum "gpu-cursor-ladder: channel allocated: $(st)" || { sum "gpu-cursor-ladder: probe FAILED"; fail=1; }
# Ryzen #204/#205: with the context DMA resolved (step 5), enabling the cursor raised INVALID_STATE (slot 0x5080 data 1 code 0x43; UPDATE), also with the
# cursor channel positioned first and with nouveau's whole curs_set in one push. NVIDIA's own driver (nvkms-evo3.c EvoSetCursorImageC3) also pushes
# PRESENT_CONTROL_CURSOR (0x2098 = MONO) and the context DMA and offset of the second slot (0x208c, 0x2094) ("HW ignores it unless stereo", but validation may not).
# Variants, each recovered from (`cursor recover` clears the exception slot as nouveau does) so the next one runs in the same boot:
#   6a  the second slot + present control one method at a time, the cursor channel's UPDATE, then the enable alone
#   6b  NVIDIA's whole set in one push (`cursor on 32 nb`: present control, both slots, control, composition) + UPDATE
variants() {
step 53 context-dma-slot1 0x208c 0xf0000001
step 54 offset-slot1 0x2094 0x50000
step 55 present-control-mono 0x2098 0
[ $stopped = 1 ] && return
echo "cursor move 100 100" > /dev/dispctl && sum "gpu-cursor-ladder: channel positioned (its own UPDATE)" || { sum "gpu-cursor-ladder: move FAILED"; fail=1; }
enabled=0
if echo "cursor raw 0x209c 0x800000cf" > /dev/dispctl; then enabled=1; sum "gpu-cursor-ladder: 6a enable after the channel's UPDATE: idle again, ENABLED"
else
  sum "gpu-cursor-ladder: 6a enable after the channel's UPDATE: failed"; sum "gpu-cursor-ladder: 6a: $(rawlog)"; sum "gpu-cursor-ladder: 6a: $(st2)"
  if echo "cursor recover" > /dev/dispctl; then sum "gpu-cursor-ladder: recovered: $(grep -a 'cursor: recover' /proc/gpu | tail -n 1 | cut -c1-300)"
  else sum "gpu-cursor-ladder: recover FAILED: $(grep -a 'cursor: recover' /proc/gpu | tail -n 1 | cut -c1-300)"; stopped=1; fi
  if [ $stopped = 0 ]; then
    if echo "cursor on 32 nb" > /dev/dispctl; then enabled=1; sum "gpu-cursor-ladder: 6b whole curs_set in one push: ENABLED"
    else
      sum "gpu-cursor-ladder: 6b whole curs_set in one push: failed"; sum "gpu-cursor-ladder: 6b: $(grep -a 'cursor: the core did not latch' /proc/gpu | tail -n 1 | cut -c1-400)"; sum "gpu-cursor-ladder: 6b: $(st2)"
      echo "cursor recover" > /dev/dispctl; sum "gpu-cursor-ladder: recovered: $(grep -a 'cursor: recover' /proc/gpu | tail -n 1 | cut -c1-300)"
      stopped=1; fail=1
    fi
  else fail=1; fi
fi
[ $enabled = 1 ] || stopped=1
sum "gpu-cursor-ladder: after the enable attempts: $(st)"
}
[ $stopped = 0 ] && variants
if [ $stopped = 0 ]; then
  echo "cursor move 100 100" > /dev/dispctl
  sleep 2
  t0=$(cut -d. -f1 /proc/uptime)
  x=0; while [ $x -lt 1800 ]; do echo "cursor move $x 200" > /dev/dispctl || { fail=1; break; }; x=$((x + 6)); done
  y=0; while [ $y -lt 1000 ]; do echo "cursor move 900 $y" > /dev/dispctl || { fail=1; break; }; y=$((y + 4)); done
  sum "gpu-cursor-ladder: sweeps took $(( $(cut -d. -f1 /proc/uptime) - t0 )) s: $(grep -a '^gpu_cursor:' /proc/kdebug)"
  echo "cursor move 940 520" > /dev/dispctl
  sleep 6
  sum "gpu-cursor-ladder: at the middle: $(st)"
  step 7 control-off 0x209c 0xcf
  step 8 context-dma-off 0x2088 0
  step 9 usage-bounds-back 0x2030 0x1110
fi
sum "gpu-cursor-ladder: end: $(st)"
sum "gpu-cursor-ladder: end: $(st2)"
grep -a "cursor:" /proc/gpu | tail -n 12 | cut -c1-400 | while read -r l; do sum "gpu-cursor-ladder: gpu log: $l"; done
echo "---- summary (the log wraps) ----"
cat $sumfile
sum "gpu-cursor-ladder: verdict exit=$fail at uptime $(up) s"
exit $fail
