# Phase 6 oracle map (channel + copy engine through GSP-RM)

For the agent that starts phase 6 with a clean context. Everything below was located and measured on 2026-09-29; nothing here needs re-discovering, only re-running the commands. State of the code before this: `gpu=gsp` boots GSP-RM, and the kernel has its own RM client (root `0xc1d00000` -> device `0xde1d0000` -> subdevice `0x5d1d0000`, `nvgpu::rm`, `Rm` in `kernel/src/gpu/gsp.rs`). Procedure and traps: skill `gpu-gsp`. Goal (`docs/gpu/gpu-plan.md`, "Fase 6"): a GPFIFO channel with the Ampere copy engine, a fence, and a measured system -> VRAM copy (GB/s).

## The trace already has phase 6

`~/constanos-gpu-oracle/trace-gsp/dmesg.txt` (nouveau r570 with `debug=gsp=trace`) contains every RM RPC nouveau sent and received while it created the VA space, the channels and the copy engine. Tool: `scripts/gpu-rpc.py` (`list`, `recv`, `dump`, `text`; see its docstring). If the trace directory is gone, `scripts/gpu-oracle.sh` recreates it (needs the Linux boot entry, phase 0 of the plan).

```
python3 scripts/gpu-rpc.py list ~/constanos-gpu-oracle/trace-gsp 10.10 14      # one row per sent RPC, decoded, with its #index
python3 scripts/gpu-rpc.py dump ~/constanos-gpu-oracle/trace-gsp 38 nvgpu/fixtures/rm-ph6-ce-alloc-c7b5   # request + reply .bin
python3 scripts/gpu-rpc.py text ~/constanos-gpu-oracle/trace-gsp 13.47 13.49   # nouveau's own "cli:.. obj:.. new obj" debug lines
```
(dmesg time; mmiotrace is +0.1229 s. Payload = what follows the 32-byte RPC header. ALLOC header 32 B, CONTROL header 24 B: see `nvgpu/src/rm.rs`.)

## What the trace shows, in order (index = `gpu-rpc.py list` index)

