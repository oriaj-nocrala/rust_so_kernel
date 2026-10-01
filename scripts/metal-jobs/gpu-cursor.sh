# Phase 2 of docs/gpu/hw-cursor-plan.md: the display engine's hardware cursor, driven from /dev/dispctl (`gpu/cursor.rs`, `nvgpu::cursor`).
#   touch build.rs; echo 3 > target/metal/budget
#   scripts/metal-run.sh --kconf 'gpu=uapi' scripts/metal-jobs/gpu-cursor.sh        (any level >= super: dispctl must open)
# Steps, each followed by the state the kernel reads back (`cursor:` line of /dev/dispctl: the channel's control/status/exception slot/FREE and the core's
# ARMED cursor registers) so a step that did not latch is named:
#   1 probe   allocate head 0's cursor channel (no core push): control must become 1, status idle (bits 18:16 = 4)
#   2 on 32   a test arrow in VRAM at 80 MiB, the core push (usage bounds 0x1114, control 0x800000cf, composition 0x72ff, ctxdma, offset 0x50000)
#   3 moves   a shell loop of `cursor move`: sweeps of the screen, the cost of one move (`gpu_cursor:` move_us in /proc/kdebug)
#   4 off     the core clear, the channel released
# What the machine can check: the ARMED registers and the exception slots. What only the person can: that a white arrow with a black outline moved across
# the screen (step 3 sweeps for ~8 s and then sits at the middle for 6 s). Passes unless a step fails or an exception shows.
sumfile=/tmp/gpu-cursor.sum
fail=0
sum() { echo "$*"; echo "$*" >> $sumfile; }
: > $sumfile
up() { cut -d' ' -f1 /proc/uptime; }
sum "gpu-cursor: start at uptime $(up) s"
st() { grep -a '^cursor:' /dev/dispctl 2>/dev/null | head -n 1 | cut -c1-400; }
st2() { grep -a '^cursor:' /dev/dispctl 2>/dev/null | sed -n 2p | cut -c1-300; }
sts() { sum "gpu-cursor: $1: $(st)"; sum "gpu-cursor: $1: $(st2)"; }
kd() { grep -a '^gpu_cursor:' /proc/kdebug; }
logs() { grep -a 'cursor:' /proc/dmesg | tail -n 4 | cut -c1-300 | while read -r l; do sum "gpu-cursor: kernel: $l"; done; }
if [ -e /dev/dispctl ] && grep -q '^dispctl' /dev/dispctl 2>/dev/null; then
  sum "gpu-cursor: $(grep '^dispctl' /dev/dispctl | cut -c1-200)"
else
  sum "gpu-cursor: /dev/dispctl does not open (needs gpu=super or higher): nothing to do"
  echo "---- summary (the log wraps) ----"; cat $sumfile; exit 0
fi
sum "gpu-cursor: before: $(st)"

step() { # step <label> <command>
  if echo "$2" > /dev/dispctl; then sum "gpu-cursor: $1: ok"; else sum "gpu-cursor: $1: REFUSED"; fail=1; fi
}
step "1 probe" "cursor probe"
sleep 1
sum "gpu-cursor: after probe: $(st)"
# 2a: without touching the usage bounds (the GOP's 0x1110 says "no cursor": the display may refuse it, and that exception is worth reading); 2b with them
# (nouveau's 0x1114). `on` waits 1.5 s for the core to latch the push and, if it did not, logs where the core stands; the second try only happens if the
# core is idle again (a core that stays busy is not poked further).
if echo "cursor on 32 nb" > /dev/dispctl; then sum "gpu-cursor: 2a on 32 nb: latched"; mode=nb
else
  sum "gpu-cursor: 2a on 32 nb: did not latch"; sts "after 2a"
  if st | grep -q 'idle 1$'; then
    if echo "cursor on 32" > /dev/dispctl; then sum "gpu-cursor: 2b on 32 (usage bounds pushed): latched"; mode=bounds
    else sum "gpu-cursor: 2b on 32 (usage bounds pushed): did not latch"; sts "after 2b"; fail=1; mode=none; fi
  else
    sum "gpu-cursor: the core is not idle after 2a: no further push"; fail=1; mode=none
  fi
fi
sts "after on"
armed=$(st | sed 's/.*core ARMED control //; s/ .*//')
[ "$armed" = "0x800000cf" ] || { sum "gpu-cursor: ARMED control is $armed, expected 0x800000cf"; fail=1; }
step "3a move to 100,100" "cursor move 100 100"
sleep 2
sum "gpu-cursor: after first move: $(st)"
# sweeps: the compositor's path is one write per input event; this is the same from a shell
t0=$(up)
x=0
while [ $x -lt 1800 ]; do echo "cursor move $x 200" > /dev/dispctl || { fail=1; break; }; x=$((x + 6)); done
y=0
while [ $y -lt 1000 ]; do echo "cursor move 900 $y" > /dev/dispctl || { fail=1; break; }; y=$((y + 4)); done
x=0
while [ $x -lt 1800 ]; do echo "cursor move $x $((x / 2))" > /dev/dispctl || { fail=1; break; }; x=$((x + 6)); done
sum "gpu-cursor: sweeps took $(( $(cut -d. -f1 /proc/uptime) - ${t0%.*} )) s: $(kd)"
echo "cursor move 940 520" > /dev/dispctl
sleep 6
sum "gpu-cursor: at the middle: $(st)"
step "4 off" "cursor off"
sleep 2
sts "after off"
exc=$(st | sed 's/.* exc //; s/ free.*//')
[ "$exc" = "0x0,0x0,0x0" ] || sum "gpu-cursor: exception slot of the cursor channel after off: $exc (read it: type bits 14:12)"
sum "gpu-cursor: $(kd)"
logs
echo "---- summary (the log wraps) ----"
cat $sumfile
sum "gpu-cursor: verdict exit=$fail at uptime $(up) s"
exit $fail
