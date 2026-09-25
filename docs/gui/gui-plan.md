# Plan: una GUI (memoria compartida → compositor → terminal con ventana)

> **Estado (2026-09-25):** fase 1 hecha y verificada en QEMU y en la Ryzen
> (ver su registro al final). Fase 2: 2.1 hecho y verificado en QEMU y en la Ryzen;
> 2.2-2.5 pendientes. Fase 3 sin empezar.

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

## Fase 2: el compositor

Un programa de userspace en Rust (`userspace/src/bin/compositor.rs`) que
es el **único** que dibuja en la pantalla y lee la entrada mientras corre.
Acepta clientes en un socket AF_UNIX (`/tmp/gui-0`, el análogo de
`wayland-0`), recibe sus búferes como `memfd` por `SCM_RIGHTS`, compone y
les manda la entrada de la ventana con foco.

### Decisiones (tomadas con el usuario, 2026-09-25)

- **Pantalla: `mmap` de la copia en RAM** (la opción (a) del esbozo), no
  un `ioctl` que copie desde un memfd. El compositor compone directamente
  en la copia en RAM y pide el volcado a VRAM con un `ioctl`. Así no hay
  una copia de más por cuadro, y la regla "solo `Framebuffer::flush` toca
  la VRAM" se mantiene.
- **Rust**, con la lógica en un crate nuevo `gui/` testeable en el host,
  como `usock` o `sched`. Encaja con la fase 3, que reutiliza la fuente
  Noto (crate de Rust) y el parser ANSI.

### 2.1 Kernel: `/dev/fb0`, modo gráfico y `mmap` de la copia en RAM

**Un dispositivo aparte, `/dev/fb0`**, distinto de `/dev/fb`, que es la
consola de texto: ash escribe ahí por sus fds 1 y 2. Se abre en
exclusiva (un segundo `open` da `EBUSY`), y **tenerlo abierto es el modo
gráfico**, el `KD_GRAPHICS` de Linux. El modo acaba en el `Drop` del
último handle, igual que `EVIOCGRAB`. Por eso un compositor que muere,
incluso con `SIGKILL`, devuelve la pantalla a la consola sin que nadie
tenga que acordarse. `dup`/`fork` comparten un `Arc` interno, y el modo
acaba cuando cae la última referencia.

En modo gráfico:

- **La consola no dibuja.** `render_bytes` deja de pintar, aunque sigue
  duplicando a serie y a `klog`, y el parpadeo del cursor (ISR) se
  detiene. Lo que ash escriba mientras tanto no se ve, igual que en
  Linux con `KD_GRAPHICS`. Al salir del modo, la pantalla se limpia y el
  cursor vuelve arriba (el mecanismo de `FB_RAW_DIRTY`).
- **`kalert!` y la pantalla de pánico siguen dibujando.** `kalert!`
  pinta encima y el compositor lo tapa al repintar esa zona; el pánico
  se queda, porque nadie repinta después.
- **`FBIO_BLIT` sobre `/dev/fb` da `EBUSY`.** Si DOOM, en modo consola,
  pintara encima del compositor, rompería lo que este cree que hay en
  pantalla.

**`mmap(fd de /dev/fb0, MAP_SHARED)`** mapea las páginas de la copia en
RAM y **reutiliza la fase 1 entera**. La copia se envuelve una vez en un
`ShmObject` *fijado* (`ShmObject::pinned(frames)`), construido con los
frames que ya tiene y guardado en un `static`. El objeto nunca muere, así
que tiene siempre su referencia sobre cada frame: `munmap` y la muerte
del proceso hacen `dec_ref` por la vía normal y nunca llegan a 0.
`set_size` da `EBUSY` sobre un objeto fijado. Así no hay un
`VmaKind::Device` nuevo, ni un camino de fallo nuevo, ni caso especial en
`fork`. Dos condiciones:

- la copia es un bloque contiguo del Buddy dentro de la ventana de
  memoria física (asignación grande del slab); hay que comprobarlo al
  construirla, no suponerlo;
- empieza `SHADOW_SKEW` bytes dentro de su asignación, así que el objeto
  cubre desde la página que contiene el primer byte y
  `FBIO_GET_INFO` dice el desplazamiento. Las páginas de los extremos
  son de la misma asignación, llena de ceros: no se expone memoria de
  nadie más.

**`ioctl`s de `/dev/fb0`** (números propios, `0x4642_00xx`, como
`FBIO_BLIT`):

