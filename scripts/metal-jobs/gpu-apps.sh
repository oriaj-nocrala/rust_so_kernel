# G5 layer 4 on the Ryzen with real apps: the GPU compositor (vk_comp) owning the screen with cpumon (a CPU-drawn Rust client, a pool buffer
# uploaded every change) and snake3d (a Vulkan client in a window, GPU buffers imported where they are) side by side.
#   probes/nvk/build.py; strip vk-comp -> disk-image-root/bin/vk_comp, vk-snake -> snake3d, vk-window -> vk_window  (15-19 MB each: dumpe2fs -h disk.img | grep Free)
#   touch build.rs; echo 5 > target/metal/budget
#   scripts/metal-run.sh --kconf 'gpu=uapi' scripts/metal-jobs/gpu-apps.sh
# The watchdog stays armed for the whole job (300 s from the start of the boot, never pinged), so the job keeps its own budget (left()) and
# skips or shortens a phase that would not fit. Phases, the ones a person looks at first:
#   A  vk_comp + cpumon + snake3d (autoplay, no input devices): what the screen should show is cpumon's graphs and a window with the 3D snake playing
#      itself, and halfway through F11 takes the focused window to fullscreen and, later, back (COMP_F11_AT: the compositor presses the key).
#      Measured: the snake's own frame rate (it is paced by the compositor), spawn times, clean exits, buffers imported = dropped, the compositor
#      ending cpumon with itself.
#   B  the regression of Ryzen #180: vk_comp with two vk_window at COMP_DELAY_MS 3, 5, 7 (they ended after one frame, the cold block cache and the
#      exec under SCHEDULER) and 2; the best must reach 50 fps.
#   C  snake3d alone in a window (baseline for A).
#   D  interactive, with the keyboard and mouse: cpumon + snake3d, no autoplay, for what is left of the budget (up to 90 s). Play with the arrows or
#      WASD (the snake window must have the focus: click it), drag and resize the windows, F11 for fullscreen and back; Q quits the snake,
#      Ctrl+Alt+Backspace the compositor (it ends cpumon too).
#   E  afterwards: nothing held (gpu_share 0/0/0), the GPU alive, no address space freed under SCHEDULER (sched: space_frees_under_lock=0),
#      the ext2 cache and memory before and after.
# Passes if A, B, C are clean (exit codes, DONE lines, no FAIL), the snake in A reaches 50 fps, the best two-client delay in B reaches 50 fps,
# D's compositor ends cleanly, and E is clean. What no script can say is whether it looks right: that is for a person.
sumfile=/tmp/gpu-apps.sum
fail=0
sum() { echo "$*"; echo "$*" >> $sumfile; }
: > $sumfile
up() { cut -d. -f1 /proc/uptime; }
TOTAL=${GPU_APPS_TOTAL:-255}   # seconds from the start of the boot; QEMU runs set a larger one
left() { echo $((TOTAL - $(up))); }
field() { echo "$1" | tr ' ' '\n' | grep "^$2=" | cut -d= -f2; }
cat /proc/gpu
grep '^fwsec:\|^gsp:\|^vaspace:\|^copy:\|^compute:\|^uapi:' /proc/gpu >> $sumfile
grep -q '^gsp: OK' /proc/gpu || { sum "gpu-apps: gsp did not boot"; fail=1; }
grep -q '^uapi: installed' /proc/gpu || { sum "gpu-apps: the GPU state was not kept for /dev/nvgpu"; fail=1; }
sum "gpu-apps: start at uptime $(up) s, budget $(left) s"
sum "gpu-apps: before: $(grep '^gpu_uapi:' /proc/kdebug)"
sum "gpu-apps: before: $(grep '^sched:' /proc/kdebug)"
sum "gpu-apps: before: $(grep '^ext2_cache:' /proc/kdebug)"
sum "gpu-apps: before: $(grep -E '^(MemTotal|MemFree)' /proc/meminfo | tr -s ' ' | tr '\n' ' ')"
sum "gpu-apps: $(grep '^gpu_vblank:' /proc/kdebug | cut -c1-120)"
for f in vk_comp snake3d vk_window cpumon; do sum "gpu-apps: /mnt/bin/$f: $(ls -l /mnt/bin/$f 2>&1 | tr -s ' ' | cut -d' ' -f5-)"; done

