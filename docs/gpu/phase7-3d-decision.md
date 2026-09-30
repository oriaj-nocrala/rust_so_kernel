# Fase 7: decisión de la pila 3D

Puerta, no trabajo (`docs/gpu/gpu-plan.md`, "Fase 7"). Este documento compara los dos caminos del plan y un tercero que salió de la medición, con cifras sacadas de las fuentes fijadas y de este kernel. **No se ha ejecutado código de GPU para escribirlo**: todo es medición estática de fuentes (marcado "medido") o estimación (marcado "est."; la base de cada estimación está en la tabla).

## Decisión

1. **El camino es NVK con un backend `nvkmd` propio de constanos** (camino C, abajo), no la uAPI DRM de nouveau y no los módulos abiertos de NVIDIA.
2. **La pared no está en la GPU, está en el espacio de usuario.** Lo que falta en el lado GPU es acotado y verificable sin Mesa (GR + una clase de cómputo sobre nuestro canal). Lo que falta en el lado usuario (ABI Linux, enlazado, C++, Rust de NAK) es lo que decide el coste, y una parte ya tiene plan propio por otras razones.
3. **No se empieza el 3D ahora.** Orden de puertas: G1 ABI Linux para `std` (proyecto ya apuntado, `rust_std_gaps`/`posix_compat_pending`) → G2 sonda de cómputo en el kernel (7a, abajo) → G3 decidir enlazado (estático o `dlopen`) → G4 backend `nvkmd` + Mesa. G2 se puede hacer ya y no depende de G1.
4. **Los módulos abiertos de NVIDIA quedan descartados** (camino B): 1,15 M de líneas de C que duplican lo que `nvgpu` ya hace, y un espacio de usuario cerrado, glibc dinámico y atado a la versión exacta del módulo.

## Fuentes (fijadas; ver `~/src/gpu-ref/PINNED`)

| Qué | Versión |
|---|---|
| Linux (nouveau, uAPI) | v7.2.2 |
| open-gpu-kernel-modules | 570.144 (= el firmware GSP que arranca constanos, D1) |
| Mesa (NVK, NAK, Zink) | `main` 20f48abe (2026-09-29, 26.3.0-devel), clon *sparse* nuevo en `~/src/gpu-ref/mesa` |
| Host (solo para `ldd`/`strace`) | driver NVIDIA 610.57.04, Mesa 26.2.1 (sin NVK ni `libvulkan`: no hay Vulkan que trazar) |

## Lo que pide NVK al kernel (medido)

`src/nouveau/winsys` (1311 líneas) y `src/nouveau/vulkan/nvkmd/nouveau` (1471) son todo el contacto con el kernel. Llaman a:

- **ioctls de nouveau** (`include/uapi/drm/nouveau_drm.h`, 586 líneas): `GETPARAM`, `CHANNEL_ALLOC/FREE`, `VM_INIT`, `VM_BIND` (`MAP`/`UNMAP`, `SPARSE`, `RUN_ASYNC`), `EXEC` (con `NO_PREFETCH`), `GEM_NEW/INFO/CPU_PREP`, `GET_ZCULL_INFO`, `GEM_PUSHBUF` (camino viejo) y `NVIF` (`NEW`/`DEL`/`SCLASS`: crea los objetos de clase sobre el canal).
- **DRM núcleo vía libdrm**: `drmGetVersion`, `drmGetDeviceFromDevId` (lee sysfs), `drmCloseBufferHandle`, `drmPrimeHandleToFD/FDToHandle`, `drmSyncobjCreate/Wait/Destroy`.
- **Clases de objeto** (`nouveau_context.c:154-195`): NVK pide a `SCLASS` la lista del canal y elige la mayor por sufijo: copia `0xb5`, 2D `0x2d`, 3D `0x97`, M2MF `0x40`/`0x39`, cómputo `0xc0`. En GA106: 3D `AMPERE_B` = `0xc797`, cómputo `AMPERE_COMPUTE_B` = `0xc7c0` (`nvkm/engine/gr/ga102.c:182-183`), copia `0xc7b5` (`ce/ga100.c:83`, la que ya usamos).

La capa `nvkmd` es un **punto de extensión de Mesa**: `nvkmd.h` define la interfaz y hoy solo existe el backend `nouveau` (DRM). Un backend de constanos son ~1,5 k líneas de C (tamaño del de nouveau, medido) contra nuestra propia interfaz, **sin** libdrm, sin nodos `/dev/dri`, sin sysfs y sin dma-buf DRM.

## Tabla de requisitos