| Petición | Argumento | Hace |
|---|---|---|
| `FBIO_GET_INFO` | `{width, height, stride_px, bpp, offset, map_len}` | Geometría, y dónde empieza el píxel (0,0) dentro del `mmap` |
| `FBIO_FLUSH` | `{n, rects: [{x,y,w,h}; ≤16]}` | `Framebuffer::touched` + `flush` de cada rectángulo, recortado a la pantalla |

Sin copia en RAM (la asignación falló y la consola está en modo directo),
`open("/dev/fb0")` da `ENODEV`. `/proc/fbinfo` gana una línea
`mode: text|graphics (pid N)`.

**Tests:** `userspace/c/fb0_test.c` en el disco (C, porque mlibc ya
tiene `mmap`/`ioctl`). Comprueba el segundo `open` con `EBUSY`, `FBIO_BLIT`
con `EBUSY`, que `mmap` + dibujar + `FBIO_FLUSH` dejan en la pantalla lo
esperado (con `screendump` en QEMU, píxel a píxel) y que la consola
vuelve tras `close`, tras `exit` y tras `SIGKILL`. Un `hw_tests` para el
`ShmObject` fijado: `munmap` de todos los mapeos y la muerte de un
proceso no liberan sus frames.

### 2.2 Kernel: `poll` real sobre `/dev/input/event*`

Hoy `fd_check_ready` (`syscall/poll.rs`) trata todo dispositivo como
siempre listo. Un compositor que espere a sus clientes y a la entrada con
un `poll` se quedaría girando al 100 %. Hace falta:

- readiness real, que diga si el anillo del teclado o del ratón tiene
  eventos (o un `SYN` pendiente de ese handle);
- despertar desde los productores, con el patrón de
  `poll_wakeup_for_fd0`: el ISR del teclado, el del ratón PS/2 y el
  sondeo USB (CPU 0, en el tick);
- lo mismo para `epoll`.

Los mismos mecanismos que stdin: registrarse y bloquearse bajo el lock
del planificador, y volver a comprobar tras registrarse (regla SMP
"comprobar y dormir es un solo paso"). Test: `poll_test` gana un caso de
`poll` sobre `event1` despertado por `qemu-debug.sh mouse-move`.

### 2.3 Userspace: lo que le falta al crate `userspace`

- **Un allocator global** (`#[global_allocator]`) sobre `mmap` anónimo:
  listas libres por tamaño y bloques grandes directos a `mmap`/`munmap`.
  Hasta ahora ningún programa de Rust usaba el heap.
- **Wrappers:** `memfd_create`, `ftruncate`, `mmap` general
  (`MAP_SHARED`, fd, offset), `ioctl`, `sendmsg`/`recvmsg` con
  `SCM_RIGHTS` y `epoll`.

### 2.4 El crate `gui/` (host, `cd gui && cargo test`)

`no_std` + `alloc`, sin syscalls. Como `usock`, **nada bloquea y los
efectos vuelven como datos**: mensajes que enviar, rectángulos que volcar
y fds que cerrar. Tres módulos:

- **`wire`**: el formato de Wayland tal cual. Cada mensaje es
  `[object_id: u32][size << 16 | opcode: u32][args…]`, alineado a 4, y los
  fds van aparte, en el orden de sus argumentos. Codificar y decodificar,
  con los mensajes partidos entre lecturas.
- **`region`**: rectángulos, recorte, unión y resta, lo que hace falta
  para calcular el daño y quitar lo tapado.
- **`compositor`**: el estado. Clientes, objetos por cliente, superficies
  con estado *pendiente* y *actual* (el `commit` de Wayland aplica el
  pendiente de golpe), orden Z, foco, puntero, y `compose(damage, dst)`
  sobre un `&mut [u32]` genérico. Así los tests de host componen en un
  `Vec` y comprueban píxeles.

**El protocolo** (propio y mínimo, con los nombres de Wayland; objeto 1 =
el compositor):

| Objeto | Peticiones | Eventos |
|---|---|---|
| compositor | `create_pool(id, fd, size)`, `create_surface(id)`, `sync(id)` | `error(obj, code)`, `delete_id(id)` |
| pool (un memfd) | `create_buffer(id, offset, w, h, stride, format)`, `destroy` | — |
| buffer | `destroy` | `release` |
| surface | `attach(buffer)`, `damage(x,y,w,h)`, `frame(id)`, `commit`, `set_title(str)`, `destroy` | `configure(w,h)`, `focus(in)`, `key(code, state)`, `motion(x,y)`, `button(code, state)` |
| callback | — | `done(ms)` |

