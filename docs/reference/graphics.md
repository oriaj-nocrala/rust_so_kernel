# Graphics: framebuffer, console, `/dev/fb0`, compositor, GUI programs

Code: `kernel/src/framebuffer.rs`, `kernel/src/drivers/{framebuffer_console,dev_fb0}.rs`, `kernel/src/memory/memtype.rs`, `hal::{memtype,fbdirty}`. Userspace: `userspace/src/bin/{compositor,panel,term,cpumon,textdemo,imgview}.rs`, crates `gui`/`vt`/`draw`/`text`/`img`. Plans: `docs/fb/`, `docs/gui/gui-plan.md`, `docs/gui/text-plan.md`, `docs/gui/perf-plan.md` (pending: 50 fps timer cap, AVX/compiler-flag candidates).

## Framebuffer (`Framebuffer`)

- **RAM shadow**: every primitive draws into a write-back RAM copy and marks a dirty rectangle.
  - `Framebuffer::flush` is **the only code that writes VRAM** (write-only, `sfence` at the end).
  - Outside a batch, each primitive flushes its own rectangle. `begin_batch`/`end_batch` (they nest) coalesce the flushes; the console wraps each `write` in one batch.
  - **Any new code that writes VRAM without going through `Framebuffer`'s primitives desyncs the shadow.**
  - If the shadow can't be allocated, the console runs in direct mode (`shadow:` in `/proc/fbinfo`).
- **Page flipping** (`attach_flip`, only with `gpu=scanout`, `docs/reference/gpu.md`): the VRAM side becomes two buffers and a `Scanout` (the display) that flips between them; the GOP mapping is never written again. The shadow stays the only thing drawn into; each buffer keeps a `DirtyRect` of what it has not received.
  - `flush` (primitives, console, `kalert!`, panic) copies to the *front* buffer (on screen, or the target of a pending flip) and marks the other stale.
  - `present(rects)` (`FBIO_FLUSH`): `Busy` while the last flip is pending; otherwise copies the other buffer's stale area plus `rects` into it and flips. The buffer on screen is never written by `present`.
  - Tested by `hw_tests::framebuffer_page_flip_writes_only_the_hidden_buffer` (RAM buffers, fake display).
  - The shadow starts `SHADOW_SKEW` bytes into its allocation: sharing the aperture's alignment caused TLB-slot collisions.
- **Write-combining**: `memtype::program_pat()` makes PAT entry 1 WC (`WB WC UC- UC WB WT UC- UC`), and `map_write_combining()` points the aperture at it. **Consequence: any mapping with `WRITE_THROUGH` set and `NO_CACHE` clear is WC, not write-through.**
- Always use `stride`, never `width`, for row offsets.
- Hot per-pixel loops: at `opt-level 0` with `build-std`, `write_unaligned` becomes a call with UB checks. Use aligned 32-bit stores and `memcpy` of whole scanlines (`fill_rect`, `blit_scaled`).
- `/proc/fbinfo` is the instrument panel: geometry, physical address, PTE PAT bits, `IA32_PAT`, the covering MTRR (MTRR and PAT reported separately, never combined), `mode`, `shadow`, `fb_wc`, `text_grid`, and per-primitive `diag::OpStat` (calls/bytes/cycles/min/max, plus `instrument_overhead`).
- On the Ryzen VRAM is across PCIe and reads are very slow; QEMU hides every framebuffer performance problem. Measure on metal (`docs/fb/console-perf.md`).

## Text console (`FramebufferConsole`, `/dev/fb`)

- ANSI parser plus the renderer `render_bytes`.
- Font: Noto Sans Mono from the `noto-sans-mono-bitmap` crate (≥0.3; 0.2 clips descenders), antialiased, size chosen from the screen height by `init_font` (1080p → 24 px).
- Supports SGR bold and reverse, and a palette tuned for a black background. The 8x8 `font8x8` path is used only by the panic screen.
- `FBIO_BLIT` (0x4642_0001): nearest-neighbour scaled, letterboxed full-frame blit (DOOM/Quake on the console). It sets `FB_RAW_DIRTY`, so the next text write clears the screen.
- **`kalert!("...")`**: one red line through the normal renderer, printed when a fault kills a process (`kill_current_user_process`). It uses **`try_lock` on `FB_STATE` and `FRAMEBUFFER`, never `lock`**, because the fault may have interrupted the lock holder. No heap formatting.
- Cursor blink runs in the tick with `try_lock`.
- `TIOCGWINSZ` reports cells, and `ws_xpixel`/`ws_ypixel` report the screen size in pixels.

