# three.js runtime: unmodified three.js on NVK, no browser

Status: **idea, not started** (handoff written 2026-10-07). Nothing here is measured yet;
every "unverified" below is a question for the first session.
Related: [`llm-as-ui-plan.md`](llm-as-ui-plan.md) (this is its app runtime),
[`capabilities-plan.md`](capabilities-plan.md) (why this runtime does not need them yet).

## Goal

A native program, `three`, that runs a three.js app (a `.js` file, ES modules) full screen or
in a compositor window, drawing through Vulkan on NVK. No browser, no DOM, no WebGL.

## Why

- LLMs write three.js very well. **The advantage holds only if the API *is* three.js**: same
  language (JS), same names, same semantics, `import * as THREE from 'three'` works. A
  "three.js-like" API in Rust or Lua means the model writes three.js anyway and is wrong at
  every difference, and the API has to be taught in the prompt.
- So: run **real, unmodified three.js** and implement what it expects from a browser.
- Outreach: "three.js without a browser, on my own kernel, on my own NVIDIA driver". There is a
  lot of three.js content on X; native is the obvious next step and nobody has it on a
  from-scratch kernel.

## The decision: WebGPU (wgpu), not WebGL

three.js generates shaders at runtime, so whatever backs it needs a shader compiler in the
process.

| Path | Shader compiler | Cost |
|------|-----------------|------|
| `WebGLRenderer` → WebGL2 shim → GLES → Zink → NVK | Mesa's GLSL frontend (C++) | `docs/gpu/phase7-3d-decision.md`: 53 477 lines of C++, a C++ runtime built against musl, a patch for Zink's `dlopen`. L/XL |
| WebGL2 shim written straight on Vulkan | glslang (C++) + a GL state machine | writing a GL driver. XL |
| **`WebGPURenderer` (`three/webgpu`) → `navigator.gpu` → wgpu → Vulkan → NVK** | **naga (Rust)**: WGSL → SPIR-V | all Rust, no C++. Deno implements WebGPU the same way (`deno_webgpu` on wgpu) |

Chosen: **wgpu**. The whole runtime is Rust std on musl, which already runs
(`linux-abi` skill), linked statically with NVK like `vk-comp/` is.

## Architecture

```
app.js  ──►  three.js (three/webgpu, unmodified, MIT)
               │  navigator.gpu, window, requestAnimationFrame, events, fetch, Image
               ▼
         shim layer (JS + Rust bindings)
               │
   rquickjs (QuickJS, ES modules)     wgpu (+ naga)     img / text / draw crates
                                         │
                                  Vulkan → NVK (static) → /dev/nvgpu
                                         │
                              present: NVK WSI window, or offscreen → compositor
```

- **JS engine:** `rquickjs` (QuickJS; MIT). No JIT: interpreter only. (PROT_EXEC is ignored and
  NX is off today, so a JIT engine might even run, but QuickJS is the small, portable choice.)
- **`navigator.gpu`:** JS objects backed by wgpu (`GPUDevice`, buffers, textures, pipelines,
  command encoders, `queue.submit`). Map the WebGPU IDL 1:1; wgpu follows it closely.
- **Renderer alias:** make `THREE.WebGLRenderer` resolve to `WebGPURenderer` (import map
  `'three'` → `three/webgpu`) so ordinary model-written code runs. Custom `ShaderMaterial` with
  hand-written GLSL will not: tell the model "use TSL nodes, not GLSL" (one line of prompt).
- **Browser shim (minimum):** `window`, `document.createElement('canvas')` returning the
  surface, `innerWidth/innerHeight/devicePixelRatio`, `addEventListener` for pointer, wheel,
  keyboard and `resize`, `requestAnimationFrame` / `renderer.setAnimationLoop` paced by vblank
  (blocking `poll(/dev/vblank)` exists), `performance.now`, `setTimeout`, `console`.
- **Assets:** `fetch`/`FileLoader` over local files (the app's directory), `Image`/`ImageBitmap`
  decoded with the `img` crate (PNG today; JPEG would be new). `GLTFLoader` should then work.
- **Input:** compositor events (`gvk_next_event` in `constanos_gui_vk.h`, or the `gui` crate
  from Rust) mapped to DOM events.
- **Present, two options:**
  1. A window on NVK's WSI (`gvk_surface_create`, `docs/reference/gpu.md` "WSI windows"): wgpu
     needs a surface from a raw handle it does not know; patch `wgpu-hal`'s Vulkan backend or
     create the `VkSurfaceKHR` ourselves and hand it over.
  2. Render to an offscreen wgpu texture and export it to the compositor as a GPU buffer.
  Start with whichever needs no wgpu patch.

## The DOM gap (the main source of failures)

Model-written three.js puts UI in HTML: a `<div>` for the score, buttons, menus, an `<input>`.
Options, cheapest first:
1. System prompt: "no DOM; draw UI in the scene" plus a tiny helper (`ui.text(x, y, str)`,
   `ui.button(...)`) drawn by us as an overlay with the `text` and `draw` crates.
2. A `CanvasRenderingContext2D` subset (fillRect, fillText, drawImage) on `draw`/`text`, so
   `CanvasTexture` and sprite labels work. Models use this a lot for text.
3. A real HTML subset: only if 1-2 prove insufficient. Do not start here.

## Steps (each one gates the next)

0. **Host spike (Linux, fast iteration).** rquickjs + wgpu + three.js `three/webgpu` running the
   spinning-cube example unmodified, presenting to a host window or dumping frames. The host
   has no `libvulkan` (`docs/gpu/phase7-3d-decision.md`): install the Vulkan loader (the NVIDIA
   ICD or lavapipe). Measure: does it run; time to parse/load three.js in QuickJS; ms/frame
   for 1, 100, 1000 meshes.
1. **LLM compatibility benchmark (still on the host).** Give Claude ~10 prompts ("a 3D game to
   learn fractions", "solar system", ...) with a short system prompt describing the limits
   (no DOM, TSL not GLSL). Record how many run unmodified, and why each failure failed.
   **This number decides whether the idea is worth porting.**
2. **Port to constanos in QEMU** (NVK on the software `/dev/nvgpu` device): wgpu with NVK
   linked statically, present path, input.
3. **Ryzen** (`metal-run` skill): fps, per-frame hitch counts (a stutter the user
   sees outranks a good average), GPU clocks.
4. Package as an app the launcher shows (`/mnt/etc/gui/apps`, icons in
   `/mnt/usr/share/icons`), which is what `llm-as-ui-plan.md` installs into.

## Unverified (answer these first)

- How mature `WebGPURenderer` is in the current three.js release, and which examples run.
- Whether wgpu's Vulkan backend can link NVK statically: it loads `libvulkan` through `ash`;
  `ash` can link instead of load, and NVK's entry is `vk_icdGetInstanceProcAddr`. Not tried.
- The features and limits wgpu demands at device creation vs what NVK reports on the GA106.
- QuickJS speed for three.js's per-frame matrix work; QuickJS memory for three.js (~1 MB of JS).
- wgpu's threads and syscalls on constanos (it may want things Mesa did not).

## Prior art

- Deno's WebGPU (`deno_webgpu`, on wgpu): the closest design.
- Babylon Native: Babylon.js outside a browser on a native renderer (bgfx).
- expo-gl + expo-three: real three.js on a native GLES binding (the WebGL path).

Licences: three.js MIT, QuickJS MIT, wgpu/naga MIT OR Apache-2.0: all compatible with the
project's MIT OR Apache-2.0.
