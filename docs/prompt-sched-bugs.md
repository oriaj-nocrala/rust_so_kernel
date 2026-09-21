# Prompt de orquestador — cerrar la extracción del scheduler y atacar los bugs que destapó

Eres el orquestador de la sesión que **cierra la línea "host-testable
extraction"** (`docs/host-testable-extraction-roadmap.md`) y, sobre todo, la
que por fin **arregla lo que la extracción destapó**. Repo: `rust_so_kernel`
(kernel bare-metal x86_64, `#![no_std]`, en /home/oriaj/rust/rust_so_kernel).

Tu trabajo NO es escribir el código tú mismo: es diseñar el plan, dividirlo en
pasos verificables, delegar cada paso a un subagente Sonnet con instrucciones
precisas y autocontenidas, y **verificar de forma independiente** lo que Sonnet
reporta antes de aceptar cada paso. Re-corre tú los comandos y lee el diff real.

**Léete entero `docs/prompt-scheduler-reloj-falso.md` antes de nada** (tono,
taxonomía A/B/C de tests, trampas operativas, restricciones duras) y
`docs/prompt-sched-crate.md` (el encargo del que sale esta sesión). Siguen
vigentes sin cambios.

---

## De dónde vienes

La sesión anterior (2026-09-10) extrajo el núcleo del scheduler al crate
`sched/`, en 5 commits, `4738fa1`..`4db1cd1`. Lee los mensajes: son largos a
propósito y contienen el razonamiento y **lo que se midió**, no solo el qué.

- `4738fa1` — el plan (`docs/sched/sched-extraction-plan.md`).
- `6dde865` — paso 1: andamiaje, `trait SchedEntity`, constantes,
  `quantum_for`, `queue_index` (el clamp estaba copiado en **7** sitios, no 6).
- `cb32d61` — paso 2: `run_queues`/`wait_queue`/`next_pid` al núcleo;
  `wait_queue` deja de ser campo público (4 ficheros del kernel migrados a
  `Scheduler::wait_queue()`/`wait_queue_mut()`).
- `df1a9f5` — paso 3: los 4 pick-next → `pop_next_ready`, `start_first` →
  `take_first_startable` (distinto a propósito, documentado), `age_processes`
  al núcleo; accesores temporales borrados.
- `6ebe732` — paso 4: `remaining_ticks`/`global_ticks` → `start_slice`/
  `advance_ticks`/`consume_quantum`, preservando el interleaving exacto de
  `tick`.
- `4db1cd1` — paso 5: `check_invariants` + 3 property tests + los sabotajes.

**Conteos verificados al cerrar:** `sched` **43**, `hal` 71, `ext2` 91, `mm`
39, `vfs` 158, `diag` 30. `cargo build` raíz limpio. `scripts/run-kernel-tests.sh`
PASS 3/3, exit 0. Mídelas tú igual al empezar; si te dan otra cosa, sospecha de
tu entorno primero.

Cada paso se verificó además con un **arranque real de QEMU**, no solo con la
suite de integración (esos 3 casos son ACPI + ext2 y **apenas tocan el
scheduler**): `ps`, un pipe, `sleep`, un job en background, y un bucle
CPU-bound que produjo `switches_total: 2149` en `/proc/kdebug` con
`scheduler_lock outstanding=0` y `tf_rewind count=0`.

---

## Lo que queda: 1 paso de cierre + una pila de bugs reales

### Paso 6 (pendiente, pequeño): cerrar la extracción

Docs y verificación, sin refactor: actualizar el doc comment de
`sched/src/lib.rs` ("What lives here" está escrito como si fuera el paso 1),
la tabla de crates y el texto del scheduler en `CLAUDE.md`, la tabla de estado
de `docs/host-testable-extraction-roadmap.md` (la fila "Scheduler (reloj
falso)" pasa a hecho), y la nota de estado de
`docs/sched/sched-extraction-plan.md` (mismo formato que
`docs/fs/vfs-extraction-plan.md`). Más una verificación en arranque
interactivo: `ps`, `fork`/`exec`, Ctrl-Z/`fg`, un job en background.

