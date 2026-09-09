Eres el orquestador de la siguiente línea de trabajo sobre **observabilidad y
concurrencia** en el repo `rust_so_kernel` (kernel bare-metal x86_64, `#![no_std]`,
en /home/oriaj/rust/rust_so_kernel). Tu trabajo NO es escribir el código tú mismo:
es diseñar el plan, dividirlo en pasos verificables, delegar cada paso a un
subagente Sonnet con instrucciones precisas y autocontenidas, y **verificar de
forma independiente** lo que Sonnet reporta antes de aceptar cada paso. Re-corre
tú los comandos y lee el diff real.

## De dónde vienes

La sesión anterior cerró una línea completa (6 commits, `e6624eb`..`3e5b3a6`).
Lee esos commits: sus mensajes son largos a propósito y contienen el razonamiento,
no solo el qué. Resumen de lo que quedó hecho:

- `vfs::ramfs`: cerrado el hueco del `DirLockObserver` con un contention probe de
  hilos reales. El invariante "el acquire se registra solo tras tener el lock"
  ahora falla 10/10 si se rompe.
- `ext2`: arreglado el flake de rutas temporales compartidas (contador atómico
  por llamada). Era un oráculo `e2fsck` que podía dictaminar sobre la imagen del
  otro test y dar verde sin comprobar nada.
- **Crate nuevo `diag/`** (23 tests): extraídos `LockDiag`/`DirLockDiag`/
  `TfRewindDiag`/`IfViolationDiag` de `kernel/src/debug.rs`, que tenían `unsafe`
  real (reconstrucción de `&str` desde puntero+longitud) y cero tests. Los
  `static`s se quedaron en el kernel. Incluye `diag::TrackedMutex`, un guard
  genérico probado pero **deliberadamente NO adoptado** (ver abajo).
- `vfs::mount`: 7 reentrancy probes de familia A sobre los métodos mutantes.
- `mm`: 2 reentrancy probes sobre el seam `FrameSource`.

## Las tres familias de test (misma taxonomía, respétala)

Este kernel corre en **un solo vCPU** (verificado: ni `src/main.rs` ni
`scripts/qemu-debug.sh` pasan `-smp`; el selftest de ACPI reporta `found 1`). El
modo de fallo real **no es "dos CPUs compiten"** sino **"el mismo hilo reentra un
lock que ya tiene"**.

- **A. Reentrancy probes** (sin hilos, deterministas). Modelan el fallo REAL. En
  ellos **el cuelgue ES la señal** y va documentado en el test; son la única
  excepción a la regla del watchdog. Plantillas: `vfs/src/mount.rs::
  direct_children_callback_from_lookup_does_not_self_deadlock` y los 7 nuevos.
- **B. Contention probes con `std::thread`**. NO simulan el kernel: hacen
  **observable la contención**, que es la única forma de validar los instrumentos.
  Declara en cada uno que modela contención de host, no el single-core.
- **C. Auditoría de instrumentos (sabotaje)**. Por cada test nuevo: rómpelo a
  propósito en una copia y demuestra con salida literal que falla o cuelga. **Sin
  esa prueba el test no se acepta.** Es el entregable central, no un extra.

## Los huecos que quedaron abiertos (esto es tu material de partida)

Ordenados por valor. El 1 es el gordo.

### 1. La disciplina `without_interrupts` de `BUDDY`/`SLAB_ALLOCATOR` no la guarda ningún test

Es el invariante del bug que costó meses (`ab58dba`, memoria
`slab_lock_self_deadlock`): el timer ISR interrumpe código que tiene tomado el
lock del allocator, y `switch_to_next` vuelve a pedirlo al crecer un `VecDeque`.

**Verificado la sesión pasada:** los locks viven en `kernel/src/allocator/mod.rs`,
y el crate `mm` **no tiene ni un lock ni interior mutability** (grep, no
suposición: los únicos `UnsafeCell`/`Cell` en `mm/src` están en andamiaje de
test). Por eso los probes de `mm` guardan una clase *relacionada* (que `mm`
recupere estado interno no reentrante), no la histórica.

Lo único que cubre el invariante real es el canario siempre activo
`SLAB_LOCK_CONTENDED` (`/proc/kdebug`). Reporta —se le vio a 0 en arranque real—
pero **su detección no está demostrada por nada que se ejecute**. Un detector que
nunca se ha visto disparar es decoración hasta que se demuestre lo contrario.

Piensa qué forma darle: ¿un seam host-testeable en `kernel/src/allocator/mod.rs`?
¿un test de QEMU en `kernel/src/hw_tests.rs` que fuerce la reentrada? ¿demostrar
el canario por sabotaje bajo QEMU, como se hizo con `EXT2_LOCK`? Evalúalas, no
des ninguna por buena.

### 2. `TrackedSchedulerGuard` no tiene test de ninguna clase

Vive en `kernel/src/process/scheduler.rs`. Sus aserciones de IF=0 en ambos
extremos son autoaplicadas en runtime, nunca verificadas. Es el candidato natural
del **paso 2 de la línea "host-testable extraction"** (memoria
`host_testable_extraction_next_steps`): *el scheduler contra un reloj falso*.
Estaba explícitamente fuera del alcance del plan de `vfs`
(`docs/fs/vfs-extraction-plan.md`, sección "Fuera de alcance") y ahora le toca.

