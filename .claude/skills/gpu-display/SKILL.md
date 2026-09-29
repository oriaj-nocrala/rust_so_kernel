---
name: gpu-display
description: Playbook for adding or changing display code in the NVIDIA GA106 driver (crate nvgpu + kernel/src/gpu/): a new gpu= level, a new /dev/dispctl command, a supervisor or channel change, porting a piece of nouveau, turning a mmiotrace into a replay fixture, or preparing a metal job. Read it before opening evo.rs/supervisor.rs/modeset.rs, it says where each thing is so those files need not be read whole. Keywords: gpu=, nvgpu, dispctl, supervisor, evo, head, SOR, window, fixture, ReplayMmio, nouveau, gpu-trace.py, phase 5.x.
---

# Display work on the GA106 (phase 5 of `docs/gpu/gpu-plan.md`)

State of the project: `docs/reference/gpu.md` (one section per `gpu=` level, current truth). Plan and per-phase results: `docs/gpu/gpu-plan.md` (long: read only "Resultados de la fase N" of the phase you touch and the row of the subphase table). Memory `gpu-plan-status` has the phase and the next step.

## The recipe (every subphase followed it)

1. **Oracle first.** Find what nouveau does in `~/src/gpu-ref/linux/drivers/gpu/drm/nouveau/` (table below), then find it in the trace (`~/constanos-gpu-oracle/trace-nogsp`, or `trace-nogsp-vrampush` for push-buffer methods).
2. **Fixture.** `python3 scripts/gpu-trace.py disp DIR T0 T1` lists labelled writes (use it to find the time window; filter with awk on the register column). `fixture DIR T0 T1 OUT [SKIP_OFFSETS..]` writes every access in a window as `R|W 0xOFFSET 0xVALUE` (drops PTIMER; pass other contexts' registers as SKIP; `extract` prints raw mmiotrace lines instead). `supers`/`train`/`push`/`core` write fixtures directly. Put it in `nvgpu/fixtures/`.
3. **Pure code in `nvgpu/src/<x>.rs`**, generic over `Mmio` (`rd32/wr32/mask/udelay`; `mask` always reads and writes). Every constant cites `file:line` of nouveau/`clc67*.h`. No logging, nothing blocks.
4. **Test by replay**: `ReplayMmio::from_extract(FIXTURE)` serves the trace's reads in order and records writes; assert `fmt(m.writes) == fmt(m.expected_writes)` (see `supervisor.rs` tests `round2_*`, `hdmi.rs` `enable_replays_the_trace`). Method lists (core/window pushes) are compared with `modeset-push.txt` rows (`hdmi.rs` `push_rows`). Known deviations from the trace (e.g. VPLL N/fN) are replaced in the expected side and commented.
5. **Sabotage**: mutate each constant/branch you added, `cargo test`, confirm a test fails (copy the file aside and restore). A survivor means a missing case: add it. (`kernel-testing` skill.)
6. **Kernel adapter `kernel/src/gpu/<x>.rs`**: owns statics, buffers, ordering, logging (`report()` = serial + `/proc/gpu` log), `render_kdebug()`, `status()`. Boot-time `setup(r, regs, ...)` (IF=0, before APs, reads only where possible) + runtime entry called from `dev_dispctl.rs`.
7. **Wire**: `hal/src/bootopts.rs` `GpuLevel` (enum, `parse`, test), `kernel/src/gpu/mod.rs` (`pub mod`, call in `probe_device` under `level >= ...`), `debug.rs` (kdebug line, bump the `{}` count), `dev_dispctl.rs`, `docs/reference/gpu.md`, a "Resultados" block in the plan, job `scripts/metal-jobs/gpu-<x>.sh`.
8. **Verify**: `cd nvgpu && cargo test`; `cd hal && cargo test`; `cd kernel && cargo build --target x86_64-unknown-none` (the root build does NOT compile them); `scripts/run-kernel-tests.sh` (QEMU has no GPU: the level must end in ENODEV/"not a GA106", never a crash). Only then metal (`metal-run` skill; `touch build.rs` first).

## Map of the code (so you do not read it whole)

`nvgpu/src/` (pure): `evo.rs` instance memory (`Ramht`, `Pramin`), channels (`Chan`, `CORE`, `window(n)`, `init_channel`, `Push::mthd`, `submit`, `kick`, `wind`, `flip`), `Faults`, `gate`; `supervisor.rs` (`Supervisor::service` per stage 1/2/3, `Config{owned,routes,clocks}`, `Event`, IEDT/OCFG lookup, DP packing, `route_get`); `init.rs` VBIOS script interpreter (add opcodes in the `match`, cite `init.c`); `mode.rs` (`Mode`, `methods(head, or)`, `cvt_rb2`, `select`); `pll.rs` (VPLL); `dp.rs` (link training); `hdmi.rs` (SOR power, route, encoder, AVI, `head_methods`, `window_methods`); `vblank.rs` (`HeadTiming`, `scan_position`, interrupt tree); `pattern.rs`; `display.rs`/`edid.rs`/`dcb.rs`/`aux.rs`/`i2c.rs`/`pad.rs` (probing).

`kernel/src/gpu/`: `mod.rs` (`Bar0`, `probe_device` sequence, statics `VBIOS`, `PROBES`); `evo.rs` (`bring_up`: RAMHT, core + window 0, `PushBuf`, `alloc_push`); `supervisor.rs` (`setup`, ISR `on_pending`, **`push_core(name, methods, may_attach)`** = the only writer of the core push buffer, `own_head`/`disown_head`, `supers_done`, `core_idle`, `regs()`, `Cmd`, `request`); `modeset.rs` (`set`: the model of a synchronous multi-step operation: `BUSY`, IF=1, bounded `wait`, restore on failure); `dplink.rs`; `scanout.rs` (window 0 flips, BAR1 WC mapping at boot); `vblank.rs` (MSI handler); `hdmi.rs` (second head).

Nouveau reference (`nvkm/engine/disp/` unless noted): `gv100.c` (channels, SOR state/HDMI ctrl/infoframes, supervisors `gv100_disp_super`), `tu102.c` (`tu102_disp_init`), `ga102.c` (SOR clock, DP links), `gm200.c` (route set/get, SCDC), `nv50.c` (supervisor stages, `nv50_sor_power`), `outp.c`/`uoutp.c` (acquire, HDMI method), `../../dispnv50/disp.c` (`nv50_sor_atomic_enable`, `nv50_hdmi_enable`), `dispnv50/headc57d.c`/`wndwc57e.c` (methods), `subdev/bios/init.c` (script opcodes), headers `clc67d.h`/`clc67e.h` (in `open-gpu-kernel-modules`, names of methods).

## A metal round (how phase 5.8 went, 4 rounds)

- `echo 1 > target/metal/budget; touch build.rs; scripts/metal-run.sh --kconf 'gpu=X' scripts/metal-jobs/gpu-X.sh`; the session dies with the reboot and is resumed by itself with the verdict (the user said: do not ask before each round). Budget is per round: set it to 1 again each time.
- **Read the saved log, not the verdict**: `target/metal/runs/<nonce>/boot.log`; the first lines of the job are lost (log wraps), so the job must repeat its summary at the end and the kernel's own `hdmi:`/`super:`/`dispctl:` lines (klog) are the evidence. Use the Read tool with an offset near the end (`grep -n` for the first `X: on:` line) if Bash is slow.
- **Put diagnostics in before the first metal round**, not after the first failure: on any failed step dump the channel's status/PUT/GET, the exception slot (`0x611020 + chid*12`: stat, data, code), `Faults`, ASSEMBLY vs what was pushed, the head's raster movement (`hdmi.rs` `window_diag`). Round 2 of 5.8 named the cause from that dump alone.
- Every failing round cost ~5 min; two ideas in one round beat one (the third round bundled notifier + ILUT + OLUT and worked).
- A failed step must leave the display recoverable (`off_after`): the ASUS on head 0 was never affected in any round.

## Where the project stands / what is next

Done and measured: fases 0-3, 5.0-5.8 (own display driver on head 0 at up to 180 Hz and on head 1 through HDMI). Not started: 5.7b (resolution change: window/framebuffer/console/compositor resize), a compositor that drives both heads (window 2 scans a static picture today; `/dev/fb1` + mmap or a second `Scanout` would be the way; see `docs/gui/gui-plan.md`), fase 4 (GSP, riskiest: firmware 570.144, `gsp-570.144.bin` 63 MB does not fit `disk.img`), fase 6 (channel + copy engine), fase 7 (3D stack decision, a document).

## Rules learned the hard way

- **Supervisors arrive by MSI on CPU 0.** Any runtime operation that waits for them runs with IF=1 (`modeset::set`, `hdmi::locked`); a syscall enters with IF=0.
- Supervisors are shared by all heads and must always release; a new head is "owned" (`own_head`) only while it works, else its bits are `Foreign` and released untouched.
- Core pushes only through `push_core` (PUSH_AT lock, refuses while the core is unfetched or a DP training runs). Window pushes: one channel, one PUT, wrap with `evo::wind`.
- Nothing per-frame goes through the vblank ISR except `vblank::service`; new ISR work states whether it is global (CPU 0) and takes no lock BAR0 users do not.
- With the SOR detached the head raises no vblank; a raster/clock change with the SOR attached runs 2.0/2.1/2.2 by itself.
- `map` of BAR1 (`memory::mmio::map`) only at boot; carve VRAM: GOP 0.., scanout 16/32 MiB, HDMI 48 MiB, instance memory near the top.
- The metal log wraps: repeat the summary at the end of the job; make jobs fail loudly with numbers, and leave "eyes' part" lines for the user.
- `Bar0` is not `Clone`; get it with `svr::regs()` at call time.
- A first UPDATE of a newly owned window must carry what nouveau pushes (notifier, ILUT, the head's OLUT) and is interlocked with the core; the core's interlock flags persist in ASSEMBLY, so the next core push must reset them (`push_core` does). Exception slots: `0x611020 + chid*12` = stat (type bits 14:12, method bits 11:0 <<2), data, code; type 5 = INVALID_STATE (`gv100_disp_exception`).
- Write for the next session: update `docs/reference/gpu.md` (state) and the plan's "Resultados" (evidence) in the same change; no history in `CLAUDE.md`.