| # | time | RPC | fixture (already in `nvgpu/fixtures/`) |
|---|---|---|---|
| 8-10 | 10.109 | ALLOC root/device/subdevice (client `0xc1d00000`) | `rm-{root,device,subdev}-*` (done in 4g) |
| 11 | 10.1103 | ALLOC `FERMI_VASPACE_A` `0x90f1`, obj `0x90f10000`, parent = device, 48 B params | `rm-vaspace-*` |
| 12 | 10.1107 | CONTROL `0x00801813` on the device, 32 B (nouveau's header has no name for it) | `rm-ctrl801813-*` |
| 16-19 | 10.279 | a second client `0xc1d00001` repeats root/device/subdevice/vaspace (nouveau's fifo/vmm path) | `rm-ph6-vaspace2-*` |
| 20 | 10.2944 | CONTROL `0x90f10106` = `NV90F1_CTRL_CMD_VASPACE_COPY_SERVER_RESERVED_PDES` on the vaspace, 184 B | `rm-ph6-vaspace-ctrl-90f10106-*` |
| 21 | 10.2950 | ALLOC GPFIFO channel class `0xc56f` (Ampere), obj `0xf1f00000`, 400 B (`NV_CHANNEL_ALLOC_PARAMS`, r570 layout) | `rm-ph6-chan-c56f-first-*` |
| 32-34 | 13.475 | client `0xc1d00000` again: ALLOC channel `0xc56f` obj `0xf1f00001`; CONTROL `0xa06f0104` (`NVA06F_CTRL_CMD_BIND`, 4 B) and `0xa06f0103` (`NVA06F_CTRL_CMD_GPFIFO_SCHEDULE`, 2 B) | `rm-ph6-chan-c56f-a-*`, `rm-ph6-chan-ctrl-a06f0104-*`, `rm-ph6-chan-ctrl-a06f0103-*` |
| 35-38 | 13.484-13.486 | a second channel `0xf1f00002` (same three RPCs) and, as its child, **ALLOC `0xc7b5` (`AMPERE_DMA_COPY_B`, the copy engine object)**, obj `0x0004c7b5`, 40 B | `rm-ph6-chan-c56f-ce-*`, `rm-ph6-ce-chan-ctrl-*`, `rm-ph6-ce-alloc-c7b5-*` |

Not in the trace as RPCs: the memory the channel uses (instance/USERD/RAMFC/GPFIFO buffers) — they are passed inside the channel ALLOC params as physical addresses (`NV_MEMORY_DESC_PARAMS instanceMem/userdMem/ramfcMem/mthdbufMem`, `gpFifoOffset`, `hVASpace`, `hUserdMemory`), so decode the 400-byte fixtures against `r570/nvrm/fifo.h` (`NV_CHANNEL_ALLOC_PARAMS`, line 24 ff) with `clang` like `nvgpu/gen/sysinfo.c` does, and compare with what `r570_chan_alloc` fills.

The many `CONTROL ... client=0xc2000006 obj=0xabcd2080` rows are nouveau talking to GSP-RM's *internal* client (handles from the static-info reply, `nvgpu/gen/staticinfo.c`), not ours.

## nouveau reading order (Linux v7.2.2, `~/src/gpu-ref/linux/drivers/gpu/drm/nouveau/nvkm/subdev/gsp/rm/`)

1. `r570/fifo.c` (217 lines): `r570_chan_alloc` = how the 400-byte channel params are filled; `r570_fifo_ectx_size`, engine-type translation.
2. `r535/fifo.c` (617): `r535_chan_alloc` (r535 variant), `r535_chan_ramfc_write` (BIND + GPFIFO_SCHEDULE controls, lines ~154-235), `r535_fifo_runl_ctor` (runlists, engines: how CE engine types are found), doorbell handle (`r535_chan_doorbell_handle`).
3. `r535/vmm.c` (191): VA space object (`NV_VASPACE_ALLOCATION_PARAMETERS`), page directory calls (`NV0080_CTRL_DMA_SET/UNSET_PAGE_DIRECTORY`), `r535_mmu_vaspace_new`.
4. `r535/ce.c` (46) and `r535/bar.c` (202): copy-engine object allocation; BAR1/BAR2 PDE setup from `bar1PdeBase/bar2PdeBase` of the static info.
5. Headers with the structs: `r570/nvrm/fifo.h`, `r535/nvrm/{alloc,ctrl,device}.h`. Class ids: `nouveau/include/nvif/class.h`. Command names by hex: `grep -rhoiE "#define +[A-Za-z0-9_]+ +\(?0x0*<cmd>" r535/nvrm r570/nvrm`.

## What has to be decided/built (in this order)

1. **Servicing the queues with GSP-RM alive.** Today only the boot reads the status queue. Events arrive at any time (`POST_EVENT`, `OS_ERROR_LOG`, `RC_TRIGGERED`, ...). Options: poll inside every `Rm::call` (simple, enough for a first copy test), or a kernel thread / the MSI handler. Choose, write it in the skill.
2. **VA space + page directory**: use the *external* mode (`ALLOC 0x90f1` with flags `0x8` + `SET_PAGE_DIRECTORY` `0x801813` with our root's VRAM address, trace #11/#12); the page tables are built by *us*. Everything known about the format (levels, PTE/PDE bits, apertures, TLB flush register, free VRAM range) is in **`docs/gpu/mmu-v3-notes.md`**; the module is `nvgpu/src/mmu.rs` (largest new pure module, tested with an independent walker).
3. **Channel**: instance/RAMFC/USERD/GPFIFO buffers, channel ALLOC (`0xc56f`), BIND, GPFIFO_SCHEDULE, then the copy-engine object `0xc7b5` under the channel.
4. **Work submission**: GPFIFO entries + pushbuffer with the copy-class methods (`clc7b5.h` in `~/src/gpu-ref/open-gpu-kernel-modules/src/common/sdk/nvidia/inc/class/`), USERD `GP_PUT`, doorbell (`NV_VIRTUAL_FUNCTION_PRIV_DOORBELL`, work-submit token from the channel's control), semaphore release as a fence.
5. **Job**: copy N MiB system -> VRAM, verify, print GB/s (`scripts/metal-jobs/gpu-copy.sh`, level `gpu=copy`); then the compositor uses it for `fb_flush` (numbers in `docs/gui/perf-plan.md`).

## Loose ends from phase 4 that phase 6 touches

- `gsp.rs` `mem::forget`s every GSP buffer and keeps no teardown path (`FREE` of the objects, `booter_unload` on SEC2): fine while the kernel never exits GSP mode, needed before any shutdown/reload story.
- `Rm` is created inside `rpc_phase` and dropped at the end of boot: make it a long-lived global (behind a lock) before runtime RPCs; the queues' shared memory (`Memory.shm`) and `Queues` counters live in `Memory`, which is `mem::forget`-ed at the end of `boot_gsp`.
- A hot reboot into constanos after a GSP boot works (five rounds, boots #95-#99): the platform reset clears WPR2 and the GSP.
