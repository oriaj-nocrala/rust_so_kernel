# Plan: cerrar la extracción del scheduler y arreglar lo que destapó

> **Estado: plan, sin empezar.** Escrito 2026-09-10, justo después de cerrar
> los 5 pasos de refactor de `docs/sched/sched-extraction-plan.md`
> (`4738fa1`..`4db1cd1`). La regla de aquella sesión era "refactor primero,
> bugs después"; esto es el después.

## Línea base medida al empezar (no heredada del doc anterior)

| crate | tests |
|---|---|
| `sched` | 43 |
| `hal` | 71 |
| `ext2` | 91 |
| `mm` | 39 |
| `vfs` | 158 |
| `diag` | 30 |

`cargo build` en la raíz limpio. `scripts/run-kernel-tests.sh` PASS 3/3, exit 0.

Verificado además, a mano, lo que el encargo afirmaba:

- **El bug de inanición es latente de verdad.** Los cuatro `Process::new*`
  (`kernel/src/process/mod.rs:288,361,424,514`) ponen `priority: 5`, y los
  únicos `set_priority` del kernel son `idle_proc.set_priority(0)` y
  `user_proc.set_priority(5)` (`kernel/src/init/processes.rs:169,230`). No
  existe `nice(2)`/`setpriority(2)` en ninguna parte del árbol.
- `age_processes` (`sched/src/core.rs:342-365`) es exactamente lo descrito:
  bucle exterior ascendente, `.min(base_priority())`,
  `queue_index(entity.effective_priority())`, sin incremento tras `remove(i)`.
- `SLAB_LOCK_CONTENDED` sigue vivo en `kernel/src/debug.rs:232,243,244,289,317`
  más el comentario de `kernel/src/allocator/mod.rs:208`.

## Decisión del usuario (2026-09-10)

**Bug 1+2 → MLFQ real: el aging puede elevar la prioridad efectiva POR ENCIMA
de la base.** El decay por preempción la devuelve. Entran también el paso 6
(docs), el bug 4 (retirar `SLAB_LOCK_CONTENDED`), el bug 5 (adoptar
`diag::IrqMutex`) y el bug 6 (los huecos de test).

## Hallazgo que reordena el trabajo

`SchedCore::check_invariants` (`sched/src/core.rs:504-517`) comprueba hoy, como
invariante (2):

```rust
let floor = MIN_EFFECTIVE_PRIORITY.min(base);
if effective < floor || effective > base { ... PriorityOutOfRange }
```

Es decir: **`effective <= base` es hoy un invariante comprobado**. El MLFQ lo
rompe por construcción, y lo rompe en el checker que usan las tres property
tests. Redefinirlo (`effective <= NUM_PRIORITIES - 1`, con la base fuera del
techo) es parte del paso, no un efecto colateral a descubrir a mitad.

Segundo hallazgo del mismo tipo: **el paso 4 no puede ir sin el paso 5.** Con
el bucle de aging arreglado (+1 real por pasada) *y* el techo aún en la base,
el estado intermedio es plausiblemente **peor** que el actual: hoy un proceso
CPU-bound que ha decaído a 1 vuelve a 5 de golpe cada 50 ticks; con solo el
paso 4 tardaría 200 ticks en recuperarse, y el decay (1 por preempción, cada
~7 ticks de quantum) gana la carrera al aging (1 cada 50) con holgura. Se
miden por separado para poder atribuir cada cambio, pero **el árbol no se deja
en ese estado intermedio**: los pasos 4 y 5 son una sola unidad de trabajo con
dos commits.

## Los pasos

Cada uno: `cargo test` del crate tocado, `cargo build` en la raíz,
`scripts/run-kernel-tests.sh`, y **arranque real de QEMU** cuando toque el
scheduler (los 3 casos de integración son ACPI + ext2 y no lo cubren). Commit
incremental, cuerpo en inglés contando el porqué y lo medido.

### Paso 1 — cerrar la extracción (paso 6 del encargo anterior)

