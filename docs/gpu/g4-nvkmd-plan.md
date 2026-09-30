# G4: backend `nvkmd` de constanos y NVK sobre la GA106

Puerta G4 de `docs/gpu/phase7-3d-decision.md`. G1 (ABI Linux), G2 (sonda de cómputo) y G3 (NAK + Mesa en C contra musl estático, corre en constanos) están hechas. Aquí: lo que hay que construir para que NVK hable con nuestra GPU, y en qué orden.

## Qué exige NVK (contrato `nvkmd`, `src/nouveau/vulkan/nvkmd/nvkmd.h`)

| Nivel | Operaciones | Qué necesita del kernel |
|---|---|---|
| `pdev` | `create_dev`, `get_vram_used` | `nv_device_info` (SM, clases, TPC/GPC, VRAM, BAR), VRAM en uso |
| `dev` | `alloc_mem`, `alloc_tiled_mem`, `alloc_va`, `create_ctx`, `get_gpu_timestamp` | objetos de memoria (VRAM o sistema), reserva de VA, contextos, reloj de GPU |
| `mem` | `map`, `unmap`, `overmap`, `sync_to_gpu/from_gpu` | mapear en la CPU (solo memoria de sistema, ver abajo) |
| `va` | `bind_mem`, `unbind` | poblar/vaciar tablas de páginas de la GPU en caliente |
| `ctx` | `exec`, `bind`, `wait`, `signal`, `flush`, `sync` | canal de GPU por contexto; lista de pushes `(VA, bytes)`; esperas y señales de sincronización |

NVK hoy usa DRM de nouveau (`VM_INIT/VM_BIND/EXEC`, syncobj). Nosotros damos un dispositivo `/dev/nvgpu` con el mismo modelo y sin DRM/libdrm.

## Restricciones medidas que fijan el diseño

- **La CPU no lee VRAM.** BAR1 es de solo escritura tras GSP-RM y PRAMIN no ve las stores del SM. Por tanto no hay memoria "visible por host" en VRAM: `bar_size_B = 0` en la info del dispositivo, y NVK ya trata ese caso (VRAM solo del dispositivo, subidas por memoria de sistema + copy engine). La memoria de sistema sí se mapea en la CPU.
- **Toda la pila de GPU vive hoy en el arranque** (`gpu=compute`): tablas de páginas locales a `compute::run`, un canal GR fijo, `Submitter` con un bloque de VRAM fijo. Solo `copy` deja un `Runtime`. G4 obliga a hacer persistente ese estado (tablas + asignador de VRAM + canal + cerrojo).
- **No hay GPU en QEMU.** Cada prueba sobre hardware cuesta una vuelta en la Ryzen. De ahí el orden: contrato y NVK primero contra un dispositivo *software*.
- El VA space es único y de RM-externo (tablas nuestras); GR necesita el contexto dorado creado en el arranque. Primera versión: **un solo proceso de GPU a la vez** (`open` exclusivo, como un DRM master) y un VA space global.

## Interfaz del kernel (`/dev/nvgpu`, ioctl)

Cabecera única `nvgpu/uapi/nvgpu.h` (la usan el kernel, en espejo Rust con tests de layout contra clang, y el backend de Mesa).

- `NVG_INFO` -> `struct nv_device_info` (la que espera NAK/NVK) + VRAM total/en uso.
- `NVG_BO_CREATE {size, flags: SYSTEM|VRAM}` -> `handle`; `mmap(fd, offset = handle << 12)` para BOs de sistema; `NVG_BO_FREE`.
- `NVG_VA_ALLOC {size, align}` -> `va` (asignador de VA del kernel; NVK guarda su propio montón, pero el kernel valida solapes); `NVG_VA_BIND {va, handle, offset, size, pte_kind}` / `NVG_VA_UNBIND {va, size}` (tablas + invalidación de TLB).
- `NVG_CTX_CREATE {engines}` -> `ctx`; `NVG_EXEC {ctx, pushes[] = {va, bytes, flags}, waits[], signals[]}`; `NVG_CTX_DESTROY`.
- `NVG_SYNC_CREATE/DESTROY`, `NVG_SYNC_SIGNAL {handle, value}`, `NVG_SYNC_WAIT {handles/values, timeout}`, `NVG_SYNC_QUERY`: temporizadores de 64 bits (semáforos temporizados de Vulkan). El kernel espera las `waits` en la CPU antes de encolar (sin esperas de semáforo en la GPU) y, tras los pushes, encola una liberación de semáforo con el valor de la señal; el estado se avanza al consultar/esperar.
- `NVG_TIMESTAMP` -> reloj global de la GPU (PTIMER).

