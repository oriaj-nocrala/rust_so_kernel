# G5 layer 4 — real apps, F11, and why the compositor's GPU channel died (session report, 2026-10-01)

Written for the next person or agent that touches `vk-comp/`, `probes/nvk/comp*`, `gui/`, `nvgpu/` fault handling, the metal jobs, or the QEMU
test harness. It records **what was built, every bug found, everything tried (including what failed and what I got wrong), the problems that
sit deeper in the architecture, and the recipes that worked**. Current-state facts live in `docs/reference/` and the skills; this is the *why*
and the *how we got there*. Status of the layer: `g5-layer4-handoff.md`. Plan: `g5-graphics-stack-plan.md`.

Contents: 1 summary · 2 what was built · 3 the Ryzen runs · 4 bug: `comp.frag` helper lanes · 5 bug: block-cache lock · 6 QEMU TLB flake ·
7 smaller defects · 8 open: cursor pacing · 9 tried and ruled out · 10 my mistakes · 11 deeper architecture problems · 12 harness and recipes ·
13 test and sabotage ledger · 14 next steps.

## 1. Summary

- Asked for: run `cpumon` and `snake3d` under the GPU compositor (`vk_comp`) on the Ryzen and review four things (the user's eyes, the zombie /
  ext2-cache fixes under real load, `snake3d` in a window with the keyboard, two real apps at once), plus F11 to go from window to full screen.