## `/dev/fb0` — graphics mode (`drivers/dev_fb0.rs`)

- Exclusive: a second open gets `EBUSY`; no shadow means `ENODEV`.
- **Holding it is graphics mode**: the console keeps parsing and mirroring but draws nothing, the cursor stops, and `FBIO_BLIT` returns `EBUSY`. `kalert!` and the panic screen still draw.
- The mode ends in `Drop` of the last handle, so killing the holder gives the console back.
- `mmap(MAP_SHARED)` maps the **RAM shadow** through a pinned `ShmObject` (a static that is never freed), so it behaves as an ordinary shared mapping.
- ioctls: `FBIO_GET_INFO` (0x4642_0010; geometry + offset of pixel (0,0)) and `FBIO_FLUSH` (0x4642_0011; up to 16 rectangles). With page flipping, `FBIO_FLUSH` is a flip at the next vblank and returns `EBUSY` (nothing copied) while the previous one is pending, `EIO` if the display refused it (the rectangles were shown on the buffer on screen).
- Test: `fb0_test` (`fb0_test hold` keeps the picture up for a screendump).

## Compositor (`compositor`, crate `gui`)

- `compositor [prog...]`:
  - holds `/dev/fb0`, grabs `event0`, reads `event1`;
  - listens on `/tmp/gui-0`, all under one `epoll`;
  - quits with Ctrl+Alt+Backspace.

  With no arguments it starts a session with `panel`.
- **Disk-resident** (`/mnt/bin`): no `/mnt`, no compositor.
- The protocol is a minimal Wayland-named one (`gui` crate: wire format, regions, `Compositor` state machine, `compose` into a `&mut [u32]`). The C side of the wire is tested against it by `gui/tests/c_wire.rs`.
- Children are started by `userspace::launch::spawn`:
  - a bare name is looked up in `/bin`, then `/mnt/bin`;
  - each child gets its own process group with default ^C/^Z;
  - fds ≥3 are closed before exec (there is no close-on-exec);
  - `GUI_DISPLAY` is set.

  Children are reaped with `syscall::reap_any`.