# comp_lines <label>: what vk_comp and the clients said, kept in the summary (the log wraps)
comp_lines() {
  grep -E '^COMP (started|cannot|program|ended|F11|client [0-9]+ (is not|disconnected)|pace \(all\)|quit|FAIL|ASSERT)|VK WINDOW|VK FAIL|VK ASSERT|VK window: [0-9]+ frames|SNAKE3D (window|resized|[0-9]+ frames|FAIL|DONE|using|swapchain|the compositor)' /tmp/comp.out | head -n 40 | while read -r l; do sum "gpu-apps: $1: $l"; done
}
# quit_stats <label> <want buffers>: the frame rate and counters in vk_comp's quit line
quit_stats() {
  q=$(grep 'COMP quit after' /tmp/comp.out)
  frames=$(echo "$q" | sed -n 's/.*with clients: \([0-9]*\) frames.*/\1/p')
  ms=$(echo "$q" | sed -n 's/.*with clients: [0-9]* frames in \([0-9]*\) ms.*/\1/p')
  imports=$(echo "$q" | sed -n 's/.*, \([0-9]*\) imports.*/\1/p')
  drops=$(echo "$q" | sed -n 's/.*, \([0-9]*\) drops.*/\1/p')
  fps10=0
  if [ "${ms:-0}" -gt 0 ] 2>/dev/null; then
    fps10=$(( ${frames:-0} * 10000 / ms ))
    sum "gpu-apps: $1: $frames compositions in ${ms} ms with clients = $((fps10 / 10)).$((fps10 % 10))/s, imports=$imports drops=$drops"
  else
    sum "gpu-apps: $1: no timing in the quit line"; fail=1
  fi
  if [ "$2" = any ]; then
    :
  elif [ -n "$2" ]; then
    # the quit line is printed before the last client's buffers are dropped (they go at the next composed frame, and the idle exit can win that
    # race: Ryzen #184 had 12/9 and 3/2): so the drops may lag; a leak shows in gpu_share at the end, which must be 0/0/0
    [ "${imports:-0}" = "$2" ] && [ "${drops:-0}" -le "$2" ] || { sum "gpu-apps: $1: $imports imports and $drops drops, expected $2 and at most $2"; fail=1; }
  else
    [ "${drops:-0}" -le "${imports:-0}" ] || { sum "gpu-apps: $1: $imports imports but $drops drops"; fail=1; }
  fi
}
# clean <label> <rc>: the checks every run must pass
clean() {
  [ "$2" = 0 ] || { sum "gpu-apps: $1: vk_comp exit=$2"; fail=1; tail -n 20 /tmp/comp.out >> $sumfile; }
  grep -q '^COMP DONE' /tmp/comp.out || { sum "gpu-apps: $1: vk_comp did not reach COMP DONE"; fail=1; }
  grep -q 'COMP FAIL\|VK FAIL\|COMP ASSERT\|VK ASSERT\|SNAKE3D FAIL' /tmp/comp.out && { sum "gpu-apps: $1: a FAIL line"; fail=1; }
  sum "gpu-apps: $1: after: $(grep '^gpu_flip:' /proc/kdebug | cut -d' ' -f2-)"
}