- Done and verified on the Ryzen (#188): both apps side by side, `snake3d` in a window with keyboard and mouse, F11 fullscreen and back, maximize by
  click, the compositor ending its programs, 90 s of interactive use with no GPU fault. QEMU: host tests, a 3-case integration test, mutation checks.
- **The big find:** the compositor's GPU channel died the first time the mouse moved (Ryzen #184-#187). Cause: `comp.frag` indexed a storage buffer
  from `gl_FragCoord`; helper lanes of 2x2 pixel quads at an **odd** rectangle edge computed index -1 = 16 GiB past the buffer. Fixed by clamping
  (section 4). It took five Ryzen boots because the kernel threw away the one thing that names the cause; the kernel now logs it.
- **Not done:** true direct scanout for F11 (the fullscreen window is still composed), explicit sync, any fix for the 30 fps pacing cliffs, any
  fix for the cursor not moving at 60 fps (section 8: observed by the user, understood only partly).
- Found and **not fixed**: an ext2 block-cache lock hazard (section 5) and a QEMU-only TLB-shootdown panic that already existed (section 6).
- All of it is committed in several commits (see `git log`); `disk-image-root/etc/180hz` and the `mlibc` / `quakegeneric` submodule states are
  deliberately not part of them.

## 2. What was built

| Piece | Where | Notes |
|---|---|---|
| F11 fullscreen | `gui/src/compositor.rs` (`toggle_fullscreen`, `is_fullscreen`, key handling in `key`) | The compositor's key, never reaches a client. Needs `set_resizable`. No title bar, no resize grip; restores placed or maximized geometry. 3 tests + 7 killed mutants (an 8th, `raise`, was redundant and removed). **Composition, not direct scanout.** |
| `set_resizable` on the C wire | `userspace/c/include/constanos_gui_wire.h`, `gui/tests/c_wire*` | Checked against the Rust encoder by `c_wire.rs`; a mutant of the opcode is killed. |
| `GVK_RESIZE`, `gvk_set_resizable` | `userspace/c/include/constanos_gui_vk.h` | The client side of `resize`. |
| `snake3d` in a window | `probes/nvk/vk_snake.c` (`SNAKE3D_WINDOW=1`) | Keys over the wire (`key_apply`, shared with evdev), swapchain + depth + views rebuilt on resize (`goto make_target`, old swapchain as `oldSwapchain`), exits quietly when the compositor goes. Host path (`-DSNAKE_HOST`) renders byte-identical frames (compared against the original source). |
| `vk_comp` ends its programs | `vk-comp/src/lib.rs` (`terminate_children`) | SIGTERM, SIGKILL after 3 s. Needed because `cpumon` never exits by itself. |
| `COMP_F11_AT`, `COMP_NO_PANEL`, input counters | `vk-comp/src/lib.rs` | F11 test hook for QEMU (no keyboard there); no default panel; `COMP input` lines (keys, pointer motions, buttons, poll wakes, `REL` records, pointer position). |
| `gui_comp_test` | `userspace/c/gui_comp_test.c` | Three cases: `vk_window`; `vk_window` + `cpumon` (ended with the compositor); `snake3d` window with F11 there and back (2 resizes, 9 buffers imported and dropped, the client itself destroyed 6). |
| Metal jobs | `scripts/metal-jobs/gpu-apps.sh` (A-E), `gpu-apps-d.sh`, `gpu-apps-d2.sh`, `gpu-apps-d3.sh` | The last three are the investigation of section 4; keep them as templates for "one variable per run". |
| `scripts/tlb-stress.sh` | | Reproduces the QEMU TLB panic of section 6 in minutes. |
| RM error logging | `kernel/src/gpu/gsp.rs`, `nvgpu/src/rpc.rs` (`payload_summary`, `rc_fault`) | First 32 error events printed with their words and text; `RC_TRIGGERED` decoded. |
| Fault explanation | `kernel/src/gpu/uapi.rs` (`Hw::va_log`, `kill_chan`), `nvgpu/src/hwq.rs` (`VaEvent`, `describe_va`) | A ring of the last 256 binds/unbinds; a dead channel's line names the slice, the covering bind or the nearest live ranges. |

## 3. The Ryzen runs

| Boot | Job | Result |
|---|---|---|
| #184 | `gpu-apps.sh` (A-E) | A OK (F11 to 1920x1080 and back; snake 51.3 fps beside `cpumon`). B: 3 ms 50.8/s, 5 and 7 ms 30/s, 2 ms 59.2/s (the old "ends after one frame" of #180 is gone). C 59.9 fps. **D (keyboard + mouse): `COMP FAIL present (-4)` at frame 609, 19 s in, `chans_dead=1`.** |
| #185 | `gpu-apps-d.sh` (D only) | Same death at frame 197 (3.7 s), after 8 key events and 3 pointer motions. The kernel line named the victim: `channel 0 (chid 1)` = the compositor. |
| #186 | `gpu-apps-d2.sh` (4 scenarios, input open) | **All four died within the first frames** (frame 1, 1, 7, 1), the compositor alone included, each after the first mouse motion. The user said: "I think they died because I moved the mouse; the pointer does not move but it brings everything down." |
| #187 | `gpu-apps-d3.sh` (same load, input NOT read vs read) | Not read: survived 21 s with **226 key and 1367 mouse USB reports arriving**, 56.6 fps. Read: died at frame 1. RM's event decoded: Xid 31, address `0x14_792a7000`. |
| #188 | `gpu-apps.sh` with the shader fix | **OK.** D lasted 90 s, `chans_dead=0`, the user played (score 3), F11 twice, maximized by click (1920x1040), `gpu_share` 0/0/0. |

## 4. Bug: `comp.frag` helper lanes (the compositor's channel died on pointer motion)

**Symptom.** Only with input devices open, the compositor's GR channel was reset by RM (`RC_TRIGGERED`), `vkQueuePresentKHR` returned
`VK_ERROR_DEVICE_LOST` (-4), `vk_comp` exited. Never without input (`COMP_NO_INPUT`): A, B and C ran thousands of frames clean.

**The reason, once the kernel printed it** (`rpc_rc_triggered_v17_02`, `open-gpu-kernel-modules/src/nvidia/generated/g_rpc-structures.h`):

| word | field | #187 value |
|---|---|---|
| 0 | `nv2080EngineType` | 1 (GR0) |
| 1 | `chid` | 1 (the compositor) |
| 3 | `exceptLevel` | 2 |
| 4 | `exceptType` | 0x1f = 31 = **Xid 31, GPU MMU fault** |
| 6 | `partitionAttributionId` (u16) | 0xe9 |
| 7, 8 | `mmuFaultAddrLo`, `Hi` | `0x792a7000`, `0x14` -> **`0x14_792a7000`** |
| 9 | `mmuFaultType` | 0 = **no PDE** (nothing mapped at all) |

`0x14_792a7000 - (1<<36)` = slice 1 (+`0x792a7000`): session slices are 16 GiB from `1<<36`. A fault exactly one slice above a buffer of
session 0 is **`4 * 0xFFFFFFFF` bytes past it: an index of `uint(-1)` into a `uint px[]`**.

**The code.** `comp.frag`: `p = ivec2(gl_FragCoord.xy) - pc.dst.xy + pc.src.xy; v = pix.px[uint(p.y*stride + p.x)]`. Every *covered* pixel
gives an index inside its rectangle. But a fragment shader also runs for **helper invocations**: the other lanes of a 2x2 quad that straddles the
primitive's edge, whose `gl_FragCoord` is outside it. A rectangle starting on an **even** pixel is made of whole quads (no helpers); one starting on an
**odd** pixel has helpers one pixel outside, `p.x = -1` (or `p.y = -1`), index negative, load 16 GiB away.
- The cursor starts at the screen centre (960,540): even, fine. One pixel of mouse motion makes it odd: dead channel.
- Windows are placed by the cascade at 40, 72, ... (even), so nothing failed until the pointer moved. Dragging a window to an odd position would have killed it too.
- The host GPU (the RTX 3050 under the proprietary driver) returns zeros for out-of-range loads, so `host-comp.sh` compared pixel for pixel and
  **passed, odd positions included** (the cursor is drawn at (50,45) there). It cannot see this class of bug.

**The fix.** `ivec2 rel = clamp(ivec2(gl_FragCoord.xy) - pc.dst.xy, ivec2(0), max(pc.dst.zw - 1, ivec2(0)));` then `p = rel + pc.src.xy`. Covered pixels
are unchanged (host harness: 0 pixels differ in 8 frames). Helper values are never shown. Verified only on the Ryzen (#188).

**The rule** (also in `gpu-g5` and `docs/reference/gpu.md`): *a fragment shader that indexes a buffer from `gl_FragCoord` must clamp into its
primitive's rectangle.* Defence in depth that was **not** done: create the compositor's device with `robustBufferAccess` (so an out-of-range load
returns 0 instead of faulting the channel). Worth doing: one flag, and an OOB in a compositor must not be a device-lost.

**How it was found, and how it should have been.** Five boots (#184-#188). What actually cracked it, in order of usefulness:
1. The experiment that **separated causes** (#187): same load, input *arriving but not read* vs *read*. It killed the "USB path / DMA / interrupt"
   family of hypotheses in one boot.
2. Making the kernel **print RM's account** (`[gsp] event ...`, then `[nvgpu] ... address A: <where>`). The address, read as "buffer + 16 GiB", named
   the bug at once. This should have been the *first* change after #184, not the fourth.
3. The user's observation that the mouse never moved (section 10).

## 5. Bug (not fixed): the ext2 block-cache lock

`hal::blockcache::CachedDevice` keeps its state in a plain `spin::Mutex` and `fill()` reads the device **while holding it**. `sys_read` reaches it
with IF=0 (it holds the fd-table lock that way). Consequences, both seen:
- **One CPU** (the `qemu-debug.sh` default!): a holder preempted mid-read is never run again; a waiter spins at IF=0. Hang within seconds of the first
  concurrent cold `exec` (gdb: the CPU in `CachedDevice::read_sectors`, IF=0).
- **SMP**: a waiter spinning at IF=0 on a `spin::Mutex` never answers a TLB shootdown, so the CPU invalidating a kernel page panics after 1 s
  (`... cpus 0x.. never acknowledged`) — this violates the CLAUDE.md rule against `spin::Mutex`.
The Ryzen is mostly spared: USB transfers already run with IF=0 and answer shootdowns while they wait (`usb/xhci/msc.rs`), so the hold is brief.

**What was tried.** (a) a spin hook calling `tlb::service_pending`, (b) interrupts off while the lock is held (`set_irq_hooks`), (c)
`service_pending` in the ATA wait loops. Measured on `scripts/tlb-stress.sh`: the unchanged kernel panics 6 of 6; with (a) the panics became hangs
(every CPU spinning for a preempted holder); with (a)+(b) the hangs went but 4 of 6 still panicked; adding (c) gave 4 of 6 (a different, single-CPU
pattern, section 6). The natural load (`gui_comp_test`) was not better and slower. **Reverted**; unit tests for the hooks were mutation-checked
(4 of 4) before the revert, so the idea is sound but its payoff was not shown. The note and the numbers are in `docs/reference/filesystems.md`.
**The proper fix** is not to hold the lock across the device read: an in-flight marker per chunk plus a write generation so a fill racing a write
discards its data. That is a design change in a hot, correctness-critical path: do it with the A/B/C concurrency tests.

## 6. QEMU TLB-shootdown panic (pre-existing; not understood to the root)

`TLB shootdown of 0x... (kernel): cpus 0x.. never acknowledged` (`tlb.rs`, 1 s bound), only in QEMU/TCG with 4 CPUs, under concurrent fork/exec.
- Frequency: `tlb-stress.sh` on the unchanged tree 6/6 (and 3 of 4 + 1 hang for the first, heavier version); `run-abi-suite.sh gui_comp_test`
  roughly one run in three (n was only 4-5 each time: treat as order of magnitude). Not seen on the Ryzen.
- Evidence from live panics (boot **without** autorun so QEMU halts, then `gdbserver` from the monitor): two CPUs in `fork_impl ->
  allocate_kernel_stack -> unmap_kernel_guard_page -> shoot` at once. The CPU holding `SENDER` panics waiting for the other, which is in the
  `SENDER` loop calling `service_pending`. TR (`cpu_id`) was right on every CPU. A second look shows `PENDING` clear: the ack did land, **more than
  1 s of guest time late**. That points at a vCPU not running (TCG scheduling, ATA PIO storms taking QEMU's global lock) rather than a protocol hole,
  but this is **not proven**.
- Consequence for work: a panic of this shape in QEMU is not evidence against the change under test. Run `scripts/tlb-stress.sh` on the unchanged
  tree first; rerun the test. The rule is in the `qemu-debug` and `kernel-testing` skills and `docs/reference/cpu.md`.

## 7. Smaller defects found and fixed

- **Test race** (`gui_comp_test`): `vk_comp` starts its own program (`cpumon`) on a thread; the client could finish and the compositor be told to
  quit before that `exec` returned, so "the program was never started". The test now waits for `COMP started`.
- **Job check too strict** (`gpu-apps.sh`): required `imports == drops` in `vk_comp`'s quit line. That line is printed before the last client's
  buffers are dropped (they go at the next composed frame and the idle exit can win): 12/9 and 3/2 appeared in a healthy run (#184) and turned it
  into FAIL. Now `drops <= imports`; leaks are measured by `gpu_share` 0/0/0 at the end, which is the real check.
- **`snake3d` reported FAIL when the compositor ended** (a present on a closed connection). A compositor going away is an ending, not a failure.
- **Default `panel`**: `vk_comp` with no program started `panel`, so a "compositor alone" experiment was not alone. `COMP_NO_PANEL=1`.
- **Counters in QEMU read zero**: the keys were injected after `vk_comp` had exited (I waited for a line that is only printed at the end).
- **Dead-channel line missing from the #184 log** while `chans_dead=1`: unexplained (the klog ring wraps at 64 KiB; the line should have been the
  newest). Later boots kept it. Treat the klog as lossy (section 11).

## 8. Open: the cursor does not move at 60 fps (user observation, partly explained)

The user's first impression on the Ryzen, from the very beginning: the pointer did not move at 60 fps; sometimes it looked smoother, without any
visible pattern. What the data says, honestly:
- The cursor is **drawn by the compositor** as one more quad, so it moves only when a composition is made, and compositions are tied to flips
  (`PRESENT` at most once per vblank, started `COMP_DELAY_MS` = 2 ms after the previous flip).
- #188 phase D (cpumon + snake3d, interaction): `COMP pace (all)`: **2622 flips < 20 ms (60 fps) and 1379 in 20-37 ms (30 fps)**, 4 slower;
  `present` 7.0 ms on average (phase A without input: 4.1; alone 3.8; #182: 0.3). About a third of the time the compositor runs at 30 fps, and then the
  cursor does too. "Sometimes smoother" fits a mix of 60 and 30 fps stretches.
- The known 30 fps cliff (#179-#182): the `PRESENT` must leave within ~9 ms of the previous flip or it lands a vblank late. With `present` at 7 ms
  and the rest of the composition, a bad stretch crosses it. Why `present` is 7 ms here and 4 ms there is **unknown**. Candidates, none tested:
  (1) the GPU shared with a full-size `snake3d` (the user maximized it to 1920x1040: its frames are slower and the compositor's work queues behind
  them, no priorities); (2) GPU clocks (P-state ramp 0.2-0.6 s; the first frame after idle ~9x slow, `docs/gpu/g5-graphics-stack-plan.md` "relojes");
  (3) more CPU uploads (`cpumon`, titles; 207 uploads in D); (4) the window sizes (the user also noticed snake slows a little after returning from F11).
- **Update, Ryzen #189 (`gpu-cursor-probe.sh`, `hw-cursor-plan.md` section 11): cursor alone ~50 compositions/s; with cpumon + snake3d in a 960x540 window the compositor is locked at 30 fps (95% of flips 20-37 ms, `present` a constant ~9.1 ms); with the snake window at 1880x1000 it runs at 59 fps (`present` 3 ms).** A bigger window is *faster*, which makes candidate (1) below unlikely and candidate (2), GPU clocks, the one to test (sample `gsp perf` during both loads).
- **Update, Ryzen #190 (`gpu-cursor-pstate.sh`, `hw-cursor-plan.md` section 12): it is the GPU's clocks, not the mouse.** In one load the P-state fell P0 -> P5 -> P8 and `present` went 1.8 -> 4.2 -> 7.1 ms with the compositor from 60 to 30 fps; at P8 `present` is a constant ~9.1 ms (past the ~9 ms deadline: the 30 fps lock) and RM does not ramp up on a light paced load (a DVFS trap); a heavier load climbs to P3 and runs at 60. Decision: not to pin the clock; the options are in section 12 of the plan (hardware cursor, pipelined or adaptive repaint, scanning out the swapchain image without the blit).
- USB mouse reports: 1367 in 21 s = ~65/s (85-96/s in #189); the cursor is sampled at the frame rate anyway, with up to a frame of latency.
- **What to measure first** (in this order; one variable per boot): (a) keep the `COMP pace (5 s)` lines in the job summary (the jobs only kept
  `pace (all)`), with the window sizes at the time; (b) per-frame hitch ring in `vk_comp` (intervals over 25 ms with timestamps, as the metal-run
  skill describes); (c) the same load with the snake window small vs maximized; (d) the cursor alone with no clients (is the cursor path itself at
  60 fps?). **Likely cures** if the cursor stays at the compositor's rate: a **hardware cursor plane** (the display engine has a cursor channel;
  removes the cursor from the composition's pacing entirely), or a cursor-only fast path.

## 9. Tried and ruled out (so nobody repeats it)

- *Console echo of typed keys* (`vk_comp` does `EVIOCGRAB` on the keyboard; the kernel then feeds only signals to the tty): ruled out by reading.
- *USB HID traffic or DMA overlapping GPU memory*: ruled out by #187 (events arrived by the thousand and nothing died unless `vk_comp` read them).
- *Renderer logic depending on the cursor position*: I read `comp_render.h` and `comp.frag` and found nothing: **because I reasoned about covered
  pixels only** (section 10).
- *The ext2 block-cache lock as the cause of the QEMU panics*: it is a real hazard (section 5) but fixing it did not remove the panics.
- *Argument from the unexplained*: "the USB poll runs in the timer ISR, so it must be the input": not supported by anything.

## 10. My mistakes (read these before trusting a conclusion of mine)

1. **I judged the shader safe by reasoning about covered pixels** and wrote, in my own notes, that the host harness "already covers the pointer at the
   edges". It does, and it cannot see an out-of-range load. A harness that passes proves only what it can observe; say what it cannot observe.
2. **I iterated experiments before making the failure explain itself.** #185 (D alone) and #186 (four scenarios) added nothing the kernel line did not
   already say; the decisive pair was the discriminating experiment (#187) and the RM decode. *Rule: after the first unexplained failure on metal, the
   next change is a diagnostic that names the reason; only then vary the load.* Each boot costs the user's PC and attention.
3. **I under-weighted the user's observation.** "The mouse never moves in the vk compositor" arrived after #186; it was the most direct clue (it
   points at pointer motion, which is a 1-pixel change in a draw rectangle). I read "3 pointer motions" in the counters as "the mouse works". Take what
   the person sees as data, and ask for it early ("what do you see on screen?").
4. **I started engineering a fix before measuring the baseline.** The block-cache work took a long time, was QEMU-only, unrelated to the goal, and its
   benefit was never shown. The first thing I should have run was the stress script on the unchanged tree (6/6 panics: the problem was not mine and
   not the cache's). I reverted it, which was right, but late.
5. **My checks were stricter than reality** (`imports == drops` at quit; "F11 goes to snake" assumed focus order; a 90 s per-test cap): a green run
   was reported FAIL by my own script. Make a job check what a real defect would break, and let the rest be informational.
6. **Small harness slips cost cycles**: `pkill -f qemu-system.*qj` killed my own shell (the pattern matched the command line); keys injected after the
   program ended; `qemu-debug.sh mouse-move/key` said "Not running" with a custom `QEMU_DEBUG_STATE_DIR` (I used the monitor socket directly instead);
   a first edit script that asserted a pattern and aborted halfway (nothing written, but I re-sent it whole). Keep edit scripts in files.
7. **Small-sample numbers stated loosely**: "about one in three" for the QEMU panic is 1-2 of 4-5 runs. The 6/6 on the stress script is the solid number.
8. **A claim that was an inference**: "the mouse worked in #188" rests on a maximize (1920x1040) that needs a click; `gpu-apps.sh` does not record the
   input counters. Probably true; not measured.
9. **Mutation checks done late for the new nvgpu helpers**: I caught it before committing (9 mutants, 8 killed, 1 equivalent), but the docs I had
   written already said "tested".

## 11. Deeper problems in the architecture (evidence, not a to-do list)

- **No GPU isolation between processes.** One `PageTables` for all sessions; a session's slice is enforced by its model, not by the GPU
  (`docs/reference/gpu.md`). This incident shows the exposure concretely: slices are 16 GiB = 4 x 2^32, so a 32-bit wrap lands exactly one slice
  away. It faulted only because the neighbour slice happened to have **no page directory there**. Had another client had memory at that address, the
  compositor would have *read it silently* (or a client could write another's). Per-channel address spaces (the instance block's own PDB) are the
  real fix; at minimum leave an unmapped guard between slices. Related: `MAX_RT` = 8 run-time channels in total.
- **Device-lost with no reason.** Mesa/NVK sees only `VK_ERROR_DEVICE_LOST`; the kernel now logs the reason, but a client cannot read it. Expose the
  last fault (`/proc/kdebug`, and `/dev/nvgpu`) and make `vk_comp` print it. And the compositor exits on any present failure: it could recreate its
  device instead of ending the session.
- **`robustBufferAccess` is not requested** by the compositor's device (see section 4).
- **The block-cache lock** and **`sys_read` doing device I/O with IF=0 under the fd-table lock** (section 5): long IF=0 sections hurt interrupt
  latency and make every wait on the way a shootdown hazard. The "IF stays 0 inside `with_files`" design deserves a second look.
- **TLB shootdown design**: one global `SENDER`, a 1 s bound that ends in a panic. Under a hypervisor a late vCPU turns into a panic. Consider
  per-CPU request slots, a longer bound under a hypervisor, and a panic message that says each target's last service time.
- **`qemu-debug.sh` defaults to one CPU**, the real target is SMP. Several hangs only exist at one CPU (and some bugs only at 4). Default to 4, or
  say loudly which a test needs.
- **The klog is lossy** (64 KiB ring, wrapped; the job summary is printed twice to survive that; a dead-channel line was missing in #184). Use the data
  partition (read-write now) as a per-job output file, or enlarge the ring, and make jobs print structured, greppable lines.
- **Jobs filter by `grep` and hide what they did not anticipate** (`gpu-apps.sh` dropped the `COMP input` lines; a `dmesg` grep matched the job's
  own summary). Prefer "keep everything the compositor printed in the last N lines" to a whitelist.
- **Frame pacing** (30 fps cliffs at 4/6 ms delays, `present` 4-9 ms, the ~9 ms deadline after a flip) is still unexplained since #179; instrument the
  kernel's flip path (when `PRESENT` is latched relative to the vblank) before more tuning.
- **Cursor in the compositor's frame**: see section 8.

## 12. Harness and recipes that worked

- **Live panic or hang in QEMU, with gdb**: boot **without** an autorun job (autorun resets after a panic; `-no-reboot` then exits QEMU and the state
  is gone), `QEMU_DEBUG_SMP=4`, type the workload with `qemu-debug.sh send`, then `echo "gdbserver tcp::1234" | socat - UNIX-CONNECT:<state>/monitor.sock`
  and `QEMU_DEBUG_STATE_DIR=<state> scripts/qemu-debug.sh gdb "thread apply all bt 12"`. `echo "info registers -a" | socat ...` shows every CPU's TR/RIP/IF.
  A TIMEOUT is not a hang: step the same snapshot twice (an advancing LBA = slow, not stuck).
- **Inject input into a headless QEMU**: `echo "mouse_move 20 10" | socat - UNIX-CONNECT:<state>/monitor.sock`, `sendkey a`, `mouse_button 1`.
- **Parallel QEMUs** need their own `QEMU_DEBUG_STATE_DIR` (short path) and a copy of `disk.img`: `scripts/tlb-stress.sh` does it.
- **A job that sets its own time budget** (`left()` from `/proc/uptime` against the 300 s watchdog) and skips or shortens phases: `gpu-apps.sh`.
- **One variable per boot, discriminating first**: `gpu-apps-d3.sh` (arrival vs reading) is the template; it also prints the kernel's USB counters
  before and after, to prove the stimulus happened.
- **Read the kernel's own account**: `grep -a '\[nvgpu\]\|\[gsp\] event' /proc/dmesg` at the end of a job; `scripts/usb-log.sh read` afterwards.
- **Decode RM events by hand** with `open-gpu-kernel-modules/src/nvidia/generated/g_rpc-structures.h`; the fault address minus `1<<36`, divided by
  16 GiB, is the session slice.
- **Host render check**: `probes/nvk/host-comp.sh` (pixel for pixel against a CPU reference); good for logic, blind to out-of-range loads.
- **Shaders**: `sh probes/nvk/gen-spv.sh` regenerates the SPIR-V headers (check that `snake3d_spv.h` did not change).
- **Compare before/after of a refactor on the host**: `host-snake.sh` with a fixed `SNAKE3D_SEED` and `cmp` the PPMs (done for the `vk_snake.c` rework).
- **After editing a Vulkan program or `comp_*`**: `python3 probes/nvk/build.py`, `strip -o disk-image-root/bin/<name> ~/src/gpu-ref/nvk-probe/vk-<name>`
  for **every** program that includes what changed (a header-only change to `constanos_gui_vk.h` touches `vk_window` too), `touch build.rs kernel/build.rs`,
  `cargo build`, and compare with `cmp` against a fresh strip.

## 13. Test and sabotage ledger

| Change | Test | Sabotage |
|---|---|---|
| F11 in `gui` | 3 tests (65 total) | 7 mutants killed (client sees F11, grip kept, bar kept, maximized not restored, no resize request, not-resizable allowed, maximized not cleared); `raise` was redundant, removed |
| `set_resizable` C encoder | `c_wire.rs` | opcode mutant killed |
| `snake3d` window + resize | `gui_comp_test` case 3 | 4 mutants killed (resize ignored, old swapchain kept — made observable by printing `buffers_destroyed` —, wire drops `RESIZE`) |
| `terminate_children`, `cpumon` under `vk_comp` | `gui_comp_test` case 2 | not mutation-checked (checks `COMP ended 1 program(s)`) |
| `rc_fault`, `describe_va`, `payload_summary` | nvgpu tests (434 total) | 8 of 9 mutants killed, 1 equivalent |
| `comp.frag` clamp | host harness (0 pixels differ) | **cannot be sabotaged on the host**: only the Ryzen shows the bug (the fix was verified there, #188) |
| block-cache hooks (reverted) | 4 of 4 mutants killed before the revert | n/a |
| `vk_comp` input counters | QEMU with injected keys/motion/buttons | not mutation-checked |

## 14. Next steps (suggested order)

1. **`robustBufferAccess` on the compositor's device**, and print the last fault reason from `vk_comp` on a device-lost. Small, and the incident class is
   then survivable.
2. **Measure the cursor/pacing problem** (section 8) with the recipe there before changing anything. Keep `COMP pace (5 s)` in the job summary.
3. **Hardware cursor** through the display engine (or a cursor-only path) if the cursor stays tied to composition pacing.
4. **Direct scanout for a fullscreen window** (F11): check the buffer has the scanout `pitch`/layout, that `PRESENT` takes an imported BO, hold the
   buffer while shown. Measure the `present` cost against composition first; it may not matter.
5. **Block cache**: the proper fix (no lock across the device read) with A/B/C tests; and revisit IF=0 inside `with_files`.
6. **GPU isolation**: guard gap between session slices now; per-channel address spaces later.
7. Make `qemu-debug.sh` default to 4 CPUs; give the jobs a per-job output file on the data partition; add the input counters to `gpu-apps.sh`.
8. Explicit sync with shared timelines (`SYNC_EXPORT`), still untouched.
