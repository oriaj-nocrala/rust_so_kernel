# G5 capa 4 (compositor en GPU): estado (traspaso, 2026-09-30; ritmo resuelto en la Ryzen #182)

Para quien retome (otro modelo o persona): qué hay hecho, cómo se prueba, qué se midió en la Ryzen y cómo se resolvieron el ritmo de fotogramas (30 fps) y los clientes que desaparecían (§4). Diseño y rebanadas: `docs/gpu/g5-graphics-stack-plan.md` "Capa 4". Reglas y mapa de código: skill `gpu-g5` (sección "Layer 4"). Lo único sin seguir del repo, `disk-image-root/etc/180hz`, no es de este trabajo.

## 1. Qué hay y funciona

Dos programas en dos lenguajes en un solo ejecutable estático musl con NVK (`probes/nvk/build.py` los enlaza):

- **Cliente**: una app Vulkan *posee su ventana* (modelo Wayland). `userspace/c/include/constanos_gui_vk.h` (conexión al compositor y ganchos) + `constanos_vk_window.h` (los 5 ganchos que el WSI de NVK necesita) → `nvk_constanos_surface_create` hace un `VkSurfaceKHR`; el swapchain es el estándar (FIFO, B8G8R8A8). Cada imagen es un buffer de VRAM exportado (`BO_EXPORT`) y enviado con `create_gpu_buffer`; un `present` espera el fence de la copia y hace `commit`; la imagen vuelve con el `release` del compositor. Pacing: cada `commit` pide un `frame` callback y el siguiente espera su `done` (máx. 100 ms). Código del WSI: `mesa-port/patches/0001-nvk-constanos.patch` (`wsi_common_headless.c`), `mesa-port/overlay/.../nvkmd_constanos.c`. Cliente de prueba: `probes/nvk/vk_window.c` (`vk_window`).
- **Compositor** `vk_comp` (el binario sale de `vk-comp/` + `probes/nvk/comp_vk.c`):
  - `vk-comp/` — **Rust con std** (musl, staticlib que exporta `main`): sockets `/tmp/gui-0`, evdev (`/dev/input/event0/1`), lanzar programas, títulos (crate `text`, fuentes Noto de `/mnt/usr/share/fonts`), y el gestor de ventanas `gui::compositor::Compositor` usado directamente. Cada fotograma convierte `Compositor::draw_list()` en `cr_op` y llama al renderizador.
  - `probes/nvk/comp_vk.c` + `comp_render.h` + `comp.{vert,frag}` — **C**: Vulkan (instancia, dispositivo, swapchain por el camino directo del WSI = la pantalla), importa los buffers de los clientes **donde están** (fd opaco → `VkBuffer` en la VRAM del cliente, leído como SSBO), una tubería gráfica con un cuadrilátero por operación (color liso o píxel del buffer 1:1). API `cr_*` en `comp_api.h`.
  - Un fotograma en vuelo: cada uno empieza esperando al anterior (`cr_wait`), avisa al gestor (`gpu_frame_done`), procesa imports/drops, construye la lista y presenta; responde los `frame` callbacks de lo que compuso, espera al flip (`cr_wait_flip`) y compone el siguiente `COMP_DELAY_MS` (2) después.
  - Los programas que se le pasan se lanzan en un hilo (`launch`) y se cosechan por pid; `COMP_EXIT_WHEN_IDLE` espera a que todos hayan salido. Cada 5 s y al final, una línea `COMP pace` (dónde va el tiempo de cada composición, separación de los flips, plazo del `PRESENT`); la línea final da los fotogramas «with clients».
- **El gestor de ventanas** (`gui/`): buffers GPU (`create_gpu_buffer`, opcode 3 del objeto compositor; `GpuOp::{Import,Drop}`, `draw_list`, `release` diferido hasta que el anfitrión avisa de que terminó el fotograma que pudo leerlo). 62 tests de host. `gui-capi/` (`gui` tras una API C) ya **no lo usa el compositor**; queda solo para el arnés de host.

