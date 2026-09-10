# Plan: extraer el núcleo de `process::scheduler` a un crate host-testeable

> **Estado: plan, sin empezar.** Escrito 2026-09-10, tras cerrar la línea de
> observabilidad/concurrencia (`e6624eb`..`13f9cef`). Es el **paso 2** de la
> línea "host-testable extraction"
> (`docs/host-testable-extraction-roadmap.md`): *el scheduler contra un reloj
> falso*. El paso 1 (`vfs` + `ramfs`) está completo
> (`docs/fs/vfs-extraction-plan.md`).

## Por qué

`kernel/src/process/scheduler.rs` son 1235 líneas donde conviven dos cosas de
naturaleza muy distinta:

- **Contabilidad**: 11 colas de listos indexadas por prioridad efectiva, una
  cola de espera, decay al ser expulsado, aging periódico contra la inanición,
  y la aritmética del cuanto. Son estructuras de datos y aritmética de enteros.
- **Máquina de contexto**: `TrapFrame`, `fxsave`/`fxrstor`, `fs_base` por MSR,
  `address_space.activate()` (CR3), `tss::set_kernel_stack`, `iretq`. Es
  hardware x86 puro.

Hoy **la primera solo se puede ejercitar arrancando QEMU**, porque vive en el
mismo fichero que la segunda (CLAUDE.md: `-Z build-std` + doble build del bin
target ⇒ colisión de lang items en `core`; verificado, no asumido).

Los bugs que la parte de contabilidad puede tener son exactamente los que un
test de host caza barato y un arranque de QEMU caza tarde o nunca: un proceso
encolado en `run_queues[i]` con `effective_priority != i` (el clamp está
**copiado en 6 sitios**, nada comprueba que coincidan), un `wake` que saca de
`wait_queue` y no vuelve a encolar, un aging que se salta elementos al
re-encolar mientras itera, un proceso hambriento que nunca sube de prioridad.
Ninguno de esos se ve en un arranque: se ve como "va lento" o como nada.

Y una nota de honestidad sobre el título de la línea: **el "reloj falso" no
necesita un seam.** `tick()` ya es un contador sin parámetros — el núcleo *es*
el reloj, y un test lo conduce llamando `tick()` N veces. Inventar un
`trait Clock` sería andamiaje sin cliente. Lo que se inyecta aquí no es el
tiempo, es **la entidad planificada**.

## Forma final

Mismo split que `ext2`/`vfs`: **núcleo puro + adaptador delgado.** La novedad
respecto a los tres crates anteriores es que aquí el núcleo es **genérico sobre
la entidad planificada** (`E: SchedEntity`) en vez de hablar de tipos planos:
las colas guardan `Box<E>`, así que el kernel sigue guardando `Box<Process>`
de verdad, sin copias ni indirecciones.

```
sched/                       (crate nuevo, no_std + alloc, host-testeable)
  ├─ entity  — trait SchedEntity (pid, prioridades, is_idle, is_ready)
  ├─ core    — struct SchedCore<E>: run_queues + wait_queue + contadores
  │            · quantum_for, allocate_pid, clamp de prioridad
  │            · enqueue (add/requeue/requeue_preempted), wake_matching
  │            · pop_next_ready  (el pick-next compartido por 4 sitios)
  │            · take_first_startable  (el de start_first, DISTINTO a propósito)
  │            · advance_ticks / age_processes / consume_quantum
  │            · iteración: iter_queued / iter_queued_mut / iter_ready_desc
  └─ invariants — check_invariants(): las 4 propiedades, para property tests

kernel/src/process/scheduler.rs   (queda el adaptador)
  ├─ struct Scheduler { core: SchedCore<Process>, running, pending_* }
  ├─ static SCHEDULERS + local_scheduler() + TrackedSchedulerGuard
  ├─ toda la máquina de contexto y los punteros per-CPU
  └─ impl SchedEntity for Process
```

**Ninguna firma pública de `Scheduler` cambia.** Los 20 ficheros que llaman a
`switch_to_next`/`block_current`/`add_process`/`running_mut`/… siguen igual.
La única excepción es el campo `pub wait_queue` — ver decisión 1.

### Restricción dura, ya verificada

`static SCHEDULERS: [Mutex<Scheduler>; MAX_CPUS]` obliga a que
`Scheduler::new()` siga siendo **`const fn`**, y por tanto `SchedCore::new()`
también, siendo genérica. Es la asunción más arriesgada del diseño y está
**comprobada compilando**, no razonada: `[const { VecDeque::new() }; N]` dentro
de una `const fn` genérica sobre `E` compila, y un
`static [spin::Mutex<Sched>; 8]` construido con ella también.

