# Prompt de orquestador — extraer el núcleo del scheduler a un crate host-testable

Eres el orquestador del **paso 2 de la línea "host-testable extraction"**
(`docs/host-testable-extraction-roadmap.md`): *el scheduler contra un reloj
falso*. Repo: `rust_so_kernel` (kernel bare-metal x86_64, `#![no_std]`, en
/home/oriaj/rust/rust_so_kernel).

Tu trabajo NO es escribir el código tú mismo: es diseñar el plan, dividirlo en
pasos verificables, delegar cada paso a un subagente Sonnet con instrucciones
precisas y autocontenidas, y **verificar de forma independiente** lo que Sonnet
reporta antes de aceptar cada paso. Re-corre tú los comandos y lee el diff real.

El usuario ya dio el **go explícito** a esta extracción (decisión tomada en la
sesión del 2026-09-09, tras evaluarla contra las alternativas). No vuelvas a
preguntar si hacerla; pregunta solo si el diseño resulta no encajar.

---

## De dónde vienes

Dos sesiones previas cerraron la línea de observabilidad/concurrencia. Lee sus
commits: los mensajes son largos a propósito y contienen el razonamiento, no
solo el qué.

- Sesión N-1 (`e6624eb`..`3e5b3a6`, 6 commits): contention probe del
  `DirLockObserver` de ramfs, arreglo del flake de rutas temporales de `ext2`,
  **crate nuevo `diag/`** (extracción de `LockDiag`/`DirLockDiag`/
  `TfRewindDiag`/`IfViolationDiag`), `diag::TrackedMutex`, y reentrancy probes
  en `vfs::mount` y `mm`.
- Sesión N (esta línea): ver `## Estado al empezar` abajo.

El prompt que originó la sesión N está en `docs/prompt-scheduler-reloj-falso.md`
y sigue siendo la referencia de tono, taxonomía y disciplina. **Léelo entero
antes de nada**: define las tres familias de test, las trampas operativas y las
restricciones duras que siguen vigentes aquí sin cambios.

---

## Estado al empezar

Sesión N (2026-09-09) cerró 4 commits, `5e32d62`..`13f9cef`:

- `5e32d62` — `memory::cow::set_ref` dejó de llamar "violación" a sus 675
  llamadas legítimas por arranque con IF=1. Contrato relajado + contador
  renombrado a `COW_IF_ENABLED_SET_REF`. Solo docs y nombres.
- `77c85c2` — `diag/target/` al `.gitignore` (estaban 435 artefactos
  trackeados; cualquier `cargo test` en `diag` hacía ilegible `git status`).
- `13f9cef` — **`diag::IrqMutex<T, C: IrqControl>`**: hace estructural la
  disciplina `without_interrupts` de los locks de allocator. `with`/`try_with`
  son la única vía al valor, y llaman `C::without_interrupts` ANTES de lockear.
  7 tests nuevos (familia A/B/C), 23 → 30 en `diag`. **No adoptado todavía por
  el kernel** — ver hueco 1 abajo.

**Conteos verificados al cerrar:** `hal` 71, `ext2` 91, `mm` 39, `vfs` 158,
`diag` **30**. `cargo build` raíz limpio, `scripts/run-kernel-tests.sh` PASS
3/3 exit 0.

### Huecos que quedan abiertos (material de partida, ordenados por valor)

1. **`IrqMutex` está hecho y sin adoptar.** Falta convertir `BUDDY`/
   `SLAB_ALLOCATOR` (`kernel/src/allocator/mod.rs`) a `IrqMutex` y pasar por
   `with`/`try_with` los **tres sitios que hoy lockean `BUDDY` fuera de ese
   fichero y no usan `without_interrupts`**, sustituyéndolo por un contrato en
   doc comment: `memory/page_table_manager.rs:384` (`unmap_page_and_free_2m`),
   `init/memory.rs:31`, y los dos `try_lock` de ISR
   (`memory/address_space.rs:482`, `init/processes.rs:136`). Rastreado: el
   llamante de `unmap_page_and_free_2m` es `sys_munmap` vía
   `with_current_process`, que sí hace `cli`, **así que hoy el contrato se
   cumple** — pero eso es lectura de código, no medición, y nada impide que el
   próximo call site lo rompa. El usuario ya aprobó la adopción.

