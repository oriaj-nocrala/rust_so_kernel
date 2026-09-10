# Plan: cerrar la extracción del scheduler y arreglar lo que destapó

> **Estado: EJECUTADO** (2026-09-10), en 8 commits, `a7fdbb5`..`477c8f3`. Con
> una desviación grande respecto al plan original: **el MLFQ se descartó a
> mitad, ya implementado**, al medirlo mejor y ver que era indistinguible de
> no tener aging (ver "Revisión de esa decisión" más abajo). En su lugar se
> construyó el reloj inyectable que esta línea de trabajo prometía en su
> propio nombre y nunca había construido.
>
> | commit | qué |
> |---|---|
> | `a7fdbb5` | este plan |
> | `f82826c` | paso 1: cerrar la extracción (docs) + el hallazgo del build que miente |
> | `e631131` | paso 2: retirar `SLAB_LOCK_CONTENDED` |
> | `be42427` | paso 3: adoptar `diag::IrqMutex` (primera adopción real del tipo) |
> | `52e7d00` | paso 4: `age_processes` sube +1 de verdad |
> | `33d528b` | `sched::Clock`/`FakeClock` — el reloj falso, en lugar del MLFQ |
> | `64245d8` | tests de reparto de CPU (`fairness.rs`) + qué guardan los tests de verdad |
> | `477c8f3` | paso 7: el hueco de seeds y un warning que `cargo test` no puede ver |
>
> **Conteos finales:** `sched` 43 → **52**, `hal` 71, `ext2` 91, `mm` 39,
> `vfs` 158, `diag` 30 (los cinco últimos sin cambio, verificados al cerrar).
> `cd kernel && cargo build --target x86_64-unknown-none` exit 0 y 0 errores;
> `scripts/run-kernel-tests.sh` PASS 3/3; `scripts/boot-matrix.sh 4 5` → 20/20
> OK, 0 hang / 0 panic / 0 double fault. `cargo build` en `sched/` sin ningún
> warning.
>
> **Lo que NO se hizo, explícitamente:** el MLFQ (descartado con datos), y el
> bug 1 tal como estaba planteado (resultó estar ya cerrado por el paso 4).
> `TrackedSchedulerGuard` y sus aserciones de IF=0 **siguen sin ningún test**;
> `EXT2_LOCK` sigue guardado solo implícitamente; y si el aging dejara de
> correr en silencio, **ninguna property test de comportamiento lo notaría**
> (solo los tests unitarios de `advance_ticks`).

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

**Bug 1+2 → MLFQ real**, más el paso 6 (docs), el bug 4 (retirar
`SLAB_LOCK_CONTENDED`), el bug 5 (adoptar `diag::IrqMutex`) y el bug 6 (los
huecos de test).

## Revisión de esa decisión, a mitad de sesión (2026-09-10, tras `52e7d00`)

**El MLFQ se descartó, ya empezado, y el trabajo se tiró.** No por cambiar de
opinión: porque medirlo mejor lo desmontó. Queda escrito aquí entero porque el
camino a la conclusión vale más que la conclusión.

El paso 4 (arreglar el bucle de aging) **por sí solo eliminó la inanición
perpetua**, que era la razón de ser del MLFQ:

| bases | antes del paso 4 | después |
|---|---|---|
| `[1,3,6,10]` | pid1 **NUNCA** en 200.000 | pid1@19 |
| `[2,4,6,8,10]` | pid1 y pid2 **NUNCA** | pid1@26, pid2@15 |

Con la inanición ya cerrada, la pregunta pasó a ser qué aportaba el MLFQ, y
para responderla hubo que admitir que **el instrumento era malo**. El harness
de inanición (heredado de la sesión anterior, y reproducido tal cual al
principio de esta) mide un único escenario degenerado: N entidades siempre
ejecutables que nunca bloquean, y una métrica *transitoria* y binaria — "¿en
qué iteración fue elegida cada una la primera vez?". Cuatro agujeros
concretos:

- **`park`/`wake_matching` no se ejercitan jamás.** Nada bloquea. Pero el
  camino que más pesa en la interactividad real es el despertar.
- **No mide régimen permanente.** `@19` frente a `@16` no dice nada sobre qué
  fracción de CPU acaba recibiendo cada entidad, que es lo que importa.
- **No hay oráculo.** Los tests fijan lo que el código hace, no lo que debería
  hacer. Por eso "mentían": todos se derivaron del comportamiento observado.
