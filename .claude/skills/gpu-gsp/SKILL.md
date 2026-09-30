---
name: gpu-gsp
description: Playbook for GSP-RM work on the NVIDIA GA106 (phases 4, 6a-6d done): the falcon / FWSEC / booter / WPR2 / RPC queue code (nvgpu + kernel/src/gpu/gsp.rs), the RM object RPCs, and phase 6 - GPU page tables (nvgpu::mmu), the GPFIFO channel and copy engine (nvgpu::chan, gpu=vaspace, gpu=copy, kernel/src/gpu/{vaspace,copy}.rs), the VRAM/VA memory map, the bring-up ladder, doorbell and token, where the NVIDIA hardware manuals are, the mutation-testing tool and the metal-run protocol. Read it before touching those modules, gpu=fwsec / gsp / vaspace / copy, adding an RM client or object, or debugging a channel that does nothing. Keywords: GSP, RM_ALLOC, VASpace, GPFIFO, doorbell, runlist, USERD, PDE, PTE, copy engine, c7b5, c56f, open-gpu-doc.
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

## Phase 6: VA space, channel, copy engine (`gpu=vaspace` boot #101, `gpu=copy` boots #105-#121)

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

## Phase 6d, done (numbers and decisions: `docs/gpu/gpu-plan.md` "Resultados de la fase 6d")

Code map additions: `kernel/src/gpu/bench.rs` (measurements after the ladder), `kernel/src/gpu/intr.rs` (interrupt sources found by leaf-status diff), run-time channel in `copy.rs` (`install`, `selftest_irq`, `selftest_fault`), `gsp.rs` `Runtime` (`with_rm`, `poll_events`, `restore_bar1`), `nvgpu::{mmu::map_huge, chan::{copy_rect_push, with_interrupt}, vblank::{arm_with, service_with}}`. `/dev/dispctl` commands (need `gpu=copy`): `copy irq <runs>`, `copy fault` (kills the channel: run last), `gsp name`, `gsp poll`. Job: `scripts/metal-jobs/gpu-copy.sh` runs all of it; `gpu_bench:`, `gpu_copyirq:`, `gpu_copyfault:`, `gpu_gsprt:`, `gpu_intr:` in /proc/kdebug.