2. **`SLAB_LOCK_CONTENDED` es un detector que no detecta nada — retíralo o
   arréglalo.** Medido, no razonado:
   - Su doc comment (`kernel/src/debug.rs:205-222`) es hoy **factualmente
     falso**. Dice *"neither `SLAB_ALLOCATOR` nor the global allocator critical
     section ever does `cli`"*, cierto el 2026-07-25 y falso desde `ab58dba`:
     `probe_contention` corre DENTRO del `without_interrupts`.
   - Su segunda vía posible (una asignación reentrante directa desde la sección
     crítica) es imposible por construcción: `mm` no enlaza `alloc` en el build
     no-test, `KernelPhysMap::virt_for` es aritmética pura, `KernelFrameSource`
     va a otro lock (`BUDDY`) y `serial_println_raw!` no asigna.
   - **Medición: 24 arranques con el `without_interrupts` de `GlobalAlloc`
     deliberadamente retirado → el canario no disparó ni una vez.** Los 24
     llevaban un marcador impreso al arrancar que demuestra en el propio
     serial.log que el binario sabotado era el que corría (`marker=1` en los
     24), y los 24 alcanzaron PID 1. Frente a un ritmo histórico de ~2 de cada
     24, esperar 2 y observar 0 es **evidencia fuerte, no prueba** (P(0)≈10% si
     el ritmo siguiera vigente); dilo así, no lo sobrevendas.
   - Añade su historial neto: el propio comentario de `allocator/mod.rs:207-232`
     documenta que `probe_contention`, colocado fuera del `without_interrupts`,
     **causaba** el mismo deadlock que detecta, a ~2 arranques de cada 24. Ha
     provocado más cuelgues de los que ha detectado.
   - Decisión pendiente del usuario: retirarlo al adoptar `IrqMutex`, o
     arreglar su doc comment y quedárselo. Ojo con la memoria
     `feedback_permanent_debug_tooling` (la convención es generalizar la
     instrumentación, no borrarla): el argumento válido aquí **no** es "el bug
     ya está arreglado" sino "este detector no puede disparar".

3. **`EXT2_LOCK` solo está guardado implícitamente y el fallo es un cuelgue.**
   Sin cambios respecto a la sesión anterior; ver
   `docs/prompt-scheduler-reloj-falso.md` §4.

### Trampa operativa nueva, aprendida a golpes en la sesión N

**No sabotees `kernel/` mientras haya un subagente vivo.** Un agente que trabajaba
solo en `diag/` encontró el sabotaje en vuelo en `kernel/src/allocator/mod.rs`,
lo tomó (razonablemente) por residuo de otra sesión, y lo revirtió con
`git checkout --` en mitad de una corrida de 24 arranques. La medición entera
quedó invalidada y hubo que rehacerla. Dos reglas que salen de ahí:
- Serializa: sabotaje de kernel y agentes vivos, nunca a la vez.
- **Todo sabotaje que se mida bajo QEMU lleva un marcador impreso al arrancar.**
  Sin él no puedes distinguir "el detector no disparó" de "corriste el binario
  equivocado", y las dos cosas se ven idénticas en el log.

---

## Las tres familias de test (misma taxonomía, respétala)

Este kernel corre en **un solo vCPU** (verificado: ni `src/main.rs` ni
`scripts/qemu-debug.sh` pasan `-smp`; el selftest de ACPI reporta `found 1`). El
modo de fallo real **no es "dos CPUs compiten"** sino **"el mismo hilo reentra
un lock que ya tiene"**.