### Los bugs, ordenados por valor

Todos están **medidos**, no razonados. Ninguno se arregló: la regla de la
sesión anterior era "refactor primero, bugs después", y ahora toca el después.

#### 1. El aging causa la inanición que existe para evitar — LATENTE

El gordo. `Scheduler::age_processes` (hoy `sched::SchedCore::age_processes`)
restaura la prioridad efectiva **hasta la base entera en una sola pasada**, y
`pop_next_ready` es prioridad estricta. Como el aging solo puede devolver una
entidad a *su propia* base, un proceso de base baja nunca puede superar a uno
de base alta perpetuamente re-elevado.

**Medido, simulando contra el crate real** (no una reimplementación), con todas
las entidades continuamente ejecutables:

| bases | aging ON | aging OFF |
|---|---|---|
| `[1,3,6,10]` | pid 1 **nunca elegido** en 200.000 iteraciones | elegido en la iteración 16 |
| `[2,4,6,8,10]` | pids 1 y 2 **nunca elegidos** en 200.000 | elegidos en 12 y 20 |
| `[5,5,5]`, `[5,5,5,5]`, `[5,5,5,5,5,5]` | sin inanición | sin inanición |

**Por qué está latente hoy:** los cuatro `Process::new*` ponen `priority: 5`,
y los únicos `set_priority` del kernel son `idle_proc.set_priority(0)` y
`user_proc.set_priority(5)` (`kernel/src/init/processes.rs:169,230`). O sea:
este kernel solo crea bases 5 y 0. Todos los procesos reales comparten una cola
y el FIFO los rota con justicia. **Se vuelve alcanzable en cuanto algo ponga
prioridades distintas** — un `nice(2)`/`setpriority(2)`, o cualquier
heurística de prioridad por tipo de proceso.

Decisión pendiente del usuario, y es de diseño, no de implementación: ¿boost
por encima de la base para los hambrientos (estilo MLFQ real), un pick-next que
no sea prioridad estricta, o dejarlo documentado como límite conocido y no
añadir nunca prioridades distintas? Pregúntaselo; no lo elijas tú.

#### 2. `age_processes` no hace lo que aparenta

El `+1` del código y el comentario del módulo (`kernel/src/process/scheduler.rs`,
"Every AGING_EPOCH ticks: boost waiting processes' eff_pri **toward** base")
describen un ascenso gradual. El bucle exterior es **ascendente** y el aging
mueve entidades a colas que aún no ha visitado, así que las re-procesa:
verificado ejecutando una réplica verbatim, una entidad con base 8 y efectiva 1
termina **una sola pasada** en efectiva 8. Consecuencia: el decay por
preempción se deshace por completo cada 50 ticks. Está documentado y fijado con
un test (`age_processes_pins_multi_boost_within_single_call`) pero **no
decidido**: es la causa raíz del bug 1.

#### 3. Dos trozos de código inerte dentro de ese mismo bucle

Ambos son síntomas del bug 2 — dentro de un bucle que converge, los detalles
por paso no sobreviven a la pasada:

- **`.min(base_priority())` es inalcanzable.** Su rama solo entra cuando
  `eff < base` e incrementa exactamente 1, así que `eff+1 <= base` siempre.
  Medido: borrarlo deja la suite entera en verde. El test que decía guardarlo
  se renombró a `age_processes_stops_exactly_at_base` y ahora dice que nadie lo
  guarda y nadie puede.
- **`queue_index(effective)` vs `queue_index(base)` es indistinguible.**
  Sustituir uno por otro no lo detecta **ningún** test, y no es un hueco de
  cobertura cerrable: brute force de 4000 poblaciones aleatorias de 2-6
  entidades, comparando el estado final completo **incluido el orden dentro de
  cada cola**, idéntico en todos los casos.

Si se arregla el bug 2, los dos dejan de ser inertes. Ese es el orden correcto.

#### 4. `SLAB_LOCK_CONTENDED` — el usuario ya decidió retirarlo, y sigue ahí

