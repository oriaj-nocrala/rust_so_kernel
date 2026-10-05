# GPU compositor visual plan (vk_comp): primitives, themes, icons

Agreed 2026-10-05. Steps 1 and 2 done (host-proven, not yet on the Ryzen); step 3 next. Start here without other context.

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
- **Themes** (`gui/src/theme.rs`): `flat` (what the CPU painter `compose` draws, the only look
  it knows), `luna` ("Luna 2026") and `9x` ("9x moderno"), each a `Theme` of `Shape`s (desktop,
  window frame with its shadow, title bar focused/unfocused, buttons as shape, bevel or flat,
  glyph colour and stroke, title text colour and shadow). `draw_list` emits `DrawOp::Shape`
  (unclipped; the renderer clips) and carries the title's colours in `DrawOp::Title`. **The
  geometry is the same in every look** (title bar `TITLE_H` × scale, buttons and hit boxes where
  they were; the frame reaches `frame_w` past the window but is not a hit area), so a switch only
  changes pixels. **F12** (the compositor's, like F11) cycles flat → luna → 9x; vk-comp starts in
  `COMP_THEME` (default `luna`) and prints `COMP theme <name>` on every change.
- Titles in vk-comp are premultiplied text, transparent around the glyphs (`titles.rs`: drawn
  white on black for the coverage, then coloured, over an optional 1 px × scale shadow), so the
  bar's gradient shows through.
- The panel is a client (`userspace/src/bin/panel.rs`) and draws itself: no theme reaches it yet.
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
   vk-comp's `cargo test` covers the title premultiplication. Left: look at both on the Ryzen,
   tune colours by eye, then the user picks. Not done: a premultiplied flag on `DrawOp::Cpu`
   (comes with icons, step 4); theming the panel (it is a client: needs the theme over the
   protocol, or the panel drawn by the compositor).
3. **Glass / blur behind** for the taskbar and start menu: copy what is behind, two-pass blur,
   tint. Costlier; measure on the Ryzen (`cr_stats` render_us).
4. **Icons**: the user generates them with a local image model. Generate at 256 px on a
   transparent (or flat, keyed-out) background, one shared prompt (light from the top left, 3/4
   view, same palette) so the set is coherent; store in the theme layout (16/32/48/256).
   Needs step 1's alpha op.
5. **Real 3D icons** (the user's actual wish, later): small meshes (e.g. glTF/OBJ) rendered by
   the compositor itself with lighting, turning or lifting on hover; a natural bridge to the
   spatial desktop (look 3). Needs a depth buffer and a mesh pipeline in the renderer.
