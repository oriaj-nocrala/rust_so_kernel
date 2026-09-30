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

`NVG_IOC_{SCANOUT_INFO,PRESENT,FLIP_STATE}`; `Framebuffer::{present_external,restore_front}`; `/dev/vblank` (poll, un `read` por espera); canales GR por contexto con RC aislado (`uapi.rs` `create_rt`/`destroy_rt`); `vk_draw` (`VK_DRAW_PRESENT` copia por CPU a `/dev/fb0`, `VK_DRAW_SCANOUT` triple buffer sin copia); job `scripts/metal-jobs/gpu-vk.sh` (todo en un arranque); `snake3d` (`probes/nvk/vk_snake.c`, shaders `snake3d.{vert,frag}` -> `gen-spv.sh`; job `gpu-snake.sh`): primera app 3D real sobre NVK (profundidad, mezcla aditiva/alfa, push constants de 160 B, sin buffers de vértices ni descriptores), se prueba en el host con `probes/nvk/host-snake.sh` (mismo fuente, `-DSNAKE_HOST`, vuelca PPM) y en QEMU con `SNAKE3D_HEADLESS=1` (dispositivo software: compila con NAK y recorre todo Vulkan sin ejecutar); extensión de Mesa en `mesa-port/overlay`.

## Estado

- **Capa 1, primera versión hecha y medida en la Ryzen #160 (2026-09-30)** (`scripts/metal-jobs/gpu-multi.sh`: `nvgpu_hw_test` con 4 sesiones a la vez, 0 fallos; un `vk_draw` presentando a 59,0 fps con 3 dibujos fuera de pantalla al lado; el 2.º presentador rechazado con EBUSY; 5 pares simultáneos; `chans_made=chans_freed=23`, `dead=0`, `chans_dead=0`): sesiones por `open` (hasta 12), un trozo de 16 GiB de VA por sesión (`hwq::session_va`), heap de VRAM compartido (`Backend::shared_vram`), `quiesce` por sesión, un solo dueño de `PRESENT`. Detalle y reglas en `docs/reference/gpu.md` "Sessions". Deuda explícita: sin aislamiento entre procesos (una sola `PageTables`); el segundo paso de la capa 1 es un VA space de RM + `PageTables` por sesión.
- Probado: host (`devmodel` 45, `hwq` 37, sabotaje), QEMU (`nvgpu_sw_test`, `nvgpu_hw_test` sección 6 con cuatro procesos, `vk_probe`, dos `vk_draw` a la vez; sabotaje del adaptador con `scripts/gpu-mutate-kernel-sessions.sh`).
- Límite a vigilar: 8 canales GR en tiempo de ejecución (`MAX_RT`) para todas las sesiones juntas; cada contexto de NVK 3D+cómputo toma uno.

- **Capa 2 CERRADA y medida en la Ryzen #161-#166 (2026-09-30)**: `BO_EXPORT`/`BO_IMPORT` y `SYNC_EXPORT`/`SYNC_IMPORT` con descriptor (`SCM_RIGHTS`), arena y heap de VRAM globales con cuenta de holders, timelines compartidas resueltas por quien las lee (`SYNCS`), `nvkmd_constanos` con import/export de memoria y de semáforos (`VK_KHR_external_memory_fd`, `VK_KHR_external_semaphore_fd`), `vk_share` (memoria + semáforo timeline Vulkan entre dos procesos, cada uno queda inactivo mientras el otro espera), `nvgpu_hw_test` secciones 7 y 8. Detalle en `docs/reference/gpu.md`. El job `scripts/metal-jobs/gpu-multi.sh` lo cubre todo y exige que al final `gpu_share:` diga 0 sesiones, 0 reservas y 0 timelines.
- Arreglos de paso: `wait4` espera a `dead_files::settle()` también al *devolver* un zombi (Linux cierra los ficheros en `do_exit`; había una ventana en que un padre veía libre un recurso aún en uso, hecha visible por 12 sesiones); `disk.img` ahora 288 MiB (los cuatro binarios Vulkan de 15 MB llenaban los 160).
- Tirón del dueño del display al arrancar otros clientes: causa = crear (y destruir) un canal GR con el cerrojo global `HW` tomado (cortes de 178/94/83 ms); arreglado sacando de ese cerrojo la parte lenta de ambos. Medido #166: 0 intervalos >25 ms (máximo 20,1 ms), 60,1 fps, espera máxima por el cerrojo 2,8 ms (144 ms antes). Quedan retenciones de 3-8 ms (`unbind`, `prepare_rt`), registradas en `gpu_uapi_slow:`.

## Siguiente paso

Capa 3 (WSI de constanos en `wsi_common`, camino directo primero; el camino ventanado y el compositor de la capa 4 ya tienen BOs y timelines compartibles).
