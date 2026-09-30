# G5: el sistema gráfico sobre NVK: cimientos primero, compositor como cliente

Decisión (2026-09-30, usuario + análisis): **WSI estándar de Vulkan como cimiento; el compositor es un cliente más**, el que tiene el privilegio de poseer el display. Estado de partida: G4 cerrado (`docs/gpu/g4-nvkmd-plan.md`): NVK corre cómputo y dibujo reales en la Ryzen, `NVG_IOC_PRESENT` apunta el display a un buffer de VRAM sin copia (60 fps, `poll(/dev/vblank)`), un canal GR por contexto con aislamiento de fallos.

## Por qué este orden

- Si se construye el compositor antes, usará `nvk_constanos_present` (extensión nuestra) y cada app tendría que conocerla. Con WSI, una app Vulkan cualquiera presenta sin saber quién hay detrás.
- **Bloqueo real: `/dev/nvgpu` es exclusivo** (`open` da `EBUSY` al segundo proceso; un único espacio de VA global). Un compositor con clientes necesita varios procesos en la GPU a la vez.

## Capas, en orden

1. **GPU multi-cliente.** Quitar la exclusividad. Primera versión: un solo espacio de VA con rangos disjuntos por sesión (sin aislamiento entre procesos: deuda explícita), cada sesión con su `Device` (modelo), sus BOs y sus contextos/canales (ya hay un canal por contexto). Después: un VA space de RM por sesión (FERMI_VASPACE_A + SET_PAGE_DIRECTORY por sesión, `PageTables` por sesión) para aislar de verdad. Puntos a mirar: `HELD` en `dev_nvgpu.rs`, el pool de tablas (`uapi.rs`, una sola `PageTables`), `Session::drop` (hoy restaura la consola y hace `quiesce` de todo el dispositivo), `RT_*` (8 canales máximo), la arena de memoria de sistema (1 GiB por sesión).
2. **Compartir BOs entre procesos** (equivalente a dma-buf): `NVG_IOC_BO_EXPORT` -> fd o nombre global, `BO_IMPORT`, paso por `SCM_RIGHTS` sobre AF_UNIX (ya hay sockets). En Mesa: importación en `nvkmd_constanos` (`alloc`/`import` de memoria externa) y `VK_KHR_external_memory_fd` (o la variante que use el WSI). Sincronización explícita: exportar timelines (`NVG_IOC_SYNC_EXPORT`).
3. **WSI propio de constanos** en el `wsi_common` de Mesa (`src/vulkan/wsi/`): plataforma nueva, `VK_KHR_swapchain`. Camino directo primero: las imágenes del swapchain son BOs de VRAM con el layout del scanout (`NVG_IOC_SCANOUT_INFO`: pitch, formato) y presentar = `PRESENT` + esperar el flip (`FLIP_STATE` + `poll(/dev/vblank)`). Camino ventanado después: las imágenes se comparten con el compositor (capa 2) y presentar = enviarle el buffer y un fence. Hoy el copiado imagen->buffer lineal lo hace `vkCmdCopyImageToBuffer` (el display lee lineal, NVK renderiza en bloques): el WSI debe hacer ese copiado/desembaldosado o pedir imágenes lineales.
4. **Compositor como cliente**: posee el display (el único con `PRESENT`; `restore_front` al morir ya existe), importa los buffers de las apps, compone en la GPU y presenta por el mismo WSI. Reemplaza al compositor actual (`userspace/src/bin/compositor.rs`, CPU + `/dev/fb0`), que sigue siendo el camino sin GPU.

## Ya hecho y reutilizable

`NVG_IOC_{SCANOUT_INFO,PRESENT,FLIP_STATE}`; `Framebuffer::{present_external,restore_front}`; `/dev/vblank` (poll, un `read` por espera); canales GR por contexto con RC aislado (`uapi.rs` `create_rt`/`destroy_rt`); `vk_draw` (`VK_DRAW_PRESENT` copia por CPU a `/dev/fb0`, `VK_DRAW_SCANOUT` triple buffer sin copia); job `scripts/metal-jobs/gpu-vk.sh` (todo en un arranque); extensión de Mesa en `mesa-port/overlay`.

## Primer paso de la próxima sesión

Capa 1, en QEMU primero (dispositivo software): quitar `HELD`/`EBUSY` del modelo y de `dev_nvgpu.rs` con un `Device` por sesión y una arena de VA repartida por sesión; test C (`nvgpu_sw_test`/`nvgpu_hw_test`) que abra el dispositivo desde dos procesos a la vez; después metal con dos procesos `vk_draw` simultáneos.
