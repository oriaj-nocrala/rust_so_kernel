---
name: gpu-gsp
description: Playbook for GSP-RM work on the NVIDIA GA106 (phases 4, 6a-6c done): the falcon / FWSEC / booter / WPR2 / RPC queue code (nvgpu + kernel/src/gpu/gsp.rs), the RM object RPCs, and phase 6 - GPU page tables (nvgpu::mmu), the GPFIFO channel and copy engine (nvgpu::chan, gpu=vaspace, gpu=copy, kernel/src/gpu/{vaspace,copy}.rs), the VRAM/VA memory map, the bring-up ladder, doorbell and token, where the NVIDIA hardware manuals are, the mutation-testing tool and the metal-run protocol. Read it before touching those modules, gpu=fwsec / gsp / vaspace / copy, adding an RM client or object, or debugging a channel that does nothing. Keywords: GSP, RM_ALLOC, VASpace, GPFIFO, doorbell, runlist, USERD, PDE, PTE, copy engine, c7b5, c56f, open-gpu-doc.
---

# GSP-RM work on the GA106 (phase 4: `gpu=fwsec` / `gpu=gsp`; phase 6: `gpu=vaspace` / `gpu=copy`)

State: `docs/reference/gpu.md` sections "`gpu=fwsec`", "`gpu=gsp`", "GSP firmware files". Evidence per subphase: `docs/gpu/gpu-plan.md` "Resultados de las fases 4b / 4a y 4c / 4d y 4e / 4f". Project skills: `gpu-display` (same recipe, display side), `metal-run`, `kernel-testing`.

## What runs at boot with `gpu=gsp` (all measured on the Ryzen, boots #93-#99)

`gsp.rs::setup`: read + validate the 3 firmware files (`gsp-570.144.bin` lives **only on the stick**, `scripts/sync-usb-data.sh` puts it there) -> build memory (`gspmem`) -> queue `SET_SYSTEM_INFO` + `SET_REGISTRY` -> FWSEC-FRTS (`fwsec`) -> `gsp_reset` + LibOS mailboxes -> booter on SEC2 (`booter`) -> RISC-V check -> read the status queue: run `RUN_CPU_SEQUENCER` commands on the host, wait `INIT_DONE` (~1.24 s) -> `GET_GSP_STATIC_INFO` -> name "NVIDIA GeForce RTX 3050". Everything with IF=0, before the APs, before vblank is armed. GSP-RM is left running with its DMA buffers (`mem::forget`). Nothing polls the queues afterwards yet.

## Code map (pure = host tests, no I/O)

| Module | What |
|---|---|
| `nvgpu/src/falcon.rs` | `Falcon{base,addr2}`; `GSP` = 0x110000. reset/select/enable/disable, `dma_wr`, `load`, `boot`, `gsp_reset`, `riscv_active`. `tests::Sim` models a falcon (scrub, DMA done, halt) |
| `firmware.rs` | `Booter::parse` (`ga102_gsp_booter_ctor`), `Bootloader::parse` (RM_RISCV_UCODE_DESC), `GspImage::parse` (ELF sections), fuse signature index (**booters: `ga100_flcn_fw_signature` = version, FWSEC: `ga102_gsp_fwsec_signature` = mask**) |
| `fwsec.rs` | VBIOS PMU table -> descriptor v3 -> image + signature + DMEM interface patch; `prepare`/`execute`; `vga_workspace`, `frts_region` |
| `gspmem.rs` | `fb_layout` (WPR2), `WprMeta`, `radix3`, `libos_args`, `log_buffer_init`, `shared_memory`, `rm_args` |
| `booter.rs` | SEC2 = `Falcon{0x840000, 0x1000}`; `prepare`/`execute`/`gsp_running` |
| `rpc.rs` | `Queues` over `Shm` (send/recv), `build_message`, `SystemInfo`, `registry`, sequencer decode/run, `gpu_name` |
| `kernel/src/gpu/gsp.rs` | adapter: `Firmware`, `build_memory`, `boot_gsp`, `rpc_phase`, `wait_for` (the receive loop), `ShmBuf` |
| `nvgpu/gen/*.c` | clang oracles for C struct sizes/offsets (run commands in `nvgpu/gen/README.md`) |

