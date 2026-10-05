# GPU compositor visual plan (vk_comp): primitives, themes, icons

Agreed 2026-10-05. Steps 1, 2, 2b (taskbar), 2c (start menu), 2d (no flat look) and 3 (glass) done,
seen on the Ryzen up to 2c (both looks kept: the user likes both, and picks in the start menu).
Step 4 (icons) next. Start here without other context.

## Where it stands

- `vk_comp` (`vk-comp/`, Rust over NVK) turns `gui::Compositor::draw_list()` into `cr_op`s
  (`probes/nvk/comp_api.h`) and the renderer (`probes/nvk/comp_vk.c`, `comp_render.h`,
  `comp.{vert,frag}`) draws them. Map: `docs/reference/graphics.md` ("GPU compositor").
- **Renderer operations**: an opaque solid rectangle (`CR_FILL`); a 1:1 copy of a buffer's pixels
  (`CR_GPU`/`CR_CPU`) whose `alpha` is `CR_OPAQUE`, `CR_KEYED` (the cursor) or `CR_PREMUL`
  (premultiplied ARGB, "over"); a **shape** (`CR_SHAPE` + `struct cr_shape`: rounded box from its
  SDF, a two-segment gradient vertical or horizontal, an inner border, a soft or hard drop shadow,
  colours `0xAARRGGBB` straight alpha). Every draw blends ONE / ONE_MINUS_SRC_ALPHA, so opaque
  draws stay exact. No sampling or scaling yet.
- **Themes** (`gui/src/theme.rs`): `luna` ("Luna 2026", the default) and `9x` ("9x moderno"),
  each a `Theme` of `Shape`s (desktop, window frame and shadow, title bar focused/unfocused,
  buttons as shape or bevel, glyph colour and stroke, title text colour and shadow, taskbar,
  menu). The old flat look is gone (step 2d). `draw_list` emits `DrawOp::Shape` (with an optional
  `clip`: a title bar's box reaches under the content so only its top corners are round, and the
  clip keeps it out; the frame is split into its shadow, square and masked under the window, and
  its ring, square and clipped below the bar, so nothing shows through a translucent window) and
  carries the title's colours in `DrawOp::Title`. **`compose`, the CPU painter (QEMU, no GPU),
  rasterises the same list in software** (`Shape::paint`, a plain gradient row by row), so both
  painters show the same look, and windows' damage is grown by the decorations' reach
  (`decor_margin`). **The
  geometry is the same in every look** (title bar `TITLE_H` × scale, buttons and hit boxes where
  they were; the frame reaches `frame_w` past the window but is not a hit area), so a switch only
  changes pixels. **F12** (the compositor's, like F11) cycles luna → 9x; vk-comp starts in
  `COMP_THEME` (default `luna`) and prints `COMP theme <name>` on every change.
- Titles are premultiplied text, transparent around the glyphs (`gui::theme::text_pixels`: drawn
  white on black for the coverage, then coloured, over an optional 1 px × scale shadow), so the
  bar's gradient shows through: vk-comp draws them with `CR_PREMUL`, the CPU compositor with
  `theme::over`.
- **Taskbar** (step 2b): the panel (`userspace/src/bin/panel.rs`) is told the look (`theme(name)`
  event, at `set_panel` and on every change) and draws only its
  buttons (start, windows, clock area: `Button::paint`, shapes through `Shape::paint`, the
  software twin of `comp.frag`) into a premultiplied **`ARGB8888`** buffer, transparent elsewhere.
  The strip under it is the compositor's (`DrawOp::Shape` before the panel's pixels in
  `draw_list`; `compose` paints the same shape in software), which is where step 3's blur goes.
  Text drawing writes `0x00RRGGBB`, so the panel puts the alpha back after its labels.