- **A. Reentrancy probes** (sin hilos, deterministas). Modelan el fallo REAL. Se
  escriben como test **positivo**: pasan normalmente y **cuelgan** si el código
  se rompe. Ese cuelgue es la señal y va documentado en el test; son la única
  excepción a la regla del watchdog. Plantillas:
  `vfs/src/mount.rs::direct_children_callback_from_lookup_does_not_self_deadlock`
  y `mkdir_reentrant_probe_does_not_self_deadlock` en el mismo fichero.
- **B. Contention probes con `std::thread`**. NO simulan el kernel: hacen
  **observable la contención**, que es la única forma de validar los
  instrumentos. Declara en cada uno que modela contención de host, no el
  single-core.
- **C. Auditoría de instrumentos (sabotaje)**. Por cada test nuevo: rómpelo a
  propósito en una copia y demuestra con salida literal que falla o cuelga.
  **Sin esa prueba el test no se acepta.** Es el entregable central, no un extra.

---

## El encargo: crate `sched`

### La forma que ya funcionó dos veces

`docs/fs/ext2-extraction-plan.md` y `docs/fs/vfs-extraction-plan.md` son las dos
plantillas. **Léelas antes de decidir nada.** La segunda tiene la forma exacta
que funcionó: núcleo puro + adaptador delgado, **el estado global se queda, la
lógica se va**, seis pasos verdes con commit cada uno. La receta destilada está
en `docs/host-testable-extraction-roadmap.md` ("La receta (patrón repetible)").

### Qué se muda y qué se queda

Todo vive hoy en `kernel/src/process/scheduler.rs` (1235 líneas).

**Se muda al crate `sched/`** — la contabilidad de colas, prioridad y tiempo:

- Las 11 `run_queues` (`VecDeque<Box<Process>>`, indexadas por
  `effective_priority`), la `wait_queue`, `remaining_ticks`, `global_ticks`,
  `next_pid`.
- Las constantes `NUM_PRIORITIES`/`BASE_QUANTUM`/`PRIORITY_QUANTUM_BONUS`/
  `AGING_EPOCH`/`MIN_EFFECTIVE_PRIORITY`.
- `quantum_for`, `allocate_pid`, el encolado por `effective_priority` con su
  clamp (`add_process`), `age_processes`, `find_process_mut`, `iter_all`, y la
  mitad pura de `tick` (incremento de `global_ticks`, epoch de aging, decremento
  de `remaining_ticks`, y devolver si el quantum se agotó).
- Los sacar-de-`wait_queue`-por-predicado que hoy están copiados en `wake`,
  `wake_with_retval` y `wake_stopped`.

**Se queda en `kernel/src/process/scheduler.rs`** — todo lo que no puede salir,
y es deliberado:

- `running: Option<Box<Process>>` y los punteros per-CPU `CURRENT_AS_PTR`/
  `CURRENT_PID_FAST`.
- `static SCHEDULERS: [Mutex<Scheduler>; MAX_CPUS]`, `local_scheduler()` y
  `TrackedSchedulerGuard` con sus dos aserciones de IF=0.
- Absolutamente toda la máquina de contexto: `TrapFrame`, `tf_note_save`/
  `tf_note_resume`, `read_fs_base`/`write_fs_base`, `fpu::save`/`restore`,
  `address_space.activate()`, `tss::set_kernel_stack`, `jump_to_trapframe`.
- `pending_stack_frees`/`pending_vma_frees` y sus dos `retain` dentro de `tick`
  (llaman a `try_free_kernel_stack`/`try_free_huge_vma`, que son física real).
- `resolve_signals`, `notify_child_death`, `notify_child_stopped`,
  `resolve_wait_status`, `queue_signal_to_group` — lógica de señales/POSIX, no
  de planificación. **Fuera de alcance de esta extracción.**

### El seam: `trait SchedEntity`

