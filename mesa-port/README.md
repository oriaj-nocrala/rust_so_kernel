# NVK on constanos

Mesa's NVK Vulkan driver, talking to `/dev/nvgpu` (constanos) instead of DRM/nouveau. Plan and status: `docs/gpu/g4-nvkmd-plan.md`.

- `patches/0001-nvk-constanos.patch`: changes to tracked Mesa files (main 20f48abe): the `nvk-constanos` Meson option; `nvkmd.c`,
  `nvk_instance.c`, `nvk_physical_device.{c,h}`, `nvk_device.c` (enumeration and sync hooks under `NVK_CONSTANOS`); and
  `util/build_id.c` (a static executable has no `dladdr`, so the program's own build-id note is found first); and the WSI (G5 layer 3):
  `nvk_wsi.c` (the `wsi_device.scanout` hooks), `wsi_common.h` (`struct wsi_scanout`), `wsi_common_headless.c` (the headless platform presents to the screen).
- `overlay/`: new files, `nvkmd/constanos/nvkmd_constanos.{c,h}` (the backend: pdev, dev, mem, va, exec and bind contexts, and the
  `vk_sync_type` over kernel timelines). `nvgpu.h` next to them is copied from `nvgpu/uapi/nvgpu.h` by `apply.sh`.
- `musl-cross.ini`: the Meson cross file (clang, musl, static).
- `apply.sh`, `build.sh`: put the changes on a checkout / build everything.

To change the backend, edit it in the checkout, then refresh the copies here: `cp` the two files back into `overlay/` and
`git -C ~/src/gpu-ref/mesa diff > mesa-port/patches/0001-nvk-constanos.patch`.

Test: `scripts/run-vk-probe.sh` runs `probes/nvk/vk_probe.c` (instance, device, memory, a compute pipeline compiled by NAK, submit,
fences, timeline and binary semaphores) on constanos.