- **No hay reloj falso.** El de esta misma línea de trabajo. Solo se puede
  medir en iteraciones, no en tiempo, así que no hay latencias.

Con un instrumento nuevo (reparto de CPU en régimen permanente + latencia de
despertar, sobre un mix con tareas que **sí** bloquean, 2.000.000 de ticks,
200.000 descartados de warm-up), el resultado fue:

| escenario | aging ON | aging OFF | MLFQ |
|---|---|---|---|
| 4 CPU-bound, base 5 | `25/25/25/25` | `25/25/25/25` | `25/25/25/25` |
| 4 CPU-bound, bases `[1,3,6,10]` | `24/24/26/26` | `25/25/25/25` | `25/25/25/25` |
| 3 CPU-bound + 1 interactiva | idéntico | idéntico | idéntico |
| hogs base 8 + interactiva base 3 | idéntico | idéntico | idéntico |

**En régimen permanente la prioridad base no hace nada.** El decay arrastra a
todo el mundo al suelo (`MIN_EFFECTIVE_PRIORITY`) y allí rotan FIFO por igual;
las 11 colas son decorativas pasado el transitorio. Y **MLFQ ≡ aging OFF** en
los cuatro escenarios. El aging actual es el único de los tres que se desvía,
y lo hace en la dirección equivocada (`24/24/26/26`: favorece a los de base
alta).

Sobre escalar/SMP, que fue la pregunta que disparó la revisión: el MLFQ es
política *intra*-CPU y no aporta nada ahí. El estado real es que
`cpu::cpu_id()` devuelve la constante `0` (`kernel/src/cpu/mod.rs:13`), no hay
arranque de APs (ni un `INIT_IPI`/`SIPI` en el árbol), y no hay balanceo ni
afinidad; `SCHEDULERS[MAX_CPUS]` es andamiaje con un solo elemento vivo.
Además el aging tal como está es **hostil** a SMP: recorre las 11 colas
enteras cada `AGING_EPOCH` ticks con el lock del scheduler tomado, O(procesos)
bajo lock. Y el obstáculo serio para SMP no son las atómicas sino que
`SchedCore` usa `VecDeque<Box<E>>`, que **asigna** — asignar bajo el lock del
scheduler es exactamente el deadlock que costó meses (`slab_lock_self_deadlock`).

**Rumbo elegido en su lugar:** construir primero el reloj inyectable que esta
línea prometió y no entregó, y promover el harness de equidad a tests reales
del crate. Es el prerequisito para evaluar cualquier política —incluida la
actual— y deja el terreno listo tanto para un modelo de tiempo virtual
(vruntime, que haría que las prioridades signifiquen algo en régimen
permanente y es *menos* código que lo que hay) como para SMP de verdad.

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

### Paso 5 — DESCARTADO (era: el techo pasa de la base a `NUM_PRIORITIES - 1`)

> Sustituido por **el reloj inyectable** — ver "Revisión de esa decisión" arriba.
> Lo que sigue es el encargo original, conservado porque su punto crítico (el
> invariante que el MLFQ rompe) sigue siendo cierto y volverá a aparecer si
> alguien retoma esta idea.

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

### Paso 6 — los dos trozos inertes (bug 3) — PARCIALMENTE HECHO EN EL PASO 4

Corrección medida: **el paso 4 despertó uno de los dos, sin necesidad del 5.**
Sustituir `queue_index(effective)` por `queue_index(base)` en el `push_back`
de `age_processes` (el sabotaje S4) pasó de no tumbar nada a **tumbar 4
tests**, uno de ellos el property test preexistente
`property_random_operations_preserve_invariants_and_conservation`, vía
`MisplacedEntity`. Era indistinguible **porque** el bucle ascendente convergía
a `effective == base` antes de acabar la pasada, haciendo ambas expresiones el
mismo valor en el momento de encolar; con el ascenso gradual difieren mientras
la entidad sube. Así que el encargo se equivocaba al llamarlo "hueco no
cerrable": estaba tapado por el bug 2, y arreglar el bug 2 lo cerró solo.

**Sigue inerte** `.min(base_priority())` (sabotaje S3: quitarlo deja los 45
tests en verde). Y seguirá inerte mientras el techo del aging sea la base: la
rama solo entra cuando `eff < base` y suma exactamente 1, así que
`eff + 1 <= base` siempre. No es un hueco de cobertura, es código
inalcanzable — o se borra, o se cambia el techo (que era lo que hacía el
MLFQ descartado).

