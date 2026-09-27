# Graphics: framebuffer, console, `/dev/fb0`, compositor, GUI programs

Code: `kernel/src/framebuffer.rs`, `kernel/src/drivers/{framebuffer_console,dev_fb0}.rs`, `kernel/src/memory/memtype.rs`, `hal::{memtype,fbdirty}`. Userspace: `userspace/src/bin/{compositor,panel,term,cpumon,textdemo,imgview}.rs`, crates `gui`/`vt`/`draw`/`text`/`img`. Plans: `docs/fb/`, `docs/gui/gui-plan.md`, `docs/gui/text-plan.md`.

## Framebuffer (`Framebuffer`)

- **RAM shadow**: every primitive draws into a write-back RAM copy and marks a dirty rectangle.
  - `Framebuffer::flush` is **the only code that writes VRAM** (write-only, `sfence` at the end).
  - Outside a batch, each primitive flushes its own rectangle. `begin_batch`/`end_batch` (they nest) coalesce the flushes; the console wraps each `write` in one batch.
  - **Any new code that writes VRAM without going through `Framebuffer`'s primitives desyncs the shadow.**
  - If the shadow can't be allocated, the console runs in direct mode (`shadow:` in `/proc/fbinfo`).
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
- ioctls: `FBIO_GET_INFO` (0x4642_0010; geometry + offset of pixel (0,0)) and `FBIO_FLUSH` (0x4642_0011; up to 16 rectangles).
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
- `REL_Y` is PS/2-signed (up is positive) and is negated for the screen.
- **Window management** (`gui::compositor`, host-tested):
  - The compositor draws the decorations (title, close, maximize).
  - A client opts in with `set_resizable(min_w, min_h)`. Dragging the border sends **one** `resize` on release.
  - `resize` is a request: the old content stays until the client sends **any buffer created after the request**, whose size then wins.
  - `close` is sent to the client, never acted on by the compositor.
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
  - Icons live on disk in `/mnt/usr/share/icons` (`disk-image-root/usr/share/icons`, synced by the root `build.rs`), never embedded. `imgview [file.png…]` shows them and prints the blit path and cost.
  - Not there yet: scaling an image (HIDPI draws icons 1:1).
- Test end to end: `scripts/gui-e2e.sh [term|wm|text]` (screendumps + serial log).
