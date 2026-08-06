# Estado de la investigación: cuelgue de arranque y corrupción del TrapFrame (julio–agosto 2026)

> Fuente: transcript `2026-08-05-003124-local-command-caveatcaveat-the-messages-below.txt`
> (sesión de Claude Code del 2026-08-05, eliminada tras documentar). Este archivo es el
> registro permanente; **no borrar sin actualizar esto primero**.

## Resumen ejecutivo

Dos bugs se investigaron como uno solo durante meses. El síntoma visible siempre fue el
mismo: el arranque de PID 1 (`busybox --install -s /tmp/bin`) cuelga o paniquea de forma
intermitente (~10 % en builds debug, ~0 % en release). La investigación (4+ sesiones)
terminó separando:

1. **CERRADO — Self-deadlock del `SLAB_ALLOCATOR`** (commits `ab58dba`, `7c0754d`).
   Causa real del cuelgue clásico. Cualquier camino de kernel interrumpible que asigne
   memoria podía colgarse: el `#[global_allocator]` tomaba su `spin::Mutex` con
   interrupciones habilitadas; el timer ISR → `switch_to_next` → `push_back` de un
   `VecDeque` de run-queue crecía → reentraba el mismo lock no reentrante en el mismo CPU
   → spin infinito. **No hace falta ni `fork` ni `exec`**: `vfs::resolve_inner` asignando
   un `Vec<&str>` para partir una ruta ya bastaba (por eso el amplificador de mkdir/symlink
   reproducía a la primera). Verificado por gdb en vivo (`_mm_pause` en
   `SpinMutex<SlabAllocator>::lock` dentro del camino del timer). Fix: `without_interrupts`
   alrededor de cada adquisición de `BUDDY` y `SLAB_ALLOCATOR` + canario permanente
   `SLAB_LOCK_CONTENDED` (`/proc/kdebug`). Medido: ~10 % → 0/20, y 20/20 con el harness
   corregido.

2. **ABIERTO — Puntero de marco de excepción (`sf`) corrupto.** El handler de excepción
   (p. ej. `page_fault_handler`) recibe un `sf` que **no apunta a la pila**: apunta a datos
   (`.text` del kernel, o la estructura de datos de un `BTreeMap`). Los valores del pánico
   "imposibles" (RIP/CS/RSP) son datos interpretados como marco, no estado de CPU. Pregunta
   abierta: *¿por qué un handler recibe un puntero de marco que no apunta a la pila?*

Este documento detalla el bug 2 (el que queda), el WIP instrumentado que lo está cazando, y
las lecciones de método (caveats) de toda la sesión.

---

## Bug 1 (cerrado): self-deadlock del slab

### Mecanismo

```
sys_mkdir → vfs::resolve_inner → alloc → SLAB_ALLOCATOR.lock()   ← lock tomado, IF=1
        ⚡ timer interrupt
        → timer_preempt_handler → switch_to_next
        → run_queues[pri].push_back(proc)   ← VecDeque crece → asigna
        → SLAB_ALLOCATOR.lock()             ← mismo CPU, mismo lock no reentrante → spin
```

- `run_queues: [VecDeque<Box<Process>>; NUM_PRIORITIES]` — `push_back` asigna al agotar
  capacidad.
