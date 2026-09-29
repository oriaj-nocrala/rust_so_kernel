# GPU (NVIDIA GA106) and its kernel prerequisites

Plan and decisions: `docs/gpu/gpu-plan.md`. This page is the current state. Pure logic: crate `nvgpu` (GPU) and `hal::pcicfg`/`hal::dma`/`hal::bootopts` (generic); kernel adapter: `kernel/src/gpu/`.

## Boot options (`kernel/src/bootopts.rs`, `hal::bootopts`)

- UEFI gives no command line. `key=value` words are read once after `fs::init` from `/mnt/etc/kernel.conf`, then `/mnt/autorun/kernel.conf` (later wins; `#` comments). Before that, everything is at its default.
- `/mnt/autorun/kernel.conf` is written by `scripts/metal-run.sh --kconf '...'` and removed with `autorun/`, so an option applies to one unattended run only.
- `disk-image-root/etc/kernel.conf` is gitignored: a checkout's own options.
- `gpu=` (`hal::bootopts::GpuLevel`): `off` (default: the GPU is not touched), `probe`, `disp`, `vblank`, `dispstate`, `chan`, `scanout`, `super`, `vpll`, `dplink`, `modes`, `hdmi`. Each level does everything the previous ones do. An unknown value is logged and read as `off`.
- `disk-image-root/etc/` is not synced to `disk.img` (only `etc/gui` is). To try a level in QEMU, write the file into the image: `debugfs -w -R "write <file> /etc/kernel.conf" disk.img` (and `rm` it after).

## `gpu=probe` (phase 1)