- The compositor ignores SIGINT/SIGTSTP; `^\` (SIGQUIT) still kills it.
- **Pacing.** With `/dev/vblank` (`gpu=vblank`, `docs/reference/gpu.md`) the RAM shadow is the back buffer: on each vblank it first `FBIO_FLUSH`es what the previous frame composed (the copy starts in the blanking interval and outruns the beam), then composes the next frame, so a change shows one frame after it is composed. The vblank fd is in the epoll set only while there is damage or a pending flush; a vblank missing for 50 ms (`VSYNC_GRACE_MS`) is not waited for. Without `/dev/vblank` (QEMU, `gpu=off`): at most one compose + flush every 16 ms, which the 100 Hz tick turns into 20 ms. With page flipping, an `EBUSY` flush keeps its rectangles; they are merged with the next frame's (bounding box past 16).
- `REL_Y` is PS/2-signed (up is positive) and is negated for the screen.
- **Window management** (`gui::compositor`, host-tested):
  - The compositor draws the decorations (title, close, maximize).
  - A client opts in with `set_resizable(min_w, min_h)`. Dragging the border sends **one** `resize` on release.
  - `resize` is a request: the old content stays until the client sends **any buffer created after the request**, whose size then wins.
  - `close` is sent to the client, never acted on by the compositor.
- **GPU buffers** (layer 4 of `docs/gpu/g5-graphics-stack-plan.md`; the model is done in `gui`, no host uses it yet): `create_gpu_buffer(id, fd, size, w, h, stride, format)` (compositor opcode 3) takes a descriptor of a client's GPU buffer. Off by default (`Compositor::enable_gpu_buffers`): the CPU compositor disconnects a client that sends one. The host takes `GpuOp::{Import, Drop}` (`take_gpu_ops`: every `Import` gets one `Drop`; free it once the frames started before are done) and composes from `draw_list()` (the whole screen, back to front, clipped: `Fill`, `Gpu`, `Cpu` for pool windows whose pixels `cpu_content` returns when `version` moves, `Title`, `Cursor`; a test rasterises it and compares it pixel for pixel with `compose`). A GPU buffer is **not** copied and **not** released at `commit`: it is released when a later commit replaces it *and* the host reported (`gpu_frame_done(epoch)`, frames in order) the frame that may have read it done; a buffer the client destroyed gets no release.
- **`panel`** has the panel role: a bottom strip, never focused. It alone receives the `toplevel*` events and may `activate`. Its launcher reads `/mnt/etc/gui/apps` (`name<TAB>command`, from `disk-image-root/etc/gui/`).

## GUI client libraries

- **Rust**: `userspace::gfx` (`Gfx::open(GUI_DISPLAY, …)`, `present`, `next_event`, `resizable`, `size`, `EV_GFX`) plus the `draw` crate (software 2D, integer-only).
- **C**: `userspace/c/include/constanos_gfx.h` (header-only).
- With `$GUI_DISPLAY` set, the program runs in a window (integer-scaled into shared memory); otherwise it uses `FBIO_BLIT` + `event0`/`event1`.
- **HIDPI** (`gfx::HIDPI`, C `GFX_HIDPI`): the program draws at `w*scale × h*scale` itself. Use it for antialiased text; leave pixel art (DOOM, Quake, `fire`) without it.
- Games lock the pointer (`lock_pointer`/`relative_motion`): only while focused; Ctrl+Alt releases it, a click takes it back.
- **Proportional text**: `userspace::text` (crate `text`: parley + swash). `Text::load()` reads fonts from `/mnt/usr/share/fonts` and falls back to `draw`'s bitmap font. Programs linking it are ~1.2 MB larger, so they are disk-resident.
- **`term`**: pty + `busybox ash` as session leader (`TERM=xterm-256color`) + the `vt` crate (xterm grid, parser, renderer, keymap). It exits when the master reports EIO.
- **Images and alpha**: `userspace::img::load(path)` (crate `img`, over `zune-png`) decodes any PNG to **premultiplied `0xAARRGGBB`**; `Canvas::blit_over` composites it ("over", exact `/255` rounding, the destination's top byte kept). `draw::blend::over_row` runs AVX2 (8 px/step, all-transparent and all-opaque blocks short-circuited) when CPUID + XCR0 say so, else scalar; both give identical bits (`cd draw && cargo test`; `cargo test --release -- --ignored bench` for timings).
  - AVX works in userspace only because the kernel enables XSAVE (`process/fpu.rs`); a program must check `draw::blend::has_avx2()`, never assume.
  - Icons live on disk in `/mnt/usr/share/icons` (`disk-image-root/usr/share/icons`, synced by the root `build.rs`; the sync adds and overwrites, never deletes), never embedded. Layout like a freedesktop theme: `<n>x<n>/<name>.png` per size the artwork exists at, optional unsized `<name>.png` at the top.
  - **HIDPI**: `img::load_icon(name, size, gfx.scale())` wants `size * scale` pixels, picks the smallest themed size at least that big (else the biggest; `img::pick_size`), and resamples **once, at load** (`Image::resized`: separable Catmull-Rom in premultiplied space, stretched when shrinking so it averages instead of aliasing; results clamped to valid premultiplied). Every frame then blits it 1:1. Never resample per frame.
  - `imgview` (no args: the `sample` icon at several logical sizes; with files: each at its logical size × scale) prints the chosen source, load and blit times.
- Test end to end: `scripts/gui-e2e.sh [term|wm|text]` (screendumps + serial log).
