---
name: gpu-gsp
description: Playbook for GSP work on the NVIDIA GA106 (phase 4 of docs/gpu/gpu-plan.md, done; 4g and phase 6 next): the falcon / FWSEC / booter / WPR2 memory / RPC queue code in crates nvgpu + kernel/src/gpu/gsp.rs, how each piece was verified against nouveau's trace and clang, the RM object RPCs (RM_ALLOC, RM_CONTROL) still to port, the mutation-testing routine, and the metal-run stability protocol. Read it before touching nvgpu::{falcon,fwsec,firmware,gspmem,booter,rpc}, gpu=fwsec / gpu=gsp, or adding an RM client.
---

# GSP-RM work on the GA106 (phase 4, `gpu=fwsec` / `gpu=gsp`)

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

## Next: 4g (RM objects) and phase 6

**4g** = an RM client: `NV01_ROOT` (client) -> `NV01_DEVICE_0` -> `NV20_SUBDEVICE_0`, via the two RPCs nouveau uses after `INIT_DONE`:
- `GSP_RM_ALLOC` (fn **103**, payload `rpc_gsp_rm_alloc_v03_00`: hClient, hParent, hObject, hClass, status, paramsSize, params) — `rm/r535/alloc.c`, `client.c` (root alloc), `device.c`;
- `GSP_RM_CONTROL` (fn **76**, `rpc_gsp_rm_control_v03_00`: hClient, hObject, cmd, status, paramsSize, params) — `rm/r535/ctrl.c`; `RM_FREE` fn 10.
- Oracle: `trace-gsp/dmesg.txt` right after `GET_GSP_STATIC_INFO` (10.088-10.115 s): `rpc fn:76 len:0x3c`, `fn:103 len:0xb8/0x78/0x44/0x70` and the `gsp:msg fn:...` replies (extract as fixtures the same way). The static info reply already carries `hInternalClient/Device/Subdevice` (offsets 1600/1604/1608, clang: `nvgpu/gen/staticinfo.c`) and `bar1PdeBase/bar2PdeBase`.
- Structure: pure builders/decoders in a new `nvgpu/src/rm.rs` (alloc/control/free messages + status -> errno mapping `r535_rpc_status_to_errno`), reply matching by function in the receive loop (the loop is `wait_for` in `gsp.rs`; move the queue+sequencer handling into a reusable `Rm` struct in the kernel adapter so later phases can send RPCs after boot, from a syscall/driver, not only at boot).
- "Done when": an `RM_CONTROL` on the subdevice (e.g. `NV2080_CTRL_CMD_GPU_GET_NAME_STRING`) answered by RM, in the metal job.

**Phase 6** (`docs/gpu/gpu-plan.md`): VA space (`FERMI_VASPACE_A`), GPFIFO channel + copy-engine class for GA10x, USERD + doorbell, semaphore fence; first use: copy system -> VRAM by CE and measure GB/s. All by RM RPCs, so 4g's `Rm` struct is the prerequisite. Also open before it: keeping GSP-RM alive after boot means the queues must be serviced (events keep arriving: `POST_EVENT`, `OS_ERROR_LOG`...) — decide where (a kernel thread / the MSI handler) before adding runtime RPCs.
