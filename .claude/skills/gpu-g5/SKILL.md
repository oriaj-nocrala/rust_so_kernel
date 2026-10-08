---
name: gpu-g5
description: Playbook for the graphics stack on NVK (workstream G5, docs/gpu/g5-graphics-stack-plan.md): several processes on the GPU at once (/dev/nvgpu sessions, VA slices, shared VRAM heap), sharing buffers and timelines between processes (BO_EXPORT/IMPORT, SYNC_EXPORT/IMPORT, SCM_RIGHTS), the lock discipline of kernel/src/gpu/uapi.rs, Mesa's nvkmd_constanos import/export, vk_share, the metal job gpu-multi.sh, and where layer 3 (the WSI) starts. Use before touching dev_nvgpu.rs, gpu/uapi.rs, nvgpu::devmodel, the uapi header, mesa-port/overlay, or when a client sees stutter/hangs next to another GPU client. Keywords: session, slot, export, import, dma-buf, timeline, SCM_RIGHTS, PRESENT owner, hitch, lock_wait, gpu_share, gpu_uapi_slow, vk_share, wsi_common.
---

# G5: many GPU clients, shared buffers and timelines (layers 1-2 done, measured on the Ryzen #160-#166; layer 3 = WSI is next)

State and rules: `docs/reference/gpu.md` "`/dev/nvgpu`" (Sessions, Sharing buffers, Sharing timelines, channel creation). Plan and what is next: `docs/gpu/g5-graphics-stack-plan.md`. Commits `0c02604`, `cd33474`. Sibling skills: `gpu-gsp` (channels, RM), `kernel-testing`, `metal-run`.

## Code map

| What | Where |
|---|---|
| Pure rules: BOs, VA, contexts, timelines, `Backend` seam (`shared_heaps`, `heap_*`, `shared_sync_*`), `bo_import`, `sync_share/import` | `nvgpu/src/devmodel.rs` (58 host tests; the test backend `Shared` there is a model of the kernel adapter) |
| VA slicing `session_va`, `SESSIONS` | `nvgpu/src/hwq.rs` |
| Interface (C header = source of truth), Rust mirror, clang oracle | `nvgpu/uapi/nvgpu.h`, `nvgpu/src/uapi.rs`, `nvgpu/gen/uapi.c` (run command in its header; numbers are asserted in `uapi.rs` tests) |
| Sessions, `STORAGE` (arena + VRAM heap + holder counts), `SYNCS` registry, `BoFile`/`SyncFile`, export/import handlers, PRESENT owner | `kernel/src/drivers/dev_nvgpu.rs` |
| GPU state under one lock `HW`: channels, tables, `prepare_rt`/`run_rt`/`finish_rt`, `ctx_destroy` in steps, `HwGuard` (records long holds) | `kernel/src/gpu/uapi.rs` |
| An ioctl that takes a descriptor / returns a file | `vfs/src/file.rs` (`ioctl_fd_arg`, `device_ref`, `ioctl_ex`, `IoctlOut`), `sys_ioctl` in `kernel/src/process/syscall/fs.rs` |
| Mesa side | `mesa-port/overlay/.../nvkmd_constanos.c` (`import_dma_buf`, `export_dma_buf`, `import/export_opaque_fd`); rebuild with `mesa-port/build.sh` (incremental, ~20 s), programs in `probes/nvk/` (`vk_share.c`, `vk_draw.c`) |
| Tests | `userspace/c/nvgpu_{sw,hw}_test.c`, `probes/nvk/vk_share.c`, metal job `scripts/metal-jobs/gpu-multi.sh` |

## Rules (each cost a round)

