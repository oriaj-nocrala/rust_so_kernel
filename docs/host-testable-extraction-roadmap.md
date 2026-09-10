# Extracción a crates host-testables — roadmap

Hacia dónde va la línea `hal`/`ext2`/`mm`, y qué compra cada paso. Es una
*dirección*, no un calendario: los pasos están ordenados por dependencia y por
relación coste/beneficio, no por fecha. Mismo espíritu que
[docs/drivers/roadmap.md](drivers/roadmap.md), aplicado al eje de testabilidad
en vez de al de drivers.

Para el precedente concreto y completo, ver
[docs/fs/ext2-extraction-plan.md](fs/ext2-extraction-plan.md).

---

## La restricción que lo fuerza

`kernel` **no compila para host**: `-Z build-std` más el doble build del bin
target bajo `--cfg test` produce dos `core` independientes que colisionan
(`error[E0152]: duplicate lang item`). Está verificado, no asumido — ver
CLAUDE.md y el comentario de cabecera de `scripts/run-kernel-tests.sh`.

Consecuencia: **toda lógica que viva dentro de `kernel/` solo se puede
ejercitar arrancando QEMU.** Un `cargo test` de host tarda milisegundos; un
arranque de QEMU con el amplificador tarda minutos y, bajo TCG con gdb, mucho
más. Esa diferencia es la que decide qué bugs se pueden cazar en un ciclo corto
y cuáles cuestan una tarde.

De ahí la regla que guía todo este eje:

> **La lógica que puede hablar en tipos planos, en vez de en los globales
> concretos de este kernel, se muda a un crate donde `cargo test` la alcance.**

---

## La línea hasta ahora

| Crate | Qué se llevó | Tests de host |
|-------|--------------|---------------|
| `hal/` | Lógica pura de driver + seams `PortIo`/`PhysMem` | 64 |
| `ext2/` | Layout on-disk, bitmaps, indirectos, dirents, symlinks, mount/repair | 89 |
| `mm/` | Buddy + slab, con seams `PhysMap`/`FrameSource` | 32 |

Los tres siguen el mismo patrón y los tres se pagaron solos: el `ext2`
descubrió (y su oráculo `e2fsck` confirmó) el fallo de `reclaim_orphans` que
corrompía cada montaje; el `mm` permitió las property tests del buddy —
484 282 operaciones, cero violaciones— que **invalidaron** una hipótesis que
llevaba semanas dirigiendo una investigación.

---

## Por qué esto y no un port estilo UML

La alternativa evidente es portar el kernel a espacio de usuario de Linux, al
estilo de User Mode Linux: gdb nativo, sanitizers, arranque en milisegundos,
cientos de instancias en paralelo. Evaluado en 2026-08-06 y **descartado como
siguiente paso**, por tres razones:

1. **Es un port de arquitectura, no un refactor.** Habría que sustituir el
   espacio de direcciones entero (`OwnedPageTable`, demand paging, COW, VMAs —
   todo montado sobre tablas de páginas x86 reales) por `mmap` del host, más
   toda la maquinaria de cambio de contexto, más la intercepción de syscalls de
   los procesos de usuario (`ptrace`/SECCOMP).
2. **No habría cazado los bugs que motivaron la idea.** Los dos de la caza de
   agosto de 2026 viven exactamente en la maquinaria que UML *reemplaza*: el
   epílogo de `sys_exit` corriendo sobre una pila de kernel ya encolada para
   liberar, y el `Box<TrapFrame>` reescrito entre SAVE y RESUME. Bajo UML no
   hay `iretq`, ni pilas de kernel por proceso, ni marcos de excepción.
3. **La extracción ya está a medias y paga desde el primer día**, mientras que
   el port solo paga al final.

Si alguna vez se quiere de todos modos, la variante mínima viable es un modo
"solo kernel": un único espacio de direcciones, procesos simulados como hilos
del host, sin ring 3 ni `ptrace`. Ejercita scheduler, VFS y concurrencia de
asignadores —donde más se gana— y evita las dos piezas caras.

---

## Paso siguiente — `vfs` + `ramfs`

**Por qué éste primero:** el seam difícil ya está puesto. `Filesystem` e `Inode`
(`kernel/src/fs/mod.rs`, `fs/vfs.rs`) ya son traits, que es justo la parte que
en `ext2` hubo que diseñar desde cero. Lo que queda es mecánico.

Qué se muda: normalización y resolución de rutas (`normalize_path`,
`resolve`/`resolve_no_follow` con su guardia de 8 saltos `ELOOP`), la tabla de
montajes y su búsqueda, `split_parent`, y `ramfs` entero (es un árbol de
`BTreeMap` en memoria: no toca hardware, no toca globales del kernel).

Qué se queda en `kernel/`: el adaptador — los globales (`MOUNTS`), las
conversiones a `Errno`, el puente con `FileHandle`/`FileDescriptorTable`, y los
relojes (`crate::time::now_unix_secs`).