El núcleo debe ser **genérico sobre la entidad planificada**, no conocer
`Process`. Lo mínimo que las funciones de arriba tocan de un `Process` es:
`pid.0` (usize), `priority` (u8, base), `effective_priority` (u8, lectura y
escritura), y `state` (solo para comparar contra `Ready` en `start_first` y
para los predicados de `wake*`). Diséñalo como un trait con esos accesores —
misma forma que `hal::PortIo` / `mm::PhysMap` / `vfs::ramfs::DirLockObserver`.

**El "reloj falso" no necesita un seam propio.** `tick()` ya es un contador sin
parámetros: el núcleo *es* el reloj, y un test lo conduce llamando `tick()` N
veces. No inventes un `trait Clock` si no lo pide el código — sería andamiaje
sin cliente.

### Cinco cosas verificadas que NO debes re-derivar (costaron tiempo)

1. **`wait_queue` es `pub` y lo tocan cinco ficheros más del kernel**:
   `process/pipe.rs:146,171`, `process/syscall/ipc.rs:315`,
   `process/syscall/fs.rs:184`, `process/syscall/process_ctl.rs:716-758,851`.
   Todos hacen `iter()/iter_mut()/position()/remove()/[idx]` y tocan
   `trapframe.rax`, `stop_status_word()`, `stop_reported`. El núcleo tiene que
   exponer `wait_queue` como campo público `VecDeque<Box<E>>` para que esos
   sitios sigan compilando sin cambios, o migrarlos uno a uno — **decídelo
   explícitamente y justifícalo**, no lo dejes pasar.
2. **Cuatro sitios comparten el MISMO pick-next y uno NO.**
   `switch_to_next` (línea ~1004), `block_current` (~688), `kill_and_switch_tf`
   (~551) y `stop_and_switch_tf` (~602) hacen exactamente
   `for priority in (0..NUM_PRIORITIES).rev() { if let Some(p) =
   run_queues[priority].pop_front() { ... } }`. `start_first` (~1051) es
   **deliberadamente distinto**: escanea `(1..NUM_PRIORITIES).rev()` (salta la
   prioridad 0), filtra `state == Ready && pid.0 != 0` (salta el idle) y usa
   `remove(i)` en vez de `pop_front`. **Esa diferencia hay que preservarla y
   documentarla, no unificarla por parecer redundante.** Si la unificas sin
   querer, el primer proceso arrancado pasa a ser el idle.
3. **`age_processes` re-encola mientras itera.** Saca con `remove(i)`, sube
   `effective_priority` hacia `priority`, y hace `push_back` en la cola nueva
   **sin incrementar `i`** porque el siguiente elemento se desplaza a esa
   posición. Salta `pid.0 == 0` (el idle nunca envejece). Es sutil y es
   exactamente lo que una property test debe fijar.
4. **`kernel` no compila para host.** `-Z build-std` + el doble build del bin
   target bajo `--cfg test` produce dos `core` que colisionan
   (`error[E0152]: duplicate lang item`). Verificado, no asumido. Por eso existe
   toda esta línea. Los tests del núcleo van en el crate nuevo, nunca en
   `kernel/`.
5. **`cargo-mutants` NO está instalado y tampoco serviría**: su modelo sustituye
   cuerpos de función y no puede expresar *reordenar dos sentencias*, que es la
   forma de casi todos los invariantes de aquí. El sabotaje manual (copiar el
   crate, romperlo, correr) es la herramienta correcta.

### Qué compra esto (los tests que hoy no existen)

Property tests sobre secuencias de operaciones, comprobando invariantes tras
cada una. Las que el roadmap nombra explícitamente:

- **Ningún proceso Ready se queda sin ejecutar indefinidamente** — que el aging
  rescata de verdad a los hambrientos, no solo que el código de aging corre.
- **`effective_priority` se mantiene en rango** `[MIN_EFFECTIVE_PRIORITY,
  priority]` tras cualquier secuencia de preempt/aging.
- **El índice de cola siempre coincide** con la `effective_priority` de la
  entidad que contiene (hoy nada lo comprueba; el clamp está copiado en 6 sitios).