- `spin::Mutex` no es reentrante y nunca hace `cli`.
- Era un invariante ya documentado para `SCHEDULER` ("siempre cli antes de tomarlo, porque
  el ISR del timer también lo toma") que nadie trasladó a los asignadores.

### Por qué busybox parecía el culpable

BusyBox no tenía nada de especial salvo que **asigna mucha memoria** (pico de asignaciones
coincide con el crecimiento de las run queues durante `--install`). La caza se centró meses
en fork/COW/buddy por aceptar el encuadre que traía el síntoma.

### Fix (ab58dba + 7c0754d)

- `x86_64::instructions::interrupts::without_interrupts` (restaura estado previo; seguro
  desde el propio ISR) alrededor de toda adquisición de `BUDDY` y `SLAB_ALLOCATOR`.
- Canario permanente `SLAB_LOCK_CONTENDED` (`kernel::debug`) que detecta cualquier camino
  que escape la regla.
- **El canario mismo causaba el deadlock que medía** (~2/24 arranques): `probe_contention()`
  corría *fuera* del `without_interrupts`; cuando su `try_lock()` acertaba, retenía el lock
  de verdad con IF=1 hasta que caía el guard → ventana recreada. Movido dentro del bloque
  protegido (`7c0754d`).

### Mediciones

| Estado | Resultado |
|---|---|
| Antes del fix (histórico) | ~10 % HANG en debug |
| Amplificador mkdir/symlink pre-fix | 15/15 fallos, 14 HANG |
| Amplificador post-fix | 0 HANG |
| Canario fuera (con bug del canario) | 6/24 fallos (25 %), 3 PANIC + 3 HANG, 2 con firma de reentrada |
| Canario dentro | 1/24 (4.2 %) — y ese fallo era el bug 2 |
| boot-matrix post-fix | 0/20, 20/20 OK |

---

## Bug 2 (abierto): el marco de excepción corrupto

### Síntomas (4 manifestaciones, un solo mecanismo)

1. `current_tf` desalineado en `switch_to_next` (scheduler.rs:903): `*proc.trapframe =
   *current_tf` con `current_tf` conteniendo basura pequeña (0x4f, 0x2f, 0x8f — bytes que
   parecen texto de rutas como `/tmp/bin/h0`).
2. Page fault de kernel en instruction fetch en `0x0` tras "Process exited, switching
   immediately" (fork+exit), con doble pánico del handler.
3. Corrupción del `BTreeMap` de ramfs (`entries` de directorios — justo lo que
   mkdir/symlink mutan).
4. Lock `entries` de ramfs permanentemente retenido (`_mm_pause` en spinlock; el lock
   "fantasma" de un contexto abandonado).

Todos con `forks_total: 0` — el segundo bug es independiente del primero.

### Siete hipótesis refutadas (con evidencia, no volver a probarlas)

| # | Hipótesis | Evidencia de refutación |
|---|---|---|
| 1 | Tamaño/desbordamiento de pila de kernel | Guard page se descartó antes; el desbordamiento que salta la guard no encaja con las firmas |
| 2 | Quake / stack pequeño | Reproduce sin Quake ni stack grande |
| 3 | Carrera de refcounts COW | Medido: 0 violaciones |
| 4 | Doble reparto de frames por el buddy | 484 282 ops aleatorias host, 0 PhantomEvents, 0 violaciones; el panic "UAF" que lo apoyaba era falso positivo (ver abajo) |
| 5 | `sti` prematuro en `sys_exit` | Refutado por el propio agente: reproduce con `forks_total: 0` (variante sin fork/exit) |
| 6 | IRETQ same-CPL no restaura RSP/SS (hipótesis **escrita en el código**, scheduler.rs:985) | Falsa en long mode: IRETQ hace pop de los 5 valores siempre. Refutada dos veces por separado; los logs la contradicen (`rsp_field` cae dentro de `kstack_range`). **Eliminada del código** durante la caza (en el WIP) |
| 7 | Rebobinado del `Box<TrapFrame>` (SAVE tardío pisa un RESUME más reciente) | `tf_rewind: count=0` con instrumentación en los 5 puntos que tocan `proc.trapframe` (incl. sys_exec), 24 000+ ciclos SAVE en 48 arranques |
| 8 | Puntero de vtable corrupto de `Arc<dyn Inode>` en ramfs | Se cayó al confirmar la correspondencia campo a campo (ver abajo): no es una vtable saltando, es el marco leído desde los datos |

Nota: también se descartó durante la sesión el uso- después-de-free del slab como fuente:
el chequeo UAF del slab estaba **roto** (ver sección de hallazgos secundarios) y disparaba
falsos positivos con direcciones que contienen el byte `0xAA`.

### El hallazgo vigente: los valores del pánico son datos de un BTreeMap

El pánico determinista (byte a byte idéntico en arranques independientes):

```
PAGE FAULT (kernel)   Reason: Protection violation
  RIP: 0x2801e9cd000   CS: 0x3   RSP: 0x1
```

vs. el volcado en vivo del nodo raíz del BTreeMap de `/tmp` en el mismo arranque:

```
[tmp_root_diag] /tmp entries raw words: [0]=0x2801e9cd000  [1]=0x3  [2]=0x690  (len()=1680)
```

Correspondencia campo a campo (5 qwords de `InterruptStackFrame` vs. cabecera del nodo):

| Campo del marco | Valor | Palabra del BTreeMap |
|---|---|---|
| RIP | `0x2801e9cd000` | `[0]` puntero al nodo raíz |
| CS | `0x3` | `[1]` altura del árbol B |
| RFLAGS | (no impreso) | `[2]` = `0x690` = 1680 = `len()` |
| RSP / SS | `0x1` y basura | resto de la cabecera |

Que **dos campos consecutivos** del marco coincidan exactamente con dos campos
consecutivos del nodo no es casualidad posible: el marco de excepción se está **leyendo
desde la dirección del BTreeMap** en lugar de desde la pila. Los valores que la
investigación trató como "estado corrupto de la CPU" son ficticios — datos de directorio
interpretados como marco. Encaja con el `sf` apuntando dentro de `.text` visto en otra
ronda (ambas asignaciones son deterministas entre arranques).

**Consecuencia:** el lock huérfano de ramfs no es la causa, es consecuencia — PID 1 muere
dentro de la sección crítica de `mkdir` (volcado: `ramfs_entries_lock: acquires=1575
releases=1574 outstanding=1, last_acquirer pid=1 op=mkdir iter=787`, con el pánico en esa
misma iteración) y el siguiente mkdir gira para siempre sobre el lock abandonado.

### Dónde queda el foco

El problema está en **cómo llega el puntero al `InterruptStackFrame` a los handlers de
excepción** — no en ramfs, ni el scheduler, ni el allocator. Pregunta abierta:

> ¿Por qué un manejador de excepción recibe un puntero de marco que no apunta a la pila?

Sospechas apuntadas (no verificadas): algún camino donde el asm de entrada
(`timer_interrupt_entry` / el stub de syscall) calcula el puntero al marco de forma
distinta según venga de ring 3 o ring 0, o escribe/lee desplazado; anidamiento de
interrupciones; algo escribiendo más de la cuenta en la pila de kernel (los bytes de ruta
en `current_tf` sugieren sobreescritura de datos sobre el área del puntero).

---

## Hallazgos secundarios de la sesión (cerrados, commits aparte)

- **El chequeo UAF del slab estaba roto** (`61409f7`): `deallocate` poisona con `0xDD…` y
  acto seguido escribe el puntero `next` encima de los primeros 8 bytes; `allocate` leía
  esos mismos 8 bytes y paniqueaba si contenían `0xAA` — es decir, miraba bytes de un
  puntero, no el poison. Cualquier objeto libre cuyo sucesor tuviera `0xAA` en su dirección
  daba falso positivo (verificado con los bytes impresos: `0x7f028e0daa10` es una dirección
  válida). Bajo `#[cfg(debug_assertions)]` → explicaba la asimetría 0/40 release vs 4/20
  debug y, sobre todo, **invalidaba la evidencia "UAF detected" que sostenía la hipótesis
  del doble reparto**. Fix: saltar `size_of::<FreeObject>()` y comprobar `0xDD` en el resto.
  Efecto neto en arranque: ninguno medible (baseline y fixed idénticos, 18/20 y 18/20) —
  el arreglo se justifica por sí mismo, no por la medición.
- **El buddy queda limpio en aislamiento**: 20 semillas, 484 282 operaciones, cero
  PhantomEvents y cero violaciones de solapamiento/conservación/alineación/rango
  (`a0fb676`). Los `PhantomEvent` que el código de producción parchea silenciosamente no
  los genera el buddy operando bien → los phantoms de producción los provoca algo externo.
- **`reclaim_orphans` no retiraba el registro del inodo** (`20e21e7`): limpiaba el bit del
  bitmap pero dejaba `i_mode`/links/punteros intactos; el Pass 1 de `e2fsck` lee la tabla de
  inodos cruda → imagen "reparada" seguía saliendo exit 4. Fix: zero + `i_dtime` (con
  `now_unix_secs` del adaptador), sweep reordenado inodos-primero (regla "leak, nunca
  dangle"), oráculo `e2fsck -fn` con fixture real de `debugfs unlink`.
- **Extracción `mm`** (`ba094c2`): buddy+slab a crate host-testable; seams `PhysMap`/
  `FrameSource`; eventos en vez de `serial_println_raw!`; sin `alloc` (el slab es el
  global allocator — recursión = crash en boot). Salida serial byte-idéntica tras la
  extracción, hasta las direcciones (`0x2801f880000`).

---

## WIP en el árbol (post-cierre: instrumentación depurada)

> **Estado: caza cerrada, instrumentación depurada** (2026-08-06). La sección de abajo
> era el inventario de la carga de la caza; ahora registra qué sobrevivió y qué se
> retiró. El criterio: se queda lo que es *fix real* o *red de seguridad barata y no
> perturbadora*; se retira todo lo que solo servía para reproducir/observar este bug
> concreto, y en particular todo instrumento que pueda entrar en pánico por una
> condición que es legítima fuera del amplificador.

### Se queda (permanente)

| Archivo | Qué | Por qué |
|---|---|---|
| `kernel/src/process/tss.rs` | `IA32_FMASK = (1<<9) \| (1<<10)` (enmascarar DF además de IF) | **Fix de la causa raíz.** Linux hace lo mismo por la misma razón |
| `kernel/src/process/timer_preempt.rs`, `kernel/src/process/syscall/mod.rs` | `cld` como primera instrucción de cada stub de asm propio | **Fix de la causa raíz** (los shims de rustc ya lo emitían; estos dos no) |
| `kernel/src/process/syscall/process_ctl.rs` | `sys_exit` mantiene IF=0 hasta el iretq (sin `drop(irq)` ni `sti` explícito) | **Fix del segundo bug** (`BAD RESUME FRAME`): un tick liberaba la kstack del moribundo bajo los pies de la CPU |
| `kernel/src/process/scheduler.rs` | `tick(interrupted_rsp)` difiere el free de una kstack encolada si el RSP interrumpido cae dentro | Mitad complementaria del fix anterior; el comentario que decía que era imposible era falso |
| `kernel/src/process/timer_preempt.rs` | `validate_resume_frame` | Red de seguridad O(1): congela el instante exacto de un marco de resume corrupto en vez de dejar que el iretq destruya la evidencia |
| `kernel/src/process/scheduler.rs` + `kernel/src/process/mod.rs` | `tf_note_save`/`tf_note_resume` + `tf_seq`/`tf_awaiting_resume`/`tf_last_resumed_seq` | Detector de rebobinado; tres asignaciones por cambio de contexto, imprime solo si dispara |
| `kernel/src/debug.rs` | `TfRewindDiag`/`TF_REWIND`, `DirLockDiag`/`RAMFS_ENTRIES_LOCK` | Contadores pasivos en `/proc/kdebug` + snapshot de pánico. **Incluye un fix real**: `record_acquire` se registra DESPUÉS de tomar el lock — antes, el waiter que gira machacaba los datos del dueño real |
| `kernel/src/fs/ramfs.rs` | `TrackedEntriesGuard` + `lock_entries(op)` | RAII adquisición/liberación; el `lock()` es el bloqueante normal otra vez |
| `mm/` (feature `slab-debug`, off por defecto) | Redzone + cuarentena + 7 tests de host | Herramienta reutilizable tras una bandera, no instrumentación de un bug |
| `userspace/src/syscall.rs` | Wrapper `symlink()` | Completa la librería de syscalls de userspace (par de `mkdir`) |

### Se retiró

| Archivo | Qué | Por qué |
|---|---|---|
| `userspace/src/bin/shell.rs` | Amplificador `HANG_HUNT_ITERS = 1500` | No debe correr en cada arranque |
| `kernel/src/process/scheduler.rs` | Centinela `BOXHUNT5`, checks `BOGUS`/`BOGUS-RANGE`, detector de integridad del box (`tf_saved_five`/`tf_saved_proc`/`tf_saved_box`/`tf_saved_switches`), `for_each_process` | `BOGUS-RANGE` da falso positivo en el primer tick tras `start_first_process` (pila del bootloader); el resto es pura arqueología del bug |
| `kernel/src/memory/mod.rs`, `kernel/src/init/memory.rs` | `box_watch_*` (page-protect, nunca armado) y `enable_physmap_nx` | Rama cerrada con verificación; el NX-trap ya cumplió |
| `kernel/src/init/devices.rs` | Volcado `[BUG2]`, reconstrucción de marco `[NX-TRAP]`, rama `[BOXWATCH]` del page fault | Dependían del NX activado y de un escaneo frágil de la pila |
| `kernel/src/process/syscall/mod.rs` | Anillo de últimas N syscalls + pánico "huérfano al retorno de syscall" | Solo tenía sentido bajo el amplificador |
| `kernel/src/fs/ramfs.rs`, `kernel/src/debug.rs` | `try_lock`+pánico CONTENDED, checks STILL-HELD/drift, `parse_hang_hunt_iter`, `panic_on_ramfs_lock_held_to_user` | Asumían "solo PID 1 toca el VFS", que es **falso** fuera del amplificador: con busybox real, dos procesos en `/tmp` harían entrar en pánico al kernel |
| `kernel/src/panic.rs`, `kernel/src/fs/ramfs.rs` | `dump_tmp_root_entries_layout_for_panic` + `TMP_ROOT_FOR_PANIC` | Hacía `force_unlock` dentro del handler de pánico y leía la disposición no especificada de `BTreeMap` |
| `kernel/src/process/timer_preempt.rs` | Instrumento de desplazamiento de RSP (`rsp_ok!`) | Hipótesis refutada |
| `kernel/src/hw_tests.rs` | `try_lock_can_fail_when_held` | Calibraba un instrumento que ya no existe (y probaba `spin`, no código nuestro) |

Aparte: los submódulos `mlibc/` y `quakegeneric/` siguen sucios por estado previo, sin
relación con esta caza — verificarlos antes de cualquier commit.

**Verificación tras la limpieza** (2026-08-06): `cargo build` limpio (sin warnings nuevos);
`cd mm && cargo test` 33/33, `--features slab-debug` 40/40; `scripts/run-kernel-tests.sh`
PASS (3/3 — antes 4, menos el test de calibración retirado); `scripts/boot-matrix.sh 6 3`
= **18/18 OK**; `/proc/kdebug` en vivo muestra `ramfs_entries_lock: acquires=208
releases=208 outstanding=0 last_acquirer=pid=2 op=symlink` y `tf_rewind: count=0`.

**Regla de verificación**: `scripts/run-kernel-tests.sh` (3/3, determinista) para validar
cambios; `scripts/boot-matrix.sh` para medir el flake. **Leer el serial.log preservado
antes de creerse cualquier veredicto de fallo del harness.**

---

## Cómo reproducir el bug 2 (receta de la caza)

> Histórico: el bug está arreglado y el amplificador ya **no** está en el árbol (ver la
> sección anterior). Esta receta se conserva porque describe cómo se construye un
> amplificador útil, no porque haya nada que reproducir hoy. Para volver a montarlo:
> `git show` de este WIP, o reescribir el bucle `mkdir`+`symlink` en `shell.rs`.

1. Build debug normal (el bug es debug-only; en release ~0).
2. Arrancar con el amplificador activo (HANG_HUNT en shell.rs, ya en el WIP):
   `scripts/boot-matrix.sh 6 4 --timeout 400` (6 instancias paralelas × 4 arranques, con
   overlays qcow2 por instancia — no comparten disk.img).
3. Fallos esperados: ~10 % (p. ej. 4 PANIC + 1 HANG de 48, iteraciones 787–1487). Los 4
   pánicos son **byte a byte idénticos** entre arranques independientes (determinismo de
   las asignaciones).
4. Firma de éxito del harness corregido: banner `built-in shell (ash)` O el amplificador
   terminando (el harness acepta ambos — antes reportó 20/20 HANG falsos por buscar solo
   el banner del amplificador).
5. Para diagnóstico en vivo: `scripts/qemu-debug.sh start --gdb` + `gdb` (attach a un
   cuelgue: la CPU gira al 100 %, es un estado vivo al que preguntarle dónde está — así se
   resolvió el bug 1).
6. Volcado de pánico con la instrumentación del WIP: `outstanding=1`,
   `last_acquirer pid=1 op=mkdir iter=N` (N = iteración que reportó el harness) y los raw
   words del BTreeMap en el mismo dump que el RIP.

---

## Caveats y lecciones de método (los "caveats" de la sesión)

1. **Las herramientas de medición mienten; leer la evidencia cruda.** Tres instrumentos
   defectuosos en una sesión:
   - El canario de reentrada del slab **causaba** el deadlock que decía medir (~2/24).
   - El harness `boot-matrix` reportó **20/20 HANG falsos** sobre un kernel sano (buscaba
     solo "HANG_HUNT completed" y el autor lo corrió sin amplificador); el serial.log
     terminaba en el prompt de ash. Corregido para aceptar ambos.
   - `record_acquire` registraba la intención antes de tomar el lock; el waiter que gira
     machacaba los datos del dueño real — el dato que se buscaba.
   Método que destapó los tres: mirar el log/bytes crudos en vez del veredicto.
2. **El bug 2 es sensible a perturbaciones que no deberían importar**: un
   `serial_println!` no relacionado lo hizo "desaparecer" una vez (y descarriló la
   investigación); la extracción a `mm` —mecánica, salida serial byte-idéntica— cambió la
   tasa de 0/4 a 18/20. Por eso: **no tocar la ruta de arranque / el ISR del timer mientras
   dure la caza** (un refactor de port I/O ahí contaminaría la medición), y "si al
   instrumentar deja de reproducir" es un dato, no un arreglo.
3. **Por debajo de 20 arranques este bug no dice nada.** El 0/4 de una mañana (conclusión
   "el flake está mucho peor") era ruido frente a 18/20 y 18/20 de dos tandas de 20.
4. **Los agentes (Sonnet) cierran turno sin esperar su propio proceso de fondo** (ocurrió
   4 veces pese a prohibirlo): las mediciones largas las lanza/quita el supervisor; al
   agente solo pedirle código.
5. **Los recuentos de los agentes han estado mal cada vez que se contrastaron** ("32
   passed" cuando la primera corrida dio 28/4): verificar diffs y suites uno mismo; el
   28/4 no se reprodujo en 50+ corridas y el mensaje del commit que culpaba al agente se
   enmendó — lo transitorio estaba en la corrida del supervisor.
6. **No arreglar hasta que la evidencia sea concluyente** (backtrace en vivo + mecanismo
   verificado en código): el bug 2 acumula 7–8 hipótesis equivocadas (pila, Quake, COW,
   buddy, sti de sys_exit, IRETQ, rewind, vtable); un parche a ciegas sería la siguiente.
   Cuando el diagnóstico es sólido pero el arreglo es grande, parar y reportar.
7. **El texto del pánico puede mentir**: RIP/CS/RSP "imposibles" eran datos de un
   directorio leídos como marco. Lo que destapó fue exigir que los dos números (RIP del
   pánico y raw words del BTreeMap) salieran en el **mismo volcado**, no comparar entre
   arranques.
8. **Contraste de variantes**: quitar el fork del repro (`mkdir`/`symlink` sueltos, sin
   fork ni exec) destapó el bug 1 en minutos — "¿y si quito X?" es más barato que
   instrumentar X. La pregunta que desbloqueó todo fue del usuario, no del agente.
9. **Amplificar > contar**: repetir la operación N veces por arranque convierte "2 fallos
   de 20 arranques" en "iteración en la que se colgó" — sensible, rápida y bisecable.
10. **Coste de verificación**: correr los tests 50 veces es aceptable si se hace barato
    (un solo comando grepeando el resumen); el bucle de calibración de un instrumento
    defectuoso fue la parte más cara de la sesión.
11. **Paralelizar QEMU es gratis en tokens** (`boot-matrix.sh`): 160 arranques en el mismo
    tiempo que 20 en serie; overlays qcow2 (kilobytes) en vez de copias de disk.img
    (96 MiB), y la base nunca se corrompe.
12. **`debugfs -f` / herramientas similares salen con exit 0 aunque sus comandos fallen**:
    los oráculos deben validar el resultado (fixture sucia antes de reparar), no el exit.

---

## Historial de commits relacionados

```
7c0754d fix(kernel): the slab-lock canary was causing the deadlock it detects
ab58dba fix(kernel): take the allocator locks with interrupts disabled
4fdac24 feat(debug): gdb stub support in qemu-debug.sh, and parallel-safe state
61409f7 fix(mm): make the slab's use-after-free check actually inspect the poison
a0fb676 test(mm): property tests for the buddy allocator's invariants
ba094c2 refactor(mm): extract the buddy and slab allocators into a host-testable crate
20e21e7 fix(ext2): retire the inode record when reclaim_orphans frees an orphan
```

Herramientas resultantes: `scripts/qemu-debug.sh gdb "info registers" "bt"` (stub sin
congelar por defecto, símbolos del kernel cargados con `virtual_address_offset` del
serial.log), `scripts/boot-matrix.sh N M` (paralelo, overlays, clasificación
OK/HANG/PANIC/DOUBLE_FAULT, logs preservados), `STATE_DIR`/`QEMU_DEBUG_DISK_IMG`
parametrizables para sesiones concurrentes.

---

## Planes conectados (contexto en curso)

La branch `refactor/portio-seam` nace de esta sesión con dos objetivos: (a) documentar este
estado (este archivo) y (b) el plan 0.B de desacople x86 (migrar `block/ata.rs`, `serial.rs`,
ISRs, `pci.rs` a `hal::PortIo` — el último paso de la migración que ya dejó pit/rtc/ac97/
acpi/mouse/keyboard seamed). **Secuencia recomendada: cerrar el bug 2 antes de tocar
`timer_preempt.rs`/`init/devices.rs`** (radio de explosión de la caza — ver caveat 2). El
WIP no solapa ficheros de 0.B; la verificación de 0.B (`run-kernel-tests.sh`) es
determinista y no pisa el flake.