## Rodajas, cada una con su verificación

| Rodaja | Contenido | Verificación |
|---|---|---|
| **G4a** | Cabecera UAPI + espejo Rust + `/dev/nvgpu` con **dispositivo software** (BOs en páginas de sistema, tabla de VA validada, `EXEC` registra los pushes y completa al instante, timelines reales). Lógica pura en un crate con tests de host. | tests de host; programa C musl en QEMU que recorre toda la interfaz; sabotaje |
| **G4b** (hecha 2026-09-30) | Backend `nvkmd_constanos` en C (`mesa-port/`) + `vk_sync_type` de constanos; NVK estático musl contra el dispositivo software. | `scripts/run-vk-probe.sh`: en QEMU enumera el dispositivo, crea dispositivo lógico, memoria (sistema y VRAM), un pipeline de cómputo compilado por NAK, envío, fence, semáforos binarios y de línea de tiempo |
| **G4c** (hecha y medida en la Ryzen 2026-09-30, boot #144) | Estado de GPU persistente (tablas, VRAM, canal GR, cerrojo) y el dispositivo *hardware* detrás de la misma interfaz: `VA_BIND` en caliente, `EXEC` real, fence de contexto. Nivel `gpu=uapi`. | Ryzen: `nvgpu_hw_test` (un programa C con shaders reales, lectura por otro camino), job `gpu-uapi.sh` |
| **G4d** (hecha y medida en la Ryzen 2026-09-30, boot #145) | Un dispatch de cómputo Vulkan de verdad en la Ryzen (SPIR-V -> NAK -> GPU) y verificación de resultados. | Ryzen |
| **G4e** | Varios contextos (canales en caliente), cola de copia, robustez (RC tras fallo), y luego gráficos (clase 3D `0xc797`) y presentación. | según el caso |

## Bloqueo dentro de `ioctl` (aplazado, rodaja propia después de G4d)

Hoy el kernel **nunca bloquea** en `SYNC_WAIT`/`EXEC`: devuelven `-EAGAIN` y el espacio de usuario duerme (`nanosleep`) y reintenta. Motivo: `sys_ioctl` llama a `FileHandle::ioctl` con el cerrojo de la tabla de fds tomado (los hilos comparten la tabla), bloquear es aparcar con `rip -= 2` (un `-> !`), y los syscalls entran con IF=0, así que esperar en un bucle deja sin CPU a quien debería señalar. Para bloquear de verdad: (1) `FileHandle::ioctl` devuelve "bloquearía" y `sys_ioctl` actúa como `sys_read` (suelta el cerrojo, registra la espera, aparca con reinicio); (2) una cola de espera de timelines despertada por `SYNC_SIGNAL` y por los fences que se completan; (3) `/dev/nvgpu` con `poll`. La cabecera no cambia.

## Riesgos

- **Canales en caliente** (G4e): cada canal GR necesita buffers de contexto en VRAM y llamadas RM en tiempo de ejecución; hoy solo se hace un canal en el arranque. G4c/G4d usan ese único canal.
- **VRAM sin lectura desde CPU**: las lecturas de resultados pasan por un shader de copia a sistema (ya usado en 7b).
- **C++ y `SPIRV-Tools` contra musl** para enlazar NVK completo (G3 lo dejó anotado).
- Cada vuelta en la Ryzen consume budget de reanudación: juntar todos los diagnósticos en el mismo arranque.

## Resultados de G4b (2026-09-30)

- NVK completo (Vulkan 1.4, NAK, el runtime de Mesa) corre en constanos como un ejecutable estático musl de 15 MB contra `/dev/nvgpu` (dispositivo software): `vkCreateInstance`, un dispositivo físico (`NVK GA106`, NVIDIA `0x10de`, discreta), dos familias de colas (gráficos+cómputo+copia y cómputo), tres tipos de memoria (VRAM, VRAM "visible" que el backend sirve con memoria de sistema, sistema), `vkCreateDevice`, buffers con memoria mapeada y de VRAM, un pipeline de cómputo (NAK, ~190 ms bajo TCG), buffer de comandos, `vkQueueSubmit`, fence, semáforo de línea de tiempo y binario, `vkQueueWaitIdle`. El dispatch no se ejecuta (`not executed (software device)`).
- Lo que salió al ejecutar el driver real, y no antes: (1) `ioctl` de musl toma un `int`: la petición llega extendida en signo (el kernel toma solo los 32 bits bajos, como Linux); (2) `cls_m2mf` debe ser `0xa140` (no 0 ni la clase Fermi) o NVK empuja métodos M2MF por un subcanal que la cola no tiene; (3) el rango de VA debe quedar bajo 2^40 (`SET_VERTEX_STREAM_SUBSTITUTE_A` guarda la dirección en 8+32 bits): `[64 GiB, 256 GiB)`; (4) los tipos `vk_sync` con `GPU_WAIT` exigen `WAIT_PENDING`, de ahí el valor `pending` de las timelines en la interfaz; (5) las entradas de Vulkan de NVK son símbolos débiles: `libnvk.a` va con `--whole-archive`; (6) un ejecutable estático no tiene `dladdr`, y NVK busca su `build-id` con él (parche en `util/build_id.c`).
- Sigue pendiente: `dlopen` no se necesita (todo va enlazado), el dispositivo es software, `bar_size_B = 0`, sin sparse ni dma-buf ni WSI. Siguiente: **G4c**, el backend hardware detrás de la misma interfaz.

## G4c: el dispositivo hardware (2026-09-30, código hecho; falta medirlo en la Ryzen)

Diseño y reglas en `docs/reference/gpu.md` ("`gpu=uapi`"). Lo que se decidió, y por qué:

- **Un solo canal GR compartido** por todos los contextos (el de `compute::run`); los demás motores y un canal por contexto son G4e. `ctx_create` acepta solo `NVG_ENGINE_COMPUTE`: NVK pide objetos 3D/2D/M2MF/copia por contexto (`nouveau_ws_context_create`), así que G4d empieza añadiendo esos objetos al canal (RM_ALLOC sin parámetros, como el de cómputo) o pasando a un canal por contexto.
- **El fence lo pone el kernel**, no el usuario: tras los empujes de cada `EXEC`, un push del kernel espera al motor (`WAIT_FOR_IDLE`) y libera el número de secuencia en un semáforo en memoria del host. Así la CPU sabe cuándo terminó y cuántas entradas del anillo liberó la GPU sin leer VRAM (BAR1 es de solo escritura tras GSP-RM; USERD se escribe "periódicamente": un valor viejo no prueba nada).
- **Tablas en caliente**: el pool sube a 8192 tablas (32 MiB, VRAM 64-96 MiB, libre); `PageTables` lleva la cuenta de las tablas tocadas y el adaptador escribe esas (y solo esas) por BAR1 antes de invalidar la TLB (`0xb830a0/a4/b0`). Una tabla recién asignada se escribe aunque esté vacía: el pool de VRAM nunca se puso a cero y una PDE que apunte a basura sería una ruta válida para la MMU.
- **Memoria de usuario**: VRAM `[1 GiB, 7.5 GiB)` (los buffers de contexto de GR empiezan en 7,75 GiB); memoria de sistema = marcos de la arena `ShmObject`, con una referencia por página mientras esté mapeada en la GPU. Un agarrador muerto o colgado no devuelve memoria que la GPU aún pueda escribir.
- **Verificación pensada para la Ryzen** (`nvgpu_hw_test`): relleno a memoria de sistema (lo lee la CPU, y las otras 768 palabras de la página siguen siendo la marca), relleno a VRAM leído por la GPU en el mismo `EXEC` (dos segmentos), **rebind** (la VA pasa de un buffer a otro: si la TLB no se invalidara, la escritura llegaría al buffer viejo), 700 envíos vacíos por un anillo de 1024 entradas y 64 fences (provoca `EAGAIN`), 29 lanzamientos en vuelo, y el dispositivo devuelto tras un `close` y tras un agarrador que muere con trabajo en vuelo. En QEMU el mismo programa corre contra el dispositivo software y solo ejercita la interfaz.
- **Incertidumbres que solo la Ryzen resuelve** (mirar primero `uapi:` en /proc/gpu): si BAR1 alcanza VRAM 7,77 GiB (canal) y 64 MiB (tablas) — si no, `spans_pramin` > 0 y todo sigue funcionando por PRAMIN, más lento; si la invalidación de la TLB por `0xb830b0` completa con GSP-RM vivo (`tlb_us_max`); si PBDMA lee empujes de memoria de sistema (la prueba de rebind lo necesita); y el 10 s de `HANG_MS`.

### Resultado en la Ryzen (boot #144, `gpu=uapi`, job `gpu-uapi.sh`: OK, 0 fallos)

- Las tres incógnitas se resolvieron a favor: BAR1 alcanza el pool de tablas (VRAM 64 MiB) y el canal (VRAM 7,77 GiB) con escrituras que aterrizan (`spans_bar1=2`, `spans_pramin=0`, write-combined); la invalidación de la TLB por `0xb830b0` completa con GSP-RM vivo (`tlb_us_max=3`); la GPU lee pushes de memoria de sistema.
- `nvgpu_hw_test` con hardware y sin ningún `skip`: relleno a memoria de sistema y a VRAM (leído por la GPU), rebind sin traducción vieja, 700 EXEC vacíos (15 ms), 29 lanzamientos en vuelo (0,7 ms), reapertura y holder muerto con trabajo en vuelo. Un EXEC con lanzamiento: 65 us hasta el fence. Contadores finales: 11 binds, 11 unbinds, 22 flushes, 737 execs = 737 fences, `dead=0`; RM responde y ASUS a 60,02 Hz.
- **No ejercitado:** `again=0`: la GPU vació el anillo más rápido que la CPU lo llenaba, así que la ruta `EAGAIN` (anillo o slots llenos) solo está probada en host. El log de metal se cortó (wrap) y solo quedan los resúmenes del job.
- Siguiente: G4d (un dispatch Vulkan real por NVK: objetos 3D/2D/M2MF/copia en el canal, `ctx_create` con todos los motores) y un test que llene el anillo a propósito.

## G4d: NVK ejecuta un dispatch de cómputo en la GPU (2026-09-30, boot #145, job `gpu-vk.sh`: OK, 0 fallos)

- **Resultado:** `vk_probe` (NVK completo, musl estático) enumera la GPU ("NVK GA106", Vulkan 1.4), compila un pipeline con NAK (3,1 ms), lo sube por la cola de subida (motor de copia), ejecuta `vkCmdDispatch` y la CPU lee `EXECUTED` (los 3*i+1234 de la sonda), más fences y semáforos de línea de tiempo. `dead=0`, RM responde, ASUS a 60,01 Hz. Contadores tras la sonda: 29 binds/unbinds, 748 execs (2 por el canal de copia), 748 fences.
- **Lo que hubo que añadir respecto a G4c:**
  - **Dos canales.** NVK crea un contexto *solo de copia* (su cola de subida, que sube el código del shader porque `bar_size_B = 0` impide mapear el heap) y contextos de cómputo. `uapi.rs` ahora lleva dos canales (`ChanKind::Gr`, `Ce`): el de copia es el canal de tiempo de ejecución de `copy.rs` (`copy::take_for_uapi` se lo quita al autotest de dispctl) y su anillo/fence/ranuras van como los del GR. Sus contextos reciben un **preludio del kernel** (`chan::bind_push`, `SET_OBJECT` de la clase de copia en el subcanal 4) antes de cada envío, porque NVK empuja métodos de copia sin ligar nunca la clase (`hwq::Queue` con `prelude_bytes`).
  - **Objeto 3D en el canal GR** (`0xc797`, `gr::H_THREED_CHAN`, asignado al final de `compute::run` para que un fallo de RM no estropee lo ya medido): NVK usa el motor 3D incluso en una cola solo de cómputo (MME para el dispatch indirecto) y hace todo su `nvk_push_draw_state_init` al crear la cola. `ctx_create` acepta compute [+ 3D] [+ copy] en el GR y copy solo en el de copia; 2D y M2MF siguen siendo `EINVAL`.
- **No probado / pendiente:** comandos de copia en una cola de cómputo/gráficos (el canal GR no tiene objeto de copia: empujarlos lo fallaría; NVK hace sus transferencias por la cola de subida); contextos concurrentes (un canal por tipo compartido); `again=0` otra vez (la ruta `EAGAIN` solo en host); `tpc_count` del INFO sigue siendo el del dispositivo software; dibujo 3D y presentación (G4e).

### G4e, primer paso: la ruta EAGAIN medida en la Ryzen (boot #146)

`nvgpu_hw_test` (fase 4b) lanza 300 copias de 32 MiB VRAM->VRAM por el canal de copia mucho más rápido de lo que corren: el kernel respondió `EAGAIN` 1897 veces (anillo/ranuras de fence llenos), todas las copias completaron, `dead=0`, y el job `gpu-vk.sh` ahora **exige** `again > 0`. Sigue pendiente de G4e: objeto de copia en el canal GR (NVK empuja copias de imagen en el subcanal 4 sin ligar la clase; no está claro qué la liga en nouveau: UVM emite `SET_OBJECT` explícito; probar sin y con preludio, al final del job porque un fallo mata el canal GR), `tpc_count` real, detección rápida de RC y canal por contexto, y el primer dibujo.

### G4e: copia en el canal GR, medida (boot #147)

RM acepta un objeto `0xc7b5` sobre COPY0 en el canal GR (`chan::H_COPY_GR`, `compute: copy object ... allocated on the GR channel too`), y una copia host -> VRAM -> host empujada en el subcanal 4 de un contexto compute+copy **sin ningún `SET_OBJECT`** (como hace NVK en su cola gráfica) funciona: datos intactos, `dead=0`. Es decir, en Ampere con GSP-RM la clase de copia queda ligada al crear el objeto, y el preludio del kernel no hace falta en el canal GR (el de la cola de subida sobre el canal de copia es inocuo y se queda). Con esto las transferencias de imagen de una cola gráfica de NVK tienen dónde ejecutarse. Siguen pendientes: `tpc_count` real, detección rápida de RC, un canal por contexto y el primer dibujo (triángulo a imagen, leída con copia a buffer).

### G4e: el primer dibujo (boot #148, `probes/nvk/vk_draw.c`, job `gpu-vk.sh`: OK, 0 fallos)

NVK dibuja un triángulo que cubre una imagen RGBA8 de 64x64 en VRAM (renderizado dinámico, limpieza a 0, vertex+fragment compilados por NAK, motor 3D del canal GR), la copia a un buffer visible por la CPU con `vkCmdCopyImageToBuffer` (objeto de copia del canal GR, sin `SET_OBJECT`) y la CPU encuentra **4096 de 4096 píxeles correctos** (255, 127-128, 63-64, 255). `dead=0`. Es el primer resultado gráfico real sobre la GPU: imagen tileada en VRAM (la PTE lleva el `pte_kind` de NVK), pipeline gráfico, estado 3D de NVK, rasterizador y ROP. Todo lo demás del job (compute, copia, `EAGAIN`, `grcopy`) sigue pasando. Pendiente: presentación (llevar una imagen a pantalla/scanout), varios contextos a la vez, `tpc_count` real, RC rápido y un canal por contexto.
