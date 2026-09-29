---
name: gpu-gsp
description: Playbook for GSP work on the NVIDIA GA106 (phase 4 incl. 4g, done; phase 6 next): the falcon / FWSEC / booter / WPR2 memory / RPC queue code in crates nvgpu + kernel/src/gpu/gsp.rs, how each piece was verified against nouveau's trace and clang, the RM object RPCs (RM_ALLOC, RM_CONTROL, done in nvgpu::rm), the mutation-testing routine, and the metal-run stability protocol. Read it before touching nvgpu::{falcon,fwsec,firmware,gspmem,booter,rpc}, gpu=fwsec / gpu=gsp, or adding an RM client.
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

## Done in 4g (`nvgpu/src/rm.rs`, `Rm` in `gsp.rs`)

Our own RM client exists after boot: `create_client` = `NV01_ROOT` (`0xc1d00000`) -> `NV01_DEVICE_0` (`0xde1d0000`) -> `NV20_SUBDEVICE_0` (`0x5d1d0000`) + `NV2080_CTRL_CMD_GPU_GET_NAME_STRING`. RPC payloads: `GSP_RM_ALLOC` fn 103 (32-byte header), `GSP_RM_CONTROL` fn 76 (24-byte header), `FREE` fn 10; a reply echoes the request with `status` filled (`0x55/0x66` busy, `0x51` no memory). Fixtures `rm-*-{req,rep}.bin` (extract sent/received RPCs from the trace dmesg as in step 3 above). `Rm::call` = send + `wait_for` (the receive loop that also runs sequencer events). The objects and buffers are never freed; the queues are only serviced at boot.

## Done in 6a/6b

`nvgpu::mmu` (page tables, pure) and `gpu=vaspace` (`kernel/src/gpu/vaspace.rs`): external `FERMI_VASPACE_A` + our tables in VRAM 64 MiB + `SET_PAGE_DIRECTORY`, measured (boot #101). Everything still runs at boot with IF=0 and `Rm::call` polling the status queue: no long-lived `Rm` yet. `Rm::alloc` returns the reply payload, `alloc`/`control` are `pub(super)`. Job pattern: copy `scripts/metal-jobs/gpu-vaspace.sh`.

## 6c in progress (channel + copy)

`nvgpu::chan` + `kernel/src/gpu/copy.rs`; status in `docs/gpu/gpu-plan.md` "Resultados de la fase 6c". Traps found: the doorbell is `0xbb0090` on Ampere (Volta's `0x810090` does nothing); the token is `(runlist<<16)|chid` with the runlist from `NV2080_CTRL_CMD_FIFO_GET_DEVICE_INFO_TABLE` (GSP's own token is for runlist 0); engine `0xb` is COPY2; `cid` in the ALLOC reply is a session counter; PFIFO registers read `0xbadf....` under GSP-RM. Reference drivers: nouveau r570 (used so far) and, per the user, **nova-core** (the current Rust driver) plus public documentation: check them before guessing.

## Next: phase 6c (channel) — read `docs/gpu/phase6-oracle.md` first

Also read `docs/gpu/mmu-v3-notes.md` (page-table format, PTE/PDE bit layout, the external VA-space recipe, TLB flush, free VRAM range, suggested `nvgpu/src/mmu.rs` design). `phase6-oracle.md` lists exactly what the trace contains (VASpace, two GPFIFO channels `0xc56f`, the copy engine `0xc7b5`, BIND/SCHEDULE controls, with `#index` and fixture names already extracted in `nvgpu/fixtures/rm-ph6-*`), the tool to re-extract or extract more (`scripts/gpu-rpc.py list|recv|dump|text`), the nouveau reading order (`r570/fifo.c` -> `r535/fifo.c` -> `r535/vmm.c` -> `ce.c`/`bar.c`), the build order (queue servicing -> VA space + MMU v3 page tables -> channel -> submission/doorbell/fence -> job) and the loose ends (no teardown, `Rm` not long-lived).