`gpu::probe()` runs in `init::boot` after `logpart::init`, before the APs are released. Output goes to the log (`gpu: ...`) and `/proc/gpu`:
- the firmware files (size, first word, FNV-1a 64 to compare with the host's copy);
- D4: each IOMMU from the IVRS (`hal::acpi::parse_ivrs`), its control register and `IommuEn`;
- the GA106 (`10de:2507`): capabilities, MSI/MSI-X, BARs with sizes; BAR0 mapped UC and `PMC_BOOT_0` decoded (`nvgpu::id`); the first 16 MiB of BAR1 mapped WC (not accessed); `pci::claim(..., "nvgpu")`.
- **No GPU register is written.** The only writes are the BAR sizing protocol's, in configuration space, with decoding off for microseconds (no CPU may draw on the GOP framebuffer then: hence before the APs).
- Metal job: `scripts/metal-run.sh --kconf 'gpu=probe' scripts/metal-jobs/gpu-probe.sh`.

## `gpu=disp` (phase 2)

Everything `probe` does, then (`gpu::probe_displays`, only if `PMC_BOOT_0` says GA106):
- **VBIOS** from the PROM (`nvgpu::vbios::read_prom`: BAR0 `0x300000`, image chain by PCIR/NPDE like nouveau's `shadow.c`). The ROM-shadow bit (`0x088050` bit 0) is cleared only if set and restored; on the target it is already clear, so no write. `/proc/gpu` shows size, time, FNV-1a of the first 148 992 bytes (= the phase 0 dump), images and the BIT version.
- **DCB** (`nvgpu::dcb`): outputs (walk of `engine/disp/nv50.c`), CCB 4.1, connector table. Logged as nouveau prints them.
- **Connectors** (`nvgpu::display::connectors`): the connector-table entries some output uses. Name = DRM type + count per type in table order (nouveau's naming): `DP-1`, `DP-2`, `DP-3` (the ASUS, AUX 3), `HDMI-A-1` (the HP, I2C port 5). The proprietary driver calls the ASUS `DP-1`; these names are not Linux's `nvidia` ones.
- **Probe** by polling, no interrupts (`nvgpu::display::probe`):
  - DP: DPCD caps (+ the 0x2200 extended copy) and EDID blocks 0-1 by I2C-over-AUX (`nvgpu::aux`: `auxgm200.c` transaction + DRM's retry policy). No sink (AUX status bit 28) = disconnected.
  - HDMI: EDID by bit-banged I2C (`nvgpu::i2c`: nouveau's `bit.c` on `busgf119.c`'s port register). NACK at 0x50 = disconnected.
  - Pads (`nvgpu::pad`) switched to AUX/I2C and put back; the AUX auto-DPCD bit (nouveau leaves it cleared) is restored too.
  - More than one EDID extension is not read (needs the E-DDC segment pointer, never used in the trace): shown as a `note:`.
- `/proc/displays`: one line per connector (`name status conn N aux|i2c N  MFG name`), then `dpcd:`, `preferred:`, `range:`, `modes:` (every DTD), `edid-fnv1a64:` and `edid-hex:`.
- Runs before the APs are released; delays are TSC busy-waits that service TLB shootdowns.
- Metal job: `scripts/metal-run.sh --kconf 'gpu=disp' scripts/metal-jobs/gpu-disp.sh`.

## `gpu=vblank` (phase 3)

Everything `disp` does, then `gpu::vblank::setup` (still at boot, IF=0, before the APs):
- **Heads** (`nvgpu::vblank`): which exist (`0x610060`) and what each scans out, from its ARM state (`0x68a000 + head*0x400`: totals, blanking, pixel clock → refresh). A head with a clock and a raster is "lit"; the lowest lit one is the **primary**.
- Read-only evidence, logged before arming: the interrupt tree and display interrupt registers as the firmware left them; the primary head's raster position sampled for 200 ms (frames counted by vline wrapping: plan B's instrument); whether its vblank status bit latches (cleared, read 40 ms later).
- **Arming** (`nvgpu::vblank::arm`, nouveau's order): unarm the VFN tree (`0xb81610`), block every leaf, reset + allow only the display bit (leaf 4, `0x04000000`), MSK=vblank for every head (`0x611cc0`), EN bit 2 for the lit ones (`0x611d80`), MSI rearm (`0x088704`), rearm (`0xb81608`). Then PCI bus mastering on (MSI is a memory write; the GOP leaves it off) and MSI to CPU 0.
- **Handler** (`on_msi`, ISR on CPU 0, global work): `nvgpu::vblank::service`, the trace's handler register for register (unarm, MSI rearm, leaves, `PMC_BOOT_0` check, reset, `0x611ec0`, per-head ack of LOADV and VBLANK, rearm). Nothing else touches BAR0 after boot, and the GPU sends no second MSI before the rearm, so no lock. Leaf bits with no handler are blocked (nouveau's storm guard).
- Counters in `/proc/kdebug`: `gpu_vblank: enabled primary_head seq last_ns msi vector spurious blocked gone disp_other head_other`, plus `gpu_vblank_headN` per lit head. `seq`/`last_ns` are the primary head's vblank count and the `ktime_get` of the last one, so a rate needs no sleep precision.
- **`/dev/vblank`** (`drivers/dev_vblank.rs`): `ENODEV` unless armed. Readable (poll/epoll) once a vblank happened after the one the handle last read; `read` never blocks and returns 16 bytes (sequence number, ns). Blocking reuses poll's input-queue machinery (`evdev::QUEUE_VBLANK`; `EventSource::seen` carries the handle's number and the re-check compares it with the live one).
- `PollSource` is pinned at 16 bytes (`const` assert in `poll.rs`): `poll_wake_where` keeps 8 waiters with a 16-entry map each on the ISR's stack, and 24 bytes overflowed it.
- Metal job: `scripts/metal-run.sh --kconf 'gpu=vblank' scripts/metal-jobs/gpu-vblank.sh` (rate over 30 s must be 60.0 ± 0.1, then `compositor fire` for 20 s for a tearing photo).

## `gpu=dispstate` (phase 5.1)

Everything `vblank` does; before arming vblank, `gpu::read_dispstate` reads the display's ARMED method state (`nvgpu::dispstate`), with no writes:
- core: `0x688000 + method` for nouveau's 243 dumped methods (`gv100.c:610-705`); window 0: `0x690800 + method` for its 80 (`gv100.c:417-505`).
- `/proc/dispstate`: `gop:` (the GOP framebuffer's physical address as a BAR1 offset, compared with window 0's `SET_OFFSET` and pitch), `headN:`, `sorN:`, `window owners:`, `window0:` (surface), then one `core MMMM VVVVVVVV` / `wndw0 MMMM VVVVVVVV` line per method, to diff against `nvgpu/fixtures/modeset-core-round1.txt`.
- On the Ryzen (boot #69): identical to the trace's ARMED column except `UPDATE`; the GOP scans out BAR1 + 0, pitch 8192, context DMA handle `0x45564144`.
- Metal job: `scripts/metal-run.sh --kconf 'gpu=dispstate' scripts/metal-jobs/gpu-dispstate.sh`.

## `gpu=chan` (phase 5.2)

Everything `dispstate` does; then, before arming vblank, `gpu::evo::bring_up` (`nvgpu::evo`):
- **Instance memory**: 64 KiB of VRAM at `0x1ffc90000` (nouveau's address), written through PRAMIN (`0x1700` = VRAM >> 16, window at BAR0 `0x700000`; the base is put back). RAMHT at +0 (0x2000 bytes, 10-bit hash, `core/ramht.c`), context DMAs from +0x2000. One object: `0xfb000000` = all of VRAM, for window 0.
- **Display init** (`tu102_disp_init` minus ownership claim, interrupts and SOR power): capability copy into `0x640000`, `0x610078` bit 0, `0x610010 = 9` (VRAM, valid), `0x610014`.
- **Channels**: core (ctrl/user 0) and window 0 (1/1). Push buffers: 4 KiB `DmaBuf`s (below 4 GiB, else 40 bits), target HOST, kept forever (`PUSH`). Bus mastering is turned on here.
- **Gate** before each UPDATE: ASSEMBLY (`user_base + m`) = ARMED (`armed_base + m`) on every dumped method except the pushed ones and PRI-error reads (`0xbadf5xxx`). The core inherits ARMED; **a window starts with a reset ASSEMBLY**, so its differing methods are pushed with their ARMED values first, then the gate runs again.
- Pushes: core `SET_INTERLOCK_FLAGS = 0`, `SET_WINDOW_INTERLOCK_FLAGS = 0`, UPDATE; window 0 the restore + `SET_CONTEXT_DMA_ISO(0) = 0xfb000000` + interlocks 0, then UPDATE. The image is the same (the GOP framebuffer at VRAM 0, pitch 8192).
- **Faults** are polled, never enabled as interrupts: CTRL_DISP `0x611c30` (supervisors, error), EXC_OTHER/WIN/WINIM `0x611854/4c/50`, compared with what was there before. The per-channel exception slots are not zero at rest (`0x80`) and are only meaningful when an EXC bit is set.
- Report: `chan:` lines in `/proc/gpu` and the log, ending in `chan: OK: ...` or `chan: STOP: ...`. Afterwards both channels stay running and idle.
- Metal job: `touch build.rs` (the root build does not watch `nvgpu`), then `scripts/metal-run.sh --kconf 'gpu=chan' scripts/metal-jobs/gpu-chan.sh`. Ryzen boot #74: OK.

## `gpu=scanout` (phase 5.3)

Everything `chan` does; if `bring_up` ended OK (it returns window 0's PUT), `gpu::scanout::setup`, still at boot before vblank is armed:
- **Buffers**: two in VRAM at 16 MiB and 32 MiB (`BUF_VRAM`; = BAR1 offsets), one WC mapping of BAR1 [16, 48) MiB. Same layout as the GOP framebuffer (pitch 8192); STOP unless window 0's ARMED surface is the framebuffer's size and pitch, pitch-linear.
- **Attach** (`Framebuffer::attach_flip`): the RAM shadow is copied into both buffers, then a flip to buffer 0. Each boot flip is timed (`scanout: flip ...: fetched/armed/loadv/vblank +N us vline V`): when window 0 fetched the push, when its ARMED `SET_OFFSET` changed, when the head's LOADV and VBLANK status bits latched.
- **Proof**: the GOP framebuffer (VRAM 0) is then painted dark red. A red screen means window 0 still scans it.
- Two more flips (buffer 1, back to 0), then `scanout: OK: ...` or `scanout: STOP: ...`.
- **Flip** (`nvgpu::evo::flip`): `SET_PRESENT_CONTROL = 1` (non-tearing, nouveau's value; the GOP leaves 0), `SET_OFFSET(0) = vram >> 8`, UPDATE; PUT written, not waited on. Refused while the previous push is unfetched. The 4 KiB push buffer wraps with a JUMP to 0 then PUT = 0 (`evo::wind`, nouveau's `nv50_dmac_wind`).
- **Done** (`Scanout::flip_done`): GET = PUT, ARMED offset = requested, and (once vblank interrupts run) the vblank sequence moved past the one at submit. **ARMED changes within µs of the UPDATE, not at the vblank** (Ryzen #76); LOADV latches at the next vblank. So the vblank condition is what makes it a completion.
- At runtime `flip`/`flip_done` run under `FRAMEBUFFER` (from `FBIO_FLUSH`, any CPU, IF=1) and touch only window 0's PUT/GET/ARMED registers; the vblank handler never touches those, so BAR0 needs no lock.
- `/proc/kdebug`: `gpu_flip: submitted latched not_yet refused shown latency_us last max avg vblanks_per_flip sum max` (latency = submit to the first `flip_done` that saw it done: an upper bound).
- Metal job: `touch build.rs`, then `scripts/metal-run.sh --kconf 'gpu=scanout' scripts/metal-jobs/gpu-scanout.sh`. Ryzen #75/#76: OK, ~1000 flips in 20 s of `compositor fire`, none refused.

## `gpu=super` (phase 5.4)

Everything `scanout` does (needs `chan` OK); then `gpu::supervisor::setup`, at boot before vblank is armed:
- Reads heads/SORs present (`0x610060`), each DP/TMDS output's SOR by pad routing (`nvgpu::supervisor::route_get`, nouveau's `gm200_sor_route_get`), the primary head (lowest lit), the SOR driving it (ARMED `SOR_SET_CONTROL`), the GOP's DP link (`DpLink::read`: `0x612300`/`0x61c10c`) and the IED scripts a detach/attach would run. STOP unless that SOR runs DP with a routed DP output (only a DP attach is ported).
- Enables CTRL_DISP interrupts for the supervisors only (`0x611cf0`/`0x611db0` = 7; nouveau sets `0x187`).
- **Interrupt**: `nvgpu::vblank::service(m, true)` acks DISP_INTR bit 12 as `gv100_disp_intr_ctrl_disp` (pending → `0x611860`, error info from `0x611848`); `vblank::on_msi` then calls `supervisor::on_pending` (ISR, CPU 0, global work).
- **Work** (`nvgpu::supervisor::Supervisor::service`, `gv100_disp_super`): 1 reads head/SOR ARM+ASSEMBLY state, 1.0/2.0 run the output's `OffInt1/2`, 2.1 (clock change) is reported `ClockNotPorted` under `super` and programs the VPLL under `vpll` (below), 2.2 runs `OnInt2`, RG divider, DP audio symbols + watermark (nouveau's formula, GA102 has no `activesym`) and the SOR clock; non-DP attach is `AttachNotPorted` (5.8); 3.0 runs `OnInt3`. Heads not owned are released untouched (`Foreign`). Always releases (`0x6107ac+h*4 = 0`, `0x6107a8 = 0x80000000`).
- **IED scripts**: `nvgpu::init`, nouveau's VBIOS script interpreter with 9 opcodes (`NOT`, `GENERIC_CONDITION`, `SUB_DIRECT`, `COPY_NV_REG`, `NV_REG`, `DONE`, `RESUME`, `TIME`, `ZM_REG`); any other opcode stops the script (`ScriptError::Unsupported`). Enough for DP-3's scripts; HDMI's need more. `OffInt2` writes `0x21234`, which PRI-faults on this GPU (nouveau does the same; the PRIVRING leaf is blocked here).
- **`/dev/dispctl`** (`drivers/dev_dispctl.rs`): `write` `detach` / `attach` pushes `SOR_SET_CONTROL(sor)` = 0 / the GOP's value + UPDATE on the core channel (under `PUSH_AT`), not waited on; `EAGAIN` while the core is not idle, `ENODEV` on open unless set up and vblank armed. `read`: one status line (ARMED control, core PUT/GET).
- Report: `super:` lines in `/proc/gpu` (boot, then one per supervisor and per request, 64 kept) and the klog; `/proc/kdebug` `gpu_super: serviced=s1,s2,s3 script_errors not_done ctrl_disp_errors requests refused work_us`.
- Metal job: `touch build.rs`, then `scripts/metal-run.sh --kconf 'gpu=super' scripts/metal-jobs/gpu-super.sh`.

## `gpu=vpll` (phase 5.5)

Everything `super` does, with `Config::clocks`: supervisor 2.1 programs `VPLL<head>` for the ASSEMBLY pixel clock (`nvgpu::pll`, nouveau's `ga100_devinit_pll_set`).
- **Limits**: VBIOS BIT 'C' v2 → PLL table (this board: `0x4f51`, version 0x50, entry = type `0x80 + head`). VPLL0-3: refclk 27 MHz, VCO 800-1620 MHz, input 19-38 MHz, M 1, N 29-255, P 1-63. Only version 0x50 is parsed.
- **Coefficients**: P and M as `gt215_pll_calc` (P = VCO max / clock, M = 1); the VPLL runs at `refclk * (N + fN/8192) / (M*P)`, so N = whole part, fN = fraction × 8192 rounded. **Not nouveau's encoding** (N one lower, fN `- 4096`): measured on the Ryzen (#78/#79), nouveau's gives 147.15 MHz for 148.5. 148.5 MHz → N 55, fN 0, P 10, which is what the GOP leaves (`0xef18 = 0x370000`).
- **Writes** (`ga100.c:52-55`): `0xef00+h*0x40 = 0x02080004`, `0xef18 = N<<16|fN`, `0xef04 = P<<16|M`, `0xe9c0+h*4 = 1`.
- **`/dev/dispctl` `clock <kHz>`** (`0` = the GOP's): pushes `HEAD_SET_PIXEL_CLOCK_FREQUENCY` and `_MAX` (`0x200c`/`0x2028 + h*0x400`, Hz) + UPDATE; same raster, so only the refresh changes. `EINVAL` below 48 Hz on that raster (the ASUS's floor) or above what the GOP's DP link carries (lanes × rate × 8 / bpp: 180 MHz on its 2×HBR); nothing retrains the link.
- Report: `vpll:` lines at boot (limits, the GOP's clock's coefficients, bounds; the GOP's VPLL registers, read only); `dispctl` status adds ARMED pixel clock and VPLL0's `0xef18`/`0xef04`; `gpu_super: clocks_set=`.
- Metal job: `touch build.rs`, then `scripts/metal-run.sh --kconf 'gpu=vpll' scripts/metal-jobs/gpu-vpll.sh` (50 Hz for ~20 s, then back to 60).

## `gpu=dplink` (phase 5.6)

Everything `vpll` does; `supervisor::setup` also finds the output's DP table entry (`dplink::setup`, `nvgpu::dp::dpout_match`: BIT 'd', version 0x42 on this board; DP-3 = entry `0x6e96`, scripts BeforeLT list `0x6ef1`, AfterLT `0x67c6`, EnableSpread `0x733c`, DisableSpread `0x732b`, DisableLT `0x734d`, lnkcmp list `0x6ec1`) and logs `dplink:` lines.
- **`/dev/dispctl` `train <lanes> <rate>`** (rate in DPCD units, `train 4 0x14` = 4x HBR2): synchronous in the writer's syscall (~130 ms of busy-waits). Refused (EAGAIN) unless the SOR is detached: the ISR sets `LINK_FREE` at each supervisor 3 when the head's SOR drives nothing; any push but `detach` clears it; the check and `TRAINING` are under `PUSH_AT`, so no push happens during a training. EINVAL when refused before any SOR write (LTTPR present, config above sink/DCB, post-LT-adjust sink), EIO when the training ran and failed.
- **Sequence** (`nvgpu::dp`, nouveau's order, replayed against `trace-nogsp` write for write, `fixtures/dp-train.txt`): pad to AUX, LTTPR probe (`0xf0000`), receiver caps, `check_config`, DisableLT (nouveau's release), sink `DP_SET_POWER` to D0 (DRM read with its `0x102` probe), EnableSpread (writes `0x00e86c = 2`: the "unidentified" write of phase 5.0), BeforeLT + lnkcmp for the rate (SOR PLL search on `0x612488`, two 20 ms condition polls), `ga102_sor_dp_links` (40 ms), lane power, TPS1 then TPS2/3/4 with the drive table (`dpcfg_match`, `gm200_sor_dp_drive`), pattern off, AfterLT; pad back, auto-DPCD bit restored.
- `nvgpu::init` now also has `ANDN_REG`, `OR_REG`, `CR` (VGA CR at `0x6013d4/5`, byte accesses: `Mmio::rd08/wr08`), `CONDITION` / `CONDITION_TIME` (BIT 'I' condition table: 4 = `0x612488` PLL lock, 7 = SOR sequencer idle, 8 = `0x61c144` link ready), `ZM_MASK_ADD`, `AUXCH` and generic condition 5 (ASSR, DPCD `0x0d`); `Target::aux` is the output's AUX channel.
- AUX: nvkm's own accesses use the hardware retry loop (`Aux::nvkm_read/nvkm_write`, up to 33 sends on DEFER); DRM's don't (`native_read/write`, `drm_read` = probe + read).
- The next `attach` programs the DP packing for the new link (2.2 reads it back from the SOR); `clock`'s upper bound follows the trained link.
- `dispctl` status adds `link NxRATE[ef]` and `free 0|1`; `/proc/kdebug` `gpu_dplink: trains ok failed refused last_ms`.
- Not ported: LTTPRs, post-LT adjust, eDP rate tables, MST.
- Metal job: `touch build.rs`, then `scripts/metal-run.sh --kconf 'gpu=dplink' scripts/metal-jobs/gpu-dplink.sh` (4x HBR2, then back to the GOP's 2x HBR, at 1080p60).

## `gpu=modes` (phase 5.7)

Everything `dplink` does; then `gpu::modeset::setup` (boot, reads only): the EDID and DPCD `gpu=disp` read on the primary head's output's AUX channel, its modes, the size the head scans out now. Logged as `modes:` lines in `/proc/gpu` (every EDID mode and the CVT-RB2 ones at 48/60/75/100/120/144/165/180 Hz inside the range, each with its VPLL coefficients and DP packing on the max link; then the GOP's mode read back).
- **Modes** (`nvgpu::mode`): `Mode::from_timing` (EDID DTDs, progressive only), `cvt_rb2` (VESA CVT 1.2 RB v2 with integer arithmetic; = `edid-decode --cvt ...,rb=2` except where its float floor lands 1 kHz lower, e.g. 1080p50), `select` (first EDID DTD of that size whose refresh rounds to the request, else CVT-RB2 if inside the range descriptor's V rate and max pixel clock; its H limits are ignored: the ASUS's say 250-250 kHz, which its own DTDs violate), `Mode::methods` (nouveau's `nv50_head_atomic_check_mode` + `headc57d_mode` + `headc57d_or`: raster size, sync end, blank end/start, `0x2074 = 1`, `HEAD_SET_CONTROL = 0`, pixel clock and max, OR word with the polarities; = the trace's round-2 push for 1080p60), `Mode::from_head` (the inverse, for the GOP's).
- **`/dev/dispctl` `mode WxH@Hz`** (`modeset::set`, synchronous in the writer's syscall, IF=1): everything checked first (EINVAL: syntax, no such mode, **another size** (window 0, framebuffer, console and compositor keep theirs: not ported), no VPLL coefficients, no link carries it). Then, only if the current link cannot carry it (`lanes*rate*8/bpp` and `dp_config`): `detach`, wait for a new supervisor 3 (`LINK_FREE`), `dplink::train` to `nvgpu::dp::max_config` (nouveau's link: sink lanes/rate capped by the board's; 4x HBR2 on the ASUS). Then one core push with the mode's methods + `SOR_SET_CONTROL` = the GOP's value + UPDATE (`Cmd::Mode`); wait for supervisor 3 again (2.1 VPLL, 2.2 packing); ARMED raster/clock must equal the mode's. Waits bounded to 1 s each. **`set` runs with IF=1** (restored after): a syscall enters with IF=0, and on CPU 0 that blocks the supervisors' MSI until it returns (Ryzen #84). A mode identical to the ARMED one waits for no supervisor (the UPDATE raises none). No detach unless retraining: while detached the head raises no vblank (#83/#84); with the SOR attached a raster/clock change runs 2.0/2.1/2.2 by itself. A failure after the detach retrains the old link (if changed) and re-attaches at the old mode, then EIO. EAGAIN while another `set`/push/training runs.
- Going back down (e.g. to 60 Hz) keeps the faster link: it carries the mode, so no retrain.
- `dispctl` read adds a `mode:` line (connector, current mode or "the GOP's", size); `/proc/gpu` `mode:` lines per request; `/proc/kdebug` `gpu_mode: sets ok failed refused retrains last_ms current_mhz`.
- `clock`'s lower bound is now the current raster at 48 Hz (was the GOP's raster).
- Metal job: `touch build.rs`, then `scripts/metal-run.sh --kconf 'gpu=modes' scripts/metal-jobs/gpu-modes.sh` (180 Hz = the ASUS's DTD 4, 420.78 MHz, retrains to 4x HBR2; then 120 Hz CVT-RB2; then 60 Hz). Ryzen #85: OK (179.86, 119.93, 60.02 Hz).

## `gpu=hdmi` (phase 5.8)

Everything `modes` does; `gpu::evo::bring_up` also puts window 2's surface handle in the RAMHT, and `gpu::hdmi::setup` (boot, IF=0) finds the connected HDMI connector `gpu=disp` probed (the HP), its EDID and DCB output (07, TMDS), head 1 (must not be lit: the GOP lights head 0 only), window 2, a SOR nothing drives (SOR-0), the HP's preferred mode (EDID DTD 1, 1080p60, 148.5 MHz) and its VPLL coefficients; allocates window 2's push buffer, maps BAR1 [48, 64) MiB WC and paints the test picture (`nvgpu::pattern`: bars, ramp, steps, white border; text from `BASIC_LEGACY`). Logged as `hdmi:` lines. Nothing scans the picture until `hdmi on`.
- **`/dev/dispctl` `hdmi on`** (`hdmi::on`, synchronous, IF=1 like `mode`): SOR power if off (`nvgpu::hdmi::sor_power_up`, `nv50_sor_power`); window 2's channel (`init_channel`, once); pad route (`route_set`, `gm200_sor_route_set`: `0x612408 = 1`) and the HDMI encoder (`nvgpu::hdmi::enable`: GCP/ACR/ctrl `0x6f04c0..`, `0x616dc0`, SCDC off, AVI infoframe from the mode's VIC, no VSI; = `fixtures/hdmi-enable.txt`); `own_head` (the supervisors also work on head 1, route SOR-0 -> HDMI output); one core push (`nvgpu::hdmi::head_methods`: display id, `SOR_SET_CONTROL(0) = 0x102`, viewport, `Mode::methods`, usage bounds, dither, procamp, window 2's usage bounds and owner) + UPDATE; wait for supervisor 3 (2.1 VPLL1, 2.2 the TMDS OnInt2 script `0x5f10`, RG divider, SOR clock, 3.0); check ARMED; window 2's push (`window_methods` + `window_lut_methods`: surface, notifier, identity ILUT; interlocked with the core, `SET_INTERLOCK_FLAGS = 1`, window bit) + UPDATE, then the core push with the head's identity OLUT (`olut_methods`) and `SET_WINDOW_INTERLOCK_FLAGS` = the window bit + UPDATE, which releases both (Ryzen #89/#90: without the LUTs and the notifier the window UPDATE raised INVALID_STATE, slot `0x5080 0x1 0x2d`); wait for idle, check ARMED offset/ctxdma; faults; head 1's refresh by counting raster wraps (head 0 measured beside it as the instrument). Any failure runs `off` again.
- The LUTs (`nvgpu::lut`: 1024 identity entries + header, half-float for the ILUT) and the 4 KiB notifier area live in the HP's 16 MiB VRAM slot (`nvgpu::hdmi::ILUT_VRAM`/`OLUT_VRAM`/`NTFY_VRAM`); the RAMHT gets the window's surface/LUT/notifier handles and the core's LUT handle (`ramht_objects`, `gpu=hdmi` only). `supervisor::push_core` clears the core's window interlock flags on the push after one that set them (they persist in ASSEMBLY: a later core UPDATE would wait for the window: `hdmi off` did not latch, Ryzen #91).
- **`hdmi off`**: encoder disable (`nvgpu::hdmi::disable`), core push `SOR_SET_CONTROL(0) = 0` + display id 0 + UPDATE, wait for the supervisors, `disown_head`. Window 2 and head 1's raster stay configured.
- `nvgpu::init` gained `ZM_REG_SEQUENCE` (0x58: HDMI's lane setup `0x61c040..0x61c07c`); `supervisor::super_2_2` attaches TMDS (RG divider + `ga102_sor_clock` with `div2` above 340 MHz); both replayed against `trace-nogsp` (`round2_hdmi_attach_replays_the_trace_for_head1`).
- Not ported: audio (ELD, `0x616528` HDA device entry), HDMI 2.0 (SCDC scrambling), non-identity LUTs, size changes, the compositor on head 1 (the picture is static, written once at boot).
- `dispctl` read adds `hdmi: HDMI-A-1 'HP 2309' head 1 SOR-0 window 2 on|off mode ...`; `/proc/kdebug` `gpu_hdmi: enabled on ons ok failed last_ms refresh_mhz`.
- Metal job: `touch build.rs`, then `scripts/metal-run.sh --kconf 'gpu=hdmi' scripts/metal-jobs/gpu-hdmi.sh` (on 40 s, off, on 15 s, off). Ryzen #92: OK.

## Oracle tools (`scripts/gpu-trace.py`)

- `aux DIR CH` lists AUX transactions on a channel; `aux DIR CH SEL OUT` writes them as a `ReplayMmio` fixture (`nvgpu/fixtures/aux-ch3-dpcd-edid.txt`).
- `i2c DIR DRIVE` decodes bit-banged I2C from a trace (proved the port-register bits: it yields the HP's EDID byte for byte).
- `core DIR [N OUT]` decodes nouveau's core-channel dumps in dmesg (one per supervisor 1: ARMED `0x688000+m` → ASSEMBLY `0x680000+m`), naming each method from `clc67d.h` (needs `~/src/gpu-ref`).
- `disp DIR T0 T1` lists display writes labelled with the nouveau code that owns each range (`DISP_CLASSES`); `mem DIR LO HI T0 T1` lists non-zero writes into a mapped range (BAR3 instance memory).
- The display push buffers are in host memory on Ampere, so a `nogsp` trace has only their PUTs. `scripts/gpu-oracle.sh` mode `nogsp-vrampush` (`kms_vram_pushbuf=1`) puts them in VRAM behind BAR1, where mmiotrace sees them.
- `supers DIR T0 T1 [OUT]` extracts each supervisor service (from `R 0x6107a8` to the release) as a `ReplayMmio` fixture, minus other contexts' accesses (`SUPER_OTHER`): `nvgpu/fixtures/super-round1.txt`, `super-round2.txt`.
- `train DIR T0 T1 CH [OUT]`: every BAR0 access of a DP link training as a `ReplayMmio` fixture, minus PTIMER, other AUX channels and `TRAIN_OTHER(_TIMES)` (the HDMI encoder enable nouveau interleaves): `nvgpu/fixtures/dp-train.txt`.
- `push DIR [T0 T1 [OUT]]` decodes those push buffers (needs a `nogsp-vrampush` trace): one block per PUT write, each method named from `clc67d.h`/`clc67e.h`/`clc67b.h` (+ `EXTRA_METHODS`, the ones nouveau writes by number). Fixture: `nvgpu/fixtures/modeset-push.txt`.
- `core` and `push` name methods through `method_names(cls)`: header indices have no bound, so offsets are claimed index 0 first across all methods, then 1, …

## PCI configuration space (`kernel/src/pci.rs`, `hal::pcicfg`)

- `config_space` reads all 256 bytes (mechanism #1; no ECAM, so no extended capabilities). `hal::pcicfg::capabilities` walks the list, bounded.
- `size_bars`: Linux's sizing protocol, decode off, under one hold of `CONFIG`.
- `enable_msi(bdf, dest_apic_id, vector)`: single-vector MSI, INTx disabled; refuses without a LAPIC (8259 fallback). `hal::pcicfg::MsiCap::enable_writes` decides the writes, host-tested against the GA106's real config space (`hal/fixtures/ga106-config.txt`: MSI at 0x68, 64-bit, no masking, no MSI-X).

## MSI vectors (`kernel/src/interrupts/msi.rs`)

- 0x50–0x5F have stubs in the IDT from the start (it is a `Once`); `msi::alloc(handler)` hands one out, `free` returns it. Dispatch is a table of atomics, no lock; EOI after the handler.
- Handlers run in ISR context on the CPU the message targets: ISR rules apply, and the driver states whether its work is global or per-CPU.
- `msi::count(vector)`, `msi::stray()` for tests and diagnostics.

## DMA (`kernel/src/memory/dma.rs`, `hal::dma`)

- `DmaBuf::alloc(len, mask)`: zeroed, contiguous, power-of-two pages from the buddy; `bus_addr()` = physical (no IOMMU, D4); fails with `AboveMask` if the block is not under the device's mask (the buddy cannot allocate below a limit).
- `DmaPages::alloc(count, mask)`: non-contiguous 4 KiB pages (for radix3 tables).
- **Release is explicit** (`free`); there is no `Drop`, so a forgotten buffer leaks rather than being handed back while a device may still write it.
- Cacheable through the physical window (x86 DMA snoops).

## Firmware (`kernel/src/firmware.rs`)

- `firmware::load("nvidia/ga106/gsp/...")` reads `/mnt/lib/firmware/<rel>` whole (max 96 MiB).
- The root `build.rs` (`ensure_firmware`) decompresses the files from the host's `/usr/lib/firmware` into `disk-image-root/lib/firmware/` (gitignored) with `LICENCE.nvidia`, and syncs them to `disk.img`; `sync-usb-data.sh` takes them to the stick.
- Staged today: `bootloader`, `booter_load`, `booter_unload` of 570.144. **`gsp-570.144.bin` (63 MB) is not**: `disk.img` is 96 MiB with ~20 MB free; phase 4 decides.
- Their first word is `0x000010de`, the `nvfw_bin_hdr.bin_magic` (`nouveau/include/nvfw/fw.h:8`).

## Tests

- Host: `cd hal && cargo test` (pcicfg, dma, bootopts, acpi IVRS), `cd nvgpu && cargo test`.
- `nvgpu` fixtures: the two EDIDs + their `edid-decode` output, an AUX trace extract, one vblank interrupt as nouveau serviced it (`vblank-service.txt`), and the modeset by steps (`modeset-*.txt`: display init, channel bring-up, instance memory, both supervisor rounds, link + AUX training, and the core state at each supervisor 1, whose ARMED column in round 1 is the state the GOP left) (committed); the VBIOS is read from `$GPU_ORACLE/static/vbios-rom.bin` (default `~/constanos-gpu-oracle`, not in git, D3) and those tests print `SKIP` without it.
- `nvgpu::evo` tests use a small EVO simulator (`Sim`: PRAMIN onto a VRAM map, PUT executes pushed methods into ASSEMBLY, UPDATE copies ASSEMBLY to ARMED).
- `nvgpu` mocks: `TableMmio` (fixed values), `ReplayMmio` (per-register read queues from a trace extract + write log to compare), `i2c::tests::DdcSim` (open-drain bus with a DDC EEPROM).
- QEMU: `hw_tests::edu_mmio_dma_msi` (`scripts/run-kernel-tests.sh`; the runner adds `-device edu,dma_mask=0xffffffffffff`): MMIO, MSI to CPU 1 (the test boot keeps IF=0 on CPU 0), DMA both ways, mask refusal. The `edu` driver (`kernel/src/edu.rs`) is test-only.