- **One GPU lock, so nothing slow under it.** `HW` serialises every session's submit and fence query. RM calls (tens of ms) and bulk VRAM clears must run without it: create = `prepare_rt` (locked) / `run_rt` (unlocked) / `finish_rt` (locked), with the slot *reserved* (`rt_reserved`) in between; destroy is the same in steps. Anything new that talks to GSP-RM or loops over memory at run time follows that shape. Measured cost of breaking it: a presenter's frame pacing cut by 178 ms when three clients started. Look at `gpu_uapi:` (`create_us_max`, `prepare_us_max`, `destroy_us_max`, `lock_wait_us_max`) and `gpu_uapi_slow:` (latest holds over 2 ms, with the `CLOCK_MONOTONIC` time) before guessing.
- **Storage has holders.** Every arena/VRAM allocation counts: its BO, each exported descriptor, each imported BO. The last release discards system pages *before* returning the range (a late discard would eat a new owner's data). Never free a range directly; go through `release_storage`. `/proc/kdebug` `gpu_share:` must read 0 sessions / 0 storage_allocs / 0 syncs when everything is closed: a leak shows there.
- **A shared timeline lives in the kernel registry, not in a session.** Whoever reads it resolves completed fences (`gpu::uapi::fence_done` works from any session), so the signaller may be idle. Signals queued *before* the export move to the registry at `sync_share` time: a Vulkan program submits, then exports, and leaving them in the session meant an idle parent never completed them for its child (found on metal only). Lock order: `SYNCS` -> `HW`; `STORAGE` -> shm/`HW`; never the reverse.
- **Descriptors:** `BO_EXPORT`/`SYNC_EXPORT` return the fd as the ioctl's **result** (constanos_ioctl in Mesa drops results: use raw `ioctl` for these two). The fd is close-on-exec. A handle cannot reach the fd table (locked during the ioctl): `ioctl_fd_arg` names the descriptor, `sys_ioctl` looks it up and passes `device_ref`.
- **No isolation between processes yet** (one `PageTables`; a session's slice is enforced by its model, not by the GPU). Debt, stated in the docs. Channel cap: `MAX_RT` = 8 run-time GR channels for all sessions.
- In C tests, don't name a helper `bind` (clashes with `sys/socket.h`); a fork test that counts sessions must synchronise its children (open -> report -> wait for go), or a fast child releases its slot before the next opens.

## Testing recipe for a change here

1. `cd nvgpu && cargo test` (devmodel first; write the test, then sabotage: copy the file, mutate one line, expect a failure, restore; `scripts/gpu-mutate.py` style or a quick python loop).
2. Kernel side in QEMU with the software device: `touch build.rs; scripts/run-abi-suite.sh nvgpu_sw_test nvgpu_hw_test`, then the whole suite. Sabotage the adapter with `scripts/gpu-mutate-qemu.py nvgpu/mutations/sharing_kernel.py` (one test, list of `(file, name, old, new)`; it rebuilds each mutant and restores the sources). Survivors must be argued: equivalent mutant, or hardware-only (then a `nvgpu_hw_test` section on metal covers it).
3. **The software device completes every EXEC at once**: it cannot show an in-flight race (work queued, not finished, nobody calling in). Those need a section in `nvgpu_hw_test` and a metal round: `touch build.rs; echo 5 > target/metal/budget; scripts/metal-run.sh --kconf 'gpu=uapi' scripts/metal-jobs/gpu-multi.sh`, then read `target/metal/runs/<nonce>/boot.log` (`grep -a gpu-multi`; the job prints its whole summary at the end because the log wraps).
4. After staging a new Vulkan program: `strip -o disk-image-root/bin/<name> ~/src/gpu-ref/nvk-probe/<vk-...>`, **`touch build.rs`** and `cargo build` (else the image keeps the old binary), and check `dumpe2fs -h disk.img | grep Free` (each of these is 15 MB; `disk.img` is 288 MiB now).

## Layer 4 (GPU compositor): slices 1-2 done (2026-09-30), slice 3 (`vk_comp`) next

Design and slices: `docs/gpu/g5-graphics-stack-plan.md` "Capa 4". Slice 1 = the window manager in `gui` (`gui/src/compositor.rs`: `create_gpu_buffer`, `GpuOp`, `draw_list`, `gpu_frame_done`; `cd gui && cargo test`). Slice 2 = the client side:

| What | Where |
|---|---|
| Hooks a program gives the WSI (a copy lives at `src/vulkan/wsi/constanos_window.h` in the Mesa tree: `apply.sh` copies it) | `userspace/c/include/constanos_vk_window.h` |
| The program's connection + hooks over the wire | `userspace/c/include/constanos_gui_vk.h`, `constanos_gui_wire.h` (`guiw_create_gpu_buffer`, checked against Rust by `gui/tests/c_wire.rs`) |
| WSI windows (`wsi_headless_surface`, windowed swapchain, `wsi_constanos_window_*`) | `mesa-port/patches/0001-nvk-constanos.patch` (`wsi_common_headless.c`, `wsi_common.h`) and `nvkmd_constanos.c` (`nvk_constanos_surface_*`) |
| Test client / stand-in compositor | `probes/nvk/vk_window.c` (`vk-window`, staged as `disk-image-root/bin/vk_window`), `userspace/c/gui_fake_comp.c`; run `scripts/run-abi-suite.sh gui_fake_comp` |

Slice 3 (the compositor itself): `gui-capi/` (the `gui` window manager behind a C ABI, `gui_capi.h`; musl static lib built by `probes/nvk/build.py`), `probes/nvk/comp_render.h` + `comp.{vert,frag}` (the renderer; `probes/nvk/host-comp.sh` checks it pixel for pixel against a CPU reference on the host's Vulkan, no Ryzen), `vk-comp/` (Rust with std on musl: the program, `vk_comp [prog...]`, env `COMP_HEADLESS`, `COMP_NO_INPUT`, `COMP_SECONDS`, `COMP_SOCKET`; titles with `text`) + `probes/nvk/comp_vk.c` (the renderer's Vulkan bring-up, `cr_*` in `comp_api.h`); `gui-capi` is only for the host harness now, QEMU test `scripts/run-abi-suite.sh gui_comp_test` (vk_comp + vk_window), metal job `scripts/metal-jobs/gpu-comp.sh`. Stage `vk-comp` like the others (`strip -o disk-image-root/bin/vk_comp ...`, disk.img free space: each is 15 MB). `cargo build` warns (`disk-image-root/bin/<name> is older than ...`, root `build.rs` `warn_stale_nvk_programs`) when a staged NVK program is older than one of its inputs (its `.c`/shaders, `vk-comp/`, `gui`/`draw`/`text` for `vk_comp`, `nvgpu.h`, `mesa-port/overlay`): rebuild before syncing the stick. It goes by mtime, so a regenerated but unchanged header warns too (`touch` the binary once you have checked). One frame in flight (a frame waits for the previous one before uploading).

**Status, measurements and how to test: `docs/gpu/g5-layer4-handoff.md`** (read it before touching `vk-comp/` or `gpu-comp.sh`). Ryzen #182: 60.0 fps with two clients at the default `COMP_DELAY_MS` = 2.
- **Answer `frame` callbacks at repaint** (right after `cr_frame`, whose present already waited for the composition's GPU work), not when the flip lands: the clients' next frame then runs on the GPU while the compositor waits for the vblank, not during the next composition (that made its present take 5 ms and miss the vblank: 30 fps, #179-#181).
- The PRESENT must go out within ~9 ms of the previous flip being seen or it lands one vblank late (`COMP pace` prints the latest on-time and earliest late offsets); 4 and 6 ms delays still give 30 fps (#182, unexplained).
- **The compositor never blocks on starting a program**: `Command::spawn` waits for the child's exec (a 15 MB static binary, seconds with a cold block cache) and a client gives up after 5 s without `configure`. Programs start on a thread (`launch`), are reaped by pid (`reap_children`), and `COMP_EXIT_WHEN_IDLE` waits for all of them.

**Full account of the 2026-10-01 session (bugs, dead ends, mistakes, recipes): `docs/gpu/g5-layer4-session-report-2026-10-01.md`. The cursor does not move at 60 fps (open, section 8 there).** **Real apps in the compositor (2026-10-01; `docs/gpu/g5-layer4-handoff.md` §7):** `snake3d` with `SNAKE3D_WINDOW=1` is a window (keys over the wire, swapchain remade at every `GVK_RESIZE`; the compositor going away ends it quietly), `cpumon` (a CPU pool client) runs as is, **F11 is the compositor's** (`Compositor::toggle_fullscreen`; composition, not direct scanout yet), `vk_comp` ends the programs it started, `COMP_F11_AT` presses F11 for QEMU tests, and the metal job is `scripts/metal-jobs/gpu-apps.sh` (phases A-E, with its own time budget under the 300 s watchdog). A QEMU TLB-shootdown panic shows in `gui_comp_test` about one run in three: it is not yours (`scripts/tlb-stress.sh`, `docs/reference/cpu.md`).

Rules and traps:
- **A fragment shader that indexes an SSBO from `gl_FragCoord` must clamp into its rectangle** (helper lanes of 2x2 quads at an odd edge read index -1 = 16 GiB past: Xid 31 and a dead channel; the host GPU hides it). When a channel dies, read the `[nvgpu] channel .. is dead: ...; address A: <where>` line (`docs/reference/gpu.md`).
- **The program is the connection's only reader.** The swapchain never reads the socket: an acquire with every image held calls the `pump` hook until a `release` arrives (the program passes it on with `nvk_constanos_surface_buffer_released`). Two readers on one stream split messages.
- **A buffer is the compositor's from `commit` until its `release`**; the WSI never hands it out before. A present waits for the copy's fence *before* `commit` (the compositor reads without waiting).
- Buffer ids come from the program's counter and are never reused (Wayland's rule: an id is free only after `delete_id`).
- Edit the Mesa checkout, then `git -C ~/src/gpu-ref/mesa diff > mesa-port/patches/0001-nvk-constanos.patch` (new files go in the overlay or `apply.sh`), `ninja -C build-musl src/nouveau/vulkan/libnvk.a src/vulkan/wsi/libvulkan_wsi.a`, `python3 probes/nvk/build.py`, `strip -o disk-image-root/bin/<name> ~/src/gpu-ref/nvk-probe/<vk-name>` for **every** Vulkan program (they all link NVK: a stale one keeps the old WSI), `touch build.rs`, `cargo build`.
- QEMU proves the protocol and the bookkeeping only: VRAM is not CPU-mappable and the software device draws nothing, so pixels need the Ryzen.

## Layer 3 (WSI): direct path written and measured on the Ryzen #168 (60.1 fps, 0 refused flips)

Done (2026-09-30): the headless surface is the screen when the pdev has a display; `wsi_common_headless.c` + `nvk_wsi.c` + `wsi_common.h` in `mesa-port/patches/0001-nvk-constanos.patch` (edit the Mesa checkout, then `git -C ~/src/gpu-ref/mesa diff > .../0001-nvk-constanos.patch`; `apply.sh` copies the overlay). Hooks in `wsi_device.scanout`, backend in `nvkmd_constanos.c`. `snake3d` uses `VK_KHR_swapchain`. Test headless in QEMU: `PROBE=vk-snake PROBE_ENV='SNAKE3D_HEADLESS=1 SNAKE3D_AUTOPLAY=1 SNAKE3D_SECONDS=5' scripts/run-vk-probe.sh --no-build`; the scanout path only on metal (`gpu-snake.sh`). Details and known debt: `docs/reference/gpu.md` "WSI". Rebuild after a WSI edit: `ninja -C ~/src/gpu-ref/mesa/build-musl src/nouveau/vulkan/libnvk.a src/vulkan/wsi/libvulkan_wsi.a` then `python3 probes/nvk/build.py` (errors in the wsi lib are hidden by `mesa-port/build.sh`'s `ninja -k 0`). Original notes below (written before it existed).

## Layer 3 (WSI) starts here

Read `docs/gpu/g5-graphics-stack-plan.md` "Notas para la capa 3". Short version: Mesa's `src/vulkan/wsi/wsi_common_headless.c` is the model for a platform that has no window system; NVK's hook is `src/nouveau/vulkan/nvk_wsi.c`; today's presentation is our extension `nvk_constanos_present` in `nvkmd_constanos.c` (used by `vk_draw`'s `VK_DRAW_SCANOUT`), which the WSI platform replaces. The display scans out linear XRGB8888 (pitch 8192 at 1080p, `NVG_IOC_SCANOUT_INFO`) while NVK renders block-linear, so swapchain images need a copy/detile (`vkCmdCopyImageToBuffer`, as `vk_draw` does) or linear images.