Decisión tomada el 2026-09-10, **sin ejecutar**. El argumento no es "el bug ya
está arreglado" (chocaría con la memoria `feedback_permanent_debug_tooling`)
sino **"este detector no puede disparar"**. Las cuatro patas están verificadas
por lectura en la sesión anterior, no de oídas:

1. `mm/src/lib.rs` **no tiene `extern crate alloc`** — en el build no-test no
   puede asignar.
2. `KernelPhysMap::virt_for` (`kernel/src/allocator/mod.rs:45-49`) es
   aritmética de direcciones pura.
3. `KernelFrameSource` reenvía a `phys_alloc`/`phys_free`, que toman `BUDDY`
   — **otro** lock, no el mismo.
4. `serial_println_raw!` es un `core::fmt::Write` sobre un `RawSerialWriter`
   de tamaño cero: no asigna.

Y `log_alloc_event` corre **fuera** del bloque `without_interrupts`. Añade: su
doc comment (`kernel/src/debug.rs:205-222`) es hoy factualmente falso, y su
historial neto es peor que neutro (el comentario de `allocator/mod.rs:207-232`
documenta que la probe, colocada fuera del `without_interrupts`, **causaba** el
deadlock que detecta, a ~2 arranques de cada 24). Medición previa: 24 arranques
con el `without_interrupts` deliberadamente retirado, cero disparos, con
marcador impreso al arrancar.

#### 5. `diag::IrqMutex` sigue hecho y sin adoptar

Aprobado por el usuario hace dos sesiones, pospuesto "después del sched". Falta
convertir `BUDDY`/`SLAB_ALLOCATOR` (`kernel/src/allocator/mod.rs`) a
`IrqMutex` y pasar por `with`/`try_with` los sitios que lockean `BUDDY` fuera
de ese fichero: `memory/page_table_manager.rs:384` (`unmap_page_and_free_2m`),
`init/memory.rs:31`, y los dos `try_lock` de ISR
(`memory/address_space.rs:482`, `init/processes.rs:136`). Hoy el contrato se
cumple (el llamante de `unmap_page_and_free_2m` es `sys_munmap` vía
`with_current_process`, que sí hace `cli`) — pero eso es lectura de código, no
medición.

#### 6. Huecos de test que quedan abiertos, dichos sin adornos

- **`property_no_ready_entity_starves` (`sched/src/invariants.rs`) no guarda lo
  que su nombre dice.** Desactivar el aging no lo tumba — y no por un bound
  generoso, sino porque desactivar el aging **no produce inanición** (bug 1).
  Renómbralo o rehazlo; hoy es la cuarta instancia del mismo patrón.
- **Los seeds fijos de `property_random_operations_preserve_invariants_and_conservation`
  nunca alcanzan** una entidad en `effective == base == MIN` vuelta a expulsar,
  así que no cazan un decay por debajo del suelo. Sí lo caza
  `requeue_preempted_does_not_decay_below_floor`. Es un hueco de los seeds, no
  del checker (verificado con un repro mínimo: el checker devuelve
  `PriorityOutOfRange` correctamente en ese escenario).
- **`TrackedSchedulerGuard` y sus aserciones de IF=0 siguen sin test de ninguna
  clase.** La extracción no las cubre: viven en el adaptador, con el `Mutex` y
  los `cli`/`sti`. Cerrarlas de verdad pide adoptar `IrqMutex` (bug 5).
- **`EXT2_LOCK` sigue guardado solo implícitamente**, y el fallo es un cuelgue.
  Sin cambios; ver `docs/prompt-scheduler-reloj-falso.md` §4.

---

## El patrón que funcionó, y que quiero que repitas

La sesión anterior encontró **cuatro tests que mentían sobre lo que cubrían**, y
los cuatro salieron del mismo sitio: de la familia C, no de leer el código.

1. Un test del encolado cuya entidad tenía `base == eff`, así que intercambiar
   una por otra era invisible. Arreglado (base 9 / efectiva 5); el sabotaje pasó
   de tumbar 1 test a tumbar 2.
2. `caps_at_base_does_not_overshoot`, que no guardaba el `.min()` que decía
   guardar, porque ese `.min()` es inalcanzable.
