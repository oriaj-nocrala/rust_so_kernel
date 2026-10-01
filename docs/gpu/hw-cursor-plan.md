# Hardware cursor for the GA106 display engine — design (not started)

**Status: design, plus Phase 0 (answers to the unknowns and a baseline measurement, section 11) done on the Ryzen #189. Nothing of the cursor itself is implemented.** Written 2026-10-01 after Ryzen #188, from a question the user asked while
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
- **Load 3 runs at 60 with a window four times larger.** More GPU work, better pacing. Not "more load = slower". The most likely reading, **not tested yet**: GPU clocks. A light load keeps the GPU in a low P-state (measured earlier: P5, 560-940 MHz with `snake3d` alone, the ramp is by utilisation: `docs/gpu/g5-graphics-stack-plan.md` "relojes"), where the compositor's copy to the scanout buffer takes ~9 ms and misses the ~9 ms deadline after the flip; a heavier load pulls the GPU to a high P-state and `present` falls to 3 ms. Another candidate is a phase lock between the clients' frame callbacks, the 2 ms delay and the vblank. **Next measurement: sample the P-state and clocks (`gsp perf`, `gpu_perf:` in `/proc/kdebug`) every second during loads 2 and 3.**
- No channel died, 0 refused flips, `gpu_share` 0/0/0. The mouse counters worked (`COMP input`).

### What it changes in this plan

- The hardware cursor is the right fix for the pointer **in every load** (load 2 shows why), but the 30 fps cliff of the *windows* is a separate problem and a P-state finding would be a separate fix (keep the GPU clocked while a compositor runs).
- New first step of phase 2: the `HEAD_SET_HEAD_USAGE_BOUNDS = 0x1114` push and whether it raises supervisors (unknown 1); then channel allocation (`0x610604`), then `SET_CONTROL_CURSOR` and the image, then `MOVE`.