## 2. Cómo se prueba

| Qué | Comando | Qué demuestra |
|---|---|---|
| Gestor de ventanas | `cd gui && cargo test` | protocolo, buffers GPU, `draw_list`, releases (sabotaje hecho) |
| API C del gestor (solo arnés) | `cd gui-capi && cargo test` | |
| **Renderizador, sin Ryzen** | `probes/nvk/host-comp.sh [dir]` | usa la GPU del host (RTX 3050) con clientes en proceso y compara cada fotograma **píxel a píxel** con una referencia por CPU (8 fotogramas, 12+ mutantes muertos) |
| Cliente + compositor falso en QEMU | `scripts/run-abi-suite.sh gui_fake_comp` | `vk_window` contra un compositor de mentira que importa cada buffer con `BO_IMPORT` y comprueba el protocolo |
| **Compositor real en QEMU** | `scripts/run-abi-suite.sh gui_comp_test` | `vk_comp` + `vk_window` reales con el dispositivo software (no dibuja): flujo, 6 buffers importados/soltados, salida limpia |
| **Metal** | `echo 5 > target/metal/budget; touch build.rs; scripts/metal-run.sh --kconf 'gpu=uapi' scripts/metal-jobs/gpu-comp.sh` | reinicia el PC del usuario; la sesión se retoma sola (ver skill `metal-run`); leer `target/metal/runs/<nonce>/boot.log` (`awk '/---- summary/{f=1} f' boot.log \| grep -a gpu-comp`). Barre `COMP_DELAY_MS` 2, 0, 4, 6 con dos clientes y 2 con uno; por ejecución guarda arranques, cómo acabó cada programa, `COMP pace (all)` y `gpu_flip:` antes/después |

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
| #181 | clientes lanzados en un hilo + arreglo del kernel (`fork`/`exec` fuera de `SCHEDULER`); barrido 9, 3, 5, 7 + un cliente a 5 | **B resuelto**: las 5 ejecuciones limpias, cada `spawn` ~250 ms. **A medido**: 9/5/7 ms -> 30 fps (2 vblanks por flip, `present` 1,3 ms a 9 ms pero 5-6 ms a 5/7); 3 ms -> 58,1 fps (10 de 307 flips tarde); un cliente a 5 ms -> 30 fps (no es el reparto). |
| #182 | `frame` callbacks respondidos al componer (tras `cr_frame`), barrido 2, 0, 4, 6 + un cliente a 2 | **60,0 fps** a 2 ms (303 de 304 flips <20 ms, el otro es el arranque; `present` 0,3 ms media / 0,4 máx.), 60,0 a 0 ms, 60,0 con un cliente; 4 y 6 ms siguen en ~30 fps (`present` 5,3 ms; el `PRESENT` más tardío que llegó a tiempo salió a 9,7 ms del flip, el más temprano que no, a 8,6). Todo limpio: 12/12 buffers, `gpu_share` 0/0/0, GPU viva. |

Lo demás en cada ronda estuvo siempre bien: GPU viva (`gpu_uapi dead=0`, `chans_dead=0`), `gpu_share: sessions=0 storage_allocs=0 syncs=0` al terminar, RM contestando, 12 imports/12 drops. **Nadie ha visto la imagen todavía** (el job no puede): hay que preguntar al usuario qué ve en pantalla (debería: dos ventanas de colores cambiando cada fotograma con barras de título con texto sobre fondo azul grisáceo, una de ellas creciendo a mitad).

## 4. Los dos síntomas: causas y arreglos