# ---- A: the two real apps, measured, F11 pressed by the compositor
if [ "$(left)" -gt 90 ]; then
  sum "gpu-apps: A: cpumon + snake3d, budget $(left) s"
  t0=$(up)
  COMP_NO_INPUT=1 COMP_SECONDS=34 COMP_F11_AT=600,900 SNAKE3D_WINDOW=1 SNAKE3D_AUTOPLAY=1 SNAKE3D_SECONDS=26 NVK_CONSTANOS_DEBUG=1 /mnt/bin/vk_comp cpumon snake3d > /tmp/comp.out 2>&1
  rc=$?
  sum "gpu-apps: A: ran uptime $t0 .. $(up) s"
  comp_lines A
  clean A $rc
  quit_stats A ""   # 9 buffers when F11 went to the snake (3 + 3 + 3), 3 when it went to cpumon: only imports = drops is required
  grep -q 'SNAKE3D DONE' /tmp/comp.out || { sum "gpu-apps: A: snake3d did not reach SNAKE3D DONE"; fail=1; }
  fps=$(grep 'SNAKE3D [0-9]* frames in' /tmp/comp.out | sed -n 's/.*(\([0-9.]*\) per second.*/\1/p')
  sum "gpu-apps: A: snake3d frame rate with cpumon beside it: '$fps' per second"
  awk -v a="${fps:-0}" 'BEGIN { exit !(a >= 50) }' || { sum "gpu-apps: A: snake3d below 50 frames per second"; fail=1; }
  [ "$(grep -c '^COMP F11 at frame' /tmp/comp.out)" = 2 ] || { sum "gpu-apps: A: the compositor did not press F11 twice"; fail=1; }
  sum "gpu-apps: A: snake3d resized $(grep -c 'SNAKE3D resized to' /tmp/comp.out) times (2 when F11 went to it; cpumon takes the focus if it mapped last)"
  grep -q '^COMP ended 1 program' /tmp/comp.out || { sum "gpu-apps: A: the compositor did not end cpumon"; fail=1; }
  [ "$(grep -c '^COMP client [0-9]* connected' /tmp/comp.out)" = 2 ] || { sum "gpu-apps: A: not both apps connected"; fail=1; }
else
  sum "gpu-apps: A: skipped, $(left) s left"; fail=1
fi

# ---- B: the regression of #180 (two vk_window, the delays that ended after one frame) and the default
best=0; best_d=0
run_two() {
  d=$1
  sum "gpu-apps: B delay=$d: flips before: $(grep '^gpu_flip:' /proc/kdebug | cut -d' ' -f2-)"
  t0=$(up)
  VK_WINDOW_FRAMES=300 COMP_NO_INPUT=1 COMP_EXIT_WHEN_IDLE=1 COMP_SECONDS=60 COMP_DELAY_MS=$d NVK_CONSTANOS_DEBUG=1 /mnt/bin/vk_comp /mnt/bin/vk_window /mnt/bin/vk_window > /tmp/comp.out 2>&1
  rc=$?
  sum "gpu-apps: B delay=$d: ran uptime $t0 .. $(up) s"
  comp_lines "B delay=$d"
  clean "B delay=$d" $rc
  [ "$(grep -c 'VK WINDOW DONE' /tmp/comp.out)" = 2 ] || { sum "gpu-apps: B delay=$d: not every client finished"; fail=1; }
  quit_stats "B delay=$d" 12
  if [ "$fps10" -gt "$best" ]; then best=$fps10; best_d=$d; fi
}
for d in 3 5 7 2; do
  if [ "$(left)" -gt 100 ]; then run_two $d; else sum "gpu-apps: B delay=$d: skipped, $(left) s left"; fail=1; fi
done
sum "gpu-apps: B: best two-client delay: $best_d ms at $((best / 10)).$((best % 10))/s"
[ "$best" -ge 500 ] || { sum "gpu-apps: B: no delay reached 50 compositions per second"; fail=1; }