### Decisiones de diseño

**1. `wait_queue` deja de ser un campo público y pasa a dos accesores.**
Es la única ruptura de API, y es deliberada. Hoy `pub wait_queue` lo tocan
cinco ficheros más del kernel (`process/pipe.rs:146,171`,
`process/syscall/ipc.rs:315`, `process/syscall/fs.rs:184`,
`process/syscall/process_ctl.rs:716-758,851`), con
`iter()/iter_mut()/position()/remove()/[idx]` y accesos a `trapframe.rax`,
`stop_status_word()`, `stop_reported`. Tres opciones evaluadas:

  - *Dejarlo `pub` dentro del núcleo* (`core.wait_queue`): diff mínimo, pero
    obliga a que `SchedCore` tenga el campo público y, para que el kernel
    llegue a él, a exponer `core` — lo que **también deja `run_queues`
    alcanzable desde fuera**, que es justo el encapsulamiento que esta
    extracción compra.
  - *Migrar los call sites a métodos del núcleo* (`wake_matching` etc.): son
    accesos genuinamente ad-hoc (escribir `rax` en el proceso encontrado,
    marcar `stop_reported`); envolverlos en métodos del núcleo metería tipos
    del kernel en el núcleo. No.
  - **Elegida** (confirmada por el usuario, 2026-09-10)**:**
    `Scheduler::wait_queue()` / `wait_queue_mut()` en el
    adaptador, devolviendo `&VecDeque<Box<Process>>`. Los ~14 accesos de esos
    4 ficheros ganan un par de paréntesis, el cambio lo verifica el compilador
    entero, y `run_queues` queda **inalcanzable desde fuera del núcleo** — que
    es lo que hace que el invariante "índice de cola == prioridad efectiva" sea
    sostenible en vez de una convención copiada 6 veces.

**2. `running` se queda en el kernel, y eso deja un hueco que hay que nombrar.**
Lo exige el encargo y lo justifica el código: cada sitio que toca `running`
está entrelazado con `activate()`/`tss`/`fpu`/`update_current_fast`. La
consecuencia, que **no** hay que tapar: la propiedad *"cada entidad está en
exactamente un contenedor"* solo la puede comprobar el núcleo sobre
`run_queues` + `wait_queue`. Mientras una entidad está fuera, en el `running`
del kernel, el núcleo no la ve. En los tests de host el arnés mantiene su
propio `Option<Box<E>>` y hace el mismo take/put, así que la propiedad **sí**
se verifica de punta a punta contra el modelo — pero contra el `running` real
del kernel no, y eso se documenta como hueco en el entregable en vez de
venderse como cubierto.

**3. Las cinco rutas de "sacar el siguiente" NO se unifican: se separan en dos,
y la diferencia se documenta.** `switch_to_next`, `block_current`,
`kill_and_switch_tf` y `stop_and_switch_tf` comparten literalmente
`for priority in (0..NUM_PRIORITIES).rev() { pop_front() }` → `pop_next_ready`.
`start_first` es **deliberadamente distinto**: escanea `(1..NUM_PRIORITIES)`
(se salta la prioridad 0), filtra `state == Ready && pid != 0` (se salta el
idle) y usa `remove(i)` en vez de `pop_front` → `take_first_startable`, con un
doc comment que dice por qué. Unificarlas por parecer redundantes hace que el
primer proceso arrancado sea el idle.

**4. El estado de la entidad no entra en el núcleo como enum.** El núcleo no
conoce `ProcessState`. `wake`/`wake_with_retval`/`wake_stopped` comparten hoy
el mismo "sacar de `wait_queue` por predicado, marcar listo, re-encolar con
clamp"; se unifican en

```rust
pub fn wake_matching(
    &mut self,
    pred: impl FnMut(&E) -> bool,
    prepare: impl FnOnce(&mut E),
) -> bool
```

donde el kernel pone el predicado (`state == Blocked`) y la preparación
(`state = Ready`, `rax = …`, `stopped_by_signal = None`). El núcleo aporta lo
único que es suyo: **el clamp y el encolado correcto**. Del trait solo salen
`is_idle()` (el idle nunca envejece ni arranca primero) e `is_ready()` (lo
necesita `take_first_startable`), que nombran los dos conceptos en vez de
hardcodear `pid == 0` dentro del núcleo.