Solo docs, cero código. `sched/src/lib.rs` ("What lives here" está escrito como
si fuera el paso 1), la tabla de crates y el texto del scheduler en
`CLAUDE.md`, la fila "Scheduler (reloj falso)" de
`docs/host-testable-extraction-roadmap.md`, y la nota de estado de
`docs/sched/sched-extraction-plan.md` (formato de
`docs/fs/vfs-extraction-plan.md`).

Verificación: arranque interactivo con `ps`, `fork`/`exec`, Ctrl-Z/`fg`, un job
en background. Esta es además la **línea base de comportamiento** contra la que
se comparan los pasos 4 y 5, así que se anota `switches_total` de
`/proc/kdebug` tras una carga CPU-bound fija.

### Paso 2 — retirar `SLAB_LOCK_CONTENDED` (bug 4)

Ya decidido, sin ejecutar. El argumento **no** es "el bug ya está arreglado"
(chocaría con la memoria `feedback_permanent_debug_tooling`) sino **"este
detector no puede disparar"**, con las cuatro patas verificadas por lectura:
`mm/src/lib.rs` no tiene `extern crate alloc`; `KernelPhysMap::virt_for` es
aritmética pura; `KernelFrameSource` reenvía a `phys_alloc`/`phys_free`, que
toman `BUDDY`, **otro** lock; `serial_println_raw!` es un `core::fmt::Write`
sobre un writer de tamaño cero. Además `log_alloc_event` corre **fuera** del
`without_interrupts`, su doc comment (`kernel/src/debug.rs:205-222`) es hoy
factualmente falso, y su historial neto es peor que neutro: el comentario de
`allocator/mod.rs:207-232` documenta que la probe, colocada fuera del
`without_interrupts`, **causaba** el deadlock que dice detectar.

Toca: `kernel/src/debug.rs` (contador, `inc_*`, `*_count`, las dos líneas del
render de `/proc/kdebug` y del snapshot de pánico), el sitio que lo incrementa,
y el comentario de `allocator/mod.rs`. Se documenta la retirada, con el
argumento, donde estaba el doc comment falso.

### Paso 3 — adoptar `diag::IrqMutex` (bug 5)

`BUDDY`/`SLAB_ALLOCATOR` (`kernel/src/allocator/mod.rs`) a `IrqMutex`, y
`with`/`try_with` en los cuatro sitios que lockean `BUDDY` fuera de ese
fichero: `memory/page_table_manager.rs:384` (`unmap_page_and_free_2m`),
`init/memory.rs:31`, y los dos `try_lock` de ISR
(`memory/address_space.rs:482`, `init/processes.rs:136`). Hoy el contrato se
cumple (el llamante de `unmap_page_and_free_2m` es `sys_munmap` vía
`with_current_process`, que sí hace `cli`) — pero eso es lectura de código, no
medición, y es exactamente lo que el tipo convierte en imposible de romper.

### Paso 4 — `age_processes` sube +1 de verdad (bug 2)

El bucle exterior ascendente reprocesa entidades que él mismo acaba de mover
hacia arriba. Arreglo: recorrido descendente, o recolectar-y-reencolar tras el
bucle. **Techo aún en la base** — este paso no cambia la política, solo hace
que el código haga lo que dice.

Medición obligatoria antes/después: la tabla de inanición del encargo, contra
el crate real (no una reimplementación), y `switches_total` bajo la misma carga
CPU-bound del paso 1.

`age_processes_pins_multi_boost_within_single_call` fija hoy el comportamiento
equivocado: se reescribe para fijar el correcto, y se deja escrito en su doc
comment que fijaba lo contrario y por qué.

### Paso 5 — el techo pasa de la base a `NUM_PRIORITIES - 1` (bug 1, MLFQ)

