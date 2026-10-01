# G5 capa 4 (compositor en GPU): estado y problema abierto (traspaso, 2026-09-30)

Para quien retome (otro modelo o persona): qué hay hecho, cómo se prueba, qué se midió en la Ryzen y **el problema abierto**: el ritmo de fotogramas del compositor y un fallo que solo ocurre en la máquina real. Diseño y rebanadas: `docs/gpu/g5-graphics-stack-plan.md` "Capa 4". Reglas y mapa de código: skill `gpu-g5` (sección "Layer 4"). Estado del repo: todo commiteado hasta `b624e9c` (lo único sin seguir, `disk-image-root/etc/180hz`, no es de este trabajo).

## 1. Qué hay y funciona

Dos programas en dos lenguajes en un solo ejecutable estático musl con NVK (`probes/nvk/build.py` los enlaza):

- **Cliente**: una app Vulkan *posee su ventana* (modelo Wayland). `userspace/c/include/constanos_gui_vk.h` (conexión al compositor y ganchos) + `constanos_vk_window.h` (los 5 ganchos que el WSI de NVK necesita) → `nvk_constanos_surface_create` hace un `VkSurfaceKHR`; el swapchain es el estándar (FIFO, B8G8R8A8). Cada imagen es un buffer de VRAM exportado (`BO_EXPORT`) y enviado con `create_gpu_buffer`; un `present` espera el fence de la copia y hace `commit`; la imagen vuelve con el `release` del compositor. Pacing: cada `commit` pide un `frame` callback y el siguiente espera su `done` (máx. 100 ms). Código del WSI: `mesa-port/patches/0001-nvk-constanos.patch` (`wsi_common_headless.c`), `mesa-port/overlay/.../nvkmd_constanos.c`. Cliente de prueba: `probes/nvk/vk_window.c` (`vk_window`).
- **Compositor** `vk_comp` (el binario sale de `vk-comp/` + `probes/nvk/comp_vk.c`):
  - `vk-comp/` — **Rust con std** (musl, staticlib que exporta `main`): sockets `/tmp/gui-0`, evdev (`/dev/input/event0/1`), lanzar programas, títulos (crate `text`, fuentes Noto de `/mnt/usr/share/fonts`), y el gestor de ventanas `gui::compositor::Compositor` usado directamente. Cada fotograma convierte `Compositor::draw_list()` en `cr_op` y llama al renderizador.
  - `probes/nvk/comp_vk.c` + `comp_render.h` + `comp.{vert,frag}` — **C**: Vulkan (instancia, dispositivo, swapchain por el camino directo del WSI = la pantalla), importa los buffers de los clientes **donde están** (fd opaco → `VkBuffer` en la VRAM del cliente, leído como SSBO), una tubería gráfica con un cuadrilátero por operación (color liso o píxel del buffer 1:1). API `cr_*` en `comp_api.h`.
  - Un fotograma en vuelo: cada uno empieza esperando al anterior (`cr_wait`), avisa al gestor (`gpu_frame_done`), procesa imports/drops, construye la lista y presenta.
- **El gestor de ventanas** (`gui/`): buffers GPU (`create_gpu_buffer`, opcode 3 del objeto compositor; `GpuOp::{Import,Drop}`, `draw_list`, `release` diferido hasta que el anfitrión avisa de que terminó el fotograma que pudo leerlo). 62 tests de host. `gui-capi/` (`gui` tras una API C) ya **no lo usa el compositor**; queda solo para el arnés de host.

## 2. Cómo se prueba

| Qué | Comando | Qué demuestra |
|---|---|---|
| Gestor de ventanas | `cd gui && cargo test` | protocolo, buffers GPU, `draw_list`, releases (sabotaje hecho) |
| API C del gestor (solo arnés) | `cd gui-capi && cargo test` | |
| **Renderizador, sin Ryzen** | `probes/nvk/host-comp.sh [dir]` | usa la GPU del host (RTX 3050) con clientes en proceso y compara cada fotograma **píxel a píxel** con una referencia por CPU (8 fotogramas, 12+ mutantes muertos) |
| Cliente + compositor falso en QEMU | `scripts/run-abi-suite.sh gui_fake_comp` | `vk_window` contra un compositor de mentira que importa cada buffer con `BO_IMPORT` y comprueba el protocolo |
| **Compositor real en QEMU** | `scripts/run-abi-suite.sh gui_comp_test` | `vk_comp` + `vk_window` reales con el dispositivo software (no dibuja): flujo, 6 buffers importados/soltados, salida limpia |
| **Metal** | `echo 5 > target/metal/budget; touch build.rs; scripts/metal-run.sh --kconf 'gpu=uapi' scripts/metal-jobs/gpu-comp.sh` | reinicia el PC del usuario; la sesión se retoma sola (ver skill `metal-run`); leer `target/metal/runs/<nonce>/boot.log` |