**5. `tick()` se parte en tres, no se mueve entero.** El `tick` actual
intercala, entre el incremento del contador global y la comprobación del epoch
de aging, dos `retain` sobre `pending_stack_frees`/`pending_vma_frees` que
llaman a `try_free_kernel_stack`/`try_free_huge_vma` — física real, no puede
salir. Para preservar el orden **exacto**, el núcleo expone
`advance_ticks() -> bool` (incrementa y dice si toca epoch), `age_processes()`
y `consume_quantum() -> bool`, y el `tick` del kernel los llama intercalando
sus `retain` donde están hoy. Un único `core.tick(callback)` sería más corto y
menos fiel.

## Migración: 6 pasos, verde en cada uno

Nunca un big-bang. Tras **cada** paso: `cd sched && cargo test`, `cargo build`
en la raíz, y `scripts/run-kernel-tests.sh` en verde (línea base medida hoy:
3 casos PASS, exit 0). Línea base del resto de crates al empezar, medida:
`hal` 71, `ext2` 91, `mm` 39, `vfs` 158, `diag` 30.

| # | Mueve | Nota |
|---|-------|------|
| 1 | Andamiaje del crate + `trait SchedEntity` + constantes + `quantum_for` + el clamp | `impl SchedEntity for Process`. Sin estado todavía; el kernel delega estas dos funciones. Prueba que la generecidad y el `const fn` encajan de verdad |
| 2 | `SchedCore<E>` con las colas y contadores + `add_process`/`allocate_pid`/`requeue_*`/`wake_matching` | Aquí ocurre la migración de `wait_queue` a accesores (decisión 1, 4 ficheros). Accesor crudo **temporal** a las run queues para que los 5 escaneos sigan compilando |
| 3 | `pop_next_ready` + `take_first_startable` | Sustituye los 5 escaneos y **borra el accesor temporal** del paso 2. Decisión 3: la diferencia de `start_first` se preserva y se documenta |
| 4 | `advance_ticks`/`age_processes`/`consume_quantum` + `iter_queued`/`iter_queued_mut`/`iter_ready_desc` | Decisión 5. Los iteradores dan servicio a `iter_all`, `find_process_mut`, `queue_signal_to_group` y el bucle de logging de `start_first` |
| 5 | `check_invariants()` + las property tests | El entregable real. Ver abajo |
| 6 | Docs (`lib.rs`, CLAUDE.md, este plan) + verificación en arranque interactivo real | `ps`, un `fork`/`exec`, Ctrl-Z/`fg` en QEMU de verdad |

## Lo que compra: los tests que hoy no existen

Property tests sobre secuencias aleatorias de operaciones (`add`/`preempt`/
`block`/`wake`/`tick`), comprobando `check_invariants()` tras **cada** una:

1. **El índice de cola siempre coincide** con la `effective_priority` clamped
   de la entidad que contiene. Hoy nada lo comprueba y el clamp está copiado
   en 6 sitios.
2. **`effective_priority` ∈ `[MIN_EFFECTIVE_PRIORITY, base_priority]`** tras
   cualquier secuencia de preempt/aging.
3. **Cada entidad está en exactamente un contenedor**, nunca en dos ni en
   ninguna (con el matiz de la decisión 2 sobre `running`).
4. **Ningún proceso Ready se queda sin ejecutar indefinidamente**: que el aging
   *rescata de verdad* a los hambrientos, no solo que el código de aging corre.
5. La aritmética de `quantum_for`/`tick`: que el cuanto se agota exactamente
   cuando debe y que el epoch de aging cae donde dice.

Y una que el código actual hace de forma sutil y que merece test propio:
**`age_processes` re-encola mientras itera** — saca con `remove(i)`, sube la
prioridad, hace `push_back` en la cola nueva y **no incrementa `i`** porque el
siguiente elemento se desplaza a esa posición. Un test debe fijar que ningún
elemento se salta ni se procesa dos veces.

### Familias de test aplicables (y la que no)

De la taxonomía A/B/C de `docs/prompt-scheduler-reloj-falso.md`:

- **Familia A (reentrancy probes)**: **no aplica al núcleo.** `SchedCore` no
  tiene locks ni interior mutability — el `Mutex` se queda en el kernel, igual
  que pasó con `mm::buddy`. Decirlo explícitamente en el entregable en vez de
  fabricar un probe que no modela nada.