3. Tests que comprobaban el índice **devuelto** por un método en vez de dónde
   había puesto la entidad de verdad. Un sabotaje nuevo ("devuelve el índice
   correcto, encola en la cola 0") pasaba las aserciones viejas y falla las
   nuevas. La clave: `mod tests` es un módulo **hijo** de `core`, así que puede
   leer el campo privado `run_queues` directamente — no hacen falta accesores.
4. `property_no_ready_entity_starves` (arriba).

**Regla destilada: por cada test nuevo, exige el sabotaje, y cuando un sabotaje
NO tumbe nada, eso es el hallazgo, no un problema que esconder.** Prohíbele
explícitamente al agente retocar el sabotaje hasta que falle.

Y la otra, que también se pagó sola: **no te creas una afirmación fuerte de un
agente sin reproducirla.** De las dos grandes de la última sesión, una la
confirmé y resultó peor de lo reportado (la inanición), y en la otra **mi
hipótesis era la equivocada** (creía que el orden dentro de la cola
distinguiría el sabotaje D; 4000 casos dijeron que no).

---

## Restricciones duras (idénticas)

- **Cero cambios de comportamiento en producción** salvo que el usuario decida
  explícitamente arreglar uno de los bugs de arriba; cada arreglo, commit
  aparte, con su medición antes/después.
- **Ningún test flaky.** `Barrier`/canales/`AtomicBool`, nunca `sleep` como
  mecanismo de sincronización. Watchdog en todo test con hilos, salvo familia A.
- **Verde en cada paso**, con commit incremental: `cargo test` del crate tocado,
  `cargo build` en la raíz, `scripts/run-kernel-tests.sh`, y **un arranque real**
  cuando el paso toque el scheduler (los 3 casos de integración no lo cubren).
- **No sabotees `kernel/` mientras haya un subagente vivo**, y todo sabotaje
  medido bajo QEMU lleva un marcador impreso al arrancar.
- `scripts/qemu-debug.sh` revienta con paths largos: usa
  `QEMU_DEBUG_STATE_DIR=/tmp/qd-algo` y `QEMU_DEBUG_DISK_IMG=/tmp/algo.img`
  apuntando a una **copia** de `disk.img`.
- **`$?` tras un pipeline es el exit del ÚLTIMO comando.** Mide con
  `timeout 60 cargo test > /tmp/out 2>&1; echo "EXIT=$?"`. **124 = colgado.**
- **Un `cargo build` de 0.03s no prueba que compilara tu código.** Métele un
  error de sintaxis a propósito y comprueba que falla (se hizo, funciona).
- Los submódulos `mlibc` y `quakegeneric` aparecen modificados desde antes: no
  los toques ni los añadas a ningún commit.
- Formato de commit: cuerpo explicativo, en inglés, que cuente el *porqué* y lo
  que se midió. Al final,
  `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>`.

---

## Cómo empezar

1. Mide tú la línea base (los 6 crates, `cargo build`, `run-kernel-tests.sh`).
2. Lee los 5 commits de la extracción, `sched/src/core.rs` entero (los doc
   comments de `age_processes` y `check_invariants` llevan los hallazgos
   escritos), y `sched/src/invariants.rs`.
3. **Pregunta al usuario qué bugs entran en esta sesión y en qué orden.** El 1
   es una decisión de diseño suya, no tuya. El 4 y el 5 ya están aprobados y
   solo hay que ejecutarlos. El paso 6 es barato y cierra la línea.
4. A partir de ahí, el patrón de siempre: plan escrito primero, un subagente
   por paso con prompt autocontenido, y verificación independiente tuya de todo
   lo que reporte.

## Entregable final

Un resumen corto con: qué se arregló y qué se midió antes/después, qué
invariantes quedan realmente guardados y **cuáles siguen sin estarlo** (sé
explícito con los huecos: es el punto de todo el encargo), los conteos reales
de tests por crate antes y después, y la lista de sabotajes ejecutados con su
resultado. Si algún detector o test resulta no detectar nada, dilo claramente y
propón retirarlo o arreglarlo — un instrumento que miente es peor que no
tenerlo.
