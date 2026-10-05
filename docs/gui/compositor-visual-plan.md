# GPU compositor visual plan (vk_comp): primitives, themes, icons

Agreed 2026-10-05; not started. Start here without other context.

## Where it stands

- `vk_comp` (`vk-comp/`, Rust over NVK) turns `gui::Compositor::draw_list()` into `cr_op`s
  (`probes/nvk/comp_api.h`) and the renderer (`probes/nvk/comp_vk.c`, `comp_render.h`,
  `comp.{vert,frag}`) draws them. Map: `docs/reference/graphics.md` ("GPU compositor").
- **The renderer has two operations only**: an opaque solid rectangle (`CR_FILL`) and a 1:1 copy
  of a buffer's pixels (`CR_GPU`/`CR_CPU`), with one keyed "transparent if top byte is 0" mode
  for the cursor. No alpha blending, no rounded corners, no shadows, no gradients, no sampling.
- Decorations (title, close, maximize), the panel (`panel` client) and window management live in
  `gui` (host-tested). PNG loading exists: crate `img` gives premultiplied ARGB, icon theme layout
  `disk-image-root/usr/share/icons/<n>x<n>/<name>.png`, `img::load_icon` resamples once at load.
- Testing without the Ryzen: `probes/nvk/host-comp.sh` runs the renderer on the host's Vulkan
  against a CPU reference and dumps PPM frames. Needs a Vulkan ICD on the host (lavapipe, from
  Mesa); the cloud container had none on 2026-10-05: install it first, or run the harness on the
  user's Linux. The Ryzen is for the final check only.

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

1. **Primitives in the renderer** (1-2 sessions):
   - a **shape** op: rounded rectangle via SDF with a linear gradient (2-3 stops), a border and
     a soft drop shadow, in one fragment shader;
   - an **alpha image** op: premultiplied ARGB (what `img` decodes), "over" blending; icons and
     anti-aliased glyphs need it;
   - extend `cr_op` + `gui::DrawOp` (host-tested in `gui`), the CPU reference in `host_comp.c`,
     and compare frames as the existing harness does. Keep the helper-invocation clamp in
     `comp.frag` (its comment: a stray index reset the GPU channel on the Ryzen).
2. **Themes as data**: a `Theme` in `gui` (title height, corner radius, gradients, colours,
   button style and glyphs, shadow size). Two themes, "Luna 2026" and "9x moderno", switchable
   with a compositor key (like F11 is), so the user picks by looking.
3. **Glass / blur behind** for the taskbar and start menu: copy what is behind, two-pass blur,
   tint. Costlier; measure on the Ryzen (`cr_stats` render_us).
4. **Icons**: the user generates them with a local image model. Generate at 256 px on a
   transparent (or flat, keyed-out) background, one shared prompt (light from the top left, 3/4
   view, same palette) so the set is coherent; store in the theme layout (16/32/48/256).
   Needs step 1's alpha op.
5. **Real 3D icons** (the user's actual wish, later): small meshes (e.g. glTF/OBJ) rendered by
   the compositor itself with lighting, turning or lifting on hover; a natural bridge to the
   spatial desktop (look 3). Needs a depth buffer and a mesh pipeline in the renderer.