El cambio de política. `age_processes` sube hacia el techo global, no hacia la
base; `requeue_preempted` baja hasta `MIN_EFFECTIVE_PRIORITY`, sin cambios. Sin
estado nuevo por entidad y **sin tocar el trait `SchedEntity`**: "lleva un
epoch sin correr" ya está representado por "está en una run queue" — la entidad
`running` vive en el adaptador del kernel, fuera del core, así que el aging
nunca la ve. `is_idle()` sigue excluyendo al idle.

Arrastra, y por eso va aquí y no en otro sitio:

- **Invariante (2) de `check_invariants` redefinido** (ver arriba). El techo
  pasa a ser `NUM_PRIORITIES - 1`; `base` deja de ser techo.
- **`property_no_ready_entity_starves` empieza a guardar lo que su nombre
  dice**: con el MLFQ, desactivar el aging **sí** debe tumbarla. Ese es el
  sabotaje que la valida, y hoy no la tumba.
- Riesgo a medir, no a razonar: con todo base 5, las entidades pueden subir a
  10, y `quantum_for(10) = 12` ticks frente a `quantum_for(5) = 7`. Slices más
  largos ⇒ menos switches. Se compara `switches_total` con el del paso 1 bajo
  carga idéntica, y se comprueba que el sistema sigue respondiendo
  (Ctrl-Z/`fg`, un job en background, `ps` mientras gira un bucle CPU-bound).

### Paso 6 — los dos trozos inertes (bug 3)

Solo tiene sentido después del 5, porque el 5 es lo que los despierta:

- `.min(base_priority())` desaparece, sustituido por un clamp al techo global.
  El test renombrado a `age_processes_stops_exactly_at_base` cambia de
  significado y hay que rehacerlo, no solo renombrarlo otra vez.
- `queue_index(effective)` vs `queue_index(base)` dejaba de ser distinguible
  porque el bucle convergía a `effective == base`. Con el MLFQ ya no convergen:
  se añade el test que los distingue y **se ejecuta el sabotaje** (sustituir
  uno por otro) para probar que ahora sí lo tumba. Si no lo tumba, eso es el
  hallazgo, y se dice.

### Paso 7 — los huecos de test que quedan (bug 6)

- Seeds de `property_random_operations_preserve_invariants_and_conservation`
  que **sí** alcancen una entidad en `effective == base == MIN` vuelta a
  expulsar. Es un hueco de los seeds, no del checker: verificado en la sesión
  anterior con un repro mínimo que el checker devuelve `PriorityOutOfRange`
  correctamente en ese escenario.
- Escribir, sin adornos, lo que sigue **sin** estar guardado: las aserciones
  IF=0 de `TrackedSchedulerGuard` (viven en el adaptador, con el `Mutex` y los
  `cli`/`sti`; el paso 3 acerca pero no cierra), y `EXT2_LOCK`, guardado solo
  implícitamente y cuyo fallo es un cuelgue.

## Reglas de la sesión (heredadas, sin cambios)

- **Por cada test nuevo, el sabotaje.** Y cuando un sabotaje **no** tumbe nada,
  eso es el hallazgo, no un problema que esconder. Prohibido retocar el
  sabotaje hasta que falle.
- Ningún test flaky: `Barrier`/canales/`AtomicBool`, nunca `sleep` como
  sincronización. Watchdog en todo test con hilos salvo familia A.
- No sabotear `kernel/` con un subagente vivo. Todo sabotaje medido bajo QEMU
  lleva marcador impreso al arrancar.
- `QEMU_DEBUG_STATE_DIR=/tmp/qd-sched` y `QEMU_DEBUG_DISK_IMG` apuntando a una
  **copia** de `disk.img`.
- `$?` tras un pipeline es el exit del ÚLTIMO comando. `timeout 60 cargo test >
  /tmp/out 2>&1; echo "EXIT=$?"`. **124 = colgado.**
- Un `cargo build` de 0.03 s no prueba que compilara nada.
- `mlibc` y `quakegeneric` aparecen modificados desde antes: no tocarlos ni
  añadirlos a ningún commit.