Qué compra: hoy, un bug de resolución de rutas o de mutación de directorios
solo se ve arrancando QEMU. Después, se ve en un `cargo test` de milisegundos —
y, sobre todo, se puede escribir la property test que hoy no existe: secuencias
aleatorias de `mkdir`/`symlink`/`rename`/`unlink` contra un modelo de
referencia, comprobando invariantes tras cada operación.

## Hecho — la lógica del scheduler contra un reloj falso

Ver [docs/sched/sched-extraction-plan.md](sched/sched-extraction-plan.md) para
el precedente completo, en 5 commits (`4738fa1`..`4db1cd1`).

Qué se mudó: run queues multinivel, decay de prioridad al ser expulsado, aging
periódico contra la inanición, y el cálculo del cuanto
(`BASE_QUANTUM + eff_pri * BONUS`) — todo con el tiempo **inyectado** en vez de
leído de un global, al crate `sched` (43 tests de host).

Qué se quedó: `TrapFrame`, el cambio de contexto, el ISR del timer, la TSS.
Nada de eso puede salir del kernel, y es deliberado.

Qué compró: las propiedades que antes nadie comprobaba — que el índice de cola
siempre coincide con la prioridad efectiva clamped, que las prioridades
efectivas se mantienen en rango, que ningún pid se duplica entre colas — ahora
verificadas por `check_invariants()` y tres property tests. Son exactamente el
tipo de invariante que una property test verifica bien y que un arranque de
QEMU verifica mal.

**Lo que la extracción destapó, no lo que compró**: el propio checker y sus
property tests demostraron que el aging de hoy *no* rescata a los hambrientos
de forma fiable — puede incluso agravar la inanición bajo contención
sostenida con prioridades base distintas — un hallazgo real, medido contra el
crate, no razonado; ver `docs/sched/sched-bugs-plan.md` para el detalle y el
plan de arreglo, todavía sin ejecutar.

---

## La receta (patrón repetible, extraído de `ext2` y `mm`)

1. **Workspace propio**: tabla `[workspace]` vacía en su `Cargo.toml`, y
   `exclude` en el raíz. Razón concreta: el perfil `panic = "abort"` de este
   workspace rompería el harness de `cargo test`, que necesita unwinding.
2. **Dependencia `path`** desde `kernel`, nunca al revés.
3. **El crate habla en tipos planos** (números de inodo, rangos de bytes, su
   propio tipo de error), nunca en tipos del VFS ni en globales del kernel.
4. **Cada dependencia hacia el kernel se convierte en un seam**: `hal::PortIo`,
   `hal::PhysMem`, `hal::block::BlockDevice`, `mm::PhysMap`, `mm::FrameSource`.
5. **Nada de logging desde el crate**: las condiciones recuperables vuelven
   como datos (`mm::buddy::PhantomEvent`, `mm::slab::AllocEvent`) y el
   adaptador del kernel las imprime. Así la salida serial queda byte a byte
   idéntica tras la extracción — se verificó, no se supuso.
6. **Los builders de imágenes/fixtures de test viven en el crate** y los
   importan tanto sus propios tests como los de integración en QEMU
   (`ext2::testimg`). Una sola fuente, nunca una copia en el kernel.

---

## Ganancia adyacente y barata: Miri

Los crates ya extraídos nunca se han pasado por **Miri**
(`cargo +nightly miri test`), que detecta justo la familia de UB —aliasing,
procedencia de punteros— que más cuesta cazar en QEMU. Es una tarde de trabajo,
no un port.

Aviso realista: `mm` puede no pasar a la primera, y no necesariamente por bugs
reales — el slab está hecho de listas intrusivas y aritmética de punteros
crudos sobre regiones falsas en los tests, que es lo que Stacked/Tree Borrows
suele rechazar. Distinguir "UB real" de "Miri es estricta aquí" es en sí mismo
información que hoy no se tiene.

---

## Tabla de estado

| Paso | Qué se lleva | Compra | Estado |
|------|--------------|--------|--------|
| `hal` | Lógica de driver + seams de hardware | 71 tests sin QEMU | hecho |
| `ext2` | Todo el detalle on-disk | 91 tests + oráculo `e2fsck` | hecho |
| `mm` | Buddy + slab | 39 tests + property tests | hecho |
| `vfs` + `ramfs` | Rutas, montajes, árbol en memoria | 158 tests, property tests del VFS | hecho |
| **Scheduler (reloj falso)** | Colas, prioridad, aging, cuanto | 3 property tests + invariantes de equidad | **hecho** (destapó defectos reales de aging/inanición, ver `docs/sched/sched-bugs-plan.md`) |
| Miri sobre lo extraído | — | Detección de UB de aliasing | barato, sin hacer |
| Port estilo UML | El kernel entero | gdb nativo, N instancias | descartado (ver arriba) |

Cada fila es la disciplina de la anterior aplicada a mayor alcance, y ninguna
es especulativa: las tres primeras ya se pagaron solas.
