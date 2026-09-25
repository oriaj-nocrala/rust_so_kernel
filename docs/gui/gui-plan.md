# Plan: una GUI (memoria compartida → compositor → terminal con ventana)

> **Estado (2026-09-25):** fase 1 hecha y verificada en QEMU (ver su
> registro al final); falta la vuelta en la Ryzen. Fases 2 y 3 sin empezar.

## Por qué ahora, y por qué así

SMP está completo y verificado en la Ryzen, y casi todo lo que una GUI en
espacio de usuario necesita ya existe:

- **Salida:** la pantalla ya pasa por una copia en RAM y write-combining
  (`docs/fb/wc-shadow-plan.md`): `fb_flush` a ~5,6 GB/s en la Ryzen, una
  pantalla de 1920x1080 en ~1,5 ms.
- **Entrada:** teclado y ratón (PS/2 y USB) como evdev real en
  `/dev/input/event0`/`event1`.
- **IPC:** AF_UNIX real con `SCM_RIGHTS`, `poll`/`epoll` — exactamente lo
  que Wayland usa entre cliente y compositor.
- **Varias CPUs:** el compositor y los clientes pueden correr a la vez.

Lo que **falta** es memoria compartida entre procesos. `sys_mmap`
(`kernel/src/process/syscall/fs.rs`) solo acepta `MAP_ANONYMOUS` privado
con `fd == -1`. Sin ella, un cliente solo puede mandar sus píxeles copiados
por un socket (8 MB por cuadro a 1080p) o dibujar el kernel por él.

**Descartado: GUI dentro del kernel.** Es el atajo habitual de los SO
hobby. Un fallo de una aplicación tumbaría la máquina, y el protocolo quedaría
atado a estructuras del kernel. Cuesta mucho deshacerlo después.

**El protocolo se parece a Wayland desde el principio** (superficie,
búfer compartido pasado por `SCM_RIGHTS`, daño, commit), aunque sea
propio y mínimo. Así portar `libwayland` más adelante sigue siendo una
opción y no una reescritura.

## Principio de ejecución

El de `docs/smp/smp-plan.md`: cada fase entra en master funcionando y
verificada, sin ramas largas a medias.

- `scripts/boot-matrix.sh 4 5` limpio, con `QEMU_DEBUG_SMP=4` y
  `QEMU_DEBUG_MEM=8G`.