Tamaños: medidos. Coste: **est.** en S (≤ 1 sesión), M (unas pocas), L (muchas), XL (proyecto). La base de cada estimación va en la columna "qué hay".

### Lado kernel/GPU (caminos A y C)

| Requisito | Qué hay | Falta | Coste |
|---|---|---|---|
| Contexto GR (`PROMOTE_CTX`, buffers de contexto por canal) | RM de larga vida, alloc de objetos, VASpace externo con nuestras tablas, canal GPFIFO 0xc56f (6a-6d) | Portar `nvkm/subdev/gsp/rm/r535/gr.c` (359 líneas) + `GET_CTX_BUFFER_INFO`; los buffers salen de nuestra VRAM/VA | M |
| Clases 3D `0xc797` y cómputo `0xc7c0` sobre el canal | Solo copia `0xc7b5` | Alloc por RM, métodos y QMD desde los manuales (`open-gpu-doc/classes/ampere`) | M |
| Canal por proceso y VM por proceso | Un canal y un VASpace, creados en el arranque | Crear/destruir en caliente (RM ya vive tras 6d), tabla de páginas por proceso (`nvgpu::mmu`, 232 tests) | M |
| Mapeo de BOs (`VM_BIND`) | `nvgpu::mmu` mapea PTE 4 KiB/2 MiB, sistema y VRAM | Semántica de MAP/UNMAP con offsets y `SPARSE`. nouveau usa `nouveau_uvmm.c` (2005 líneas, sobre GPUVM de Linux); lo esencial es bastante menos. Sin `SPARSE` al principio | M-L |
| `mmap` de BOs en el proceso | `sys_mmap`: anónimo, memfd, `MAP_SHARED` | `mmap` de un handle de dispositivo (VRAM por BAR1 WC y memoria de sistema) | M |
| Envío + sincronización (`EXEC`, syncobj temporal) | Fence en memoria del host y MSI de fin de copia (vector 7, 150/150 en 6d) | Cola de envíos por proceso, syncobj como objeto de kernel con espera bloqueante correcta. nouveau: `exec.c` 408 + `sched.c` 523 + `fence.c` 541 | L |
| Errores por canal | RC events medidos en 6d (MMU_FAULT_QUEUED, NOCAT, RC_TRIGGERED; RM sigue vivo) | Mapear a "contexto perdido" hacia el proceso. La cola de estado de RM se sondea a 10 Hz (GSP-RM no interrumpe): errores con hasta ~100 ms de latencia | S-M |
| Presentar en pantalla | Scanout propio desde VRAM (5.3), CE medido | Un BO exportable por handle vía `SCM_RIGHTS` (ya existe en `usock`) que el compositor use como origen del scanout | M |

Solo en el **camino A** (uAPI DRM de nouveau): además `/dev/dri/renderD128` y `card0`, sysfs (`/sys/dev/char`, `/sys/class/drm`), `GET_CAP`/`GEM_CLOSE`/`PRIME`/`SYNCOBJ_*` de DRM núcleo y libdrm portado (L). Es trabajo que el camino C no hace.

### Lado usuario (caminos A y C; los dos usan Mesa)