**B. Los clientes desaparecían a los ~45 ms (#180).** `vk_comp` lanzaba los programas con `Command::spawn` en el hilo principal, y `spawn` espera a que el `exec` del hijo cargue el binario (15 MB estáticos; segundos cuando la caché de bloques ext2, 32 MiB, ya no lo tiene: no caben `vk_comp` + `vk_window`). Mientras, el compositor no leía el socket: el primer cliente esperaba 5 s su `configure`, se iba, y `COMP_EXIT_WHEN_IDLE` cerraba todo. Reproducido en QEMU (2ª ejecución de un bucle). Además, el kernel copiaba el espacio de direcciones en `fork` y liberaba el viejo en `exec` **con `SCHEDULER` tomado**: con un proceso grande todas las CPUs se paraban segundos (gdb: tres CPUs girando en `local_scheduler()` y la cuarta en `release_user_pages`). Arreglos: programas en un hilo (`launch`, `reap_children`, salida por inactividad solo cuando no queda ninguno), y `fork_impl`/`sys_exec` hacen ese trabajo fuera del lock (`docs/reference/processes-and-scheduling.md`). Un `SIGKILL` que se veía al hilo lanzador era el `exit_group` final de `vk_comp` (benigno); el mensaje de muerte por señal dice ahora qué señal fue.

**A. 30 fps (#179-#181).** El `PRESENT` tiene que salir a menos de ~9 ms del flip anterior visto (lo que pasa de ahí aterriza un vblank tarde). Al responder los `frame` callbacks cuando aterrizaba el flip, los clientes dibujaban su siguiente fotograma justo cuando el compositor componía: la GPU se repartía y el `present` del compositor (que espera en CPU a su copia al buffer de pantalla) pasaba de 1,3 a 5-6 ms, lo que lo empujaba más allá del plazo. Ahora los callbacks se responden al componer (como Weston): los clientes dibujan mientras el compositor espera al vblank, y la siguiente composición sale sola en la GPU. Con `COMP_DELAY_MS` = 2: 60,0 fps (#182).

**Queda sin explicar:** por qué con 4 y 6 ms el `present` vuelve a costar 5 ms (y 30 fps) si los clientes ya acabaron, y por qué el plazo es ~9 ms tras ver el flip y no ~16 (¿se ve el flip tarde respecto al vblank real? `flip_done` exige que haya pasado una interrupción de vblank). No afecta al valor por defecto.

**Siguiente (por orden):** que el usuario mire la pantalla durante `gpu-comp.sh` (dos ventanas de colores sobre fondo azul grisáceo, títulos con texto, cambiando cada fotograma, creciendo a mitad); `snake3d` ventanado (rebanada 4); `reap_zombie` y `retiring` aún liberan espacios de direcciones bajo `SCHEDULER`; la caché ext2 (32 MiB) no cabe dos binarios Vulkan.

## 5. Decisiones de diseño que no conviene deshacer sin motivo

- Se eligió que la **app posee la ventana** (no el WSI); sin extensión Vulkan nueva: NVK y la app son un solo ejecutable, así que `nvk_constanos_surface_create` es una función C normal.
- Compositor **directo a GPU** (sin etapa intermedia por CPU); el compositor por CPU (`userspace/src/bin/compositor.rs`) sigue siendo el de las máquinas sin GPU.
- La máquina del compositor en **Rust** y el renderizador en **C** (la prueba de enlace mostró que es viable); `gui-capi` quedó solo para el arnés `host-comp.sh`.
- Un fotograma en vuelo y subidas de CPU (ventanas de pool, títulos, cursor) a buffers visibles por el host; los buffers de los clientes se leen **en su VRAM**.

## 6. Pendiente

`snake3d` ventanado y con teclado (rebanada 4 del plan), dos apps reales a la vez en la Ryzen, medir el rendimiento cuando la GPU está en P8 (el primer fotograma tras un reposo va ~9x lento: `docs/gpu/g5-graphics-stack-plan.md` "relojes"), sincronización explícita con timelines compartidas (`SYNC_EXPORT`) en vez de esperar el fence en CPU, y limpiar los mutantes/pruebas de `vk-comp/` (el crate no tiene tests de host propios: la lógica está en `gui`).