# ---- C: snake3d alone in a window
if [ "$(left)" -gt 70 ]; then
  t0=$(up)
  COMP_NO_INPUT=1 COMP_EXIT_WHEN_IDLE=1 COMP_SECONDS=40 SNAKE3D_WINDOW=1 SNAKE3D_AUTOPLAY=1 SNAKE3D_SECONDS=12 NVK_CONSTANOS_DEBUG=1 /mnt/bin/vk_comp snake3d > /tmp/comp.out 2>&1
  rc=$?
  sum "gpu-apps: C: ran uptime $t0 .. $(up) s"
  comp_lines C
  clean C $rc
  quit_stats C 3
  grep -q 'SNAKE3D DONE' /tmp/comp.out || { sum "gpu-apps: C: snake3d did not reach SNAKE3D DONE"; fail=1; }
  sum "gpu-apps: C: snake3d alone: '$(grep 'SNAKE3D [0-9]* frames in' /tmp/comp.out | sed -n 's/.*(\([0-9.]*\) per second.*/\1/p')' per second"
else
  sum "gpu-apps: C: skipped, $(left) s left"
fi

# ---- D: interactive, for whoever is at the machine
d=$(( $(left) - 35 )); [ "$d" -gt 90 ] && d=90
if [ "$d" -ge 20 ]; then
  sum "gpu-apps: D: interactive for $d s (play, drag, resize, F11; Q quits the snake, Ctrl+Alt+Backspace the compositor)"
  t0=$(up)
  COMP_SECONDS=$d SNAKE3D_WINDOW=1 NVK_CONSTANOS_DEBUG=1 /mnt/bin/vk_comp cpumon snake3d > /tmp/comp.out 2>&1
  rc=$?
  sum "gpu-apps: D: ran uptime $t0 .. $(up) s"
  comp_lines D
  # the compositor ends by the clock (or Ctrl+Alt+Backspace) with the snake still playing: it must leave quietly (no FAIL), and the compositor
  # prints its summary before it drops the clients, so imports = drops does not apply here
  clean D $rc
  quit_stats D "any"
  grep -q 'SNAKE3D the compositor went away\|SNAKE3D DONE' /tmp/comp.out || { sum "gpu-apps: D: snake3d neither finished nor saw the compositor go"; fail=1; }
  sum "gpu-apps: D: input devices: $(grep -c 'cannot open the input devices' /tmp/comp.out) failures to open"
else
  sum "gpu-apps: D: skipped, $(left) s left"
fi

# ---- E: what must be true afterwards
u=$(grep '^gpu_uapi:' /proc/kdebug)
sum "gpu-apps: end: $u"
sum "gpu-apps: slow holds of the GPU lock: $(grep '^gpu_uapi_slow:' /proc/kdebug)"
[ "$(field "$u" dead)" = 0 ] || { sum "gpu-apps: the GPU was declared dead"; fail=1; }
[ "$(field "$u" chans_dead)" = 0 ] || { sum "gpu-apps: a channel was lost"; fail=1; }
s=$(grep '^gpu_share:' /proc/kdebug)
sum "gpu-apps: $s"
echo "$s" | grep -q 'sessions=0 storage_allocs=0 syncs=0' || { sum "gpu-apps: something is still held after every program ended"; fail=1; }
sc=$(grep '^sched:' /proc/kdebug)
sum "gpu-apps: end: $sc"
[ "$(field "$sc" space_frees_under_lock)" = 0 ] || { sum "gpu-apps: an address space was freed under SCHEDULER"; fail=1; }
sum "gpu-apps: end: $(grep '^ext2_cache:' /proc/kdebug)"
sum "gpu-apps: end: $(grep -E '^(MemTotal|MemFree)' /proc/meminfo | tr -s ' ' | tr '\n' ' ')"
sum "gpu-apps: processes left: $(ps 2>/dev/null | grep -c -E 'c[p]umon|s[n]ake3d|v[k]_comp|v[k]_window')"
echo 'gsp name' > /dev/dispctl && sum "gpu-apps: RM still answers" || { sum "gpu-apps: RM does not answer"; fail=1; }

echo "---- summary (the log wraps) ----"
cat $sumfile
sum "gpu-apps: verdict exit=$fail at uptime $(up) s"
exit $fail