**Rules learned (each cost metal rounds):**
- **GSP-RM's boot rewrites `NV_PBUS_BAR1_BLOCK` (`0x1704`)**: BAR1 stops mapping VRAM (stores lost, reads `0xbad0ac..`/`0xffffffff`). `gsp::restore_bar1` puts the firmware's value back; anything that writes VRAM through BAR1 after the GSP boot (the compositor's `fb_flush`, scanout buffers) depends on it. If a BAR1 access "works" but nothing shows, read `0x1704`.
- **A verification that can pass on data left there earlier proves nothing**: scribble the destination first (through PRAMIN, which always works), then copy, then read back through a *different* path. Boots #110-#117 measured a dead BAR1 as if it worked.
- The link (PCIe 8 GT/s x8, ~6.2 GB/s useful) is the ceiling for both the CPU's WC stores and the copy engine; 2 MiB pages and bigger copies do not help. Measure the link before optimising the copy.
- Interrupts: the CE's completion interrupt is a VFN leaf bit (vector 7 for CE2); find a vector by clearing every leaf status, doing the work and diffing (`intr::discover`), the status latches even when the source is not allowed. GSP-RM queues replies and events **without any CPU interrupt**: serve its status queue by polling.
- RC: a channel fault makes RM reset the channel and send `MMU_FAULT_QUEUED` + `POST_NOCAT_RECORD` x5 + `RC_TRIGGERED` (r570: `POST_NOCAT_RECORD` = 0x1020); RM keeps answering. The channel must be recreated to be used again (not done: nothing needs it yet).
- Run-time ring writes go through BAR1 (write-only view; no shared PRAMIN window); the doorbell is a BAR0 write; the fence is a host-memory page.

## Next (what is left of phase 6)

1. Only if the compositor is to use the copy engine (decision in the plan: not now): map the shadow and the scanout buffers (VRAM 16/32 MiB) in the VA space at boot (`Buffers::map` shows how; `map_huge_range` for 2 MiB-aligned contiguous ranges), a system-wide submit lock, a CPU fallback, recreating the channel after an RC, `Framebuffer::copy_out` calling `chan::copy_rect_push`.
2. Recreate a channel after RC (dispatch `RC_TRIGGERED` from `poll_events`).
3. Phase 7 of the plan (see `docs/gpu/gpu-plan.md`).

## Phase 7a: GR context and compute class (`gpu=compute`), done on the Ryzen #136 (details: `docs/gpu/gpu-plan.md` "Resultados de la fase 7a")

Code: `nvgpu::gr` (context buffers from `GET_CONTEXT_BUFFERS_INFO`, `entries`/`promote_params`, `plan`/`mappings`, compute pushes), `nvgpu::mmu::map_big` (64 KiB pages), `kernel/src/gpu/compute.rs` (`prepare` before the tables, `run` after `copy`; the golden context `golden`/`try_golden`/`golden_tables`, then the GR channel and three rungs), `Rm::free`, `gsp::take_events`. Job `gpu-compute.sh`. Oracle: `nvgpu/fixtures/rm-ph7-gr-*` (trace RPCs #21-#25).

Rules learned:
- **The golden channel needs an RM-managed VA space over our own tables** (`FERMI_VASPACE_A` without `EXTERNALLY_OWNED`, then `COPY_SERVER_RESERVED_PDES` naming the root, PD2 and PD1 on VA 0's path: `PageTables::directories(0)`), with the context buffers already mapped there (trace VAs, `gr::trace_placement`). In our externally owned space the golden `PROMOTE_CTX` refuses (31) any MAIN entry with a VA (#126-#134); handing RM zeroed directory pages (#128) fails the same way, since nothing is mapped. The normal GR channel's promote in the external space is accepted.
- A push buffer of 4 KiB cannot carry 4 KiB of inline data: split writes (`gr::INLINE_CHUNK_WORDS`).
- A failed promote can leave GSP-RM unresponsive for the rest of the boot ("nothing from GSP-RM in 10000 ms"): put the most informative attempt first, keep the number of channel alloc/free cycles small.
- An unmapped VA in a MAIN entry makes RM walk it: MMU fault + RC (channel error 87) events.
- GSP-RM's `POST_NOCAT_RECORD` about `GFW_BOOT_PROGRESS` is noise; the firmware has no log ELF, so its logs are undecodable here. When RM answers 31 with no message, compare the call's *context* with nouveau's (what the objects point to in memory), not only the bytes of the RPC.
- r570 `ENGINE_ID_COUNT` is 0x1a (26 buffers per engine), the r535 header in nouveau's tree says 0x19.

## Phase 7b: a compute shader (`gpu=compute`, rungs 4-6), done on the Ryzen #139 (details: `docs/gpu/gpu-plan.md` "Resultados de la fase 7b", `docs/reference/gpu.md`)

Code: `nvgpu::qmd` (QMD V03_00 `build`, `sm_config`, the shaders' constants), `nvgpu::gr::{dispatch_push, l2_flush_push, KERN_*}`, `compute.rs` (`launch`, `check_output`, `l2_flush`). Oracles: `nvgpu/gen/qmd.c` (words + `MW` ranges from `clc6c0qmd.h`; commands in its header), SASS from the host's `/opt/cuda/bin/{nvcc,nvdisasm}` (`nvgpu/gen/shader/`, `extract.py`).

Rules learned:
- To add a shader: write the `.cu`, `nvcc -arch=sm_86 -cubin`, `extract.py cubin kernel OUT.bin` (prints registers and constant-bank size), `include_bytes!` it, pin its instructions in a test (`nvdisasm -c -hex`). Parameters come from `c[0x0][0x160]` (CUDA ABI); a QMD needs cbuf 0 of at least `.nv.constant0` bytes.
- A launch is: `SET_OBJECT`, memory windows, `INVALIDATE_SKED_CACHES`, `SEND_PCAS_A`, `SEND_SIGNALING_PCAS2_B`; wait with `WAIT_FOR_IDLE` + a semaphore, or with the QMD's own `RELEASE0`. It worked the first time it ran (#137): no `SET_SHADER_LOCAL_MEMORY`, no SPA version, when the shader uses no local memory.
- **Verify output with a scribbled destination and read it by another path.** For host memory the CPU is that path. For VRAM, PRAMIN does **not** show the SM's stores (open finding: not after waits, MMIO or `MEM_OP` L2 flushes, or `STG.E.STRONG.SYS`), so read VRAM with a `copy` shader into a host page.
- BAR1 reads of VRAM fail after GSP-RM (write-only view): not an alternative CPU read path.
- Metal budget: three boots (#137-#139) were spent on a 5-line question (the PRAMIN view); when a check fails, add every independent diagnostic to the same boot before relaunching.