- **Start menu** (step 2c): "Apps" opens a **popup** (`set_popup`, `xdg_popup`-like: above
  everything, undecorated, never focused; a click outside or Escape hides it with `popup_done`,
  and that click goes nowhere; it goes with its parent) holding the apps and the **theme selector**
  (`set_theme`, the panel's alone). Its look is `Theme::menu` (`gui::theme::Menu`): the frame with
  its shadow is the compositor's shape under the popup (step 3 blurs there), the panel draws the
  rest: Luna's blue header ("constanos"), white apps column and light-blue "Tema" column, footer;
  9x's grey menu with the navy side banner (the name drawn bottom to top) and an etched separator.
  The panel makes a new popup surface each time it opens (its height, and
  so its offset above the strip, depends on the theme) and logs `panel: menu <label>@x,y ...` in
  screen coordinates, which `gui-e2e.sh wm` uses to launch apps (W1, W5, W6) and checks (W7).
- Decorations (title, close, maximize), the panel (`panel` client) and window management live in
  `gui` (host-tested). PNG loading exists: crate `img` gives premultiplied ARGB, icon theme layout
  `disk-image-root/usr/share/icons/<n>x<n>/<name>.png`, `img::load_icon` resamples once at load.
- Testing without the Ryzen: `probes/nvk/host-comp.sh` runs the renderer on the host's Vulkan
  against a CPU reference and dumps PPM frames. Needs a Vulkan ICD and glslang: in the cloud
  container `apt-get install -y mesa-vulkan-drivers libvulkan-dev glslang-tools` (lavapipe) is
  enough. Float-computed ops (shapes, premultiplied) compare within `tol` = 2 per channel (lavapipe
  is within 1); the reference is itself checked on pixels with known answers. The Ryzen is for the
  final check only.
- **Mutation run** (step 1): 15 mutants of `comp.frag` and `comp_render.h` (radius, border,
  gradient direction and split, shadow offset/mask/reach/blur, coverage AA, premultiplication,
  blend factor, the premultiplied mode) all killed. Rebuild `comp_spv.h` with `gen-spv.sh` after
  editing a shader (it also rewrites `snake3d_spv.h`: `git checkout` that if unchanged).

## Candidate looks (the user's mockups; undecided)

1. **"Luna 2026"**: Windows XP Luna redone with shaders: glossy gradient title bars, rounded
   corners, soft shadows, a glass taskbar and start menu, glossy semi-3D icons. Recommended first:
   2D (input and focus unchanged) and uses exactly the primitives below.
2. **"9x moderno"**: Windows 98 layout (start menu with side banner, bevelled controls) with
   modern lighting and reflections. Same primitives; another theme once 1 exists.
3. **Spatial 3D desktop**: windows as panels with depth and perspective, a macOS-like dock,
   "attention field" (focused windows come closer), glass and blur, procedural wallpapers.
   A separate, long project (perspective, 3D picking of the window under the pointer); later a
   "mode" reusing the same primitives.

Decide by seeing them: build the engine, then switch themes on the real screen.

## Steps

1. **Primitives in the renderer** — done on the host, see "Where it stands"; left: run `vk-comp`
   on the Ryzen once (the push block grew to 112 bytes and blending is on for every draw: compare
   `cr_stats` render_us with before). Original scope:
   - a **shape** op: rounded rectangle via SDF with a linear gradient (2-3 stops), a border and
     a soft drop shadow, in one fragment shader;
   - an **alpha image** op: premultiplied ARGB (what `img` decodes), "over" blending; icons and
     anti-aliased glyphs need it;
   - extend `cr_op` + `gui::DrawOp` (host-tested in `gui`), the CPU reference in `host_comp.c`,
     and compare frames as the existing harness does. Keep the helper-invocation clamp in
     `comp.frag` (its comment: a stray index reset the GPU channel on the Ryzen).
2. **Themes as data** — done on the host, see "Where it stands". Proof: 9 tests in
   `gui/src/compositor/tests.rs` (geometry unchanged in every look, Luna's order frame → bar →
   title → buttons → content, unfocused colours, 9x bevels swap when pressed, F12, fullscreen has
   no decorations, scale 2) with 12 mutants of `ops_decorations`/F12 killed; `gui-capi`'s C test
   reads a `GUI_DRAW_SHAPE`; `host_comp.c` frames `5d-luna`/`5e-9x` run the real window manager
   through the renderer (within 3 per channel: three blended layers at a rounded corner);
   vk-comp's `cargo test` covers the title premultiplication. Seen on the Ryzen: both looks work
   (render 1.3-1.6 ms per frame with blending on). Colours still open to tuning by eye.
2b. **Taskbar** — done, see "Where it stands". Pool buffers may be premultiplied `ARGB8888`
   (`DrawOp::Cpu { premul }` → `CR_PREMUL`; `compose` blends with `theme::over`, exact `/255`).
   Proof: `gui` tests (an ARGB window over others, in compose and the draw list, the oracle
   included; the theme event; the strip as a shape; compose's strip; `Shape::paint` = `pixel` over
   inside the clip; bevels; `over`; `sqrt`), 9 mutants killed; `host_comp.c` frame
   `5c2-argb-panel` and the strip in `5d-luna`; in QEMU with the CPU compositor (F12 through
   the monitor) the start button, window buttons (focused down, 9x sunken), the Apps list and the
   clock area were checked on screendumps; `gui-e2e.sh` and `gui-e2e.sh wm` pass. Not on the
   Ryzen yet.
2c. **Start menu** — done, see "Where it stands". Proof: `gui` tests (a popup above everything and
   not a window, kept on screen; a click inside is its own, outside or Escape closes it and goes
   nowhere, a commit shows it again; its parent's end closes it; bad `set_popup`s are errors; only
   the panel may `set_theme`; its frame under it in both painters), 12 mutants killed;
   `gui-e2e.sh wm` W7 (shows above the strip, Escape, a click outside, a theme picked in it, back
   to flat) and every launch going through it; the three looks checked on QEMU screendumps.
2d. **No flat look** — done (the user: "cajas negras sin alma"). `compose` became a software
   rasteriser of the draw list (so QEMU and a GPU-less machine show the themes too); the frame
   and bar got the split and clip above; `gui-e2e.sh` asks `gui/examples/theme_px` (the same
   `Shape::paint`) what a point should look like instead of hard-coding flat colours, and finds
   the cursor by its bitmap (black text on Luna's white menu fooled the looser test). Proof: `gui`
   tests (a transparent window shows neither its frame nor its bar; a shadow falls on the window
   below), the renderer's clip mutant killed, the four `gui-e2e.sh` modes.
   Next on it: keyboard navigation, icons per item (step 4), submenus.
2e. **Later (agreed, not now): one compositor.** `vk_comp` becomes *the* compositor with two
   render backends: GPU (`cr_*` over NVK, as now) and software (`gui::Compositor::compose` +
   `/dev/fb0`, `FBIO_FLUSH` of the damage and the vblank pacing, taken from
   `userspace/src/bin/compositor.rs`), the software one chosen when `cr_init` finds no GPU (or by
   an env var). `vk-comp` gets a cargo feature for NVK so a software-only build is a plain musl
   `cargo build`, which `kernel/build.rs` can put on `disk.img` for QEMU; `gui-e2e.sh` then drives
   `vk_comp`, and `compositor.rs` is deleted. Expected differences to settle with the e2e: the
   console keyboard grab, Ctrl+Alt+Backspace, starting the panel. Until then the CPU compositor is
   a test bench and fallback only: no features of its own (glass is drawn there as its tint,
   without blur).
3. **Glass / blur behind** — done on the host, not yet on the Ryzen. `cr_shape.backdrop_blur` (and
   `gui::theme::Shape::glass`): at such a shape the renderer pauses the frame, copies the region
   behind it (its box in its clip, grown by the radius) out of the target into a buffer, blurs it
   with `blur.comp` in two Gaussian passes (sigma radius / 2) and draws the fill over that copy
   (`comp.frag` mode 3, `misc.w` bit 1). Up to 4 per frame; the swapchain images need
   `TRANSFER_SRC` (else glass is unblurred, and vk-comp says so); `cr_stats.glass` counts them.
   Luna's taskbar strip (blue, ~78%, radius 10) and menu frame (radius 14) are glass, its menu
   columns translucent (white 75%, light blue 66%) and its header/footer slightly so; 9x stays
   opaque. The CPU painter draws the tint over what is behind as it is (no blur). Proof:
   `host_comp.c` (its own blur reference; `5c-shapes` checks a glass smooths what is behind it,
   `5d-luna` one backdrop, `5f-glass` a window dragged behind the taskbar and a menu-like popup:
   two), 22 renderer mutants killed (6 of the glass). Left: measure `render` on the Ryzen with the
   menu open (two blurs of ~1280 x 60 and ~350 x 400 per frame).
4. **Icons**: the user generates them with a local image model. Generate at 256 px on a
   transparent (or flat, keyed-out) background, one shared prompt (light from the top left, 3/4
   view, same palette) so the set is coherent; store in the theme layout (16/32/48/256).
   Needs step 1's alpha op.
5. **Real 3D icons** (the user's actual wish, later): small meshes (e.g. glTF/OBJ) rendered by
   the compositor itself with lighting, turning or lifting on hover; a natural bridge to the
   spatial desktop (look 3). Needs a depth buffer and a mesh pipeline in the renderer.
