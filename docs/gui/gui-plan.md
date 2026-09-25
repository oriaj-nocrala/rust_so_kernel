# Plan: una GUI (memoria compartida → compositor → terminal con ventana)

> **Estado (2026-09-25):** fase 1 hecha y verificada en QEMU y en la Ryzen
> (ver su registro al final). Fase 2: 2.1 hecho y verificado en QEMU y en la Ryzen;
> 2.2 a 2.5 hechos y verificados en QEMU y en la Ryzen (2.2 en el boot #43,
> 2.3 y 2.5 en el #45; 2.4 son tests de host). Fase 2 cerrada. Fase 3 planificada
> (decisiones del 2026-09-25); 3.1 a 3.3 hechos, 3.2 y 3.3 verificados en la
> Ryzen (boot #46). Siguiente: 3.4 (`vt/`).

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

Un emulador de terminal, cliente del compositor, que ejecuta `ash` sobre un
pseudo-terminal. Hoy no hay pty: la "terminal" es la consola del kernel, y
ash habla con ella por `/dev/console` y `/dev/fb`. Lo que hay del tty es
poco y es global:

- `tty::TERMIOS` y `FOREGROUND_PGID` son statics: hay un solo tty.
  `sys_ioctl` decide si un fd es un tty mirando si su handle se llama
  `"serial"` o `"fb"`, y `sys_read` tiene un camino aparte para la fd 0.
- **No hay disciplina de línea.** `ICANON`/`ECHO` se guardan pero no se
  aplican; solo `ISIG` funciona (`tty::feed_input`). La edición y el eco
  los hace ash en modo crudo.
- **No hay sesiones.** `setsid` es "hazte líder de grupo". No existen
  `getsid`, tty de control, `SIGHUP` ni `SIGWINCH` (y `SIGWINCH`/`SIGURG`
  terminan el proceso por defecto: en Linux se ignoran).
- La consola no tiene rejilla de celdas: el parser ANSI de
  `framebuffer_console.rs` dibuja directamente en el framebuffer y hace
  scroll moviendo píxeles.

### Decisiones (tomadas con el usuario, 2026-09-25)

- **La disciplina de línea va en un crate de host nuevo, `tty/`**, como
  `usock`: máquina de estados pura, sin bloqueos, con los efectos como
  datos. La usa el pty ya; la consola podría migrar después.
- **Sesiones reales:** `sid` por proceso, `setsid`/`getsid` reales, tty de
  control, `termios` y grupo en primer plano por tty, `SIGHUP` al cerrar el
  maestro y `SIGWINCH` con `TIOCSWINSZ`.
- **El emulador va en un crate nuevo, `vt/`, y la consola del kernel se
  queda como está.** Su rendimiento está medido en la Ryzen
  (`docs/fb/console-perf.md`); pasarla a una rejilla obligaría a medirlo
  todo otra vez. Migrarla es una opción para después, no parte de esta fase.

### 3.1 El crate `tty/` (host, `cd tty && cargo test`)

`no_std` + `alloc`. Los valores de `termios` son los de **este port**
(`mlibc-port/.../abi-bits/termios.h`: `ISIG = 0x40`, `ICANON = 0x10`,
`ECHO = 0x01`…), no los de Linux, y el struct ocupa 68 bytes, como
`kernel::tty::Termios`, que pasa a ser un reexport.

- **`LineDiscipline`**: `termios`, cola de entrada y línea en edición.
  - `receive(bytes) -> Input`: lo que teclea el maestro. Aplica
    `ISTRIP`/`INLCR`/`IGNCR`/`ICRNL`, `ISIG` (`VINTR`/`VQUIT`/`VSUSP` →
    señal devuelta como dato, y vacía la cola salvo `NOFLSH`), `ICANON`
    (`VERASE`/`VKILL`/`VEOF`/`VEOL`/`\n`) y el eco
    (`ECHO`/`ECHOE`/`ECHOK`/`ECHONL`). El eco vuelve como bytes para la
    salida del maestro. Este ABI no tiene `VWERASE` ni `ECHOCTL` (sus
    `c_cc` son 11), así que no se implementan.
  - `read(buf) -> Read`: en canónico, como mucho una línea, y `VEOF` en
    una línea vacía es fin de fichero (0 bytes). En crudo, `VMIN`/`VTIME`:
    `VMIN > 0` y `VTIME = 0` es lo habitual; `VMIN = 0` y `VTIME = 0`
    devuelve lo que haya; con `VTIME > 0` devuelve el plazo, y lo arma el
    kernel.
  - `output(bytes) -> bytes`: `OPOST`/`ONLCR`/`OCRNL`/`ONLRET`.
  - `readable()`, `set_termios` (con `TCSETSF` vacía la entrada) y `flush`.
- **Las reglas de control de trabajos**, puras: con `(pgid y sid del que
  llama, sid y grupo en primer plano del tty, si ignora o bloquea la
  señal, si su grupo es huérfano)` deciden **permitir**, **mandar
  `SIGTTIN`/`SIGTTOU` y reiniciar** o **`EIO`**. Una lectura desde segundo
  plano da `SIGTTIN`; una escritura con `TOSTOP` y un `tcsetpgrp`/
  `TCSETS` desde segundo plano dan `SIGTTOU`. Con la señal ignorada o
  bloqueada, la escritura se permite y la lectura da `EIO`.
- **`Winsize`**, y el tamaño que cambia devuelve "manda `SIGWINCH`".

Tests como en `usock`: cada regla por separado, secuencias reales (ash en
crudo, `cat` en canónico con retroceso y `^U`, `^D` a mitad de línea, un
`^C` que vacía la cola) y propiedades (la cola nunca pasa de su capacidad;
en canónico, `read` nunca devuelve media línea salvo que el búfer sea más
pequeño).

### 3.2 Kernel: sesiones

- `Process` gana `sid`, heredado en `fork` y `clone`. `setsid` real:
  falla con `EPERM` si ya existe un grupo con el pid del que llama y, si
  no, crea sesión y grupo nuevos. `getsid` (124).
- `setpgid` sigue las reglas de POSIX: solo sobre uno mismo o un hijo
  (`ESRCH`), en la misma sesión y sin ser líder de sesión (`EPERM`), y
  unirse a un grupo solo si existe en la sesión (`EPERM`).
- `SIGWINCH` (28) y `SIGURG` (23) se ignoran por defecto, como en Linux.
- `/proc/<pid>/stat` da la sesión real en el campo 6.

El tty de control (`TIOCSCTTY`, `TIOCNOTTY`, `/dev/tty`, `TIOCGSID`) y el
estado por tty pasan al 3.3: sin un pty no hay ningún tty sobre el que
actuar ni con el que probarlos. La consola sigue con su estado global y su
camino de siempre (`feed_input`, fd 0), y sus `ioctl`s no cambian.

### 3.3 Kernel: el pty

- **El tty de control** (de 3.2): `Process` gana `ctty`. `TIOCSCTTY`
  desde un líder de sesión sin tty, y el primer `open` del esclavo sin
  `O_NOCTTY` por un líder sin tty, como en Linux (`tty::jobctl`).
  `TIOCNOTTY` lo suelta, `/dev/tty` abre el del proceso (`ENXIO` si no
  tiene) y `TIOCGSID`. `termios`, grupo en primer plano, sesión y
  `winsize` viven en el pty. Cuando el líder de sesión muere, la sesión
  pierde su tty de control.
- **`/dev/ptmx`**: cada `open` crea un par (hasta 16) y devuelve el
  maestro. `TIOCGPTN` da el número y `TIOCSPTLCK` lo desbloquea; el
  esclavo, `/dev/pts/N`, no se puede abrir hasta entonces. `/dev/pts`
  lista los pares vivos, igual que `InputDirInode`. Un par muere cuando
  se cierran el maestro y todos los esclavos.
- **Tabla global `PTYS`** (como `SOCKETS`), con los bloqueos de las
  tuberías: colas FIFO de lectores y escritores con `WaitCell`,
  `arm_wait`, `deliver_to_waiter`, y el pid consultado antes de tomar el
  lock del par (el ABBA con `fork` de `pipe_multi_test`). Esclavo →
  maestro es un anillo simple, porque `OPOST` se aplica al escribir.
  Maestro → esclavo pasa por `LineDiscipline::receive`, y su eco vuelve
  al anillo del maestro.
- **Cierre**: al cerrar el maestro, `SIGHUP` + `SIGCONT` al grupo en
  primer plano y al líder de sesión; el esclavo lee 0 y escribe `EIO`.
  Con todos los esclavos cerrados, el maestro lee `EIO` (Linux) y
  `poll` da `POLLHUP`.
- **`poll`/`epoll` de verdad** sobre las dos puntas: un
  `PollSource::Pty` en la instantánea de `poll.rs`, despertado desde el
  lado que escribe. Sin esto, el terminal, que espera al socket del
  compositor y al maestro con un `epoll`, giraría al 100 %.
- `TIOCSWINSZ` en cualquiera de las dos puntas guarda el tamaño y manda
  `SIGWINCH` al grupo en primer plano; `TIOCGWINSZ` lo devuelve.
  `FIONREAD`.
- **mlibc**: `posix_openpt`/`grantpt`/`unlockpt`/`ptsname`/`openpty`
  sobre esos `ioctl`s; `ttyname` si hace falta. Borrar `busybox.elf` tras
  cambiar cabeceras. `CONFIG_SCRIPT` de BusyBox da un cliente de pty
  real para probar sin GUI (`script -c 'ls' /tmp/out`), y `stty`/`tty`/
  `reset`, que no están activados.
- **Test:** `userspace/c/pty_test.c` en el disco. Cubre ida y vuelta en
  crudo y en canónico, eco, retroceso, `^C` → `SIGINT` al grupo en primer
  plano y no a otro, `SIGTTIN` desde segundo plano, `SIGHUP` al cerrar el
  maestro, `EIO`/`POLLHUP` al cerrar el esclavo, `SIGWINCH`, `poll`
  despertado por la otra punta, y ash de verdad en el esclavo (escribir
  `echo hola` y leer `hola` de vuelta).

### 3.4 El crate `vt/` (host, `cd vt && cargo test`)

`no_std` + `alloc`, sin syscalls.

- **`Grid`**: celdas `{ch, fg, bg, attrs}`, cursor, región de scroll,
  pantalla alternativa (`?1049`, para `vi`/`less`/`top`) y el daño en
  filas.
- **`Parser`**: el conjunto de la consola (CUP/CUU…/ED/EL/SGR con 16 y
  256 colores, negrita, inversa) más lo que piden `vi`, `less` y `top`:
  `DECSTBM`, IL/DL/ICH/DCH/ECH, guardar y restaurar el cursor, `?25`
  (cursor visible), `?7` (autowrap) y `DSR 6n`, cuya respuesta vuelve como
  dato para escribirla en el maestro. Lo desconocido se ignora sin
  romper el estado. La paleta es la de la consola.
- **`render(grid, damage, dst, stride)`** a un `&mut [u32]` con
  `noto-sans-mono-bitmap` (el mismo crate y tamaños que la consola) y el
  cursor. Los tests componen en un `Vec` y comprueban píxeles, igual que
  en `gui`.
- **`keymap`**: código `KEY_*` + modificadores → bytes (letras con
  Mayús/Bloq Mayús, Ctrl-letra, flechas y `Home`/`End`/`Supr`/`RePág`
  como las manda la consola, Alt → `ESC` delante). Es el mapa US de
  `hal::keyboard::KeyDecoder`, desde códigos evdev en vez de Set-1.

### 3.5 `term`: el terminal con ventana

`userspace/src/bin/term.rs`, en Rust y embebido. Abre un pty, hace `fork`,
y el hijo hace `setsid`, abre el esclavo (que así queda como tty de
control), lo pone en 0/1/2, cierra el resto y hace `exec` de
`busybox ash`. El padre crea una superficie de 80×25 celdas y espera con
un `epoll` al socket del compositor y al maestro:

- bytes del maestro → `Parser` → `render` del daño → `attach` + `damage`
  + `commit`, como mucho uno por `frame`;
- `key` del compositor → `keymap` → escritura en el maestro;
- el hijo muere o el maestro da `EIO` → el terminal sale.

`compositor term` es la forma de lanzarlo. Prueba de extremo a extremo:
`scripts/gui-e2e.sh` gana un modo `term`, que teclea `echo hola` por
`sendkey` y comprueba el `screendump`, más `^C` sobre un `sleep` y `vi`
abriendo y cerrando.

### Fuera de alcance (fase 3)

Redimensionar la ventana (el protocolo no tiene `configure` desde el
cliente), selección y portapapeles, historial de scroll, Unicode más allá
del latín básico de la fuente, `TIOCSTI`, paquetes (`TIOCPKT`), y pasar la
consola del kernel a `tty`/`vt`.

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

### Fase 2.3 (2026-09-25)

Hecho como se planeó. Lo que decidió la forma del allocator
(`userspace/src/heap.rs`) fue el `mmap` del kernel, no un diseño genérico:

- **64 VMAs por proceso.** Clases de potencias de dos de 16 B a 64 KiB,
  con listas libres intrusivas, cortadas de chunks de 1 MiB que nunca se
  devuelven. Por encima de 64 KiB, un `mmap` por bloque y `munmap` al
  liberar. Así miles de objetos pequeños ocupan pocos VMAs y solo los
  grandes (framebuffers, `Vec` enormes) gastan uno cada uno. El resto de
  un chunk abandonado no cuesta RAM: nunca se toca, nunca se pagina.
- **`munmap` exige el VMA exacto**, dirección y longitud. `large_len`
  recalcula la longitud desde el `Layout` con el mismo redondeo del
  `mmap`, incluido el de 2 MiB: una petición de 2 MiB o más se convierte
  en un VMA de páginas de 2 MiB cuya longitud el kernel redondea. Los
  chunks se quedan en 1 MiB para no caer nunca en ese camino.
- **Alineación:** `mmap` da 4 KiB. Un bloque de clase `c` queda alineado a
  `min(c, 4096)`. Una alineación mayor que 4096 devuelve null en vez de
  fingirse.
- Un spin lock que cede la CPU (`yield`) al esperar. Hoy los programas de
  Rust tienen un solo hilo; el lock deja correcto un hilo futuro.

Wrappers nuevos en `syscall.rs`: `mmap` completo (`MAP_SHARED`, fd,
offset), `memfd_create`, `ftruncate`, `ioctl`, `sendmsg`/`recvmsg` con
las estructuras de Linux, y encima `send_fds`/`recv_fds` (`SCM_RIGHTS`,
hasta 8 fds; lo que no cabe se cierra y sale `MSG_CTRUNC`), y
`epoll_create`/`epoll_ctl`/`epoll_wait` (`epoll_event` empaquetado, 12
bytes).

**Test: `userlib_test`** (Rust, embebido), 30 comprobaciones: `Box`/`Vec`/
`format!`, reutilización de un bloque liberado, toda alineación hasta
4096 y el rechazo de 8192, 100000 cajas en 1-3 chunks, bloques de 1 y
3 MiB a cero y devueltos (`MemFree` igual antes y después), 200 × 5 MiB
seguidos, un `Vec` que crece a 4 MB por todos los caminos de `realloc`;
memfd con dos mapeos que se ven entre sí y `EBUSY` al encoger; un memfd
pasado a un hijo por `SCM_RIGHTS` que escribe en su propio mapeo y el
padre lo lee; 2 fds en hueco para 1; `epoll` con su dato de vuelta;
`TIOCGWINSZ`. **Probado por sabotaje:** con la longitud de `munmap`
desfasada una página fallan "unmapped on drop" y "memory given back", y
el bucle de 5 MiB agota los VMAs y muere.

**Verificado en QEMU con `-smp 4`, 512 MiB y 8 GiB:** `userlib_test`
PASS; `mmap_test`, `poll_test`, `ipc_ping`, `pipe_test` y `signal_test`
en 0; `boot-matrix.sh 4 3` 12/12 (todos los programas de Rust, PID 1
incluido, enlazan ahora el allocator).

### Fase 2.4 (2026-09-25)

El crate `gui/` (`cd gui && cargo test`, 27 tests, sin QEMU), con los
cuatro módulos: `region`, `wire`, `protocol` (las peticiones y eventos
tipados, separados de `wire`) y `compositor`. Compila también para
`x86_64-unknown-none` como dependencia de `userspace`, y el `build.rs`
raíz vigila `gui/src` (a diferencia de `hal`/`usock`, sus cambios acaban
en programas embebidos).

Decisiones que el plan no fijaba:

- **Cada superficie guarda su propia copia.** Como `release` sale en el
  mismo `commit`, el cliente puede reescribir el búfer enseguida; sin
  copia no habría de dónde repintar una ventana al destaparla. `commit`
  copia solo el daño (o todo, si la superficie cambia de tamaño o se
  mapea) y `compose` lee únicamente copias, nunca memoria del cliente.
  Coste: `w x h x 4` bytes por ventana.
- **El mapeo del pool es lo único síncrono.** `client_data` recibe un
  cierre `map(fd, size)`; el fd se añade a `take_fds_to_close()` tanto si
  el mapeo sale bien como si no. Los fds recibidos que ningún mensaje
  reclama también se cierran al quitar al cliente.
- **Un búfer guarda el `Rc` de la memoria de su pool**, así que destruir
  el pool no invalida sus búferes (regla de Wayland). `attach` captura el
  búfer entero, así que destruirlo antes del `commit` tampoco rompe nada.
- **`compose(dst, stride)` devuelve los rectángulos que volcar**, como
  mucho 16 (lo que acepta `FBIO_FLUSH`); con más, su caja envolvente.
- Colocación en cascada desde (40, 40), barra de título de 20 px (color
  según el foco), cursor de 11x16 dibujado por software, y
  Ctrl+Alt+Retroceso marca `quit_requested()` sin llegar a ningún cliente.
  `configure` propone la mitad de la pantalla.

**Tests:** `region` contra un mapa de bits (300 semillas × 30 operaciones
de unir, restar y recortar, comprobando además que los rectángulos nunca
se solapan); `wire` con el mensaje cortado en cada byte posible;
`protocol` ida y vuelta de toda petición y evento; el compositor
conducido con peticiones codificadas de verdad y comprobado píxel a
píxel, sobre una pantalla con `stride` mayor que el ancho cuyo relleno
debe quedar intacto. **Probados por sabotaje**, cada uno detectado por su
test: quitar la comprobación de límites del búfer, ignorar el daño
(copiar todo), invertir el orden Z y no terminar el arrastre al soltar
(este último no lo detectaba nadie hasta añadir un movimiento después de
soltar).

### Fase 2.5 (2026-09-25)

`compositor` y `gui_demo` (Rust, embebidos) sobre el crate `gui`. Primera
ventana en QEMU a la primera: cascada en (40, 40), barra de título con
foco, degradado animado, cursor por software.

- **`compositor [prog...]`**: `/dev/fb0` mapeado, `event0` con
  `EVIOCGRAB` (se descarta lo que el anillo tenía de antes) y `event1`,
  escucha en `/tmp/gui-0` y espera con un solo `epoll`. Como mucho un
  `compose` + `FBIO_FLUSH` cada 16 ms. Lanza cada `prog` tras escuchar, y
  **el hijo cierra todo fd ≥ 3 antes del `exec`**: el kernel no aplica
  close-on-exec, y un cliente que heredase `/dev/fb0` y el `event0`
  capturado mantenía el modo gráfico y el teclado tras morir el
  compositor. Un cliente que no lee sus eventos (`send` con
  `MSG_DONTWAIT` incompleto) se desconecta, como hace libwayland. Antes de
  mapear un pool se comprueba con `fstat` que el memfd es tan grande como
  dice el cliente: tocar una página más allá del objeto mataría al
  compositor, no al cliente.
- **`REL_Y` del ratón viene con el convenio PS/2** (positivo hacia
  arriba, lo que esperan DOOM y Quake); el compositor lo niega.
- **argv para Rust**: `userspace::args` + la macro `entry!`, un `_start`
  desnudo que pasa el `rsp` de entrada. `println!` pasó a expandir a un
  bloque (no servía en un brazo de `match`).
- **Ritmo**: `gui_demo` va a 48-49 fps y no a 60. El `epoll_wait` con
  plazo se despierta en el siguiente tick de 10 ms, así que un cuadro de
  16 ms cuesta 20.
- **Medido en QEMU**: 1797 cuadros en 1555 ms de `compose` + `FBIO_FLUSH`
  (~0,9 ms por cuadro, reloj en ms, así que orientativo). La cifra que
  importa es la de la Ryzen, con VRAM en WC; los fallos de página del
  primer toque a un búfer de 1080p quedan por medir allí.

**Bug del kernel encontrado aquí** (arreglado y con test propio): matar el
compositor con `SIGKILL` colgaba todas las CPUs. Una muerte por señal o
por fallo dejaba la tabla de fds en el zombi, y el `waitpid` que lo
recogía la soltaba con `SCHEDULER` tomado; el `Drop` de un socket AF_UNIX
vuelve a tomarlo. Hasta la recogida, además, sus pares no veían EOF.
Ahora `kill_current` mueve la tabla a `process::dead_files`, que se vacía
sin locks y con IF=0 al entrar a cada syscall y en el bucle idle (el
primer intento la vaciaba con IF=1 en el idle y el `Drop` de una tubería
hizo saltar la aserción de `local_scheduler`). Test: caso E de
`lifecycle_test` (EOF en socket y tubería *antes* de recoger al hijo;
con el arreglo saboteado da FAIL). Y la pantalla de pánico se caía con un
pánico anidado al dibujar un mensaje con un carácter no ASCII (la fuente
8x8 tiene 128 glifos): ahora dibuja `?`.

**Prueba de extremo a extremo: `scripts/gui-e2e.sh`** (11 comprobaciones
sobre `screendump` y el log): ventana y cursor en su sitio, movimiento
1:1, arrastre por la barra, clic y teclas en la ventana y no en ash,
Ctrl+Alt+Retroceso, EOF en el cliente y consola de vuelta. **PASS por
PS/2 y por USB** (`QEMU_USB_KBD=1 QEMU_USB_MOUSE=1 QEMU_DEBUG_NO_PS2=1`,
8 GiB). A mano, además: dos clientes (orden Z, clic para subir y foco) y
`SIGKILL` del compositor.

**Regresión** con `-smp 4 -m 8G`: 17 programas de prueba en 0
(`wait_intr_test`, `sigsuspend_test`, `lifecycle_test`, `pipe_multi_test`,
`pipe_cow_test`, `socket_test`, `mlibc_signal_test`, `jobctl_test`,
`shm_test`, `fork_exec_test`, `pthread_test`, `userlib_test`,
`poll_test`, `ipc_ping`, `pipe_test`, `signal_test`, `mmap_test`),
`run-kernel-tests.sh` PASS, `boot-matrix.sh 4 4` 16/16, `gui` 27/27.

**Verificado en la Ryzen (boot #45, `target/metal/gui-job.sh`, sin nadie
delante):** `userlib_test`, `lifecycle_test` (caso E incluido),
`input_poll_test`, `wait_intr_test`, `sigsuspend_test`, `socket_test`,
`pipe_multi_test` y `fb0_test` en 0. El compositor a 1920x1080 (stride
2048, VRAM en WC), matado con `SIGKILL` dos veces: el cliente ve el EOF,
`/dev/fb0` queda libre (`mode: text`) y la máquina sigue viva (antes de
7716122 ese `SIGKILL` colgaba todas las CPUs). `gui_demo` a 50 fps tanto
con 320x200 como con 1900x1000 (el techo lo pone el tick de 10 ms, no el
volcado). `fb_flush` durante las dos pruebas: 831 volcados, 3,19 GB, a
~5,6 GB/s, así que una ventana de 1900x1000 (7,6 MB por cuadro) cuesta
~1,4 ms de volcado por cuadro. `invariants=ok`. Los fallos de página del
primer toque no se midieron aparte; no aparecen en el ritmo.

### Fase 3.1 (2026-09-25)

Crate `tty/` (`cd tty && cargo test`: 48 tests, 0,4 s), en cuatro módulos:
`termios`, `ldisc`, `jobctl` y `pty`. Todavía no es dependencia del kernel;
lo conectan 3.2 y 3.3.

- **La semántica del par también está en el host**, no solo la
  disciplina de línea. `pty::Pty` decide EOF y `EIO` tras el cuelgue,
  `EIO`/`POLLHUP` en el maestro cuando se cierra el último esclavo (y no
  antes de que se abra ninguno), el bloqueo `TIOCSPTLCK` y a quién va cada
  señal. Así el adaptador del kernel queda como el de `usock`: bloquear,
  despertar y mandar señales, sin reglas propias.
- **Contrapresión en los dos sentidos.** `receive` deja sin consumir lo
  que no cabe en la entrada (4096 bytes), así que el escritor del maestro
  espera a un lector del esclavo. La salida del esclavo se procesa
  (`ONLCR`) al escribir, nunca a medias (un `\n` que sería `\r\n` con
  un solo hueco espera), así que el escritor del esclavo espera al lector
  del maestro. En canónico, una línea llena descarta caracteres pero
  sigue aceptando los que la editan o la terminan.
- **`VEOL` a 0 está desactivado** (`_POSIX_VDISABLE`). Si no, todo NUL
  terminaría una línea, porque `sane()` deja `VEOL = 0`.
- **Cuelgue:** `SIGHUP` + `SIGCONT` al líder de sesión y al grupo en
  primer plano, y `detach_session` para que el kernel quite el tty de
  control a toda la sesión.
- **Sin implementar, a propósito:** `IXON`/`IXOFF` (`^S`/`^Q` llegan como
  bytes), paridad, y borrar un tabulador por columnas exactas.
- **Probado por sabotaje:** 7 de 7 detectados (vaciar la cola con `ISIG`,
  `VDISABLE`, `EIO` antes de abrir un esclavo, una línea leída que dejaba
  una marca de EOF, el hueco del fin de línea, `SIGTTOU` ignorada y el
  despertar del escritor del maestro).

### Fase 3.2 (2026-09-25)

Sesiones reales: `Process::sid`, `setsid` y `getsid` reales, `setpgid`
con las reglas de POSIX y `SIGWINCH`/`SIGURG` ignoradas por defecto (hasta
ahora terminaban el proceso: un cambio de tamaño del terminal habría
matado a todo programa sin manejador). `sys_getsid` en el port de mlibc.
Test nuevo en el disco, `session_test` (20 comprobaciones: `setsid` desde
un líder y desde un hijo, herencia en `fork`, `setpgid` entre sesiones,
sobre un no hijo, a un grupo inexistente y al de un hermano, `getsid` de
nadie, las dos señales y el campo 6 de `/proc/self/stat`).

El tty de control se mueve al 3.3 (ver arriba).

**Verificado en QEMU** (`-smp 4`, 8 GiB): `session_test` PASS; en 0
`jobctl_test`, `lifecycle_test`, `wait_intr_test`, `sigsuspend_test`,
`mlibc_signal_test`, `pipe_multi_test`, `socket_test`, `fork_exec_test` y
`userlib_test`; `^C`, `^Z` y `jobs` en ash; `boot-matrix.sh 4 5` 20/20;
`run-kernel-tests.sh` PASS; `gui-e2e.sh` 3/3.

**`gui-e2e` destapó una carrera en `gui_demo`**, no en el kernel. Si el
compositor cierra justo cuando `gui_demo` manda un cuadro, el `send`
falla con `EPIPE` y `gui_demo` salía con 1 sin decir nada, así que la
comprobación 6 no veía "compositor gone". Ahora ese camino lo dice
también.

**Bug abierto, anterior al 3.2** (se reproduce igual en `ab6a120`): con
un trabajo parado, `sleep 2 & wait` en ash no vuelve nunca. Traza con
`kdebug proc on`: tras recoger el `sleep`, `waitpid(-1, WNOHANG|WUNTRACED)`
da 0, porque el hijo parado existe y su parada ya se contó, y ash vuelve a
`sigsuspend` sin nada que lo despierte (`dowait` repite `waitone` mientras
el pid sea ≥ 0). En el host, la `busybox` 1.36.1 de Arch hace ese mismo
último `wait4` = 0 y termina sin `rt_sigsuspend` (`strace`). Queda saber
si esa `busybox` no es el mismo código que el submódulo o si hay una
diferencia de kernel que falta.

### Fase 3.3 (2026-09-25)

El pty: `/dev/ptmx`, `/dev/pts/N` (se listan mientras el maestro esté
abierto), `/dev/tty`, el tty de control (`Process::ctty`) y el control de
trabajos sobre él. `kernel/src/ipc/pty.rs` es el adaptador del crate
`tty`, con la forma de `ipc/unix.rs`: `PTYS`/`WAITERS` como
`diag::IrqMutex`, bloqueo que **reinicia la syscall** (registrarse,
`rip -= 2`, `WAKE_EPOCH`) y los `Effects` del crate aplicados sin `PTYS`
tomado. Reiniciar en vez de completar desde quien despierta tiene aquí
otra razón: una lectura del esclavo que se repite vuelve a pasar el
control de trabajos, así que un trabajo que pasó a segundo plano mientras
dormía se para con `SIGTTIN` en vez de quedarse la entrada del primer
plano.

- **`SIGTTIN`/`SIGTTOU` y reinicio.** Una lectura (o escritura con
  `TOSTOP`) desde segundo plano manda la señal a su grupo y bloquea en una
  espera que la señal pendiente interrumpe en el acto (`block_current` no
  duerme con una señal que actúa); la entrega decide repetir la llamada.
  Un `ioctl` no puede bloquear: rebobina `rip` y devuelve el número de la
  syscall, que es lo que `rax` tiene que tener para que `syscall` vuelva a
  ejecutarse.
- **`poll`/`epoll` reales** en las dos puntas (`PollSource::Pty`, con
  `FileHandle::pty_end` en `vfs`, la técnica de `socket_id`).
- **mlibc:** `sys_ptsname`, `sys_unlockpt`, `sys_ttyname` (por el número
  de inodo del esclavo, al no haber `/proc/self/fd`) y un `tcflush` real;
  `FLUSHO` en `termios.h` (un bit libre, que `stty` necesitaba). **BusyBox:**
  `FEATURE_DEVPTS`, `script`, `stty`, `tty`, `reset`, `ttysize`. `script`
  necesita `SHELL=/tmp/bin/sh`, porque no hay `/bin/sh`.
- **`O_NONBLOCK` en `open`** para los handles que lo admiten (sockets y
  ptys); antes solo `fcntl` lo ponía.

**Bug del kernel encontrado aquí** (anterior, arreglado): un `SIGKILL` no
mataba a un proceso parado. Solo `SIGCONT` reanudaba a los parados, así que
`kill -9` dejaba la señal pendiente en un proceso que no volvía a
ejecutarse y el `waitpid` de su padre no volvía nunca. Ahora `SIGKILL`
también lo reanuda (`signal::resumes_stopped`), en `kill` y en las señales
del terminal. Lo encontró el caso E de `pty_test`, que mata al hijo parado
por `SIGTTIN`.

**Sin implementar, a propósito:** el plazo de `VTIME` (una lectura que lo
esperaría devuelve lo que haya), y colgar el terminal cuando muere el
líder de sesión (Linux manda `SIGHUP` a su grupo en primer plano; aquí el
cuelgue es el cierre del maestro).

**Verificado en QEMU** (`-smp 4`, 8 GiB): `pty_test` (11 casos: abrir y
desbloquear, crudo, canónico con eco y borrado, `^C`, `SIGTTIN`, `SIGHUP`
al cerrar el maestro, `EIO`/`POLLHUP` al cerrar el esclavo, `SIGWINCH`,
`poll` despertado por la otra punta, `/dev/tty` y ash de verdad en un
esclavo). A mano, `script` de BusyBox con ash dentro: `tty` da
`/dev/pts/0`, `stty size` hereda el tamaño de la consola, y `^C`, `^Z`,
`jobs` y `fg` funcionan dentro del pty; al salir, la consola recupera su
`^C`. En 0 `session_test`, `jobctl_test`, `lifecycle_test`,
`wait_intr_test`, `sigsuspend_test`, `mlibc_signal_test`,
`pipe_multi_test`, `pipe_cow_test`, `socket_test`, `fork_exec_test`,
`userlib_test`, `poll_test`, `input_poll_test`, `shm_test` y `fb0_test`;
`boot-matrix.sh 4 5` 20/20; `run-kernel-tests.sh` PASS; `gui-e2e.sh`
PASS; `vfs` 166 tests.

**Verificado en la Ryzen (boot #46, `target/metal/pty-job.sh`, sin nadie
delante):** `pty_test` con sus 11 casos en PASS, ash en un esclavo
incluido; `session_test` PASS; en 0 `jobctl_test`, `lifecycle_test`,
`wait_intr_test`, `sigsuspend_test`, `mlibc_signal_test`, `socket_test`,
`pipe_multi_test` y `userlib_test`. `script` de BusyBox: `tty` da
`/dev/pts/0` y `stty size` da `44 174`, el tamaño de la consola a
1920x1080. `sched: max_concurrent=4 invariants=ok`. El principio del log se
perdió (el anillo de 64 KiB se desbordó con las trazas de `exec`), pero la
sección de resultados y el `METAL-DONE exit=0` están enteros.