- **Cada entidad está en exactamente un contenedor** (`running` / una `run_queue`
  / `wait_queue`), nunca en dos ni en ninguno.
- La aritmética de `quantum_for`/`tick`: que el quantum se agota exactamente
  cuando debe y que el epoch de aging cae donde dice.

---

## Cómo quiero que orquestes

1. Antes de delegar nada, lee tú: `docs/prompt-scheduler-reloj-falso.md` entero,
   los dos planes de extracción, `kernel/src/process/scheduler.rs` completo, y
   los cinco ficheros que tocan `wait_queue`. Corre las suites para tener tu
   propia línea base medida (`cd <crate> && cargo test` para cada uno,
   `cargo build` en la raíz, `scripts/run-kernel-tests.sh`).
2. **Escribe primero `docs/sched/sched-extraction-plan.md`**, con la misma
   estructura que `docs/fs/vfs-extraction-plan.md`: por qué, forma final, las
   decisiones de diseño numeradas (incluida la de `wait_queue` del punto 1 de
   arriba), la tabla de N pasos, qué queda fuera de alcance, y los riesgos.
   Enséñaselo al usuario antes de empezar a mover código.
3. Para cada paso, da a un subagente Sonnet un prompt **autocontenido**: qué
   archivos tocar, qué firma exacta, qué comportamiento debe quedar idéntico,
   qué comando exacto correr, y **qué debe reportar como salida literal**.
   Prohíbele resumir. Dile qué rutas de scratchpad usar y que borre las copias
   de sabotaje al terminar.
4. **Por cada test nuevo, exige la prueba de sabotaje**, con el exit code medido
   correctamente (ver trampas en el prompt anterior; `124 = colgado` y es válido
   en familia A).
5. Después de cada informe, **re-corre tú los comandos y lee el diff real**. Al
   menos un sabotaje por paso, rehazlo tú de forma independiente.
6. Si un paso revela que el diseño asumido no encaja, **para y replantea**. Pasó
   en las dos sesiones anteriores y fue el resultado más valioso de ambas: no
   fuerces el plan.
7. Puedes lanzar 2-3 agentes en paralelo si tocan crates distintos. No más:
   pierdes la capacidad de verificar y los `cargo build` se pisan.

---

## Restricciones duras (idénticas a la sesión anterior)

- **Cero cambios de comportamiento en producción** salvo que encuentres un bug
  real; si lo encuentras, **para, repórtalo al usuario** y trátalo como commit
  aparte. Refactor primero, bugs después.
- **Ningún test flaky.** Sincroniza con `Barrier`/canales/`AtomicBool`, nunca con
  `sleep` como mecanismo de sincronización (solo como presupuesto de observación
  acotado, y documentado como tal). Todo test con hilos necesita watchdog
  (`recv_timeout` sobre un canal, nunca `join()` pelado) — excepto familia A.
- **Verde en cada paso**, con commit incremental: `cargo test` del crate tocado,
  `cargo build` en la raíz, y `scripts/run-kernel-tests.sh`. Nunca un commit
  gigante.
- No toques la lógica de locking/interrupciones del kernel sin decisión explícita
  del usuario.
- Los submódulos `mlibc` y `quakegeneric` aparecen modificados desde antes: no
  los toques ni los añadas a ningún commit.
- Formato de commit: cuerpo explicativo, en inglés, que cuente el *porqué* y lo
  que se midió. Al final,
  `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>`.

---

## Entregable final

Un resumen corto con: qué tests se añadieron y de qué familia (A/B/C) es cada
uno, qué invariantes quedan realmente guardados y **cuáles siguen sin estarlo**
(sé explícito con los huecos: es el punto de todo el encargo), los conteos reales
de tests antes y después por crate, y la lista de sabotajes ejecutados con su
resultado. Si algún detector existente resulta no detectar nada, dilo claramente
y propón retirarlo o arreglarlo — un instrumento que miente es peor que no
tenerlo.