Es la unión de `wl_compositor`, `wl_shm`, `wl_surface`, `xdg_toplevel` y
`wl_seat` en pocos objetos. Portar `libwayland` más adelante supondría
separarlos, no cambiar de modelo. Formato único: `XRGB8888`. El
compositor mapea cada pool una vez. Un pool que el cliente encoge no
puede romperlo, porque `ftruncate` con mapeos da `EBUSY` desde la fase 1.
Un `offset + stride*h` fuera del pool es un `error` que desconecta al
cliente, nunca una lectura fuera de límites.

**Composición y ritmo:** en `commit`, el compositor copia el daño de la
superficie a la pantalla, junto con lo que haya debajo y encima, y manda
`release` del búfer enseguida, porque ya lo ha copiado. `frame(id)` se
responde con `done` tras el siguiente volcado, como mucho a 60 Hz. No
hay vsync que esperar.

### 2.5 El compositor y un cliente de prueba

- `compositor`: abre `/dev/fb0` y `event0`/`event1` (con `EVIOCGRAB`,
  descartando lo que el anillo tenga de antes), escucha en `/tmp/gui-0` y
  espera a todo con `epoll`. Ventanas en cascada, una barra de título
  lisa (el texto llega con la fuente, en la fase 3), clic para foco y
  subir, arrastrar por la barra y cursor dibujado por software.
  **Ctrl+Alt+Retroceso lo cierra**: con el teclado capturado, es la única
  salida sin un segundo terminal.
- `gui_demo`: una ventana con un degradado animado que pide `frame` en
  cada cuadro y registra teclas y clics en su propia salida.
- **Prueba de extremo a extremo en QEMU** con `screendump`: el fondo, la
  ventana en su sitio, que se mueva al arrastrarla con `mouse-move`, y la
  consola de vuelta tras Ctrl+Alt+Retroceso.
- Medir: tiempo de composición y volcado por cuadro en `/proc/fbinfo`
  (`OpStat`) y fallos de página del primer toque a un búfer de 1080p.

### Fuera de alcance (fase 2)

Varios monitores, cambio de modo, vsync, transparencia (todo opaco),
redimensionar ventanas desde el compositor, portapapeles, arrastrar y
soltar, cursores de cliente, `libwayland` real y clientes en C (DOOM en
ventana va después).

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
20/20 OK.