Hay dos planes escritos que sirven de plantilla y **debes leer antes de decidir**:
`docs/fs/ext2-extraction-plan.md` y `docs/fs/vfs-extraction-plan.md`. El segundo
tiene la forma exacta que funcionó: núcleo puro + adaptador delgado, el estado
global se queda, la lógica se va, seis pasos verdes con commit cada uno.

Si resulta ser un refactor grande, **pregunta al usuario antes de comprometerte**,
igual que se hizo antes de extraer `vfs` y antes de crear `diag`.

### 3. `diag::TrackedMutex` está hecho, probado y sin adoptar

Guard genérico que fija estructuralmente el orden `before_lock` fuera de la
sección crítica → `on_acquire` estrictamente después del lock → `on_release`
estrictamente después de soltarlo, y que un `try_lock` fallido no reporte nada.
Los cuatro sabotajes lo tumban.

**El usuario decidió NO adoptarlo aún**, por un hallazgo que debes entender antes
de replantearlo: los dos guards existentes usan **órdenes de release opuestos** y
no es "uno bueno y otro con bug" — intercambian la dirección del error.
`TrackedSchedulerGuard` desbloquea y luego registra (puede **sobre**-reportar);
`vfs::ramfs::TrackedEntriesGuard` registra y luego desbloquea (puede
**sub**-reportar un lock que sí está tomado). Corolario, escrito en
`diag/src/tracked.rs`: el `"anything but 0/1 means a guard leaked"` de `LockDiag`
**no es propiedad del contador**, vale solo porque `SCHEDULER` se toma siempre con
`cli`, y no sobreviviría a una segunda CPU.

Si haces el paso 2, la adopción en el scheduler deja de ser un refactor gratuito y
pasa a ser parte del diseño. Decídelo entonces, con el usuario.

### 4. `EXT2_LOCK` solo está guardado implícitamente, y el fallo es un cuelgue

`kernel::hw_tests::ext2_memdisk_roundtrip` lo guarda de rebote: `take_child` hace
`EXT2_LOCK.lock()` y luego llama a `self.lookup`, así que un lock en `lookup`
autobloquea. Verificado por sabotaje: ese test **cuelga a mitad** (imprime su
nombre, nunca `[ok]`, el tercer test no arranca). Ya está documentado en
`kernel/src/fs/ext2.rs`. El crate `ext2` no tiene locks, así que ningún test de
host lo alcanza. Queda como está salvo que se te ocurra algo mejor que un cuelgue.

### 5. `cow_if_violations_set_ref` = 675 por arranque, benigno pero engañoso

Investigado y cerrado la sesión pasada: los llamantes son `sys_exec`/`elf_loader`
con IF=1; `demand_paging` (IF=0 por la interrupt gate) no dispara. **Es benigno**:
`FRAME_REFCOUNTS[idx] = count` es un store de un byte, indivisible, sobre un frame
recién asignado en propiedad exclusiva.

Pero el `# Safety` de `set_ref` dice *"Must be called with interrupts disabled"*,
un contrato que se viola 675 veces por arranque en la ruta normal. **El contrato
está de más, no los llamantes.** Recomendación pendiente: relajarlo y renombrar el
contador, porque hoy grita "violación" por algo que está bien y el próximo que
cace un hang lo perseguirá. No se cambió por ser producción y tocar
`/proc/kdebug`. **Consúltalo con el usuario, es una decisión suya.**

## Hechos ya verificados (no los re-derives, costaron tiempo)

- **Líneas base medidas, con el árbol en `3e5b3a6`:** `hal` 71, `ext2` 91, `mm` 39
  (46 con `--features slab-debug`), `vfs` 158, `diag` 23. `cargo build` en la raíz
  limpio. `scripts/run-kernel-tests.sh` PASS 3/3, exit 0. Mídelas tú igual al
  empezar, pero si te dan otra cosa, sospecha de tu entorno primero.
- **`cargo-mutants` NO está instalado**, y comprobado que tampoco serviría: su
  modelo de mutación sustituye cuerpos de función y no puede expresar *reordenar
  dos sentencias*, que es la forma de casi todos los invariantes de aquí. El
  sabotaje manual (copiar el crate, romperlo, correr) es la herramienta correcta.
- `#[cfg(test)] extern crate std;` en un crate `#![no_std]` funciona; ya está
  puesto en `vfs/src/lib.rs`. Nota que `hal`/`ext2`/`mm`/`diag` usan en cambio
  `#![cfg_attr(not(test), no_std)]`. La forma de `vfs` es más estricta (obliga a
  escribir `std::` explícito en los tests) y es la preferible en crates nuevos.
- La máquina tiene 24 cores. Los contention probes de familia B corren de verdad.
- El flake de `ext2` **está arreglado**. Si vuelves a ver rojo intermitente ahí,
  es algo nuevo, no lo de `docs/fs/ext2-test-flake.md`.