Lo que sigue es el encargo original:

- `.min(base_priority())` desaparece, sustituido por un clamp al techo global.
  El test renombrado a `age_processes_stops_exactly_at_base` cambia de
  significado y hay que rehacerlo, no solo renombrarlo otra vez.
- `queue_index(effective)` vs `queue_index(base)` dejaba de ser distinguible
  porque el bucle convergía a `effective == base`. Con el MLFQ ya no convergen:
  se añade el test que los distingue y **se ejecuta el sabotaje** (sustituir
  uno por otro) para probar que ahora sí lo tumba. Si no lo tumba, eso es el
  hallazgo, y se dice.

### Paso 7 — los huecos de test que quedan (bug 6)

**Corrección medida sobre `property_no_ready_entity_starves`.** El encargo
decía que con MLFQ desactivar el aging la tumbaría. Es falso, y no por el
MLFQ: **desactivar el aging no produce inanición en ningún modo** (la entidad
de base más baja se elige en la iteración 16 de las 200 que el test permite,
con aging ON, OFF o MLFQ). Ese sabotaje no puede validar ese test nunca.

La razón, y es el hallazgo que hay que fijar: **lo que evita la inanición no
es el aging, es el decay de `requeue_preempted`** — la entidad de base alta
que monopoliza la CPU decae hasta el suelo y deja de ganar. El sabotaje que
debería validar ese test es por tanto **quitar el decay**, no quitar el aging.
Sin ejecutar todavía; es el primer trabajo de este paso.

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

---

## Los sabotajes ejecutados, y qué cazó cada uno

Todos reproducidos por el orquestador, no aceptados del reporte del agente que
los implementó. "0" no es un fallo del sabotaje: es el hallazgo.

| # | Sabotaje | Tests caídos |
|---|---|---|
| S1 | bucle de aging ascendente otra vez | 4 |
| S2 | `+1` → `+2` en el aging | 4 |
| S3 | quitar `.min(base_priority())` | **0** — código inalcanzable, ver abajo |
| S4 | `queue_index(base)` en el `push_back` del aging | 4, incl. un property test preexistente |
| S7 | quitar el decay de `requeue_preempted` | 5, incl. `property_no_ready_entity_starves` |
| S8 | `advance_ticks` siempre `false` (aging nunca corre) | 4 — **todos unitarios de `advance_ticks`** |
| S9 | `advance_ticks` siempre `true` | 5 |
| S10 | `>=` → `>` en el cruce de epoch | 4 |
| S11 | no actualizar `last_epoch_tick` al disparar | 4 |
| S12 | `park` descarta la entidad | 8, incl. la conservación de B1 |
| S13 | `wake_matching` devuelve `true` sin mover nada | 3 — **la conservación de B1 NO lo caza** |
| S14 | quitar el suelo de `requeue_preempted` | 1, y **solo gracias al seed 24** |

## Qué queda guardado y qué no

**Guardado de verdad:**

- Índice de cola == `queue_index(prioridad efectiva)` de cada entidad
  (`MisplacedEntity`), prioridad dentro de rango (`PriorityOutOfRange`), y
  ningún pid duplicado (`DuplicatePid`). Los tres, por `check_invariants` y las
  property tests.
- El aging sube exactamente un paso por llamada, y converge (S1, S2, S4).
- La mecánica del cruce de epoch (S8-S11).
- El decay, y que es él —no el aging— quien evita la inanición (S7).
- El suelo del decay, ahora también desde la suite aleatoria (S14 + seed 24).
- El reparto de CPU en régimen permanente, incl. el camino `park`/
  `wake_matching`, que hasta ahora no ejercitaba ningún test (`fairness.rs`).

**NO guardado, y hay que decirlo:**

- **Que el aging corra.** S8 (aging desactivado del todo) solo lo cazan los
  tests unitarios de `advance_ticks`. Ninguna propiedad de comportamiento del
  scheduler lo nota, porque —medido— desactivar el aging no cambia el reparto
  de CPU ni produce inanición. El hueco es real, pero cerrarlo exigiría que el
  aging hiciera algo observable, y hoy no lo hace.