- **Familia B (contention probes)**: tampoco, por la misma razón.
- **Familia C (sabotaje)**: **obligatoria, por cada test nuevo.** Es la única
  auditoría real de que las property tests detectan lo que dicen detectar.
  `cargo-mutants` no está instalado y no serviría (su modelo sustituye cuerpos
  de función y no puede expresar *reordenar dos sentencias*, que es la forma de
  casi todos estos invariantes). Sabotaje manual: copiar el crate, romperlo,
  correr, medir el exit del `timeout` — no el de un pipeline.

## Después de los 6 pasos (mismo encargo, líneas aparte)

Dos cosas decididas por el usuario el 2026-09-10, ambas **después** de cerrar
la extracción, cada una en su propio commit, para no mezclar un refactor de
scheduler con cambios en la disciplina de interrupciones:

- **Retirar `SLAB_LOCK_CONTENDED`.** El argumento no es "el bug ya está
  arreglado" (eso chocaría con la convención de
  `feedback_permanent_debug_tooling`: generalizar la instrumentación, no
  borrarla) sino **"este detector no puede disparar"**: su vía documentada es
  imposible desde `ab58dba` (la probe corre *dentro* del `without_interrupts`,
  y su doc comment en `kernel/src/debug.rs:205-222` dice hoy lo contrario, que
  es factualmente falso), y la otra —una asignación reentrante desde la sección
  crítica— es imposible por construcción (`mm` no enlaza `alloc` en el build
  no-test). Medido: 24 arranques con el `without_interrupts` deliberadamente
  retirado, **cero disparos**, con marcador impreso al arrancar que prueba en
  el propio serial.log que corría el binario saboteado. Frente a un ritmo
  histórico de ~2/24, observar 0 esperando 2 es evidencia fuerte, no prueba
  (P(0)≈10%). Su historial neto es peor que neutro: el comentario de
  `allocator/mod.rs:207-232` documenta que la probe, colocada fuera del
  `without_interrupts`, **causaba** el mismo deadlock que detecta.
- **Adoptar `diag::IrqMutex`** (hecho y probado en `13f9cef`, sin adoptar) en
  `BUDDY`/`SLAB_ALLOCATOR`, más el contrato en doc comment para los sitios que
  lockean `BUDDY` fuera de `kernel/src/allocator/mod.rs`.

## Fuera de alcance

- **Señales y POSIX.** `resolve_signals`, `notify_child_death`,
  `notify_child_stopped`, `resolve_wait_status`, `queue_signal_to_group`: es
  lógica de señales, no de planificación. Se quedan enteras.
- **`TrackedSchedulerGuard` y sus aserciones de IF=0.** Siguen sin test de
  ninguna clase, y esta extracción **no** los cubre: viven en el adaptador,
  con el `Mutex` y los `cli`/`sti`. Cerrarlos de verdad pide adoptar
  `diag::IrqMutex` (ya hecho y probado, `13f9cef`) en `SCHEDULERS`, que es un
  cambio de la disciplina de interrupciones del kernel y necesita decisión
  explícita del usuario. Queda nombrado como hueco, no tapado.
- **Multi-CPU.** `SCHEDULERS` es un array de 8 pero solo se usa el índice 0
  (un vCPU; `cpu_id()` lo confirma). El núcleo no sabe de CPUs y no debe.
- **Cualquier cambio de comportamiento.** Esto es un refactor: mismas colas,
  mismo orden de encolado, mismos valores de cuanto, mismo `serial_println!`.
  Si aparece un bug real: parar, reportarlo, commit aparte.

## Riesgos

- **El más grande: mover un invariante sin darse cuenta.** Tres concretos, ya
  identificados y con nombre: la diferencia de `start_first` (decisión 3), el
  `i` que no se incrementa en `age_processes`, y el orden de los `retain`
  dentro de `tick` (decisión 5). Los tres se mueven **con su comentario**, no
  solo con su código.
- **`const fn` genérica.** Verificado que compila; el riesgo residual es que
  algo del núcleo (un `Vec` con capacidad, un `spin::Mutex`) rompa la
  constness más adelante. Mitigación: el `static SCHEDULERS` es el guard y
  falla en compilación, inmediatamente.
- **El paso 2 toca 4 ficheros ajenos al scheduler.** Es el único paso con diff
  ancho. Todo el cambio lo verifica el compilador (un campo que desaparece),
  así que el riesgo es de ruido, no de silencio.
- **Tentación de "arreglar de paso".** No. Refactor primero, bugs después, con
  las property tests ya disponibles para probarlos.