- `scripts/run-kernel-tests.sh` en verde.
- Tests de host de los crates que se toquen.
- Un job de `scripts/metal-run.sh` en la Ryzen al cerrar cada fase.
  **Salida de los tests visible en el job, nunca a `/dev/null`**: el
  veredicto solo mira el exit del job (así se escapó el fallo de
  `lifecycle_test` en el boot #36).

---

## Fase 1: memoria compartida

### Qué se implementa

| Syscall | Nº Linux | Alcance |
|---|---|---|
| `memfd_create(name, flags)` | 319 | Crea un objeto de memoria anónimo detrás de un fd. `MFD_CLOEXEC` sí; `MFD_ALLOW_SEALING`/sellos no (ver fuera de alcance) |
| `ftruncate(fd, len)` | 77 | Fija el tamaño del objeto. **Solo crecer** en esta fase, o encoger si nadie lo tiene mapeado; si no, `EBUSY` (ver decisiones) |
| `mmap(..., MAP_SHARED, fd, off)` | 9 | Mapea páginas del objeto. `offset` alineado a página |
| `mmap(..., MAP_SHARED \| MAP_ANONYMOUS, -1, 0)` | 9 | Memoria compartida anónima: un objeto sin fd, compartido a través de `fork` |
| `fstat` sobre un memfd | 5 | `st_size` real, `S_IFREG` |
| `munmap` | 11 | Sin cambios de interfaz; ahora también quita mapeos compartidos |

`MAP_PRIVATE` sobre un memfd (una copia COW del objeto) queda **fuera** de
esta fase y devuelve `EINVAL`. Ningún compositor ni cliente lo usa.

### Diseño

**El objeto: `ShmObject`** (`kernel/src/memory/shm.rs`, nuevo).

```rust
pub struct ShmObject {
    pages: IrqMutex<Vec<Option<PhysFrame>>>, // índice = página del objeto
    size: AtomicU64,                         // bytes, lo que dice ftruncate
    mappings: AtomicUsize,                   // VMAs vivas que lo referencian
}
```

- Las páginas se asignan **al primer fallo**, igual que las anónimas, pero
  se guardan en el objeto. Así el segundo proceso que falla en esa página
  encuentra el mismo frame y no pide uno nuevo. Se ponen a cero al
  asignarlas. **Nunca el frame cero compartido:** una escritura posterior
  tendría que sustituirlo en todos los espacios de direcciones a la vez.
- **Contabilidad con los contadores COW que ya existen** (`memory/cow.rs`).
  El objeto tiene una referencia sobre cada frame suyo, y cada PTE que lo
  mapea tiene otra. Todo el código que libera frames de usuario
  (`unmap_page_and_free`, `release_user_pages` al morir el proceso) ya
  hace `dec_ref` y libera al llegar a 0. Por tanto, un mapeo compartido que
  desaparece nunca libera un frame que el objeto sigue teniendo, **sin
  tocar ese código**. El frame se libera cuando el objeto muere (último
  fd cerrado y último mapeo quitado) y hace `dec_ref` de cada página.
- **Riesgo conocido: los contadores son `u8` saturantes.** Un frame con
  más de 254 referencias se queda saturado y los `dec_ref` posteriores lo
  llevarían a 0 antes de tiempo, con un frame liberado mientras sigue
  mapeado. En una GUI cada búfer lo mapean 2 o 3 procesos, pero la
  condición es fácil de provocar a propósito. Por eso `mmap` de un
  `ShmObject` **rechaza con `ENOMEM`** al llegar a 200 mapeos
  (`mappings`), y hay un test que lo comprueba.

**El VMA.** Hace falta `VmaKind::Shared` y que el VMA sepa **de qué
objeto** es y en qué offset empieza. Dos formas:

1. **`Vma` deja de ser `Copy`** y lleva `Option<Arc<ShmObject>>` +
   `offset_pages`. Soltar el VMA (munmap, exec, muerte) suelta el objeto
   solo, por `Drop`. Coste: hay ~24 sitios que copian `Vma`/usan
   `.copied()` y pasan a `.clone()`, y cada `clone` de un VMA compartido
   toca un contador atómico.
2. **`Vma` sigue `Copy`** y lleva un `u32` que indexa una tabla global
   de objetos con contador explícito. Cambia menos, pero cada camino que
   quita un VMA tiene que acordarse de soltar la referencia.

**Elegida la 1** (decidido con el usuario). La 2 repite a mano lo que
`Arc` ya hace, y los fallos de "alguien se olvidó de soltar" son justo los
que este kernel ha pagado caro antes (`large_pipe_transfer_hang`: un `Arc`
perdido antes de un `-> !`). Medido al hacerlo: los sitios eran
`.copied()` → `.cloned()` y un `shm: None` en cada constructor.

**El fd: `MemfdHandle`** (`FileHandle`) guarda `Arc<ShmObject>`. `dup` y
`SCM_RIGHTS` funcionan sin más, porque ya pasan `Box<dyn FileHandle>`.
`sys_mmap` llega del fd al objeto igual que los sockets llegan a su
socket: un método nuevo en `vfs::FileHandle`,
`fn shm_object(&self) -> Option<...>`, con implementación por defecto
`None`. Como `vfs` no conoce los tipos del kernel, el método devuelve un
identificador opaco o un `Arc<dyn Any>`; eso se decide al escribirlo,
con el precedente de `socket_id()`.

**Caminos que cambian:**

- **Fallo de página no presente en un VMA `Shared`:** tomar (o asignar)
  el frame del objeto, `inc_ref` y mapearlo con los permisos del VMA.
  Todo bajo el lock del espacio de direcciones y después el del objeto,
  en ese orden: espacio de direcciones → `ShmObject::pages` → `BUDDY`.
  Un acceso más allá de `size` es `SIGBUS` en Linux; aquí se mata el
  proceso como en un fallo sin VMA, y se documenta la diferencia.
- **Fallo de escritura sobre página presente en un VMA `Shared`:** nunca
  se copia. Si el VMA es de solo lectura, es un fallo del proceso; si es
  escribible, no debería ocurrir (se mapea escribible desde el principio).
- **`fork`:** hoy protege contra escritura *todas* las páginas presentes
  (`AddressSpace::fork`). Los VMAs `Shared` se saltan eso: el hijo mapea
  el mismo frame **escribible**, con `inc_ref`, y el padre no cambia.
  Sin esto, la primera escritura tras un fork copiaría la página y
  rompería la compartición en silencio. Hay un test que lo comprueba.
- **`munmap`:** ya pasa por `unmap_page_and_free`, que hace lo correcto
  por los contadores. Solo hay que soltar el objeto.
- **TLB:** los mapeos de cada espacio se invalidan con
  `tlb::invalidate_page(pml4, …)` como cualquier otro PTE de usuario.
  **Encoger** un objeto mapeado exigiría desmapearlo de *todos* los
  espacios que lo tienen. Esa es la razón de que `ftruncate` hacia abajo
  sea `EBUSY` mientras haya mapeos.
- **Páginas grandes:** `sys_mmap_anon` usa páginas de 2 MiB desde 2 MiB.
  Los objetos compartidos usan solo 4 KiB en esta fase. Un búfer de
  1080p son ~2000 fallos la primera vez que se toca, y medirlo es parte
  de la fase 3.

**mlibc:** faltan los sysdeps de `memfd_create` y `ftruncate`, y hay que
comprobar si `mmap` ya pasa `fd`/`offset`. `MAP_SHARED` ya vale `0x01`
en `abi-bits/vm-flags.h`, igual que en Linux. Cualquier cambio de
cabeceras obliga a `rm kernel/embedded/busybox.elf` (ver CLAUDE.md).

### Tests

- **`userspace/c/shm_test.c`** (disco, con la salida visible):
  1. `memfd_create` + `ftruncate` + `mmap`, escribir y leer; `fstat` da
     el tamaño.
  2. Un hijo de `fork` escribe y el padre lo ve, **en los dos sentidos**
     (que no haya COW).
  3. Pasar el fd por `SCM_RIGHTS` a un proceso que no lo heredó; él lo
     mapea, escribe y el primero lo ve.
  4. Cerrar el fd con el mapeo vivo: la memoria sigue siendo válida.
     `munmap` antes de cerrar: igual.
  5. `MAP_SHARED|MAP_ANONYMOUS` compartido a través de `fork`.
  6. Mapeo `PROT_READ` + escritura: el proceso muere (en un hijo).
  7. Acceso más allá del tamaño: el proceso muere (en un hijo).
  8. `ftruncate` hacia abajo con mapeos: `EBUSY`.
  9. 201 mapeos del mismo objeto: el 201 falla con `ENOMEM`.
  10. Fugas: `MemFree` de `/proc/meminfo` antes y después de 100 ciclos
      crear→mapear→fork→escribir→soltar vuelve a su valor.
  11. SMP: dos procesos en CPUs distintas escriben y leen alternándose
      por un contador en la memoria compartida (con `futex` o spin), 10⁵
      vueltas, sin perder ninguna.
- **Integración QEMU** (`hw_tests.rs`), solo si algo no se puede alcanzar
  desde userspace, por ejemplo que `release_user_pages` de un proceso
  muerto no libere los frames del objeto.

### Fuera de alcance (fase 1)

Sellos de memfd (`F_ADD_SEALS`: Wayland los usa, pero no los exige),
`MAP_PRIVATE` sobre fds, `mmap` de ficheros de ext2/ramfs, `shm_open`/
`/dev/shm`, `mremap`, `msync`, `munmap` parcial, y compartir páginas de
2 MiB.

---

## Fase 2: el compositor (esbozo, a detallar al cerrar la fase 1)

Un programa de userspace en Rust (`userspace/src/bin/compositor.rs` o un
crate propio) que:

- es el **único** que abre `/dev/fb` y los `event*`;
- acepta clientes en un socket AF_UNIX (`/tmp/gui-0`, el análogo de
  `wayland-0`);
- por cliente: `create_surface`, `attach(buffer_fd, w, h, stride)` vía
  `SCM_RIGHTS`, `damage(rect)`, `commit`; y hacia el cliente: eventos de
  entrada de la ventana con foco, y `frame_done` para regular el ritmo;
- compone en su propio búfer y lo lleva a la pantalla.

**Decisión abierta: cómo llega el compositor a la pantalla.**

- **(a) `mmap` de `/dev/fb` que mapea la copia en RAM** (no la VRAM) más
  un `ioctl(FBIO_FLUSH, rect)` que llama a `Framebuffer::flush`. Sin
  copias extra, y la regla "solo `flush` toca la VRAM" se mantiene. Pero
  la consola del kernel dibuja en esa misma copia, así que hace falta un
  modo "gráfico" (el `KD_GRAPHICS` de Linux) en el que la consola deja de
  dibujar mientras el compositor tiene `/dev/fb` abierto.
- **(b) Un `ioctl` que copia un rectángulo de un memfd a la pantalla**
  (un `FBIO_BLIT` con daño). Es más simple, pero añade una copia por
  cuadro.

La (a) encaja mejor con lo que ya existe; la (b) es el camino rápido para
una primera versión. En los dos casos, `kalert!` y la pantalla de pánico
tienen que seguir viéndose con el compositor activo.

Los búferes van siempre como `memfd` por `SCM_RIGHTS`, que crea el
cliente: el protocolo nunca pasa punteros ni direcciones, y un cliente
que muere solo suelta su referencia al objeto.

**Probar en QEMU:** `scripts/qemu-debug.sh` ya tiene `screendump` y
`mouse-move`.

## Fase 3: primer cliente, un terminal con ventana

Un emulador de terminal cliente del compositor que ejecuta `ash` sobre un
pseudo-terminal. **Falta un pty** (`/dev/ptmx` + `/dev/pts/N`): hoy la
"terminal" es la consola del kernel, y ash habla con ella a través de
`/dev/console` y `/dev/fb`. El pty es un trabajo del kernel en sí mismo,
y conviene hacerlo al empezar esta fase o antes. Reutiliza la fuente Noto
y el parser ANSI de `framebuffer_console.rs`, extraído a un crate de host
(`hal` o uno nuevo) para que lo usen el kernel y el terminal.

Después, candidatos: DOOM en una ventana (su port ya dibuja en un búfer
RGB), un reloj, `top` con ventana.

## Registro

### Fase 1 (2026-09-25)

Hecha como se planeó, con estas diferencias y hallazgos:

- **`VmaList` pasó de `[Option<Vma>; 64]` a `Vec<Vma>`** (tope de 64
  igual). Con el campo `shm`, `Vma` creció de 32 a 48 bytes y el primer
  arranque dio un **double fault al cargar PID 1**: a `opt-level 0`,
  `IrqMutex::new(VmaList::new())` copia la lista por la pila (una sonda de
  más de 8 KiB), y eso ya no cabía en la pila de arranque de 80 KiB del
  bootloader. Con el `Vec`, una lista vacía son tres palabras y `fork`
  copia solo los VMAs que existen. `add` asigna bajo el lock del espacio
  de direcciones, lo que el orden de locks permite.
- **`FileHandle::shm_object()`** devuelve `Arc<dyn Any + Send + Sync>` y
  el kernel hace `downcast::<ShmObject>()`. No hizo falta un
  `FileHandle::truncate`: `ftruncate` va por `shm_object()` y traduce
  `ShmError` a errno directamente (`EFBIG`/`EBUSY`/`ENOMEM`), sin añadir
  variantes a `FileError`.
- **El handle está en `kernel/src/ipc/memfd.rs`**, no en `memory/`: el
  módulo `memory` no importa `process` (invariante del CLAUDE.md).
- **mlibc:** `sys_vm_map` abortaba (`__ensure`) con todo `mmap` no
  anónimo y descartaba `flags`, `fd` y `offset`; ahora los pasa. Como el
  kernel toma cualquier dirección no nula como `MAP_FIXED`, la pista sin
  `MAP_FIXED` ya no se pasa (Linux la trata como orientativa). mlibc
  define `memfd_create` solo con la opción Linux, que este port deja
  apagada, así que el wrapper vive en `generic.cpp` y `setup-mlibc.sh`
  saca su declaración del guard de `<sys/mman.h>`. Se añadieron
  `sys_ftruncate`, `sys_memfd_create` y **`sys_yield`**: faltaba, y cada
  `sched_yield()` imprimía el `__ensure` de sysdep ausente.
- **Bug encontrado, no arreglado aquí: un pipe guarda un solo lector en
  espera** (`pipe.rs`, `read_waiter: Option<_>`). Si dos procesos se
  bloquean leyendo el mismo pipe, el segundo pisa al primero, que no
  despierta nunca; es el fallo que tenía el antiguo `channel.rs`. El
  primer diseño del caso 9 lo destapó (cuatro hijos bloqueados en un
  pipe, uno solo despertó). El caso 9 ahora mata a sus hijos con
  `SIGKILL`; el bug queda para un cambio propio.

**Verificado en QEMU:** `shm_test` 11/11 con `-smp 4` y con `-m 8G`
(frames por encima de 512 MiB), `MemFree` estable en 100 ciclos (12 kB
de deriva); `pipe_cow_test`, `fork_exec_test`, `lifecycle_test`,
`socket_test`, `mmap_test`, `pthread_test`, `producer_consumer`,
`fpu_test`, `mlibc_signal_test` y `sigsuspend_test` en 0;
`run-kernel-tests.sh` PASS; `boot-matrix.sh 4 5` con 4 CPUs y 8 GiB:
20/20 OK. **Pendiente:** la vuelta en la Ryzen.
