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
| **G4c** | Estado de GPU persistente (tablas, VRAM, canal GR, cerrojo) y el dispositivo *hardware* detrás de la misma interfaz: `VA_BIND` en caliente, `EXEC` real, fence de contexto. Nivel `gpu=uapi`. | Ryzen: mismo programa C de G4a con un shader real; lectura por otro camino |
| **G4d** | Un dispatch de cómputo Vulkan de verdad en la Ryzen (SPIR-V -> NAK -> GPU) y verificación de resultados. | Ryzen |
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
