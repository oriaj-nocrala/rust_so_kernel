# Hardware cursor for the GA106 display engine — design, phases 1-2 done, head 0 blocked

**Status (2026-10-01, Ryzen #212-#219): phase 0 (section 11), `nvgpu::cursor` (phase 1) and the `/dev/dispctl cursor ...` adapter (phase 2) are done. The cursor ENABLES and MOVES on head 1 (HDMI; one move = 1-2 us) but the enable raises INVALID_STATE (code 0x43) on head 0, the ASUS on DP, whatever was tried: section 14. Phases 3-4 (ioctls, compositor) not started.** Written 2026-10-01 after Ryzen #188, from a question the user asked while
reading `g5-layer4-session-report-2026-10-01.md` section 8 ("the cursor does not move at 60 fps"). Everything under "What the hardware offers"
was checked against NVIDIA's own headers and nouveau's source (citations inline); everything under "Unknowns" was not.

Sibling docs: `g5-layer4-session-report-2026-10-01.md` (the observation, section 8), `gpu-plan.md` (phase 5, the display driver this extends),
skill `gpu-display` (the recipe every display subphase followed, used below), `docs/reference/gpu.md` (state of `gpu=` levels and `/dev/nvgpu`).

## 1. Problem

Today the pointer is **one more quad in the compositor's frame** (`DrawOp::Cursor` -> `CR_CPU`, `comp.frag` mode 2). So:
- it moves only when a whole composition is made and presented, at most once per vblank, started `COMP_DELAY_MS` (2 ms) after the previous flip;
- a pointer-only change recomposes and presents the whole 1920x1080 screen;
- it follows the compositor's pace: in Ryzen #188 phase D (cpumon + a maximized snake3d) **2622 flips were < 20 ms (60 fps) and 1379 were 20-37 ms
  (30 fps)**; the cursor did the same. The user saw exactly that: not 60 fps, "sometimes smoother", no visible pattern;
- when the compositor stops (device lost, a long frame) the pointer stops; with direct scanout of a fullscreen client (the F11 follow-up) there is
  no composition to put it in at all.
Why `present` costs 7 ms in phase D and 4 ms in A is a separate, still unexplained problem (report section 8). A hardware cursor does **not**
fix window frame pacing; it removes the pointer from it.

## 2. Why a hardware cursor (and when not)

What other systems do: every mainstream stack keeps the pointer on a dedicated display plane and moves it **asynchronously from the frame loop**
(Linux KMS cursor plane, which Weston, KWin and mutter update from the input event; Windows and macOS likewise; X11 since the 90s). The software
quad is the fallback.

| | hardware cursor | quad in the frame (today) |
|---|---|---|
| latency | next vblank after the position write, whatever the GPU is doing | one composition (+ its delay) behind the input |
| pace | the display's refresh, independent of any client or GPU load | the compositor's frame rate (60 or 30 here) |
| cost of a pointer-only move | two MMIO writes | a full recomposition and `PRESENT` |
| with a fullscreen / direct-scanout client | works | needs a composition |
| limits | one per head, at most 256x256, ARGB8888 (or A1R5G5B5), hot spot < 256, the image fixed through the core channel, not in screen captures, pre-scale for HiDPI | none |

Keep the quad as the fallback (no GPU level, QEMU, an unusual cursor, screenshots that must include it).

## 3. What the hardware offers (verified)

- **The model.** `open-gpu-doc/classes/display/README.txt`: *"cursor: This channel is used to position the cursor. There is one cursor channel
  per head. The cursor format and buffer is specified through the core channel. ... The cursor channel allows low-latency cursor position
  updates, asynchronously to the core channel."* Ampere (GA102/GA106) uses class `0xC67A`, which nouveau drives with the Volta cursor code:
  `dispnv50/curs.c:34-43` (`GA102_DISP_CURSOR` -> `cursc37a_new`), `nvkm/engine/disp/ga102.c:139` (`gv100_disp_curs`). Class values:
  `nvif/class.h:126-129` (`0xc37a`, `0xc57a`, `0xc67a`).
- **Position = the cursor channel's immediate registers, no push buffer.** `dispnv50/cursc37a.c:28-46`: wait for space, then write
  `SET_CURSOR_HOT_SPOT_POINT_OUT(0)` (`clc37a.h:109-111` / `clc67a.h:108-110`: offset `0x208`, X in bits 15:0, Y in 31:16) and `UPDATE` (`0x200`,
  `clc67a.h:46`) in the channel's user region (`gv100.c:336`: `0x690000 + (user - 1) * 0x1000`).
- **Channel life cycle** (`nvkm/engine/disp/gv100.c:555-608`): `gv100_disp_curs = { .ctrl = 73, .user = 73 }` (so chid 73 + head); init writes `1`
  to `0x6104e0 + ctrl*4` and waits until the status `0x610664 + (ctrl-1)*4` has bits 18:16 = 4 (idle); fini sets bit 4, waits idle, clears bit 0;
  the channel's interrupt enable is `0x611dac` bit `0x10000 << head`.
- **Image and enable = core-channel methods, pushed only when the shape or the enable changes** (`dispnv50/headc37d.c:123-151`, constants
  `clc67d.h:855-862` for C67D, `clc37d.h:826-870` for the fields):
  `HEAD_SET_CONTROL_CURSOR(h)` (`0x209c + h*0x400`: ENABLE bit 31, FORMAT 7:0 = `A8R8G8B8` `0xCF` or `A1R5G5B5` `0xE9`, SIZE 9:8 =
  32/64/128/256, HOT_SPOT_X 19:12, HOT_SPOT_Y 27:20, DE_GAMMA 29:28), `HEAD_SET_CONTROL_CURSOR_COMPOSITION(h)` (`0x20a0 + h*0x400`: nouveau
  pushes K1 = 0xff, cursor colour factor K1, viewport colour factor NEG_K1_TIMES_SRC, mode BLEND), `HEAD_SET_CONTEXT_DMA_CURSOR(h, 0)` (`0x2088`,
  the ctxdma handle) and `HEAD_SET_OFFSET_CURSOR(h, 0)` (`0x2090`, offset >> 8). The head's usage bounds must allow it:
  `HEAD_SET_HEAD_USAGE_BOUNDS` field CURSOR (`clc67d.h:746-751`: none / 32 / 64 / 128 / 256; nouveau sets 256, `headc57d.c:240`).
- The core's `SET_INTERLOCK_FLAGS` has an `INTERLOCK_WITH_CURSOR` bit per head (`clc37d.h:155-174`), so a core update that changes the cursor image
  can be interlocked with the cursor channel.

## 4. What the repo has today

- `nvgpu/src/evo.rs`: our own EVO driver (no RM), generic over `Mmio`: `Chan { ctrl, user }`, `CORE`, `window(n)` (ctrl = user = 1 + n),
  `init_channel` (push buffer based), `Push::mthd`, `flip`, `wind`, the `Sim` used by tests, the method-offset constants `CORE_UPDATE`,
  `CORE_SET_INTERLOCK_FLAGS`... **No cursor code.** (`evo.rs:601` mentions waiting for "a cursor" in a doc comment only.)
- `kernel/src/gpu/supervisor.rs`: `push_core` is the only writer of the core push buffer (under `PUSH_AT`; it also resets the core's interlock flags
  which persist in ASSEMBLY). `kernel/src/gpu/scanout.rs`, `evo.rs`: window 0, the scanout buffers, `PRESENT`.
- `nvgpu/src/hdmi.rs:246-273`: head 1 (HDMI) gets `HEAD_SET_HEAD_USAGE_BOUNDS = 0x1114` (low 3 bits = 4 = cursor 256x256, as nouveau).
  `nvgpu/src/mode.rs:182`: head 0 (the GOP's, the ASUS) **does not push usage bounds: the GOP's value stays**.
- `/dev/nvgpu` (`kernel/src/gpu/uapi.rs`, `drivers/dev_nvgpu.rs`): `PRESENT` with an owner session; `Session::shown` holds the storage on screen
  (a rule learnt the hard way: an image the display reads must be held until the display has let go of it).
- Compositor: `vk-comp/src/lib.rs` handles the pointer (`comp.pointer_motion`, `DrawOp::Cursor{x,y}`), `cursor_pixels()` is an 11x16 bitmap.

## 5. Design

One thin layer per step, each testable alone, following the `gpu-display` recipe (oracle -> fixture -> pure code -> replay test -> sabotage ->
adapter -> `gpu=` level -> job).

1. **`nvgpu/src/cursor.rs` (pure, generic over `Mmio`)**: constants with `file:line` citations (section 3); `core_methods_set(head, fmt, size, hot,
   ctxdma, offset)` and `core_methods_clear(head)` as method rows; `ChanCursor { head }` with `ctrl = user = 73 + head`, `init`, `fini`, `idle`;
   `move_to(m, head, x, y)` = the two writes. Tests: replay against `Sim` (extended with the cursor registers) and, if the oracle trace holds a
   cursor, a fixture; method rows compared against what nouveau pushes (`headc37d_curs_set`); **sabotage every constant and branch**.
2. **Kernel adapter `kernel/src/gpu/cursor.rs`** behind a new `gpu=` level (after `scanout`/`modes`; `hal::bootopts::GpuLevel`, `probe_device`,
   `debug.rs`, `dev_dispctl.rs`, `docs/reference/gpu.md`, a "Resultados" block in the plan): owns the cursor image storage, the channel, the order
   of operations, `gpu_cursor:` counters (`sets`, `moves`, `refused`, `last_us`), logging. The core push goes through `push_core`; the move
   is **lock-light** (its own small lock, never `HW`, never the core push lock) because it will be called per input event.
   `/dev/dispctl` gets test commands first: `cursor on <w> <h>`, `cursor move <x> <y>`, `cursor off`; a metal job paints a known pointer and sweeps it.
3. **`/dev/nvgpu` ioctls (only the `PRESENT` owner)**: `NVG_IOC_CURSOR_SET {bo, offset, size, hot_x, hot_y, enable}` and
   `NVG_IOC_CURSOR_MOVE {x, y}`; ENODEV/ENOTTY when there is no hardware cursor (QEMU, level too low). The image BO is **held while it is on
   screen** exactly like `PRESENT` holds its buffers (`hold_storage`; a cursor image freed under the display would be the same bug class as
   Ryzen #171). Header `nvgpu/uapi/nvgpu.h` is the source of truth; clang oracle `nvgpu/gen/uapi.c`.
4. **Compositor**: upload the cursor as a 32x32 (or larger) ARGB BO at start; on a pointer event call `MOVE` **from the input handler, not the frame
   loop**; keep the quad as the fallback when the ioctl says no; a pointer-only event no longer sets `has_damage`. Client-supplied cursors
   (`set_cursor(buffer, hot_x, hot_y)` in the window protocol; resize arrows, text beam) are a later step; HiDPI means a pre-scaled image per scale.
5. **Measure** (section 7) and write the "Resultados" block.

## 6. Unknowns (answer these first; each is a cheap look, not a build)

1. **Head 0's usage bounds.** The GOP's value stays on head 0; if its CURSOR field is "none", enabling a cursor needs `HEAD_SET_HEAD_USAGE_BOUNDS`
   changed, which in nvdisplay is a modeset-class change involving the supervisors (bandwidth/ISO accounting) and the SOR/`own_head` rules of the
   `gpu-display` skill. Read the ARMED value (`0x2030 + ...`, as `hdmi.rs` does) on the Ryzen before anything else.
2. **The ctxdma object for the cursor surface.** `HEAD_SET_CONTEXT_DMA_CURSOR` takes a handle in the display RAMHT; window 0's ISO ctxdma may
   already cover the VRAM the image will sit in (the scanout buffers live in carved VRAM); otherwise one more RAMHT entry (`Ramht`, `evo.rs`).
3. **Does the cursor channel's `UPDATE` need an interlock with the core** after the first enable, and does the position take effect at the next
   vblank or at once? (nouveau uses `INTERLOCK_WITH_CORE_DISABLE` on EVO and a plain `UPDATE` on `NVC37A`.) Measure with the raster-position
   instrument (`vblank::scan_position`) before promising "within a frame".
4. **Position semantics**: the register is the hot-spot position in the output; is X/Y signed (16-bit fields)? What happens off-screen or with a
   negative position (a cursor partly outside)? Clamp in the compositor regardless.
5. **Alpha**: premultiplied or not (`COMPOSITION` K1 = 0xff and the factor selects nouveau uses imply straight "blend"); check the picture on screen.
6. **Channel allocation**: nouveau creates it as an object of the display; our driver programs channels directly (`init_channel`). The cursor channel
   has no push buffer, so `init` is the control write and idle wait of section 3; confirm there is no RAMHT binding or error-slot setup needed
   (the exception slots are `0x611020 + chid*12`, `evo.rs`/`gpu-display` skill: read them in the metal job's diagnostics).
7. **180 Hz**: `disk-image-root/etc/180hz` (uncommitted, deliberately) switches the ASUS to 180 Hz; a hardware cursor would then move at the
   display rate while a composited one stays at the compositor's. Interaction with `modeset::set` (does a mode change keep the cursor state?).
8. **The console and `restore_front`**: when the `PRESENT` owner closes, the console's picture comes back; the cursor must be disabled then
   (`Session` close path), or the last pointer stays on the console.
9. **Dual head**: head 1 (HDMI) has its own cursor channel (73 + 1); out of scope until the compositor drives both heads.

## 7. How to know it worked

No camera, so proxies, in order of strength:
1. `gpu_cursor: moves=` and the input counters of `vk_comp` (`COMP input`): moves ~ mouse reports (~65/s measured), **independent of the
   compositor's composition count**.
2. **Compositions while only the pointer moves = 0**, and the compositor idle (no `PRESENT`) while the pointer moves over a static screen.
3. The user's eyes with the load that showed it (a maximized `snake3d` + `cpumon`): the pointer smooth while the snake stays at 44-51 fps.
4. A cursor-only run (no clients) before and after: the quad's update rate vs the hardware's.
Instrument first (a counter per `MOVE`, the time from the input read to the write), then the change.

## 8. Phases

0. **Answer the unknowns of section 6** and measure the baseline: **done**, section 11 (`scripts/metal-jobs/gpu-cursor-probe.sh`, `dispctl peek`).
1. `nvgpu::cursor` + tests + sabotage.
2. Kernel level + `/dev/dispctl cursor ...` + metal job `gpu-cursor.sh` (a pointer swept over the screen; the user's eyes; leaves the display
   recoverable on any failure, as every phase-5 job does).
3. `/dev/nvgpu` ioctls + `vk_comp` (fallback kept) + session-close cleanup.
4. Client cursors in the window protocol; HiDPI; screenshots (a software cursor on demand).

## 9. Alternatives considered

- **A window channel as the cursor** (nvdisplay windows support alpha blending, scaling, blend order): any size and format, but a window channel and
  interlocked core updates per move: more cost and complexity than the dedicated channel; keep as a fallback if the cursor channel's limits bite.
- **Move the cursor from the kernel's input path directly** (mouse report -> the write): the lowest latency and independent of any user process,
  but it puts pointer policy (clamping, multi-window, shapes) in the kernel. Calling `MOVE` from the compositor's input handler gets nearly the
  same latency (one syscall) and keeps the policy where it belongs.
- **A cursor-only fast path in the compositor** (re-present the previous scene with only the cursor changed): avoids the recomposition but not the
  `PRESENT` or the pacing coupling; a lot of machinery for something the hardware does for free.
- **Doing nothing and fixing `present`'s 7 ms**: still needed for the windows, but it would leave the pointer tied to frame pacing.

## 10. Non-goals

Fixing the 30 fps cliffs of window frames, direct scanout for F11, a second head, animated or colour-key cursors, cursor in screenshots.

## 11. Phase 0 results (2026-10-01, Ryzen #189, `scripts/metal-jobs/gpu-cursor-probe.sh`)

New instrument, kept: **`/dev/dispctl peek <offset>`** reads one display register from an allow-list (`nvgpu::evo::peek_allowed`: `0x610000-0x611fff` and
`0x640000-0x6dffff`, 4-aligned; 7 mutants killed) and the next `read` shows a `peek:` line. Everything below was read with it or taken from the
oracle (`~/constanos-gpu-oracle/trace-nogsp`, `nvgpu/fixtures/modeset-core-round1.txt`) and the manuals.

### Answers to the unknowns of section 6

| # | Question | Answer (and how sure) |
|---|---|---|
| 1 | Head 0's usage bounds | **ARMED = ASSEMBLY = `0x1110`: the cursor field (bits 2:0) is 0 = none** (live on the Ryzen; same in the oracle dump). nouveau's value is `0x1114` (W256_H256 | OLUT_ALLOWED | TAPS_2 | UPSCALING_ALLOWED, `headc57d.c:240`): the GOP's value is nouveau's minus the cursor. **nouveau pushes it only inside its mode function** (the comment there says it "doesn't belong here"). So enabling a cursor means a core push of `HEAD_SET_HEAD_USAGE_BOUNDS(0) = 0x1114`; **whether the display wants a supervisor cycle for that is still unknown** (phase 2 experiment, through `push_core` as the `clock` command does). Head 1: `0x1000` (also none; `hdmi.rs` already pushes `0x1114` there). |
| 2 | The ctxdma for the image | Our RAMHT is keyed `(chid, handle)` and only has window 0's and the HDMI window's entries. `HEAD_SET_CONTEXT_DMA_CURSOR` is a **core** method, so it needs one new entry on chid 0 (the core) naming all of VRAM (nouveau uses the core's VRAM ctxdma: `curs507a_prepare`). Read from the code, not verified on hardware. The GOP left `CONTEXT_DMA_CURSOR` and `OFFSET_CURSOR` = 0. |
| 3 | `UPDATE` semantics | `SET_INTERLOCK_FLAGS` (`0x204`) is 0 by default: no interlock with the core (and bits 7:0 interlock with the cursors), `UPDATE` (`0x200`) alone is what nouveau writes (`cursc37a_update`). **When the position takes effect (next vblank or at once) is not in the headers: measure.** |
| 4 | Position semantics | X 15:0, Y 31:16, the hot spot's position in the output; nouveau passes `crtc_x/y`, which can be negative, so presumably two's complement. Not verified; clamp in the compositor regardless. |
| 5 | Alpha | nouveau sets K1 = 0xff, cursor factor K1, viewport factor `NEG_K1_TIMES_SRC`, mode BLEND: **premultiplied** alpha (`K1*src + (1 - K1*alpha_src)*dst`). The GOP's `COMPOSITION = 0x2ff` has viewport factor ZERO (opaque). Check the picture. |
| 6 | The channel | **PIO channel** (`NVC67A_CURSOR_IMM_CHANNEL_PIO`): no push buffer, no PUT/GET (`+0`/`+4` read `0xbadf5040`), no RAMHT binding; `FREE` at `+8` reads 4. Live: **control `0x610604 = 0` (unallocated), status `0x610784 = 0x01000000` (not idle), interrupt mask `0x611dac = 0`.** The oracle has nouveau's init: `W 0x611dac` 0 -> 1 -> `0x10001` -> ..., **`W 0x610604 = 1`, then `R 0x610784 = 0x40000` (idle = bits 19:16 == 4)**, then the user region `0x6d8000` mapped. The manual agrees (`NV_PDISP_CHN_NUM_CURS(i) = 73 + i`, `NV_PDISP_FE_CHNCTL_CURS(i) = 0x610604 + i*4`). The exception slot exists: `0x611020 + 73*12 = 0x61138c` reads 0 (`data`/`code` are PRI errors until the channel exists; `NV_PDISP_FE_EXCEPT` has 81 slots, nouveau reads 73 + head). **The oracle holds no cursor position write and no `SET_CONTROL_CURSOR` push** (nouveau moved no cursor in that capture): the tests will be built from the headers and nouveau's code, and the first metal job is the oracle. |
| 7-9 | 180 Hz, console restore, dual head | Not touched by phase 0. |

State the GOP left, as read: head 0 `CONTROL_CURSOR = 0xe9` (disabled, A1R5G5B5, 32x32), `COMPOSITION = 0x2ff`, window 0's channel running (`0x6104e4 = 0x13`, status idle), core idle.

### Baseline: the cursor as it is (a quad in the compositor's frame), the person moving the mouse all the time

| Load (15-20 s) | compositions/s | pointer motions/s | flips < 20 ms / 20-37 ms / > 37 | `present` avg | snake3d |
|---|---|---|---|---|---|
| 1 `vk_comp` alone | **50.5** | 85 (USB 85/s) | 746 / 3 / 8 | 2.1 ms | - |
| 2 + cpumon + snake3d, window 960x540 | **30.3** | 95 | **25 / 577 / 2** | **9.1 ms** (9.1-9.6 steady) | **30.6 fps** |
| 3 + cpumon + snake3d, window 1880x1000 | **58.9** | 94 | 1156 / 19 / 1 | 3.0 ms | 59.1 fps |

- Alone, the cursor path works at about the display's rate (50-56 compositions/s while the mouse reports ~85/s: coalescing, one composition per vblank at most).
- **Load 2 is locked at 30 fps (95% of the flips 20-37 ms), compositor and snake together, with `present` a constant ~9.1 ms: the known 30 fps cliff, caught live. The cursor then updates 30 times a second.** That is the user's "not 60 fps".
- **Load 3 runs at 60 with a window four times larger** (explained in section 12: the heavier load keeps the GPU at a higher P-state). More GPU work, better pacing. Not "more load = slower". The most likely reading, **not tested yet**: GPU clocks. A light load keeps the GPU in a low P-state (measured earlier: P5, 560-940 MHz with `snake3d` alone, the ramp is by utilisation: `docs/gpu/g5-graphics-stack-plan.md` "relojes"), where the compositor's copy to the scanout buffer takes ~9 ms and misses the ~9 ms deadline after the flip; a heavier load pulls the GPU to a high P-state and `present` falls to 3 ms. Another candidate is a phase lock between the clients' frame callbacks, the 2 ms delay and the vblank. **Next measurement: sample the P-state and clocks (`gsp perf`, `gpu_perf:` in `/proc/kdebug`) every second during loads 2 and 3.**
- No channel died, 0 refused flips, `gpu_share` 0/0/0. The mouse counters worked (`COMP input`).

### What it changes in this plan

- The hardware cursor is the right fix for the pointer **in every load** (load 2 shows why), but the 30 fps cliff of the *windows* is a separate problem and a P-state finding would be a separate fix (keep the GPU clocked while a compositor runs).
- New first step of phase 2: the `HEAD_SET_HEAD_USAGE_BOUNDS = 0x1114` push and whether it raises supervisors (unknown 1); then channel allocation (`0x610604`), then `SET_CONTROL_CURSOR` and the image, then `MOVE`.

## 12. Phase 0b: the 30 fps lock is the GPU's clocks (Ryzen #190, `scripts/metal-jobs/gpu-cursor-pstate.sh`)

`dispctl gsp pstate` (new: one RM control instead of nine) sampled once a second beside the loads of section 11; same load, one variable per phase.

| Phase (15 s, cpumon + snake3d autoplay) | P-state trace | `present` per 5 s window | compositor | snake3d |
|---|---|---|---|---|
| P1 960x540, mouse **not read** | P0 (t11-17) -> **P5 (t18-23) -> P8 (t24-26)**, *while the load went on* | **1.8 -> 4.2 -> 7.1 ms**; flips < 20 ms: 275, 276, 91 of ~290 | 60 fps, then 60, then half at 30 | 52.1 fps |
| P2 960x540, mouse read and moved | **P8 all 15 s** (it started at P8 and never ramped) | **9.2, 9.1 ms constant**; flips < 20 ms: 2, 3 of ~150 | **30 fps lock** | 30.2 fps |
| P3 same as P2, no sampler | (not sampled) | 9.2 -> 5.5 -> 9.1 ms | 30, 60, 30 | 38.1 fps |
| P4 1880x1000, mouse read and moved | **P3/P5 alternating** (P8 -> P5 -> P3 within 2 s) | 3.2, 2.9, 3.0 ms | 60 fps | 58.6 fps |
| P5 alone, mouse read and moved | P5 -> P8 | 4.3-4.7 ms | ~53/s (the mouse's pace) | - |

- **The clocks are the cause; the mouse is not**: P1 (no mouse read) goes 60 -> 30 fps as its P-state falls P0 -> P5 -> P8; P2 and P3 show the lock with the mouse; P3 without the sampler behaves the same, so asking RM for the P-state does not disturb it.
- **`present` is ~2 ms at P0, ~4 ms at P5, ~9 ms at P8**, and the compositor misses the ~9 ms deadline (flip seen -> `PRESENT`; `COMP_DELAY_MS` 2 + 9) at P8: the cliff of #179-#182 (delays of 4 and 6 ms gave 30 fps at P5-like costs for the same reason).
- **A DVFS trap**: a light, frame-paced load keeps RM at P8; at P8 the frames take longer, so fewer frames are made (30 fps), so the load is lighter still, so RM stays at P8. A heavier load (P4) climbs to P3 and everything is fast. That is why a *bigger* window was *faster*.
- The GPU starts at P0 after the boot and RM lowers it within ~7 s of a light load.

### Decision (the user, 2026-10-01): **do not pin the GPU's clock high for the compositor's life.**
A desktop compositor can be the protagonist of the machine and idle most of the time; holding peak clocks would burn peak watts for nothing. The fix is to stop the pacing from depending on the clock, not to buy it with power. In order of preference, none started:
1. **Hardware cursor** (this document): the pointer stops depending on the frame loop at all, at any P-state.
2. **Hide the composition's latency instead of shortening it**: the loop is serial (wait for the flip, 2 ms, compose + `present`, wait for the next flip); with `present` at 9 ms the `PRESENT` always leaves past the deadline. Start the composition *before* the flip it follows (render into the back image while the previous flip is pending; `PRESENT` the moment it lands), or schedule the repaint from an adaptive estimate of render + present time (Weston's repaint window, mutter's dynamic max render time). Costs one frame of latency for content and a buffer; makes 60 fps possible up to ~16 ms of composition at any clock.
3. **Do less GPU work per frame**: scan out the swapchain image directly (block-linear window surface) instead of the buffer-blit copy that `wsi_common` does into a scanout-layout buffer (an 8 MB detile per frame at 1080p); less work also means less power.
4. **A hint to RM, bounded**: a burst boost while frames are in flight with a decay (`PERF_AGGRESSIVE_PSTATE_NOTIFY`-style), only if 1-3 are not enough. Never a pin.
Whether `present`'s 9 ms at P8 is the copy itself or queueing behind the client's frame on the shared GR engine is not known: `vkCmdCopyImageToBuffer` timing and the GPU's own timestamps would say.

## 13. Phase 0c: where a compositor frame's time goes (Ryzen #191/#192, the kernel pacing instrument)

New permanent instrument (`nvgpu::pacing`, `kernel/src/gpu/pacing.rs`; 12 mutants killed): `/proc/kdebug` `gpu_pacing:` (per channel, submission -> first look that found it done; how long after the previous
vblank each `PRESENT` arrives, in 2 ms buckets; the `PRESENT` ioctl's duration) and `gpu_pacing_trace:` (the latest 60 events: `S<chan>.<seq>` submitted,
`D<chan>.<seq>` seen done, `P<us after the vblank>`, `E<us>`, `V<seq>`); `dispctl trace reset`. Job `scripts/metal-jobs/gpu-comp-pacing.sh`. Channel 0 is
the compositor's, channel 2 the snake's; each compositor frame is two submissions (the render, then the buffer-blit of the WSI) and one `PRESENT`.

One frame, read from the traces (`+us` from the previous vblank; the frame starts 2 ms after it, `COMP_DELAY_MS`):

| | render S0->D0 | blit S0->D0 | `PRESENT` arrives | snake's frame (ch2) |
|---|---|---|---|---|
| P0, cpumon + snake3d | **1.8 ms** | 0.13 ms | 4.3 ms after the vblank | 0.23 + 0.07 ms |
| P5 | 3.4 ms | 1.0 ms | 6.6-8.4 ms | 1.45 + 0.4 ms |
| P8 | **6.8 ms** | 2.3 ms | **11-13 ms** (past the ~9 ms deadline) | 2.9 + 0.8 ms |
| P8, **cpumon only** (no other GPU channel) | **6.0-6.5 ms** | 2.3 ms | 12-13 ms | - |
| P3 (the 1880x1000 snake load) | 1.8 ms | 0.24 ms | 4.2-4.5 ms | 1.3 ms |

- The `PRESENT` ioctl itself takes **4-5 us**: it is not a cost.
- **Not "waiting behind the snake's channel"**: with only cpumon (no other GPU client) the render still takes 6 ms at P8; adding the snake adds ~0.5 ms.
- **The blit scales with the memory clock** (0.13 -> 2.3 ms is 18x; the memory clock goes 7001 -> 405 MHz): it is bandwidth bound (16 MB moved per frame).
- **The render has a floor of ~1.8 ms even at P0/P3**, for a background and three rectangles: far more than 4 Mpixel of shading needs on this GPU. With only cpumon at P8 it is 6 ms. **Working hypothesis (not yet tested): the CPU-drawn windows (cpumon's 1280x920 = 4.7 MB, titles, cursor) are read by the fragment shader straight from system memory over PCIe every frame, though cpumon changes twice a second**, and the PCIe link and the clocks fall with the P-state. The control that decides it is the snake alone (a VRAM source, no CPU window): the job now runs it.
- Consequence for the argument of section 12: a GPU that runs a game at 60 fps is not slow; our compositor is wasteful (it renders the whole 1920x1080 screen and blits it, 24 MB of traffic per frame even when only the pointer moved, and may read pixels from host memory). A compositor on a proprietary driver sits at the lowest P-state all day and is smooth because its per-frame work is a fraction of ours.

Candidate fixes, all about less work (none started; no clock pinning): (1) keep CPU windows' pixels in VRAM and upload only when their `version` moves (a staging copy); (2) damage tracking: render and blit only what changed (a pointer move or a cpumon update touches a few hundred KB), which with a hardware cursor makes pointer frames nearly free; (3) scan out the swapchain image without the blit; (4) pipelined or adaptive repaint to hide what is left.


### 13b. The control (Ryzen #193, `gpu-comp-pacing.sh` phases C / S / L, run in that order)

Passes (exit 0, no channel lost). Per frame, from the traces; the budget is `COMP_DELAY_MS` (2 ms) + render + blit ≤ ~9 ms (the `PRESENT` deadline of Ryzen #181):

| load | P-state | render | blit | `PRESENT` arrives after the vblank | flips |
|---|---|---|---|---|---|
| **S** snake3d only (960x540, a VRAM window, no CPU window) | P8 | **2.3 ms** | 2.3 ms | 4.7-7.3 ms (buckets 4-8 ms) | **308 of 309 vblanks: 60 fps at P8** |
| S | P5 | 1.3 ms | 1.5-1.8 ms | 4.7-5.7 ms | 309 of 309 |
| **C** cpumon only (1280x920, CPU window) | P0 | 1.6 ms | 0.13 ms | 2.5 ms | (a frame every 0.5 s: cpumon's rate) |
| C | P8 | **6.4 ms** | 2.9-3.9 ms | 5.6-17 ms | |
| **L** both | P8 | **6.6 ms** | 2.3 ms | **11.2-11.3 ms** (bucket 10-12 ms, 142-154 of ~155 presents) | ~150 per 5 s: **30 fps** |

- **The 30 fps lock is the CPU-drawn window, not the GPU's clock as such**: with the snake alone the same compositor, at the same P8, makes 60 fps (render 2.3 + blit 2.3 + 2 ms delay = 6.8 ms < 9). Adding cpumon moves the render from 2.3 to 6.6 ms (+4.3 ms) and the `PRESENT` to 11.3 ms: it lands past the deadline, the flip lands a vblank late, and the loop (which starts the next frame 2 ms after the flip) runs at one frame per two vblanks.
- Why a CPU window costs that: `comp_cpu_source` (`probes/nvk/comp_render.h`) keeps its pixels in a **host-visible buffer** (system memory); the fragment shader reads them over PCIe on every frame in which the window is on screen, whether or not it changed (uploads are 34 in 454 frames). The cost scales with the P-state (P0 1.6 ms for cpumon alone, P8 6.4 ms). What the control does not separate is that cost from the cpumon rectangle simply being bigger (1280x920 vs 960x540 of shading): a VRAM copy of the window (fix 1 below) is the experiment that tells them apart, and it is the next change to make.
- The blit is the same 2.3 ms at P8 in S and L (memory-clock bound, independent of the content): the second biggest item, and the one a direct scanout (fix 3) removes.
- So the budget at the lowest P-state is 2 + 2.3 (render of a VRAM window) + 2.3 (blit) = 6.6 ms: it fits, with room for a few windows. **No clock pinning needed**; what is needed is that no window's pixels are read from system memory per frame.

Order of the next changes (user to confirm; none started), cheapest and most decisive first: (a) CPU window sources into a VRAM image, uploaded (one staging copy on the transfer path) only when `version` moves; expected P8 render for L ~2.5-3 ms and 60 fps; (b) `COMP_DELAY_MS=0`/1 as a one-line experiment (frees 1-2 ms of the budget); (c) damage tracking and direct scanout (halve the memory traffic); (d) the hardware cursor (phases 1-4 above) for pointer-only frames.

### 13c. Fix 1 measured: CPU windows in VRAM (Ryzen #194, `gpu-comp-pacing.sh`, 960x540 snake + cpumon, 15 s each, `COMP_CPU_HOST=1` = the old way)

`comp_render.h` now keeps a CPU window's pixels in a device-local buffer and copies them from a host-visible staging buffer, inside the frame's command buffer, only when `version` moves. Host harness: pixel-exact in both modes, and removing the copy makes every frame differ (sabotage). QEMU `gui_comp_test` passes.

| run | P-state in the 5 s windows | ch0 avg (render + blit) | `PRESENT` arrives after the vblank | presents per 5 s (310 vblanks) |
|---|---|---|---|---|
| L2 old way (host windows) | P8, P8, P8 | **4.55 ms** | 10-12 ms (all) | 144-161: **30 fps** |
| L1 VRAM windows | P0, P5, P5 | 0.45 / 0.73 / 1.96 ms | 2-4 ms, then 4-6 ms | 262 / 305 / 294: **~57-60 fps** |
| L3 VRAM + `COMP_DELAY_MS=0` | P8, P5, P5 | 2.26 / 2.0 / 1.9 ms | 2-4 ms (202-227 of ~290) | 283 / 298 / 278 |
| L4 VRAM again | P5, P5, P8 | 2.3 / 1.95 / 1.94 ms | 4-6 ms | 269 / 295 / 275 (P8: 89% of vblanks) |

- **The 30 fps lock is gone with no change to any clock**: the same load that gave 30 fps at P8 gives ~55-60 fps. Render + blit fell from 4.55 to ~1.9 ms at P8 and the `PRESENT` arrives 4-6 ms after the vblank (deadline ~9 ms). Confirms that the lock was the shader reading host memory per frame.
- `COMP_DELAY_MS=0` moved nothing measurable (L3 ~ L4): not worth changing the default.
- The GPU now settles at P5 rather than P8 under this load (it has more to do per second); no pinning was needed.
- **What it costs: the frames that upload.** `cpumon only` (every frame is an upload of 4.7 MB; ~2 frames/s) now averages 8-10 ms per submission pair at P8 (was 4.5): the staging copy of a whole 1280x920 window goes through the GR channel at P8 and the `PRESENT` lands 2-16 ms after the vblank, so those frames can miss. They are rare (cpumon changes twice a second) but they are the visible hitch to expect when a CPU window updates. Next: upload only the rows that changed (the window manager knows the damage), or a copy on the CE channel off the frame's critical path.
- Still the cursor-pacing question of Section 11: not touched here; the hardware cursor (phases 1-4) is the next piece, and damage tracking now has two uses.

### 13d. Fix 1b: copy only the rows that changed (Ryzen #195, `gpu-comp-pacing.sh`)

`comp_plan_upload` (`probes/nvk/comp_render.h`) compares a new version of a CPU window with the last one (a CPU shadow copy), and the frame copies only the changed row ranges (up to 16, ranges closer than 4 rows merged) from the staging buffer into VRAM: damage found by comparing, so it does not depend on what a client says it damaged (cpumon says the whole window). The quit line now shows the KiB uploaded. Host harness: three changed rows = exactly 1920 bytes and the picture exact; sabotage (every row "changed"; staging write skipped) is caught.

| run | KiB per upload | ch0 avg per submission at P8 | note |
|---|---|---|---|
| cpumon alone, host windows (old way) | 4266 (the whole window is read every frame) | 4.45-5.7 ms | |
| cpumon alone, VRAM, whole-window copy (#194) | 4266 | 8.3-9.7 ms | the regression this fixes |
| cpumon alone, VRAM, **changed rows only** (#195) | **1815** | **1.8 ms** (the heaviest frame: render 5.6 + blit 3.0 ms) | |
| cpumon + snake, VRAM rows only | 2009 | 1.9 ms, 295/309 and 276/309 presents at P5/P8 | as #194: the snake's frames do not upload |
| cpumon + snake, host windows (old) | 4073 | 4.55-4.78 ms, 143-161 presents: 30 fps | |

- cpumon changes ~43% of its bytes per update on average (row granularity); the mean frame is now cheap and the worst one (a big redraw) still costs ~8.6 ms at P8 (render 5.6, blit 3.0), which fits when it is alone on the GPU.
- What is left per frame is the 2.3 ms blit (memory-clock bound) and the render of the whole screen: fix 3 (scan out the image directly / render and blit only the damaged rectangle) and the hardware cursor (pointer-only frames) are the remaining pieces; none started.


## 14. Phases 1-2: what the hardware taught (Ryzen #196-#220)

Code: `nvgpu/src/cursor.rs` (pure, 14 tests, mutation-checked), `kernel/src/gpu/cursor.rs` (adapter), `/dev/dispctl cursor probe | image | raw <m> <v>... [il] | on | onmode | move | update | recover | ilock off | head <n> | intr`,
jobs `scripts/metal-jobs/gpu-cursor.sh` (probe, on, sweep, off), `gpu-cursor-ladder.sh` (one core method per push), `gpu-cursor-head1.sh`, `gpu-head-diff.sh` (ARMED head 0 vs head 1).

**What works (measured).** The cursor channel (chid 73 + head, PIO) allocates exactly as nouveau does (control `0x610604` = 1, status `0x610784` = `0x40000`). On **head 1** the whole sequence enables the cursor (ARMED `HEAD_SET_CONTROL_CURSOR = 0x800000cf`) and `move` is two stores costing 1-2 us (#212). Every core method of the set is accepted on head 0 *one at a time while the cursor is disabled*: composition `0x72ff`, offset, control disabled, usage bounds `0x1114` (one supervisor cycle, served), the context DMA in both slots, `PRESENT_CONTROL_CURSOR`.

**Found on the way (each of these was a real bug, none about the cursor itself).**
1. **GSP-RM's boot makes the display's context DMA lookups hang when the instance memory is at the top of VRAM.** Any `HEAD_SET_CONTEXT_DMA_*` on the core left `CHNSTATUS_CORE = 0xa20c0005` (`STG1_STATE = 5 CTX_DMA_LOOKUP`) for ever (#198-#201: own handles, flags 0x05 and 0x45, the channel's interrupt on, the OLUT's handle). At `gpu=hdmi` (before the GSP) the same push resolved (#202). `evo::INST_VRAM` moved from `0x1ffc90000` (nouveau's address, inside GSP's reserved top) to **104 MiB**; 256 MiB read back `0xbad0ac82` through PRAMIN (#203). Window flips never showed it (no lookup after the first). Compositor regression-checked on the Ryzen #220.
2. The core's exception slot: `0x611020 + chid*12`; `stat` bits 14:12 = reason (5 INVALID_STATE), 11:0 = method >> 2, `data`, `code`. After an exception the core sits in `WAIT_FOR_UPD` (`CHNSTATUS_CORE = 0xa00c0007`) until `0x611020 = 0x90000000` (nouveau's clear, `cursor recover`); **the failed state stays in ASSEMBLY**, so every later UPDATE fails again until the offending method is pushed back (`control = 0xcf`). A group of methods that is only valid whole (the output LUT's four) must go in one push (`cursor raw m v m v ...`).
3. `CHNSTATUS_CORE` decodes: bits 3:0 `STG1_STATE`, 20:16 `STATE` (0xb IDLE, 0xc BUSY), 31 method executing (`dev_display_withoffset.ref.txt:421-470`).

**The open problem: head 0 refuses the enable (INVALID_STATE, data 1, code 0x43, at the UPDATE).** Tried, all with the same exception:
the plain enable; the channel positioned and UPDATEd first; nouveau's whole `curs_set` in one push; NVIDIA's (`nvkms-evo3.c EvoSetCursorImageC3`: present control, both slots); interlocked from the core side only (the core then waits for ever in `WAIT_FOR_UPD`) and from both sides; at `gpu=hdmi` (no GSP); in the push that re-attaches the head after a detach (`onmode`); the head's display id (`0x10`, nouveau's value; the GOP leaves 0); dither `0x10`; procamp; `HEAD_SET_DSC_CONTROL = 8`; window 0's usage bounds (`0xf`, `0x117fff`); all of those in one push. The output LUT (head 1 has one) raises code `0x41` on head 0 even as a four-method push.
`gpu-head-diff.sh` (#218) read the ARMED core state of both heads after `hdmi on`: the ONLY differences are exactly that list (dither, display id, usage bounds, the LUT 0x2280-0x228c, DSC control, window usage/owner). So what is left is **not a head method**: most likely what the HDMI flow does around the LUT, which head 0 never got: window 0 with its own ILUT and notifier, interlocked with the core's OLUT push (`hdmi.rs`: head push, window push with `SET_INTERLOCK_FLAGS = 1` + `SET_WINDOW_INTERLOCK_FLAGS`, core push with the OLUT and the window bit; #89/#90 showed a window UPDATE without them raising INVALID_STATE `0x2d`), i.e. nouveau's full head-0 programming instead of the GOP's.

Next steps, in order of cost: (a) replay that flow on head 0 (window 0's state with ILUT + notifier through `scanout.rs`'s window 0 push buffer, then the core's OLUT push interlocked), then the enable; (b) use a **window channel as the cursor plane** on head 0 (the design's alternative A: window 1 owned by head 0, alpha blended, position by a window update; the machinery is the HDMI window's, proven); (c) enable the hardware cursor only on head 1 (useless for the ASUS). Do not pin or hack the compositor around it: the software cursor (a quad) stays the fallback.