## Trampas operativas que ya mordieron (evítalas)

- **`scripts/qemu-debug.sh` revienta con paths largos.** El socket unix de monitor
  tiene un límite de 108 bytes y la ruta del scratchpad lo excede
  (`UNIX socket path ... is too long`). Usa `QEMU_DEBUG_STATE_DIR=/tmp/qd-algo`
  (corto) y `QEMU_DEBUG_DISK_IMG=/tmp/algo.img` apuntando a una copia de
  `disk.img` — dos QEMU escribiendo el `disk.img` real a la vez lo corrompen.
- **`$?` después de un pipeline captura el exit del ÚLTIMO comando, no el de
  `timeout`.** Esto invalidó una medición de sabotaje: daba `EXIT=0` sobre un test
  que en realidad estaba colgado. Redirige a fichero y mide el exit del `timeout`
  directamente: `timeout 45 cargo test X > /tmp/out 2>&1; echo "EXIT=$?"`.
  **124 = colgado, y es un resultado VÁLIDO y esperado en familia A.**
- **Un `cargo build` rápido no significa que no haya compilado.** Si dudas de si el
  build lee tu código, mételE un error de sintaxis a propósito y comprueba que
  falla. La caché incremental da tiempos de 0.25s que parecen no-ops.
- **Un subagente Sonnet murió a mitad por límite de sesión** (`rate_limit`, HTTP
  429). Dejó el trabajo a medias en el árbol. Comprueba `git status` tras cada
  agente y prepárate para terminar tú el paso.

## Sobre confiar en los subagentes

La memoria `verify_subagent_reported_numbers` dice que los conteos autoreportados
de Sonnet han estado mal antes. **Matiz de la sesión pasada: esta vez sus informes
fueron honestos** — uno reportó 151 cuando mi prompt había supuesto 152, y otro
declaró por iniciativa propia que su probe no alcanzaba el bug histórico. Sigue
verificando todo, pero el fallo que sí ocurrió fue distinto y más sutil:

> **Un agente midió la línea base con la receta débil que le di y obtuvo 0 fallos
> en 25 corridas, concluyendo implícitamente que el bug no existía.** Al medirlo yo
> con `cargo test` completo (24 hilos en vez de 2): 1 fallo en 20 antes del
> arreglo, 0 en 20 después.

Lección: **exige que el agente diga cuándo su medición no probó nada**, en vez de
reportar el número desnudo. Y desconfía de las recetas de reproducción escritas en
los docs del repo hasta reproducirlas tú.

## Restricciones duras

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
  del usuario (`without_interrupts` alrededor de los locks alocantes, `cli`/`sti`
  alrededor de `SCHEDULER`).
- Los submódulos `mlibc` y `quakegeneric` aparecen modificados desde antes: no los
  toques ni los añadas a ningún commit.
- Formato de commit: cuerpo explicativo, en inglés, que cuente el *porqué* y lo que
  se midió — mira los 6 commits anteriores como referencia de tono y longitud. Al
  final, `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>`.

## Cómo quiero que orquestes

1. Antes de delegar nada, lee tú: los 6 commits anteriores, `diag/src/tracked.rs`
   (su doc comment tiene el hallazgo del compromiso de órdenes),
   `kernel/src/process/scheduler.rs` (`TrackedSchedulerGuard` y `local_scheduler`),
   `kernel/src/allocator/mod.rs`, y los dos planes de extracción. Corre las suites
   para tener tu propia línea base medida.
2. Escribe un plan de pasos concretos, cada uno con su commit y un criterio de
   "listo" verificable por comando.
3. Para cada paso, da a un subagente Sonnet un prompt **autocontenido**: qué
   archivos tocar, qué firma exacta, qué comportamiento debe quedar idéntico, qué
   comando exacto correr, y **qué debe reportar como salida literal**. Prohíbele
   resumir. Dile explícitamente qué rutas de scratchpad usar y que borre las copias
   de sabotaje al terminar.
4. **Por cada test nuevo, exige la prueba de sabotaje**, con el exit code medido
   correctamente (ver trampas arriba).
5. Después de cada informe, **re-corre tú los comandos y lee el diff real**. Al
   menos un sabotaje por paso, rehazlo tú de forma independiente.
6. Si un paso revela que el diseño asumido no encaja, **para y replantea**. Pasó la
   sesión pasada con el orden de release del guard y fue el resultado más valioso
   del día: no fuerces el plan.
7. Puedes lanzar 2-3 agentes en paralelo si tocan crates distintos. No más: pierdes
   la capacidad de verificar y los `cargo build` se pisan.

## Entregable final

Un resumen corto con: qué tests se añadieron y de qué familia (A/B/C) es cada uno,
qué invariantes quedan realmente guardados y **cuáles siguen sin estarlo** (sé
explícito con los huecos: es el punto de todo el encargo), los conteos reales de
tests antes y después por crate, y la lista de sabotajes ejecutados con su
resultado. Si algún detector existente resulta no detectar nada, dilo claramente y
propón retirarlo o arreglarlo — un instrumento que miente es peor que no tenerlo.