| Requisito | Qué hay (medido) | Falta | Coste |
|---|---|---|---|
| Syscalls Linux | 77 números (5 propios, 400-404); el dispatcher no tiene `mprotect`, `mremap`, `madvise`, `sigaltstack`, `getrandom`, `gettid`, `tgkill`, `clock_nanosleep`, `openat`/`newfstatat`, `pread64`/`pwrite64`, `readv`, `pipe2`, `dup3`, `eventfd2`, `ppoll`, `prlimit64`, `uname`, `set_robust_list`; `nanosleep` toma ns, no `timespec` | Lista de `rust_std_gaps`: la traza de un programa glibc arrancando en el host (`nvidia-smi`, hasta su fallo) toca `mprotect` ×20, `getrandom`, `clone3`, `rseq`, `set_robust_list`, `prlimit64`, `eventfd2`, `pipe2`, `madvise`, `gettid` | M-L |
| `futex` | Solo `WAIT`/`WAKE`, **timeout ignorado** (`sync.rs:27`) | `WAIT_BITSET`, timeouts, requeue: Mesa usa hilos (colas de envío, `util_queue`) y `cnd_timedwait` | M |
| `clone` | ABI propia `(entry, stack, tcb)` para mlibc (`process_ctl.rs:373`) | Hilos con ABI Linux o que Mesa use el pthread de mlibc (hoy funciona) | S si mlibc |
| Enlazado | **Sin enlazado dinámico**; el cargador rechaza `ET_DYN` | Zink hace `dlopen("libvulkan.so.1")` (`zink_screen.c:3484`) y el cargador de Mesa `dlopen` de los drivers (`loader.c:899`). Sin `ld.so`: parchear Zink para enlazar `vk_icdGetInstanceProcAddr` de NVK directamente y usar solo Vulkan/Zink estático. El cargador Vulkan de Khronos no se ha medido | L (parche) o XL (`ld.so`) |
| C++ | Ninguno en el árbol | Zink necesita el frontend GLSL: 53 477 líneas de C++ en 57 ficheros (`src/compiler/glsl`). **NVK solo** (Vulkan, sin GL) no lo necesita: NVK 35 402 líneas de C, NIR 121 938, SPIR-V 20 434, runtime Vulkan 56 885 | L si Zink |
| Compilador NAK | — | **53 988 líneas de Rust** dentro de Mesa, con `std` (`rustc-hash`). Hay que enlazarlo con mlibc; el `std` que corre hoy es el de musl (estático). Dos libcs en un binario: **riesgo sin resolver** (o un target `std` propio, o NAK sin `std`) | L, riesgo alto |
| WSI | Compositor propio estilo Wayland | Mesa trae `headless`, `display`, `drm`, `wayland`, `x11` (`src/vulkan/wsi`, 19 018 líneas); ninguno habla con nuestro compositor. Un WSI propio sobre BO exportable | M |
| Construir Mesa | — | `meson` cruzado para un target con mlibc, estático | L |

## Camino A: NVK + Zink sobre uAPI DRM de nouveau

Todo lo de las dos tablas **más** la capa DRM. Tiene una ventaja real: Mesa sin tocar el backend. Pero el backend es de 1,5 k líneas y la capa DRM (nodo, sysfs, libdrm, syncobj de núcleo) cuesta más que reescribirlo. **Descartado en favor de C.**

## Camino B: módulos abiertos de NVIDIA sobre una capa `os_*`

Medido en `open-gpu-kernel-modules` 570.144:

| Zona | Líneas C/H |
|---|---|
| `src/nvidia` (RM del lado CPU) | 1 152 607 |
| `src/common` | 449 641 |
| `src/nvidia-modeset` | 109 941 |
| `kernel-open/nvidia-uvm` | 145 166 |
| `kernel-open/nvidia` (pegamento con Linux: `nv.c` 5927, `os-interface.c` 2682, `nv-pci`, `nv-mmap`, `nv-dma`, `nv-vm`) | 40 786 |

- La capa `os_*` son 141 funciones (`os-interface.h`), pero el pegamento real contra Linux son ~13 k líneas que habría que reescribir: PCI, DMA, mmap, interrupciones, procfs, hilos.
- Los ioctls de usuario son 26 `NV_ESC_RM_*` (`nv_escape.h`): `RM_ALLOC`, `RM_CONTROL`, `MAP_MEMORY`, etc.
- **Sustituye a `nvgpu`**: el RM del lado CPU arranca el GSP él mismo y llevaría el display. Se pierde el modeset propio (D6), que es lo medido y funciona.
- **Espacio de usuario cerrado y dinámico**: `libcuda.so` (host, 610.57.04) depende de `libpthread`, `libm`, `libc`, `libdl`, `librt` de glibc y del cargador. Hace falta `ld.so` + glibc completos. El módulo comprueba la versión exacta (`NV_ESC_CHECK_VERSION_STR`, `nv.c:2535`): nuestro firmware es 570.144, el del host 610.57.04, así que habría que bajar el espacio de usuario 570.144.
- Da CUDA, pero ningún fallo del blob se puede depurar (principio 2 del plan: sin cita, no entra).

**Descartado.** Solo lo reabriría alguien que necesite CUDA y acepte `ld.so` + glibc.

## Camino C: NVK con backend `nvkmd` propio (recomendado)

Como el A en el lado GPU, sin la capa DRM: constanos expone una interfaz mínima (un `/dev/gpu` con ioctls de canal, VM, BO, envío y espera, hechos sobre `nvgpu::rm`/`mmu`/`chan`), y un backend `nvkmd_constanos` de ~1,5 k líneas (est., por tamaño de `nvkmd/nouveau`) la habla. Reutiliza todo lo hecho en 4 y 6 y no depende de `ld.so`.