Stage de binarios tras tocar NVK/WSI/compositor (cada uno son 15 MB; todos enlazan NVK): `python3 probes/nvk/build.py`, `strip -o disk-image-root/bin/<nombre> ~/src/gpu-ref/nvk-probe/<vk-nombre>` (`vk_comp`, `vk_window`, `snake3d`, `vk_draw`, `vk_probe`, `vk_share`), `touch build.rs`, `cargo build`. Un binario viejo en el disco conserva el WSI viejo.

Notas de enlace (importantes): `build.py` enlaza el `staticlib` de Rust con `-Wl,--allow-multiple-definition` porque el `std` de Rust del proyecto y el que lleva dentro NVK (libs Rust de Mesa) definen ambos `rust_eh_personality` (inocuo: `panic=abort`). Prueba de enlace en `probes/nvk/rust-link-probe/`. Con `main` exportado a mano `std::env::args()` sale vacío en musl: se usan `argc/argv`.

## 3. Mediciones en la Ryzen (job `gpu-comp.sh`: `vk_comp` con dos `vk_window`, 1920x1080, el compositor posee el display)

| Boot | Versión | Resultado |
|---|---|---|
| #175 | compositor C, sin pacing de clientes | OK: 900 fotogramas compuestos, ~60 fps, 12 buffers importados/soltados. (En realidad cada vblank servía a **un** cliente alternando: casualidad del reparto 1:1.) |
| #176 | compositor Rust + títulos | FALLO del job (solo 454 fotogramas): los clientes **no estaban regulados** (modo buzón): presentaban a ~120/s y la mitad de sus fotogramas se descartaba sin verse. |
| #177 | + `frame` callbacks en el gancho `commit` | OK (1800 fotogramas a 60 fps, 899 commits regulados por cliente, 0 expiraciones), pero **cada cliente iba a ~32 fps**: el compositor componía en cuanto llegaba *un* commit, así que cada vblank servía a un cliente. |
| #178 | + esperar al flip (`cr_wait_flip`) antes de responder los callbacks | igual (cada cliente a ~30 fps; a los 30 s del tiempo máximo del job se cortó la conexión de los clientes, 896-897 de 900). |
| #179 | + componer una vez por vblank, `COMP_DELAY_MS`=9 ms tras el flip | OK pero **el compositor hace 30 fps** (150 fotogramas cada 5 s; 902 fotogramas para 900 commits por cliente = ya comparten fotograma; 300 commits por cliente tardan 9,8 s). Se pierde un vblank de cada dos. |
| #180 | job barre `COMP_DELAY_MS` 3, 5, 7, 9 (300 fotogramas por cliente cada vez) | **FALLO**: las ejecuciones con 3, 5 y 7 ms terminan a los ~45 ms con «1 fotograma, 0 buffers importados, 2 clientes vistos» (los dos clientes se conectaron y desaparecieron casi a la vez, sin ningún `VK FAIL` visible y con `vk_comp` saliendo con código 0). La de 9 ms funciona (30,9 fps). En QEMU con retardo 3 (headless) todo va bien (127 fotogramas, 12 buffers). El log del boot #180 solo conserva las líneas que el job seleccionaba (grep), por eso no se ve el motivo. |

Lo demás en cada ronda estuvo siempre bien: GPU viva (`gpu_uapi dead=0`, `chans_dead=0`), `gpu_share: sessions=0 storage_allocs=0 syncs=0` al terminar, RM contestando, 12 imports/12 drops. **Nadie ha visto la imagen todavía** (el job no puede): hay que preguntar al usuario qué ve en pantalla (debería: dos ventanas de colores cambiando cada fotograma con barras de título con texto sobre fondo azul grisáceo, una de ellas creciendo a mitad).