- **`TrackedSchedulerGuard` y sus aserciones de IF=0.** Sin test de ninguna
  clase, igual que antes de esta sesión. Vive en el adaptador, con el `Mutex` y
  los `cli`/`sti`. `diag::IrqMutex` (paso 3) cerró el problema equivalente para
  `BUDDY`/`SLAB_ALLOCATOR` de forma estructural en vez de con un test; aplicar
  la misma idea aquí es el camino natural.
- **`EXT2_LOCK`**, guardado solo implícitamente, y cuyo fallo es un cuelgue.
  Sin cambios.
- **Que `wake_matching` encole donde debe.** S13 no lo caza la conservación de
  B1: la entidad sigue contada, solo que en la cola equivocada. Lo cazan tests
  concretos, no la propiedad general.

## Instrumentos retirados o corregidos

- **`SLAB_LOCK_CONTENDED`: retirado** (`e631131`). No porque el bug esté
  arreglado sino porque **no puede disparar**, con las cuatro patas verificadas
  por lectura. Su doc comment era además factualmente falso desde `ab58dba`.
- **`switches_total` bajo una carga fija: no vale como evidencia de cambios
  pequeños.** Cuatro corridas de la carga idéntica en el mismo arranque dieron
  2141, 2969, 2063, 2068 — más varianza que cualquier diferencia entre
  versiones. Se usó (mal) en `52e7d00` para afirmar un "+5%"; corregido en
  `33d528b`.
- **`.min(base_priority())` en `age_processes`: código inalcanzable.** No es un
  hueco de cobertura. Se documenta como tal en `sched/src/lib.rs`; borrarlo es
  un cambio neutro que nadie ha hecho aún.
- **`property_no_ready_entity_starves`: NO retirarlo.** El encargo pedía
  renombrarlo o rehacerlo por "no guardar lo que dice". Medido: sí guarda algo
  real. Lo que estaba mal era el sabotaje con que se validó (quitar el aging);
  el correcto es quitar el decay, y con ese cae.

---

## Siguiente paso propuesto: vruntime — alcance medido (2026-09-10)

Decidido no empezarlo en esta sesión. Estos números se midieron al evaluarlo,
para que la próxima no tenga que re-derivarlos.

**Por qué vruntime y no más heurísticas de aging:** hoy la prioridad base **no
determina el reparto de CPU en régimen permanente** (ver la tabla de arriba y
`sched/src/fairness.rs`). Un modelo de tiempo virtual es lo que hace que sí lo
determine, y es *menos* código que las 11 colas + decay + aging que hay ahora.
El instrumento para evaluarlo ya existe: `fairness.rs` + `sched::FakeClock`.

**Superficie a tocar, contada:**

- **18 métodos** distintos de `SchedCore` usados por el adaptador, en **31 call
  sites** (`grep -oE "\.core\.[a-z_]+\(" kernel/src/process/scheduler.rs`).
- **25 usos** de `priority`/`effective_priority` en `kernel/src/`, más el campo
  en `Process` y el trait `SchedEntity`.
- `kernel/src/process/scheduler.rs` son 1232 líneas; `sched/src/core.rs`, 1318.

**Cuatro cosas que NO son opcionales** si se quiere que quede mejor que lo
actual, y no peor:

1. **`min_vruntime` + `place_entity`.** Sin esto, un proceso que ha dormido
   mucho despierta con un vruntime bajísimo y monopoliza la CPU. Es el punto
   donde un vruntime a medias es peor que el decay+aging de hoy.
2. **El idle necesita tratamiento aparte.** En CFS es una clase de scheduling
   distinta, no una entidad con peso muy bajo. Aquí hoy se distingue por
   `is_idle()` (`pid == 0`) en `age_processes`/`requeue_preempted`/
   `take_first_startable`.
3. **`take_first_startable`** tiene semántica propia de arranque (salta la cola
   0, salta el idle, puede sacar del medio de una cola) y está documentado por
   qué unificarlo con `pop_next_ready` arrancaría el sistema en el idle.
4. **El slice deja de ser `quantum_for`** (`BASE_QUANTUM + eff_pri * BONUS`) y
   pasa a ser latencia objetivo / nº de ejecutables, ponderado.

**Lo que vruntime NO arregla:** la estructura ordenada sigue asignando, así que
el obstáculo real para SMP —asignar bajo el lock del scheduler— sigue ahí. Eso
es trabajo aparte, igual que `cpu_id()` real, el arranque de APs y el balanceo.