**Verificado en la Ryzen (boot #38, 24 CPUs):** `shm_test: PASS (0
failed)`, con `MemFree` estable (33 407 676 → 33 407 728 kB en 100
ciclos) y `pipe_cow_test`, `fork_exec_test`, `lifecycle_test`,
`socket_test` y `pthread_test` en 0; `sched: invariants=ok`. El anillo
del log dio la vuelta y se perdieron las líneas de los casos 1-9, pero
el recuento final de fallos los cubre.

### Fase 2.1 (2026-09-25)

Hecho como se planeó, con estas diferencias y hallazgos:

- **Abrir un dispositivo puede fallar.** `DeviceEntry::open` devuelve
  `Result<_, Errno>`: `/dev/fb0` necesita `EBUSY` y `ENODEV`, y hasta ahora
  todo dispositivo abría siempre. `vfs::Errno` gana `ENODEV`.
- **En modo gráfico, `render_bytes` solo pinta para `kalert!`**, sin
  borrar la pantalla ni dibujar el cursor. Si al entrar se hubiera
  marcado `FB_RAW_DIRTY`, el primer `kalert!` habría borrado el cuadro
  del compositor.
- **El test de "no libera los frames" es de userspace, no un `hw_tests`.**
  Mapear las ~1000 páginas, tocarlas y desmapear (tres veces, y un hijo
  que muere con ellas mapeadas) deja `MemFree` igual al kB. Si el
  `ShmObject` fijado no retuviera su referencia, subiría en el tamaño de
  la pantalla (4 MB en QEMU).
- **De paso: `pause(34)` y `getppid(110)`.** mlibc no tenía la sysdep de
  `pause()`, y el `__ensure` de sysdep ausente *vuelve*, así que
  `for (;;) pause();` giraba imprimiéndolo. Ahora es un `rt_sigsuspend`
  con la máscara actual, dentro del kernel. `getppid()` devolvía siempre
  1: el caso nuevo de `sigsuspend_test` que prueba `pause` hizo
  `kill(getppid(), SIGUSR1)` y la señal le llegó a init.
- **Limitación conocida, a la vista:** `kill` no despierta a un proceso
  bloqueado en `nanosleep`, pipes o futex (sí en `sigsuspend`/`pause`).
  Un `SIGKILL` a un proceso dentro de `sleep(100)` tarda esos 100 s.
  Linux interrumpe esas esperas con señales fatales. Está documentado en
  `sys_kill` como compromiso aceptado; no se ha tocado aquí.

**Verificado en QEMU:** `fb0_test` 16/16. Con `screendump`, el fondo, el
cuadrado rojo (volcado de pantalla completa) y el verde (volcado por su
propio rectángulo) salen exactos píxel a píxel, a 1280x800. Después, la
consola vuelve al cerrar, al salir y tras `SIGKILL`. `sigsuspend_test`
6/6 (casos E y F nuevos). Con `-smp 4 -m 8G`: `fb0_test`,
`sigsuspend_test`, `shm_test`, `lifecycle_test`, `pipe_cow_test`,
`pipe_multi_test`, `fork_exec_test`, `socket_test`, `mlibc_signal_test`,
`pthread_test` y `fpu_test` en 0. `run-kernel-tests.sh` PASS,
`boot-matrix.sh 4 5` con 4 CPUs y 8 GiB 20/20, `cd vfs && cargo test`
165/165.

**Verificado en la Ryzen (boot #41, `target/metal/fb0-job.sh`):**
`fb0_test` 16/16 a 1920x1080, stride 2048 (`map_len` 8 849 472, el mismo
`offset` 2112), con la VRAM en WC. `MemFree` de 33 407 460 a 33 407 396 kB
con 8,6 MB de pantalla mapeados y desmapeados. `sigsuspend_test` 6/6 (F:
1 ms), `shm_test`, `lifecycle_test`, `pipe_cow_test`, `pipe_multi_test`,
`fork_exec_test`, `socket_test` y `pthread_test` en 0, e
`invariants=ok`. El anillo del log dio la vuelta, pero solo perdió el
arranque: todas las líneas de resultados están.

### Fase 2.2 (2026-09-25)

Hecho como se planeó, con estas diferencias:

- **Cómo sabe `poll` qué fd es de entrada: `FileHandle::event_source()`**
  (`vfs`), la técnica de `socket_id()`. El despertador no llega a la
  tabla de fds del proceso dormido, así que el handle publica dos cosas
  que se copian al `PollWaiter`: qué cola global lo alimenta
  (`drivers::evdev::QUEUE_KEYBOARD`/`QUEUE_MOUSE`) y si ya tiene
  registros propios (el `SYN_REPORT` pendiente del teclado, el resto de
  un paquete del ratón). El `SocketMap` de `poll.rs` pasa a ser un mapa
  fd → `PollSource` (`Socket`, `Input`, `Other`).
- **Un solo despertador, `poll_wake_where`**, para stdin, sockets y
  entrada. Antes cada uno paraba en el primer proceso que encontraba; ahora
  despierta hasta 8 por evento, y al devolver a la lista una espera que no
  estaba lista usa `or_insert`, para no pisar una espera nueva del mismo
  pid (el caso de un proceso que vence su timeout, y en otra CPU ya está
  en otro `poll`).
- **`POLLOUT` en un evdev siempre está listo**, como en `evdev_poll` de
  Linux.
- **El ratón USB se despierta desde `usb::poll`, no desde
  `handle_hid_event`.** Ese código corre con `CONTROLLERS` tomado e IF=0,
  a veces dentro de una transferencia de almacenamiento. El sondeo de
  cada tick mira si la cola del ratón tiene algo y despierta ahí: como
  mucho un tick (10 ms) de retraso.
- **El test es un programa aparte, `input_poll_test`** (C, en el disco),
  y no un caso de `poll_test`: el despertar necesita que el host mueva el
  ratón o pulse teclas. Sin argumentos comprueba lo que no necesita
  entrada: `poll`/`epoll_wait` con timeout 0 dan 0, un timeout de 150 ms
  se duerme entero, `POLLOUT` está listo, `SIGUSR1` corta la espera con
  `EINTR`. Con `wait [epoll]` espera hasta 10 s en `event0`+`event1` y
  dice qué lo despertó y cuándo.

**Verificado en QEMU con `-smp 4`:** `input_poll_test` PASS, 8 casos. Con
`wait`, `poll` y `epoll`, ratón (`mouse-move`) y teclado (`key shift`),
PS/2 y también USB (`QEMU_USB_KBD=1 QEMU_USB_MOUSE=1 QEMU_DEBUG_NO_PS2=1`).
Los 8 despiertan a los ~2 s, que es el retraso con que el host manda el
evento, y no a los 10 s del timeout. `poll_test`, `ipc_ping`,
`socket_test`, `wait_intr_test`, `pipe_multi_test` y `sigsuspend_test`
en 0, `invariants=ok`. `run-kernel-tests.sh` PASS, `boot-matrix.sh 4 4`
(4 CPUs, 8 GiB) 16/16, `cd vfs && cargo test` 166/166.