Riesgos abiertos, en orden:
1. **NAK (Rust) con mlibc** (dos libcs; ver tabla). Es lo que puede matar el camino; hay que resolverlo antes de invertir en el backend.
2. Enlazado sin `dlopen` (parche a Zink; o NVK a pelo, solo Vulkan).
3. `futex` con timeouts y `mprotect`: bloquean cualquier hilo de Mesa.
4. La clase 3D no está probada con nuestro cliente RM (7a).

## 7a: sonda de cómputo en el kernel (siguiente paso de GPU, sin Mesa)

Lo único de los riesgos de arriba que se puede probar ya, con la escalera de 6c: reutilizar el canal de `gpu=copy`, promover el contexto GR, crear un objeto `0xc7c0` y ejecutar un cómputo trivial (un hilo que escribe un valor en memoria del host; `clc7c0.h` + QMD de `open-gpu-doc/classes/ampere`), con el mismo contrato de fence y verificación cruzada de 6d (borrar el destino antes, leer por otro camino). Si falla en GR/`PROMOTE_CTX`, el camino C tiene un problema que ningún trabajo de userspace arregla. Nivel de arranque `gpu=compute`. Referencias de lectura: nouveau `r535/gr.c` (359 líneas) y `nvkm/engine/gr/ga102.c`.

## Qué no se midió

- El cargador Vulkan de Khronos (no está en el clon) y si NVK enlaza sin él.
- Cuánto de `std` usa realmente NAK ni si compila `no_std`.
- Nada de esto se ha ejecutado en la Ryzen.
- Los costes S/M/L/XL son estimaciones mías, no mediciones.


## Resultados de G3 (2026-09-30): NAK sobre musl estático corre en constanos

Sonda: `probes/nak/` (receta en su `README.md`). Mesa `main` 20f48abe compilado **cruzado a `x86_64-linux-musl`, estático**.

- **Se compila entero:** NIR, util, el runtime de Vulkan, NVK y NAK (Rust con el `std` de musl), 320 pasos de Meson en 27 s. Los fallos fueron solo cabeceras de terceros que faltaban (libudev, libelf, spirv-tools, libdisplay-info, `xf86drm.h`) y dos funciones de pthread de glibc 2.30 que las cabeceras de libstdc++ declaran y musl no. Ninguno era del ABI ni de constanos.
- **Se ejecuta en constanos:** un binario estático musl de 13,7 MB (sin símbolos) construye un shader con `nir_builder`, lo pasa por `nak_preprocess_nir`, `nak_postprocess_nir` y `nak_compile_shader` para SM86 y obtiene 7 instrucciones SASS, 112 bytes, huella FNV-1a `428b75c4542e3770`. **Idéntico al del host** (glibc nativo y musl en el host). Ninguna syscall sin implementar (`ENOSYS`); el `std` de Rust (panics con mensaje, asignador, `OnceLock`, `HashMap`) funciona.
- **El riesgo de las dos libcs desaparece:** no hay que enlazar NAK con mlibc. Toda la pila 3D (Mesa en C y NAK en Rust) va contra **musl**, una sola libc, y G1 ya hace que el kernel la hable. mlibc sigue siendo la libc de BusyBox, DOOM y Quake.
- **`std` de NAK:** solo lo puro (`mem`, `slice`, `ptr`, `cmp`, `fmt`, `BinaryHeap`, `HashMap`, `OnceLock`, atómicos, `RefCell`, `io::stderr`); no usa hilos, ficheros, red ni procesos. `no_std + alloc` sería posible pero ya no hace falta.
- **Lo que el enlace final de `libvulkan_nouveau` todavía pide compilado contra musl:** `libdrm` (lo sustituye el backend `nvkmd` de G4), `libelf` (cubin), `libudev` y `libdisplay-info` (WSI de pantalla; se puede evitar), `SPIRV-Tools` (C++) y un runtime de C++. Para la sonda bastó `libstdc++.a` de gcc (compilada para glibc) con musl, porque solo se usan las pocas piezas de C++ de `util` (ASTC, `qsort`); **excepciones e hilos de C++ no están probados**. Para Zink haría falta libc++ o libstdc++ construidos contra musl.
- **Sin medir todavía:** `dlopen` (Zink); si todo va enlazado estático en un solo proceso no hace falta, pero no está probado. El cargador Vulkan de Khronos. El tiempo de compilación de shaders reales (el de la sonda es un shader de 7 instrucciones).
- **Decisión de G3:** enlazado **estático, todo contra musl**. Siguiente puerta: **G4**, el backend `nvkmd` de constanos (~1,5 k líneas estimadas), que reemplaza a libdrm.
