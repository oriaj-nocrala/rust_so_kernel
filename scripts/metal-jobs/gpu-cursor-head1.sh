# Is the cursor's INVALID_STATE (code 0x43, Ryzen #204-#211, with or without the GSP, interlock, modeset-style push) about HEAD 0's inherited state (the GOP's, never
# programmed by this driver as a whole), or about the cursor method set itself? Head 1 (HDMI) is lit by `hdmi on` with the full nouveau-equivalent head and window
# state this driver programs (usage bounds included): the same cursor sequence on it.
#   touch build.rs; echo 3 > target/metal/budget; scripts/metal-run.sh --kconf 'gpu=hdmi' scripts/metal-jobs/gpu-cursor-head1.sh
sumfile=/tmp/gpu-cursor-head1.sum
fail=0
sum() { echo "$*"; echo "$*" >> $sumfile; }
: > $sumfile
up() { cut -d' ' -f1 /proc/uptime; }
st() { grep -a '^cursor:' /dev/dispctl 2>/dev/null | head -n 1 | cut -c1-400; }
st2() { grep -a '^cursor:' /dev/dispctl 2>/dev/null | sed -n 2p | cut -c1-300; }
rawlog() { grep -a 'cursor: raw' /proc/gpu | tail -n 1 | cut -c1-400; }
sum "gpu-cursor-head1: start at uptime $(up) s"
[ -e /dev/dispctl ] || { sum "gpu-cursor-head1: no /dev/dispctl"; grep -a 'STOP' /proc/gpu | head -n 3; echo "---- summary (the log wraps) ----"; cat $sumfile; exit 1; }
if echo "hdmi on" > /dev/dispctl; then sum "gpu-cursor-head1: hdmi on: ok"; else sum "gpu-cursor-head1: hdmi on FAILED (is a monitor on HDMI?)"; grep -a 'hdmi:' /proc/gpu | tail -n 4 | cut -c1-300; echo "---- summary (the log wraps) ----"; cat $sumfile; exit 1; fi
sleep 2
echo "cursor head 1" > /dev/dispctl || { sum "gpu-cursor-head1: head select FAILED"; fail=1; }
echo "cursor image 32" > /dev/dispctl && sum "gpu-cursor-head1: image written"
stopped=0
step() { # step <n> <label> <method> <value>
  [ $stopped = 1 ] && return
  if echo "cursor raw $3 $4" > /dev/dispctl; then sum "gpu-cursor-head1: $1 $2: idle again"
  else sum "gpu-cursor-head1: $1 $2: CORE NOT IDLE (or refused)"; stopped=1; fail=1; fi
  sum "gpu-cursor-head1: $1 $2: $(rawlog)"; sum "gpu-cursor-head1: $1 $2: $(st2)"
}
# head 1's methods are head 0's + 0x400. Its usage bounds are already nouveau's 0x1114 (hdmi on pushed them).
step 1 composition 0x24a0 0x72ff
step 2 offset 0x2490 0x50000
step 3 control-disabled 0x249c 0xcf
step 4 context-dma 0x2488 0xf0000001
step 5 context-dma-slot1 0x248c 0xf0000001
step 6 offset-slot1 0x2494 0x50000
step 7 present-control 0x2498 0
echo 'cursor probe' > /dev/dispctl && sum "gpu-cursor-head1: channel allocated: $(st)" || { sum "gpu-cursor-head1: probe FAILED"; fail=1; stopped=1; }
echo "cursor move 100 100" > /dev/dispctl
if [ $stopped = 0 ]; then
  if echo "cursor raw 0x249c 0x800000cf" > /dev/dispctl; then sum "gpu-cursor-head1: 8 enable: ENABLED (so head 0's state is what differs)"; enabled=1
  else
    sum "gpu-cursor-head1: 8 enable: failed"; sum "gpu-cursor-head1: 8: $(rawlog)"; sum "gpu-cursor-head1: 8: $(st2)"
    echo "cursor recover" > /dev/dispctl; fail=1; enabled=0
  fi
  sum "gpu-cursor-head1: after: $(st)"
  if [ "$enabled" = 1 ]; then
    t0=$(cut -d. -f1 /proc/uptime)
    x=0; while [ $x -lt 1800 ]; do echo "cursor move $x 200" > /dev/dispctl || { fail=1; break; }; x=$((x + 6)); done
    sum "gpu-cursor-head1: sweeps took $(( $(cut -d. -f1 /proc/uptime) - t0 )) s: $(grep -a '^gpu_cursor:' /proc/kdebug)"
    sleep 6
    echo "cursor raw 0x249c 0xcf" > /dev/dispctl && sum "gpu-cursor-head1: cursor disabled again"
  fi
fi
grep -a "cursor:" /proc/gpu | tail -n 8 | cut -c1-400 | while read -r l; do sum "gpu-cursor-head1: gpu log: $l"; done
echo "---- summary (the log wraps) ----"
cat $sumfile
sum "gpu-cursor-head1: verdict exit=$fail at uptime $(up) s"
exit $fail