## How each piece was verified (repeat this for anything new)

1. **Read nouveau first** (`~/src/gpu-ref/linux/drivers/gpu/drm/nouveau/nvkm/{falcon,subdev/gsp,subdev/gsp/rm/r535,r570}`), r570 for the 570.144 firmware. Cite `file:line`.
2. **Confirm the variant against the trace** (`~/constanos-gpu-oracle/trace-gsp`): I once ported `ga102_gsp_fwsec_signature` where the booter uses `ga100_flcn_fw_signature`; the trace read (`0x824148 = 1`) exposed it.
3. **Fixtures**: `python3 scripts/gpu-trace.py fixture trace-gsp T0 T1 OUT` (window in dmesg seconds; the mmiotrace clock is +0.1229 s). Drop *timed* poll reads (a poll's read count depends on speed) and serve them from a model / `fallback`; keep polls whose read count is data-driven. RPC payloads are in `trace-gsp/dmesg.txt` (`debug=gsp=trace`): `rpc fn:N len:...` + `rpc: 0000000: ..` hexdumps (sent) and `gsp:msg fn:N` + `msg: ...` (received). Extract with a small python regex into `nvgpu/fixtures/*.bin` (see `rpc72.bin`, `rpc65reply.bin`). The `seq ...` dmesg lines are nouveau's own command trace.
4. **Replay test**: `ReplayMmio::from_extract(FIXTURE)`, run the whole sequence, compare *all writes in order* and `unread(reg) == 0`. Compare line by line, never `assert_eq!` on a 30 KiB string (it floods the context).
5. **C oracle**: sizes/offsets from `clang` on the real headers, asserted in tests (`nvgpu/gen`). nouveau's `nvrm/nvtypes.h` needs `u8/u16/u32/u64` typedefs (see `sysinfo.c`).
6. **Sabotage** every constant and branch: copy the file, mutate one thing, `cargo test`, restore. Use `subprocess.run(..., stdin=DEVNULL, timeout=240)`; a mutation that loops forever shows as TIMEOUT (counts as detected). Wait on a marker in the log, **never `pgrep -f <name>`** (it matches the waiting command itself), never `grep DONE` when a source line contains DONE. Real gaps found this way: constants without assertions, values equal on this board (`hdr == len == 6`, phys bases 0) — patch a copy of the real VBIOS/firmware in the test to make them differ; equivalent mutants (an alignment already implied) are documented, not chased. The user's rule: close every gap.
7. **Metal**: `touch build.rs` (root build does not watch nvgpu), `echo 5 > target/metal/budget`, `scripts/metal-run.sh --kconf 'gpu=gsp' scripts/metal-jobs/gpu-gsp.sh`. `--no-reboot` prepares/deploys without rebooting (then `--abort` and relaunch, the kernel hash skips the redeploy). The session resumes by itself with the verdict; **read `target/metal/runs/<nonce>/boot.log`**, `grep '^\[fb\] gsp:'`. A round is ~2 min. Linux boots after each round with `nvidia` healthy (the reboot clears WPR2 / GSP state).

## Traps

- Loops with IF=0 must be bounded (`Mmio::udelay`), never unbounded in the kernel; a hung boot is reset by the 300 s watchdog.
- `Bar0` asserts on out-of-range offsets: validate RM-supplied register addresses before executing (`validate_sequencer`).
- `cargo build` in `kernel/` writes to the **root** `target/`, not `kernel/target/`.
- FWSEC-FRTS fails if WPR2 is already set (GPU not power-cycled): `setup` stops with a message.
- `heap_size_min` (170 "MB" vs bytes) never applies in nouveau; kept.
- Messages: cmdq/msgq entries are 4 KiB pages, 63 each, one kept free; the doorbell is any write to GSP `0xc00`; message checksum = XOR of u64 words folded to 32 bits.

## Done in 4g (`nvgpu/src/rm.rs`, `Rm` in `gsp.rs`)

Our own RM client exists after boot: `create_client` = `NV01_ROOT` (`0xc1d00000`) -> `NV01_DEVICE_0` (`0xde1d0000`) -> `NV20_SUBDEVICE_0` (`0x5d1d0000`) + `NV2080_CTRL_CMD_GPU_GET_NAME_STRING`. RPC payloads: `GSP_RM_ALLOC` fn 103 (32-byte header), `GSP_RM_CONTROL` fn 76 (24-byte header), `FREE` fn 10; a reply echoes the request with `status` filled (`0x55/0x66` busy, `0x51` no memory). Fixtures `rm-*-{req,rep}.bin` (extract sent/received RPCs from the trace dmesg as in step 3 above). `Rm::call` = send + `wait_for` (the receive loop that also runs sequencer events). The objects and buffers are never freed; the queues are only serviced at boot.

## Phase 6: VA space, channel, copy engine (`gpu=vaspace` boot #101, `gpu=copy` boot #105)

Runs inside the boot's RPC phase (IF=0, `Rm::call` polls the status queue); no long-lived `Rm` yet. State and evidence: `docs/gpu/gpu-plan.md` "Resultados de las fases 6a/6b/6c"; module summary: `docs/reference/gpu.md`.

| Module | What |
|---|---|
| `nvgpu/src/mmu.rs` | `PageTables` (5 levels, 4 KiB pages; `map`/`map_range`/`unmap`/`translate`, `images()` for the adapter), `pte`/`pde` encodings |
| `nvgpu/src/chan.rs` | channel ALLOC params (368 B), BIND/SCHEDULE/copy-object params, `runlist_for_engine`, `doorbell_token`, `gp_entry`, `incr_header`, `copy_push`/`release_push`, the VRAM/VA layout constants |
| `nvgpu/src/rm.rs` | + `vaspace_params`, `vaspace_from_reply`, `set_page_directory_params` |
| `kernel/src/gpu/vaspace.rs` | `gpu=vaspace`: ALLOC 0x90f1, tables through PRAMIN (read back), SET_PAGE_DIRECTORY; calls `copy::{prepare,map,run}` when `gpu=copy` |
| `kernel/src/gpu/copy.rs` | `gpu=copy`: buffers, channel, doorbell, the ladder, the measured round trip |

**Memory map** (constants in `nvgpu::chan` and `vaspace.rs`; a test checks they do not overlap). VRAM: 0 GOP fb; 16/32 MiB scanout; 48 MiB HDMI; **64 MiB page tables** (pool of 64 x 4 KiB); 96 MiB 6b test mapping (1 MiB); **128 MiB channel** (instance +0, USERD +0x1000, GPFIFO +0x2000, push buffer +0x4000, fence +0x5000); **144 MiB copy destination**; `0x1f4000000..` GSP heap + WPR2; `0x1ffc90000` display instance memory. VA (all mapped by our tables): 4 GiB 6b test; `0x2_0000_0000` GPFIFO, `+0x10000` push, `+0x20000` fence; `0x2_1000_0000` dst; `0x2_2000_0000` src (system pages); `0x2_3000_0000` back (system pages).

**Bring-up ladder** (`copy.rs::run`): each rung adds one layer and the first that fails is named: (1) bare semaphore release, (2) VRAM -> VRAM 4 KiB, (3) system -> VRAM 4 KiB, (4) the 4 MiB round trip. Keep the ladder when changing anything: three bugs were stacked and each hid the next.
- The **only proof** that the GPU ran something is the semaphore the copy engine releases. `USERD` `GP_GET`/`Get` are written back **periodically** (`dev_ram.ref`, "RAMUSERD is updated at regular intervals"): a stale value proves nothing.
- After a failure RM sends **no event**, and PFIFO registers (`0x2100`, `0xb65000`) read `0xbadf....` under GSP-RM: there is no host-side view of the PBDMA. Diagnose by rungs, not by registers.

**Rules learned (each one cost a metal round):**
- PD0 is a 16-byte dual PDE: **big-page half first, small-page half second** (`PD0_SMALL = 8`). I once inferred it backwards from nouveau's `pt[0]`; `dev_mmu.ref` and `nvkm_vmm_ref_hwpt` (`type = desc->type == SPT`) settle it.
- Doorbell = BAR0 `0xb80000 + 0x30000 + 0x90`, value `(runlist << 16) | chid` (`NV_VIRTUAL_FUNCTION_DOORBELL`; Volta's `0x810090` does nothing). GSP's own `GET_WORK_SUBMIT_TOKEN` reply is for runlist 0 (CPU-RM recomputes it): take the runlist from `NV2080_CTRL_CMD_FIFO_GET_DEVICE_INFO_TABLE` on **our own subdevice** (works), engine `0xb` = COPY2 = runlist 1 (COPY0 is 9, CE0/CE1 share runlist 0 with GR).
- The requested chid is the hardware chid (the USERD slot flags say so); the `cid` in the ALLOC reply is a session counter.
- Board facts come from the trace fixtures (`nvgpu/fixtures/rm-ph6-*`); the two channel ALLOCs of nouveau are reproduced byte for byte by `chan::alloc_params`.

**Sources, in order of trust** (the user's rule: do not invent, cite): NVIDIA's hardware manuals saved at `~/src/gpu-ref/open-gpu-doc` (commit in `~/src/gpu-ref/PINNED`; `grep` them: `manuals/ampere/ga100/dev_{ram,pbdma,runlist,ctrl,vm}.ref.txt`, `manuals/turing/tu104/dev_mmu.ref.txt`, `manuals/ampere/ga102/dev_ce.ref.txt`, `classes/dma-copy/clc7b5.h`, `ampere/host/ampere_interrupt_map.csv`), then OpenRM 570.144 (`~/src/gpu-ref/open-gpu-kernel-modules`, `src/nvidia/src/kernel/gpu/fifo/`), then nouveau r570. **nova-core** (the current Rust driver) only boots the GSP in v7.2.2; FIFO, MMU and VFN IRQ are still TODO upstream, so it is no reference for this phase.

**Metal practicalities:**
- `cat target/metal/budget` before a round; each failed round consumes one automatic resume; at 0 stop and report (the user may raise it: `echo 5 > target/metal/budget`).
- The reboot ends the session mid-command: after the resume read `target/metal/runs/<nonce>/{verdict,boot.log}` (`grep -a '\] copy:'`), do not relaunch.
- A metal reboot can empty `/tmp` (the session scratchpad): keep helper scripts in the repo (`scripts/gpu-mutate.py`, examples in `nvgpu/mutations/`).
- Mutation testing: `scripts/gpu-mutate.py FILE FILTER MUTATIONS.py`; write a fresh list per change.

## Next: 6d (what is left of phase 6; details in the plan)

1. Stability: 5 consecutive `gpu=copy` boots (`echo 5 > target/metal/budget`; job `scripts/metal-jobs/gpu-copy.sh`).
2. Long-lived `Rm` + servicing the status queue at run time (`Memory` is `mem::forget`-ed at the end of `boot_gsp`; events such as RC/MMU faults arrive at any time).
3. Fence by interrupt instead of polling: read `ampere_interrupt_map.csv`, nouveau's `r535_engn_nonstall` / `tu102_vfn_intr`, and `LAUNCH_DMA` `INTERRUPT_TYPE` in `clc7b5.h` first.
4. Map the scanout buffers into the VA space (`Target::Vram`) and copy the compositor's shadow buffer into them; 2 MiB pages need a PTE at PD0: check `NV_MMU_VER2_DUAL_PDE_IS_PTE` in `dev_mmu.ref.txt` before writing it.
5. Measure against the CPU write-combined blit (5.6 GB/s, `docs/gui/perf-plan.md`); the round trip measured 6.1-6.4 GB/s with 4 KiB pages and 4 MiB copies, so integrate into the compositor only if it wins or frees the CPU.