## 4. El problema abierto (dos síntomas, quizá una causa)

**A. 30 fps en vez de 60 con componer una vez por vblank.** Hipótesis (no medida): el `PRESENT` se emite demasiado tarde dentro del intervalo (a ~12-13 ms del vblank con `COMP_DELAY_MS`=9) y el flip no engancha en el vblank siguiente, sino en el posterior. Pistas: el camino directo de `snake3d` hace 60 fps emitiendo el `PRESENT` justo después del vblank anterior; la espera `wait_flip`/`flip_pending` del WSI (`nvk_constanos_wait_flip`, `/dev/vblank`, `FLIP_STATE`) y el motor de pantalla (`kernel/src/gpu/`, ver skill `gpu-display`) deciden cuándo "late" un flip. Hay que **medir** cuánto antes del vblank hay que enviar el `PRESENT` (barrido de `COMP_DELAY_MS`, que es lo que intentó el job de #180).

**B. Con retardo 3/5/7 los clientes desaparecen a los ~45 ms.** No hay datos del motivo. Candidatos: el compositor echa a los clientes (`flush()` de `vk-comp/src/lib.rs` suelta a quien no lee sus eventos: `send` con `MSG_DONTWAIT` que no envía todo), un error de protocolo (`take_disconnects`), o los clientes mueren (vk_window crashea o sale). La condición de salida del compositor en esas ejecuciones fue `COMP_EXIT_WHEN_IDLE` (todos los clientes vistos se fueron). Lo siguiente es tener el log completo de esas ejecuciones.

**Qué haría falta para avanzar (en este orden):**
1. Relanzar el job tal como está (`b624e9c`: ya guarda hasta 24 líneas `COMP`/`VK` de cada ejecución y barre en el orden 9, 3, 5, 7 para ver si el fallo sigue al retardo o a la posición). Leer `target/metal/runs/<nonce>/boot.log` (`grep -a gpu-comp`, el log da la vuelta: el resumen se imprime otra vez al final).
2. Si el motivo de B es "no lee sus eventos": ¿por qué dependería del retardo? Mirar capacidad del buffer del socket AF_UNIX del kernel (`usock`, `docs/reference/ipc.md`) y la secuencia de eventos de arranque (`configure`, `focus`, `release`, `delete_id`, `done`).
3. Para A: instrumentar el tiempo entre el vblank y el `PRESENT` y si el flip se acepta (`gpu_flip:` en `/proc/kdebug`, `NVG_IOC_FLIP_STATE`), y probar el compositor con 1 solo cliente (¿60 fps?) para separar el pacing del reparto entre clientes.

## 5. Decisiones de diseño que no conviene deshacer sin motivo

- Se eligió que la **app posee la ventana** (no el WSI); sin extensión Vulkan nueva: NVK y la app son un solo ejecutable, así que `nvk_constanos_surface_create` es una función C normal.
- Compositor **directo a GPU** (sin etapa intermedia por CPU); el compositor por CPU (`userspace/src/bin/compositor.rs`) sigue siendo el de las máquinas sin GPU.
- La máquina del compositor en **Rust** y el renderizador en **C** (la prueba de enlace mostró que es viable); `gui-capi` quedó solo para el arnés `host-comp.sh`.
- Un fotograma en vuelo y subidas de CPU (ventanas de pool, títulos, cursor) a buffers visibles por el host; los buffers de los clientes se leen **en su VRAM**.

## 6. Pendiente más allá del problema abierto

`snake3d` ventanado y con teclado (rebanada 4 del plan), dos apps reales a la vez en la Ryzen, medir el rendimiento cuando la GPU está en P8 (el primer fotograma tras un reposo va ~9x lento: `docs/gpu/g5-graphics-stack-plan.md` "relojes"), sincronización explícita con timelines compartidas (`SYNC_EXPORT`) en vez de esperar el fence en CPU, y limpiar los mutantes/pruebas de `vk-comp/` (el crate no tiene tests de host propios: la lógica está en `gui`).
