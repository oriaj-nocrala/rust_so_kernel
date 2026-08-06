# Bug 2 — findings (sesión 2026-08-05)

> Fichero nuevo; NO sustituye a `docs/hang-hunt-status.md` (ese sigue siendo el
> registro permanente). Esto es el diario de la caza del bug 2 con evidencia viva.

# ═══════════════════════════════════════════════════════════════════════════════
# RESUMEN EJECUTIVO (actualizado al cierre, 2026-08-05) — leer esto y parar
# ═══════════════════════════════════════════════════════════════════════════════

## CAUSA RAÍZ: el flag de dirección (DF) no se limpia en las entradas al kernel

La causa raíz del bug 2 (y de las tres manifestaciones que llevaban meses sin
explicación) es que **el kernel entra con DF=1** cuando el código interrumpido
estaba dentro de un `memmove` solapado hacia atrás (entre su `std` y su `cld`).
`rep movsb` obedece a DF: con DF=1 copia HACIA ATRÁS. Dos vectores, ambos
verificados en el código:

1. **`kernel/src/process/tss.rs:154`**: `wrmsr(IA32_FMASK, 1 << 9)` — FMASK solo
   limpia IF (bit 9); **DF (bit 10) no está enmascarado**, así que el DF de
   usuario entra intacto al kernel por `syscall`. (Linux enmascara DF en FMASK.)
2. **Cero `cld` en `kernel/src/`** antes del fix (grep): los stubs custom
   (`timer_preempt.rs::timer_interrupt_entry`, `syscall/mod.rs::syscall_entry_fast`)
   no lo emitían. (Los shims de rustc de los handlers `x86-interrupt` SÍ emiten
   `cld` — verificado en page_fault/keyboard — pero los dos stubs de asm propio no.)

**Observado directamente (no inferido)**: en TODOS los fires pre-fix, el marco
grabado (el contexto interrumpido) tenía RFLAGS con **bit 10 (DF) puesto**
(`rflags=0x10606`/`0x10602`), y los marcos "now" con DF limpio (`0x10202`).
Además, el `rip` del récord fue **`copy_backward` en todos los fires** — el memmove
de compiler_builtins que hace `std`→`rep movsb`→`cld`. Un tick entre `std` y `cld`
entra al kernel con DF=1: ventana de pocas instrucciones → intermitencia ~4-8%.

## Las tres manifestaciones y por qué (un solo mecanismo)

1. **Orphan-lock de ramfs** (el HANG histórico): el memcpy del box copy
   (`*proc.trapframe = *current_tf`, que compila a `rep movsb`) con DF=1 escribe a
   `box_ptr-159..box_ptr`, NO al box → el box conserva su marco rancio → el resume
   iretqa el marco de una syscall anterior → la syscall en vuelo (con su guard de
   `entries` tomado) queda abandonada → `outstanding=1` → el siguiente mkdir gira.
2. **`[BUG2]` (salto a datos como código)**: los 160 bytes ANTERIORES al box
   quedan pisados con basura → punteros de código corruptos en objetos vecinos del
   slab → saltos a `&MOUNTS` (vfs.rs:205) o al nodo del BTreeMap.
3. **Box no actualizado (el centinela sobrevive)**: un `write_volatile` de 8 bytes
   (store directo, no `rep`) sí llega al box; el `memcpy` de 160 bytes por el MISMO
   puntero no — la firma exacta de un `rep movsb` con DF=1.

## El fix y su medición

- `kernel/src/process/tss.rs`: `IA32_FMASK = (1<<9) | (1<<10)`.
- `kernel/src/process/timer_preempt.rs` y `kernel/src/process/syscall/mod.rs`:
  `cld` como primera instrucción de cada stub de entrada.
- **Medición**: dos campañas de 24 post-fix = **48/48 OK, 0 fallos**, frente al
  ~4-8% previo (1-2/24). `run-kernel-tests.sh` PASS, `cd mm && cargo test` verde.
- Auditoría de entradas (ninguna otra sin `cld`): el ISR del timer y syscall
  (corregidos); los shims de rustc de los handlers IDT (emiten `cld`);
  `jump_to_trapframe` es RESUME y debe NO limpiar DF (restaura el del marco);
  `user_test_fileio` son programas de usuario; los `asm!` son hojas.

## El otro bug cerrado en esta caza (independiente)

**`BAD RESUME FRAME`**: `sys_exit` reabría interrupciones (`drop(irq)`+`sti`)
ANTES del cambio real de pila, mientras el epílogo corría sobre el kstack del
proceso moribundo ya encolado para liberar → un tick liberaba la pila bajo los
pies de la CPU. Fix: IF=0 hasta el iretq + red en `timer_preempt.rs`. Medido 0/48.

## Los diez instrumentos defectuosos (la lección más transferible)

1. Canario de reentrada del slab que causaba el deadlock que medía.
2. Harness boot-matrix con 20/20 falsos (buscaba solo un banner).
3. `record_acquire` registrando antes de tomar el lock.
4. Umbral de watchpoint con un cero de menos (0x2800… < .text).
5. Timeout de boot-matrix matando arranques sanos-pero-lentos.
6. `rax` del anillo de syscalls pisado por el valor de retorno.
7. Contador de drift no reproducido.
8. **El check del offset r15/rip** (offset 0 leído como rip → 100% falsos;
   invalidó conclusiones previas de identidad).
9. Checks del tramo medio con ramas por conmutación (perturbaron y suprimieron
   el bug: 0/24).
10. **Procedencia de punteros**: un instrumento que observa memoria escrita por
    otro camino debe compartir la PROCEDENCIA del puntero, no solo el orden —
    `volatile` y `compiler_fence` ordenan accesos, no hacen que el compilador
    crea que dos punteros distintos apuntan al mismo sitio.

## Qué instrumentación se quedó vs se retiró (DECIDIDO Y EJECUTADO, 2026-08-06)

El inventario pieza por pieza, con archivos, está en
`docs/hang-hunt-status.md` ("WIP en el árbol"). Resumen del criterio:

- **SE QUEDA**: los dos fixes de causa raíz (FMASK con DF + `cld` en ambos stubs de
  asm propio), el fix de `sys_exit` (IF=0 hasta el iretq) y su mitad complementaria
  (`tick(interrupted_rsp)` difiere el free de una kstack aún en uso),
  `validate_resume_frame`, el detector de rebobinado (`tf_note_save`/`tf_note_resume`
  + `TF_REWIND`), el contador pasivo `RAMFS_ENTRIES_LOCK`, `SLAB_LOCK_CONTENDED`, y
  la feature `slab-debug` de `mm` (redzone+cuarentena, off por defecto, con tests).
- **SE RETIRA**: el amplificador `HANG_HUNT_ITERS`, el centinela BOXHUNT5, los checks
  BOGUS/BOGUS-RANGE, el detector de integridad del box y sus campos en `Process`,
  el page-protect (`box_watch_*`, nunca llegó a armarse), el NX-trap
  (`enable_physmap_nx` + el volcado `[BUG2]`/`[NX-TRAP]` del page fault handler), el
  anillo de syscalls, el instrumento de desplazamiento de RSP, y **todos los
  detectores de orphan que entran en pánico** (CONTENDED / STILL-HELD / drift /
  `panic_on_ramfs_lock_held_to_user`).
- **Por qué esos últimos no podían quedarse aunque el doc los listara como
  candidatos**: asumen "solo PID 1 toca el VFS y `entries` nunca está en contienda",
  cierto *bajo el amplificador* y falso en cuanto arranca busybox de verdad. Al
  quitar el amplificador dejan de ser detectores y pasan a ser una forma de que dos
  procesos usando `/tmp` a la vez hagan entrar el kernel en pánico. Un instrumento
  cuya validez depende del banco de pruebas se retira con el banco de pruebas: ése
  es el undécimo instrumento defectuoso, evitado a tiempo.
- **PERMANENTE (bug de build real, no es instrumentación)**: el build no vigila
  `mm/`/`hal/`/`ext2/` — regla: `touch build.rs` tras editar las crates extraídas.

> El detalle cronológico completo está debajo, sin tocar. Este resumen NO sustituye
> el diario; es la puerta de entrada.

# ═══════════════════════════════════════════════════════════════════════════════

## Estado del arte

Bug 2 (ABIERTO): un handler de excepción recibe un `sf` que no apunta a la pila
(datos de `.text` o de un `BTreeMap` de ramfs). Los valores del pánico
(RIP=0x2801e9cd000, CS=0x3, RSP=0x1) son datos de directorio leídos como marco.

## Reproducción y evidencia viva (esta sesión)

- `scripts/boot-matrix.sh 6 4 --timeout 400` (debug, WIP amplificador activo):
  **2/24 HANG (8.3 %)**, iteraciones **847/848**. Los serial.log de los dos
  HANG quedan en `/tmp/boot-matrix-1960075/serial-inst{1,6}-boot*-HANG.log`.
- Ambos HANG terminan en un diluvio de líneas idénticas:
  `[HANGHUNT-DIAG] SAVE kernel-mode tf: pid=1 cs=0x8 rip=0x100002227c2 rflags=0x10202 rsp_field=0x2801fe9fac8 ss_field=0x10 tf_box=0x2801f31ed00 kstack_top=0x2801fea0000`
  — el CPU está atascado ejecutando la MISMA instrucción una y otra vez.
- `addr2line` sobre el ELF con `virtual_address_offset=0x10000000000`:
  `0x100002227c2` → **`core::core_arch::x86::sse2::_mm_pause`** (`pause; ret`).
  O sea: el proceso gira en un spinlock.
- **Backtrace vivo** (gdb, `--gdb` + attach durante el HANG):
  ```
  #0  _mm_pause () at sse2.rs:26
  #1  core::hint::spin_loop () at hint.rs:287
  #2  core::sync::atomic::spin_loop_hint () at atomic.rs:4507
  #3  spin::relax::Spin::relax () at spin-0.10.0/relax.rs:29
  #4  SpinMutex<BTreeMap<String,Arc<dyn Inode>>>::lock (self=0x2801fec5f50) at spin-0.10.0/spin.rs:186
  #5  Mutex<...>::lock (self=0x2801fec5f50) at mutex.rs:186
  #6  RamDirNode::lock_entries (self=0x2801fec5f50) at kernel/src/fs/ramfs.rs:118
  #7  RamDirNode::mkdir (self=0x2801fec5f50) at kernel/src/fs/ramfs.rs:307
  #8  vfs::mkdir (path=...) at kernel/src/fs/vfs.rs:436
  #9  sys_mkdir (path_ptr=...) at kernel/src/process/syscall/fs.rs:341
  #10 syscall_handler (syscall_num=83) at kernel/src/process/syscall/mod.rs:501
  #11 syscall_handler_asm (regs=0x2801fe9ff60) at mod.rs:171
  #12 syscall_entry_fast ()
  #13-26 marco de usuario (0x000071000001fec8 …)
  ```
  Cadena **completa y válida sobre la pila de kernel** (los return addresses en
  `x/20gx $rsp` a `0x2801fe9fac8` son los de `spin_loop`/`relax`/`SpinMutex::lock`).
  PID 1 gira en el `SpinMutex` de `entries` del directorio raíz de `/tmp`
  (`self=0x2801fec5f50`) desde `RamDirNode::mkdir` → manifestación 4 del doc.

## Hipótesis descartada EN ESTA SESIÓN (corrección de hecho)

Re-derivé la hipótesis 6 ("IRETQ same-CPL no restaura RSP") por la puerta de
atrás, argumentando que en ring-0 la CPU solo empuja RIP/CS/RFLAGS y que
`box.rsp` sería basura del stack interrumpido. **Falso en x86-64 long mode**: la
CPU empuja SIEMPRE los 5 qwords (SS, RSP, RFLAGS, CS, RIP) en toda interrupción,
con o sin cambio de privilegio, y `IRETQ` siempre hace pop de 5. La evidencia
viva es consistente: `box.rsp = 0x2801fe9fac8` = el RSP real del kstack, y el
`$rsp` vivo en el attach vale lo mismo. El resume es correcto; la hipótesis 6
queda descartada como ya decía el doc.

## DUEÑO DEL LOCK — RESUELTO (gdb sobre el cuelgue vivo, HANG en iter 1207)

Volcado completo en `/tmp/hang-gdb-out.txt` (HANG att=25, iter 1207):

```
RAMFS_ENTRIES_LOCK (0x100002ea2e8):
  +0  acquires = 0x96e = 2414
  +8  releases = 0x96d = 2413        → outstanding = 1 (UN acquire sin release)
  +16 last_pid = 1                   → el dueño es PID 1
  +24 last_op_ptr = 0x1000019f008    → "symlink" (bytes en el ELF, file 0x19f008)
  +32 last_iter = 0x4b6 = 1206       → iteración 1206 (la previa al cuelgue en 1207)
  +40 last_op_len = 0x7 = 7          → "symlink" (7 chars)
      (layout: repr(Rust) reordena last_iter@32 / last_op_len@40)
SpinMutex entries raíz /tmp (self=0x2801fec5f50):
  byte 0 = 0x01                      → state=1: el lock está TOMADO
  +8  BTreeMap word0 = 0x2801e9cd000 → puntero al nodo raíz
  +16 word1 = 0x3                    → height = 3
  +24 word2 = 0x96d = 2413           → length (≈ 2 entradas × 1206 iteraciones)
```

**Interpretación:**

1. **Dueño = PID 1, `symlink`, iteración 1206.** No es auto-deadlock por
   reentrada del `mkdir` actual: es la sección crítica de `symlink` de la
   iteración anterior, que **tomó el lock y nunca soltó su `TrackedEntriesGuard`**
   (el `record_release` del `Drop` jamás disparó). El `mkdir` de la iteración
   1207 gira sobre el lock huérfano (manifestación 4 del doc, confirmada).
2. **Reentrada descartada por código**: `vfs::symlink` (vfs.rs:442-447) hace
   `split_parent` + `resolve` + una sola llamada (`RamDirNode::symlink`,
   ramfs.rs:356-364); `resolve` no retiene ningún lock de ramfs (devuelve un
   `Arc<dyn Inode>` clonado). `RamDirNode::symlink` toma `entries` UNA vez y
   nunca vuelve a entrar: `contains_key`, `Arc::new(RamSymlinkNode{target:
   target.to_string()})`, `entries.insert` — ninguna de esas llamadas re-toma
   el mismo `entries`. Tampoco ningún `Drop` alcanzable desde la sección
   crítica toca el mismo directorio.
3. **Datos SANOS**: el BTreeMap es coherente (node=0x2801e9cd000, height=3,
   length=2413). Es un **lock huérfano sobre datos sanos** — el contexto que
   tomó el lock fue abandonado a mitad de la sección crítica, no datos
   corruptos.
4. **Correspondencia campo a campo del doc CONFIRMADA en vivo**: el
   `word0` del BTreeMap de `/tmp` (0x2801e9cd000) es EXACTAMENTE el valor del
   RIP del pánico determinista del doc (0x2801e9cd000). El marco corrupto del
   pánico es este mismo mapa, leído como `InterruptStackFrame`.

**Última SAVE normal antes del diluvio** (serial.log inst del HANG, línea 1719):
`rip=0x100001e49e7` = `InterruptGuard::drop` (`sti; ret`, irq_guard.rs:46) — el
epílogo de retorno de syscall. Y el `debug.log` de QEMU (`-d int,guest_errors`)
registra SOLO `v=20` (timer IRQ) durante todo el boot+hang: **el abandono fue
sin fault** (un `iretq`/`ret` hacia memoria mapeada, NX off, nunca dispara
excepción) — consistente con un resume de trapframe corrupto o un return
address pisado, no con un page fault ni un #GP.

**El mecanismo que queda abierto**: ¿cómo se abandona la sección crítica de
`symlink` (el guard nunca droppea)? El resume mid-syscall es correcto (iretq
pop de 5, RSP real). Candidatos, sin cerrar:

- (a) Un write al heap pisa el `Box<TrapFrame>` de pid=1 (0x2801f31ed00) entre
  un SAVE y su RESUME → el iretq restaura un RIP/RSP corrupto → la continuación
  del symlink se pierde → guard filtrado. El box vive en el heap, junto a los
  objetos del BTreeMap/Strings.
- (b) El timer dispara con RSP en el heap → `current_tf` (el frame del ISR)
  apunta a datos → el SAVE copia basura al box → resume divergente. **Gap en la
  instrumentación del WIP**: el check BOGUS de `switch_to_next`
  (scheduler.rs:960, `raw < 0x1000_0000`) NO detecta `current_tf` apuntando al
  heap (≥ 0x2801…), que es justo el caso sospechoso. Extenderlo a
  "current_tf fuera de todo kstack_range" es el discriminante barato.

### Experimentos planeados / siguientes

- **Discriminante barato (código, toca solo `scheduler.rs` WIP — pedir
  permiso)**: en el check BOGUS, además de `raw < 0x1000_0000`, marcar
  `current_tf` que no caiga dentro de ningún `kstack_range` conocido (imprimir
  todos los `kernel_stack` de `iter_all()`). Si el bug dispara esa condición
  antes del colapso, se confirma (b) y se caza el primer evento corruptor.
- Reproducir y, en lugar de esperar el HANG, vigilar el PRIMER save cuya
  `rsp_field`/`rip` no caiga en kstack/.text → es el instante del seed.

## EXPERIMENTO DISCRIMINANTE EJECUTADO (2026-08-05) — resultado NEGATIVO

Se añadió a `scheduler.rs` (autorizado, solo aditivo) el check **`BOGUS-RANGE`**:
`current_tf` (el puntero al marco del timer ISR) debe caer SIEMPRE dentro del
kstack del proceso en ejecución
`[proc.kernel_stack - (1<<KERNEL_STACK_ORDER), proc.kernel_stack)` — O(1), una
comparación contra el kstack del proceso ya en mano, sin `iter_all()`.
El check BOGUS clásico (`raw < 0x1000_0000`) queda intacto.

**Resultado: `BOGUS-RANGE` NO disparó NUNCA** — ni en el boot con HANG del
boot-matrix (1/24, iter 801) ni en 10 boots individuales adicionales (9 sanos +
1 sin resolver), todos con `bogus_range=0 bogus_classic=0`. El rebuild es real
(el RIP del flood cambió de `0x100002227c2` a `0x10000222e72`). La tasa bajó de
8.3% (2/24) a 4.2% (1/24) — perturbación de timing del propio instrumento
(caveat 2), el bug sigue reproduciéndose.

**Refutación (con evidencia)**: en TODOS los switch-ticks, el `current_tf` del
timer ISR estuvo dentro del kstack del proceso en ejecución. La hipótesis
"el timer dispara con RSP en el heap en un switch-tick" (seed del write-al-heap
al box) queda **descartada**. Además:
- el `debug.log` de QEMU (`-d int,guest_errors`) solo registra `v=20` (timer):
  el abandono de la sección crítica es sin fault;
- TODAS las SAVE diag tienen `rip` válido en `.text` (resueltos a símbolos) y
  `rsp_field` dentro del kstack — ningún SAVE capturó un marco corrupto.

**Lo que queda**: el abandono del symlink (iter ~1206) es un desvío sin fault
con RSP válido. El siguiente discriminante apunta al contenido del box ENTRE un
SAVE y su RESUME inmediato (offsets 120-136 del box, campo `rip/cs/rflags`), o
al camino no-switch del timer (6 de cada 7 ticks, invisible para el check, que
solo corre en switch). Zona siguiente: instrumentar el instante de un resume
que no continúe en el RIP guardado (detectar divergencia en el primer tick
post-resume), o volcar el box inmediatamente antes de cada iretq del resume.

## FASE A — REINTERPRETACIÓN CONFIRMADA (el syscall retornó sin el Drop)

El hang captura `mkdir` de la iteración N+1; el huérfano es `symlink` (o
`mkdir`) de la N. Ambas las ejecuta PID 1 sobre la misma pila. La evidencia:

- Serial crudo (boot-matrix #1, hang iter 847):
  `1263:[fb] hunt: iter 847 begin` → `1264:[SAVE...]` (flood). El amplificador
  (userspace) avanzó de 846 a 847 ⇒ **la syscall de symlink de la iteración 846
  retornó a espacio de usuario**.
- Mismo patrón en boot-matrix #2 (iter 801: `1195: iter 801 begin` → `1196:` flood).
- Gap de exactamente 1 iteración entre el huérfano (N) y el giro (N+1).
- `acquires=2414 / releases=2413` = **un solo evento**, no deriva.

**Conclusión**: NO es "PID 1 murió dentro de la sección crítica" (como decía el
doc): es "PID 1 **salió** de la sección crítica sin ejecutar el `Drop` del
`TrackedEntriesGuard`". El desvío está en un puntero de código en el retorno,
que se saltó el epílogo de Rust.

## FASE B — CHECK DE SALIDA DE SYSCALL: NO DISPARÓ (resultado)

Se añadió (autorizado) un check en `syscall_handler_asm` (syscall/mod.rs): si
`RAMFS_ENTRIES_LOCK.outstanding() != 0` justo antes de retornar al asm, imprime
el trapframe. Se añadió `DirLockDiag::outstanding()` (debug.rs, O(1), 2 atomic
loads). `run-kernel-tests.sh`: 3/3 PASS antes de medir.

**Resultado**: NO disparó en ningún boot (5/24 fallos del boot-matrix: 2 HANG +
3 PANIC, rate subió de 4.2% a 20.8% por la perturbación — caveat 2). El desvío
**se salta también el epílogo de `syscall_handler_asm`** — la syscall con el
lock filtrado no llega nunca al check. El check en la salida es el lugar
equivocado: hay que cazar el desvío en el RESUME del timer (timer_preempt.rs,
regla 4 — pedir permiso), o en el instante del salto al heap.

## FASE B+ — LOS PÁNICOS REVELAN EL DESTINO DEL DESVÍO (pánico determinista completo)

Los 3 pánicos del boot-matrix 2097617 (iters 45, 1044, 1410) son idénticos en
lo esencial:

```
PAGE FAULT (kernel)  Address: 0x2801fec5f58  Error: 0b10001  ← PRESENT + INSTRUCTION FETCH
RIP: 0x2801e9cd000  CS: 0x3  RSP: 0x1
ramfs_entries_lock: acquires=2089 releases=2088 outstanding=1 last_acquirer pid=1 op=mkdir iter=1044
[tmp_root_diag] /tmp entries raw words: [0]=0x2801e9cd000 [1]=0x3 [2]=0x828 (len=2088)
```

- **`Address: 0x2801fec5f58` = el propio BTreeMap** (`self+8` del SpinMutex del
  root de /tmp; `self=0x2801fec5f50` confirmado en gdb). DETERMINISTA en los 3
  pánicos.
- `Error: 0b10001` = bit0 (present) + bit4 (**instruction fetch**): el CPU
  **saltó a ejecutar el heap como código** y falló al fetchear en el BTreeMap.
  (NX: el kernel no escribe EFER.NXE; el bit 4 es el I/D del error de PF, y la
  página en cuestión no es ejecutable — o NX heredado de OVMF, o el fault es
  fetch contra una página cuyo permiso lo niega.)
- Los campos del marco (RIP=word0, CS=word1, RSP=0x1) = las palabras del mapa.
- La op huérfana VARÍA (mkdir en este boot, symlink en el gdb) — cualquier
  sección crítica puede ser abandonada.
- El `[tmp_root_diag]` del mismo volcado lee el mapa como `[node=0x2801e9cd000,
  height=3, length=2088]` → el layout real es `[node, height, length]` (los
  labels `height?/node?` del código están invertidos).

**Lo nuevo**: el RIP "imposible" del doc (0x2801e9cd000) es, además de datos
del BTreeMap, la **dirección real a la que salta la ejecución corrupta** (el
puntero al nodo raíz). La ejecución corrupta corre datos del heap como código.

## Estado del mecanismo

Cadena causal confirmada hasta el desvío; el SEED (qué corrompe el primer
puntero de código) sigue abierto:

1. ✅ HANG = lock `entries` de ramfs huérfano (outstanding=1), dueño PID 1.
2. ✅ El syscall retornó a usuario sin ejecutar el `Drop` del guard (Fase A).
3. ✅ El desvío se salta el epílogo de `syscall_handler_asm` (Fase B, negativo).
4. ✅ El destino del desvío es el heap: ejecución de datos del heap como código
   (fault determinista en 0x2801fec5f58, el BTreeMap).
5. ❓ El seed: un puntero de código corrompido a 0x2801e9cd000 (el node ptr).
   Con el resume mid-syscall correcto (iretq pop 5, RSP real), la corrupción
   debe ocurrir ENTRE un SAVE y su RESUME inmediato, o en el camino no-switch
   del timer (6/7 ticks, invisible para todos los checks actuales).

### Siguiente instrumento (pide permiso: toca timer_preempt.rs, regla 4)

En `timer_preempt_handler`, justo después de `switch_to_next` devuelve el box,
validar el frame que se va a iretq: `rip` en `.text` y `rsp` en el kstack del
proceso. Si la validación falla → el box ya estaba corrupto ANTES del resume →
es el instante exacto del seed. Alternativa: en el asm del timer, antes del
`iretq`, comprobar `[box+120]` (rip) contra `.text`. Es la única ventana que
caza el seed y no está ya instrumentada.

## FASE C+ — VALIDACIÓN DEL FRAME EN EL RESUME (timer_preempt.rs): NEGATIVO

Añadido (autorizado excepcionalmente, solo aditivo, asm intacto):
`validate_resume_frame()` en `timer_preempt.rs`, que valida el frame que el asm
va a iretq, en AMBOS caminos (switch y no-switch), justo antes de devolver:
`cs` ∈ {0x8, 0x23} con `ss` coherente (0x10/0x1b), `rip` no-heap y no-página-nula
(ring 0; frontera = `physical_memory_offset()` real, no constante) o en espacio
usuario (ring 3), `rsp` en el kstack del proceso o en la banda de la pila de
boot, `rflags` bit 1 = 1. Al disparar: panic inmediato con frame completo +
`pid` + `ramfs_entries` diag. O(1), sin asignar.

**Mediciones `boot-matrix.sh 6 4` (mismo comando, condición 3):**
- ANTES del cambio (árbol con BOGUS-RANGE + syscall-exit check): **4/24 (16.7%)** (3 HANG + 1 PANIC).
- DESPUÉS (v1 con falso positivo): **24/24 PANIC (100%)** — el check paniqueaba en el PRIMER tick tras `start_first_process`: el CPU está sobre la **pila de boot** (`rsp=0x18000014df0` < `phys_offset`), no en el kstack del proceso. Falso positivo del check, corregido permitiendo la banda `[0x100000, phys_offset)` (solo se alcanza legítimamente en esa ventana de boot; el kstack vive en ≥ `phys_offset`).
- DESPUÉS (corregido): **2/24 (8.3%)** (2 HANG). El bug sigue reproduciéndose; la perturbación del instrumento movió la tasa (caveat 2).

**Resultado**: la validación **NO disparó** en ningún boot con el bug (los 2 HANG
tienen frames válidos en el resume). `git`: el árbol actual lleva el check
permanente como red de seguridad.

**Reencuadre del bug (consecuencia del negativo)**: el frame que se iretq es
SIEMPRE válido (`cs`/`ss`/`rip`/`rsp`/`rflags` coherentes en switch y no-switch).
La corrupción que hace saltar al heap **no es un trapframe corrupto en el
resume** — ocurre DURANTE la ejecución ya reanudada, cuando la syscall normal
usa un valor corrupto (un puntero de un objeto vivo del heap) y salta al heap.
Candidatos vivos, en orden:
1. **Corrupción de heap de objetos vivos** (doble-free del slab → solapamiento
   de asignaciones, o overflow de un Vec/String/nodo BTreeMap) que pisa un
   objeto que la syscall usa después (BTreeMap, String, `Process`, o el propio
   box — este último se re-SAVEa encima, por eso la validación no lo ve).
2. GPR corrupto usado como destino de `call`/`jmp` mid-run (menos probable:
   los GPR se restauran del box recién salvado).
3. Return address del kstack corrupto (descartable: ningún escritor alcanza el
   kstack — es un bloque Buddy exclusivo).

## HALLAZGO SECUNDARIO — el primer tick tras `start_first_process` corre sobre la PILA DE BOOT

Descubierto por el falso positivo del check BAD RESUME FRAME (24/24 PANIC): el
primer timer tick tras `start_first_process` (el `sti` en `process/mod.rs:625`)
interrumpe al CPU sobre la **pila de boot del bootloader**, NO sobre el kstack
del proceso. Evidencia: `rsp=0x18000014df0` con `cs=0x8`, `rip=0x100001c8e6d`
(.text válido), `rflags=0x10206` — un frame legítimo cuyo `rsp` está
estrictamente por debajo de `physical_memory_offset` (≈0x2801e000000; el kstack
vive en ≥ ese offset). La pila de boot solo se usa en esa ventana
[`sti`, iretq del primer proceso]; después, todo ring-0 corre sobre kstacks de
proceso. No estaba documentado en ninguna parte. Implicación para cualquier
check de rangos: la banda `[0x100000, physical_memory_offset)` es legítima solo
para ese tick de boot; el check lo refleja.

## WATCHPOINT DE HARDWARE SOBRE EL CAMPO `rip` DEL BOX (en curso)

Condicional, para no ahogarse en los writes legítimos del SAVE:
`watch *(unsigned long*)0x2801f31ed78 if *(unsigned long*)0x2801f31ed78 > 0x2800000000`
(el campo `rip` del box de PID 1 está en `0x2801f31ed00 + 0x78` = `0x2801f31ed78`,
confirmado en el volcado en vivo; la condición solo para cuando se escribe un
valor de rango heap/kstack donde debe ir un `rip` de `.text` o usuario, ambos
< `0x2800000000`).

Predicción ANTES de mirar — caso 2: si dispara, lo más probable es que el
writer sea `switch_to_next` copiando un `current_tf` que apunta a datos (el box
recibe un rip basura del ISR); caso 1: un writer fuera del scheduler (overflow
de un objeto vecino); caso 3: nunca dispara y el box no es la víctima.

**RESULTADO: CASO 3 — el box NO es la víctima.** El watchpoint condicional
disparó 4 veces en un boot, y las 4 son el `memcpy` de `switch_to_next`
(scheduler.rs:992, el SAVE `*proc.trapframe = *current_tf`) copiando desde el
kstack, con `box.rip` SIEMPRE válido en `.text`
(`0x100001e56a7`, `0x1000020772a`, `0x1000024b4a0`), `box+0x70` (rax) = valor de
kstack. **Ningún writer externo/overflow toca el campo `rip` del box.**

**CORRECCIÓN DEL INSTRUMENTO (calibración, Prioridad 0)** — el watchpoint
condicional estaba ROTO por un umbral con un cero de menos: `0x2800000000`
(40 bits, = 1.7e11) es MENOR que el `.text` del kernel (`0x1000_0000_0000` =
1.1e12), así que la condición `> 0x2800000000` era verdadera para TODO rip de
kernel válido → el watchpoint era efectivamente incondicional y disparaba en
cada SAVE legítimo (verificado: `gdb p 0x100001e56a7 > 0x2800000000` = 1). La
frontera heap/código real es `physical_memory_offset` ≈ `0x2801_0000_0000`
(4.4e12); el umbral correcto es `0x280000000000` (44 bits).

**Calibración con el umbral correcto**: watchpoint INCONDICIONAL sobre
`box+0x78`, contando fires y valores ≥ `0x280000000000`:
- **80/80 fires**, **0 valores heap**. El boot avanzó ~168 iteraciones durante
  la ventana (los SAVE diag pasaron de 163 a 300 líneas). Todos los fires son
  el `memcpy` del SAVE con `box.rip` válido en `.text`.

**Conclusión (ahora sí calibrada)**: el campo `rip` del `Box<TrapFrame>` de
PID 1 se escribe SOLO por el SAVE de `switch_to_next`, siempre con valores
`.text` válidos, nunca con valores de heap. Combinado con el negativo de
`BAD RESUME FRAME` (box nunca corrupto en el resume), **el box queda
descartado como víctima** — con el instrumento verificado (no pierde escrituras:
80/80). El puntero de código que salta al heap vive en otro objeto.

Nota: la fase del watchpoint quedó SIN CONCLUSIÓN hasta esta calibración
(caveat 1 del doc: no aceptar veredictos de instrumentos sin calibrar). El
negativo del box es ahora sólido.

## CAVEAT DE MÉTODO PERMANENTE (cuarto instrumento defectuoso en esta caza)

> **El umbral de un watchpoint condicional debe validarse contra las fronteras
> reales de memoria en runtime; un umbral por debajo del `.text` convierte la
> condición en una tautología y el watchpoint en incondicional silencioso.**
>
> En esta caza: `0x2800000000` (40 bits) está por debajo del `.text` del kernel
> (`0x1000_0000_0000`), así que `box.rip > 0x2800000000` era verdadera para
> TODO rip de kernel válido. El watchpoint "condicional" filtraba cero; disparó
> en saves legítimos y la interpretación "caso 2/caso 3" fue durante una fase
> soportada por ese instrumento roto. Se corrigió validando el umbral contra
> `physical_memory_offset()` real (≈ `0x2801_0000_0000`) y recalibrando con
> watchpoint incondicional + conteo de fires (80/80). Regla para lo que queda:
> cualquier watchpoint condicional se calibra ANTES (control que demuestre que
> no pierde escrituras y que el umbral separa las regiones reales).

## TEST DE PROPIEDADES DEL SLAB EN `mm` (autorizado, sin QEMU) — 35/35

Añadidos a `mm/src/slab.rs` (solo tests, `#[cfg(test)]`; el kernel no los
compila), cerrando el hueco de que el buddy tiene su suite (`a0fb676`) y el
slab no:

1. **`double_free_is_detected_by_the_uaf_check_in_debug`** (`#[should_panic]`):
   un doble-free en debug NO da solapamiento silencioso — el check UAF ya
   corregido (`61409f7`) detecta la segunda entrega del mismo objeto como
   "Use-after-free detected" y paniquea.

   **DEGRADACIÓN DE LA CONCLUSIÓN (matiz del supervisor, correcto)**: esto NO
   descarta el doble-free. El check inspecciona el poison SOLO en el momento de
   `allocate`. Un doble-free cuyo objeto se **reasigna legítimamente entre las
   dos liberaciones** (y se sobrescribe con datos válidos) no deja poison que
   el check mire, y aun así deja el objeto **dos veces en la free list** → dos
   `allocate` posteriores devuelven la misma dirección → dos dueños vivos
   escribiendo el mismo objeto = **exactamente el síntoma perseguido** (puntero
   de un objeto vivo pisado por otro dueño) SIN panic de UAF. El test 2
   ("free list sin duplicados") solo vale bajo uso CORRECTO — la pregunta es si
   algún camino del kernel libera dos veces. → **doble-free: NO descartado; el
   check UAF no cubre el caso de reasignación-intermedia.**

   **PERO — test host directo del caso (añadido después del matiz)**: verifiqué
   la reasignación-intermedia en host
   (`double_free_with_reallocation_in_between_does_not_reuse_twice`, 36/36):
   en ESTA implementación de free list (`allocate` toma la cabeza, `deallocate`
   empuja), la reasignación intermedia **CONSUME la primera entrada** de la
   lista, así que la segunda liberación añade el objeto UNA sola vez → dos
   allocates posteriores devuelven objetos DISTINTOS, sin solapamiento. El
   matiz NO se aplica a esta implementación: el único modo de solapamiento es
   el doble-free consecutivo, que el check UAF SÍ detecta (2 tests
   `#[should_panic]`). El doble-free queda de nuevo descartado como causa del
   solapamiento en debug — salvo que un overflow corrompa los punteros `next`
   de la free list (vuelve a la hipótesis del overflow).
2. **`free_list_yields_unique_objects_under_correct_usage`**: la free list no
   entrega duplicados bajo uso correcto (ciclo alloc-all/free-all/re-alloc).
3. **`overflowing_one_object_lands_in_its_adjacent_live_neighbor`**: los
   objetos del slab son contiguos SIN redzone/canary; un overflow de un objeto
   pisa el vecino vivo — el modo de corrupción por overflow es mecánicamente
   POSIBLE (no descartado), aunque aún no probado en el kernel.

**Estado del candidato overflow**: sigue en pie como hipótesis (el test
demuestra que el slab no protege contra él), pero la prueba definitiva
(canario/redzone en `mm/`) está EN ESPERA de decisión (cambia el allocator y
perturba el timing del flake).

## HALLAZGO SECUNDARIO — el sistema de build NO vigila `mm/`/`hal/`/`ext2/`

`build.rs` (raíz) `build_kernel()` vigila `kernel/src`, `userspace/*`, `mlibc-port`,
`doom-port`, `quake-port`, `scripts`, `busybox-config`, `kernel/Cargo.toml`,
`kernel/.cargo/config.toml`, `kernel/build.rs` — **pero NO `mm/`, `hal/` ni
`ext2/`** (las crates extraídas, dependencias `path` del kernel). Editar
`mm/src/slab.rs` NO dispara el build script raíz → el build anidado (que sí
recompilaría mm) nunca corre → el kernel ELF queda stale. Verificado: el
binario quedó en 03:59 tras editar mm a las 04:59; `touch mm/*` y `cargo clean
-p mm` no lo arreglaron (bug de huella de cargo con `-Z build-std`); y `cargo
build --target x86_64-unknown-none` directo desde `kernel/` va al target del
WORKSPACE RAÍZ (kernel es miembro de `members=["kernel"]`), no a
`kernel/target` — el `build.rs` usa `--target-dir` explícito precisamente por
eso. **Las mediciones del canario hasta el clean completo + build con
`--target-dir kernel/target` usaron el kernel SIN canario (inválidas).** Para
cualquier cambio futuro en `mm`/`hal`/`ext2`: `touch build.rs` (raíz) o el
build anidado con el `--target-dir` correcto. Esto también es un bug de build
real (afecta a quien edite las crates extraídas).

## CANARIO/REDZONE DEL SLAB — DISEÑO E IMPLEMENTACIÓN (autorizado)

`mm/src/slab.rs`, `#[cfg(debug_assertions)]` (release intacto: ni layout ni
camino de código cambian). Diseño:
- Cada slot de objeto mide **2*object_size** en debug: región de datos
  `[slot, slot+obj)` + redzone final de `object_size` bytes
  `[slot+obj, slot+2obj)`. El doblado preserva la alineación (slot múltiplo de
  object_size; una redzone de tamaño fijo rompería la alineación de las cachés
  power-of-2).
- **Patrón `0xC5`**: distinto del `0xAA` (poison de allocated, causó falsos
  positivos históricos) y del `0xDD` (poison de freed, invariante del UAF).
- Checks en `allocate` (objeto libre con redzone rota) y en `deallocate` (el
  caller desbordó su región de datos). Panic inmediato
  `"SLAB REDZONE BROKEN: obj=… cache=… side=trailing offset=… found=… expected=…"`.
- **ADVERTENCIA DE LAYOUT**: el doblado de slots cambia TODAS las direcciones
  del heap en debug (aunque en la práctica las direcciones clave observadas —
  box 0x2801f31ed00, BTreeMap 0x2801fec5f58, nodo 0x2801e9cd000 — coincidieron
  porque son slot-0 de las mismas páginas del buddy). **No comparar con
  volcados anteriores sin recalcular** — sería el quinto instrumento mentiroso.
- Tests host (37/37): `redzone_detects_overflow_past_object_end`
  (`#[should_panic]`), `redzone_detects_overflow_into_free_object_neighbour`
  (`#[should_panic]`), doble-free con/sin reasignación, free-list única, etc.
  Instrumento calibrado en host ANTES de medir en QEMU.

## CANARIO/REDZONE DEL SLAB — MEDICIÓN Y RESULTADO

**Kernel con canario** (verificado: "SLAB REDZONE BROKEN" presente en el ELF;
layout cambiado — el box pasó de 0x2801f31ed00 a 0x2801f31ea00, el kstack
intacto 0x2801fea0000). `run-kernel-tests.sh` 3/3 antes de medir.

**`boot-matrix.sh 6 2 --timeout 300`: 7/12 fallos (58.3%)** — todos HANG
(amplificador llegando a iter ~1120-1291, sin pánico). La tasa NO es
comparable con el 2/24 del kernel anterior (el canario cambió el layout del
heap → perturbación, caveat 2); el dato es la tasa nueva, no su magnitud.

**EL CANARIO NO DISPARÓ EN NINGÚN BOOT** (0 "SLAB REDZONE BROKEN" en los 7
HANG). Con el amplificador haciendo miles de alloc/free (Strings, nodos
BTreeMap, Vecs) por boot y el canario comprobando cada alloc/free, ningún
objeto del slab desbordó su región de datos.

**Refutación (quinto negativo bien medido)**: la hipótesis de overflow de un
objeto del slab hacia su vecino queda **REFUTADA** para los objetos que se
asan­llan/liberan (los que churn): el canario los cubre y no disparó. Alcance
residual (límite del instrumento): dos objetos persistentes que nunca se
liberan (box, RamDirNode, Process, nodo raíz del BTreeMap) desbordándose entre
sí no serían vistos — pero el box está limpio (watchpoint 80/80) y el BTreeMap
coherente (volcados). El puntero corrupto que salta al heap no proviene de un
desbordamiento de objeto del slab observable.

Nota de medición (correcta): la primera tanda tras el canario (24 boots, todos
"serial 0 bytes HANG") usó el kernel SIN canario — el sistema de build no
recompiló mm (ver hallazgo del build.rs arriba). La medición válida es la de
arriba (7/12, canario confirmado en el ELF y por el cambio de layout).

## CUARENTENA (DELAYED REUSE) — DISEÑO, CALIBRACIÓN Y RESULTADO

Discriminador del punto ciego del canario (UAF con reasignación intermedia).
`mm/src/slab.rs`, `#[cfg(debug_assertions)]` (release: reutilización inmediata,
sin struct ni camino de código). Diseño:
- Al liberar, el objeto **no vuelve a la free list**: entra en una FIFO de
  cuarentena por caché (`QUARANTINE_CAP = 128`). Solo se reutiliza tras 128
  frees más (o cuando la free list se agota y se drena uno).
- En cuarentena conserva el poison `0xDD` COMPLETO; al drenar se **verifica
  entero** (bytes 0..min(size,256) = 0xDD). Roto → panic
  `"SLAB QUARANTINE VIOLATION: obj=… cache=… offset=… found=… bytes=…"` con
  volcado de los primeros bytes (huella del escritor obsoleto).
- El doble-free también queda detectado (la copia duplicada se drena con 0xAA
  si se asignó en medio).

Calibración host: **40/40** (incluye `quarantine_detects_stale_owner_write_into_
freed_object` `#[should_panic]`, `quarantine_delays_reuse_until_drain`,
`quarantine_reuses_drained_object_after_churn`, y los tests de doble-free/
redzone reescritos para drenar la cuarentena). `run-kernel-tests.sh` 3/3.
Kernel con cuarentena confirmado en el ELF ("SLAB QUARANTINE VIOLATION").

**Medición `boot-matrix.sh 6 2 --timeout 300`: 6/12 fallos (50%)** — 5 HANG +
1 PANIC (el pánico con la firma clásica: PAGE FAULT error 0x11, Address =
0x2801fec5e18 = el BTreeMap con el layout desplazado, RIP = el nodo raíz).
**La cuarentena NO disparó** (0 "QUARANTINE VIOLATION", 0 "REDZONE BROKEN" en
los 6 fallos). La tasa apenas cambió (canario-solo: 7/12; cuarentena: 6/12) —
la comparación está contaminada por el cambio de layout (el box se movió de
0x2801f31ea00 a 0x2801f31e600), pero el dato es que el bug persiste con la
cuarentena activa.

**Interpretación (sexto negativo, parcial)**: no hubo escritura de un dueño
obsoleto DENTRO de la ventana de cuarentena (128 frees), y el retraso de la
reutilización no hizo caer la tasa. Ambos argumentan contra UAF-por-reciclado
como mecanismo — con un límite honesto: la cuarentena no cubre la escritura de
un dueño obsoleto DESPUÉS de que el objeto se reutilice (el objeto ya es de
otro dueño vivo; no hay poison que mirar). Ese caso "dos dueños vivos" solo lo
cubriría un mecanismo que marque el objeto como de otro dueño (fuera de
alcance de un canario/cuarentena).

**Estado acumulado**: 6 negativos bien medidos — (1) resume, (2) current_tf,
(3) syscall-exit, (4) box-rip (80/80), (5) canario (0 fires), (6) cuarentena
(0 violations, tasa estable). No es overflow de slab observable, no es UAF con
escritura en freed-dentro-de-la-ventana, no es el box, no es el resume. El
mecanismo del salto al heap sigue sin cerrar.

## FASE "¿QUIÉN HIZO LA LLAMADA?" — la pregunta del puntero gordo (en curso)

**La observación del supervisor que reencuadra**: CR2 = `&self.entries`
(0x2801fec5f58 / 0x2801fec5e18 según layout) + error 0x11 (instruction fetch)
= la CPU intentó **ejecutar en la dirección del campo**, la firma de una
llamada indirecta cuyo destino debía ser código y contiene la dirección de ese
campo → vtable corrupta o puntero gordo `Arc<dyn Inode>` con su campo de
metadatos pisado. Y los 8 bytes entre el SpinMutex (0x2801fec5f50) y el map
(0x2801fec5f58) = exactamente el desplazamiento (data, vtable) de un puntero
gordo leído desde la dirección desplazada.

**Análisis (sin gdb en vivo, consistente con la hipótesis)**:
- El RIP del pánico (0x2801e9cd000 / 0x2800080a000) = el `word0` del map = el
  puntero al NODO RAÍZ — que es también la dirección REAL a la que saltó la
  ejecución corrupta (el nodo raíz se EJECUTA; el fault 0x11 es en el map, la
  siguiente "instrucción" del nodo = un puntero al map interpretado como
  destino de salto). Un puntero gordo leído desde la zona del map tomaría como
  vtable el `word0` (el nodo raíz) → salto al nodo → ejecutar datos del nodo
  como código.
- Las SAVE más frecuentes de los HANG/PANIC resuelven a `InterruptGuard::drop`
  (0x1e5967), `SlabGlobalAlloc::alloc` (0x1f4efd) y `dealloc` (0x1f4f6c) — el
  timer interrumpe sobre todo en el epílogo de syscall y el allocator; no
  revelan el call site.

**Fase 1 (captura viva del return address) — bloqueada por la tasa**: el
breakpoint en `page_fault_handler` con condición `error == 0x11` (el error está
en `*($rsp)` en la entrada del shim) quedó armado y verificado, pero el PANIC
es ~1/12 con el kernel de cuarentena y 16 intentos gdb no lo cazaron (la
mayoría de fallos son HANG, que NO producen el PF 0x11). El breakpoint de
EJECUCIÓN en el nodo raíz (que cazaría el HANG y el PANIC, ~50%) es ~10x más
lento en TCG (iter ~184 tras 300s) — impráctico en un turno. La captura del
return address del `call` fallido requiere: (a) un kernel con mayor tasa de
PANIC (revertir canario/cuarentena sube los panics — decisión de código), o
(b) una campaña gdb larga, o (c) instrumentar el salto al nodo raíz (breakpoint
de hardware en 0x2801e9cd000-like en un boot dado) con paciencia.

**Estado**: la hipótesis del puntero gordo corrupto (vtable pisada con un
puntero a datos del map) es ahora la **más consistente** con toda la evidencia
(canario limpio, cuarentena limpia, box limpio, salto al nodo raíz = word0 del
map, sensibilidad al layout). El call site exacto sigue pendiente de la captura
viva.

## CANARIO/CUARENTENA — APAGADOS POR DEFAULT (feature `slab-debug`, off)

Para recuperar el layout de referencia y la tasa de PANIC (~3/24) y capturar el
call site con KVM a toda velocidad (sin gdb/TCG), el canario y la cuarentena
quedan **desactivados por default** vía la feature `slab-debug` en `mm`
 (default off). El código y los tests se quedan: `cd mm && cargo test` (sin
 feature) = 33 tests; `cargo test --features slab-debug` = 44 (incluye redzone +
 cuarentena). Para reactivar: `--features slab-debug` en el build del kernel
(DEBUG only). El kernel actual se construye SIN la feature → slot de referencia,
direcciones pre-instrumentación.

## FASE "¿QUIÉN HIZO LA LLAMADA?" — INSTRUMENTACIÓN EN KERNEL (declarada ANTES)

`page_fault_handler` (devices.rs): cuando `error_code == 0x11` (present +
instruction fetch = la llamada indirecta corrupta), imprime en el MISMO volcado
del pánico:
- `ret_addr = [sf.stack_pointer]` — la palabra superior de la pila del fault =
  la dirección de retorno que empujó el `call` fallido. **El dato.**
- `interrupted_rip` (sf.instruction_pointer) e `interrupted_rsp`.
- `shim rdi/rsi/rax` (lectura de los slots que el shim x86-interrupt empujó al
  guardarlos, offsets derivados del disassembly: rdi@sf-56, rsi@sf-64,
  rax@sf-16) — en una llamada virtual de Rust, rdi = self.
- CR2 y error, para correlacionar.

Coste en el camino normal: **una comparación** (`error_code == 0x11`); cero
salidas. Aditivo, no toca el flujo del handler.

**EXPECTATIVA DECLARADA (antes de correr)**: `ret_addr` resolverá (tras restar
el `virtual_address_offset` 0x10000000000) a un call site en la zona de manejo
de `Arc<dyn Inode>`/vtable del VFS (p.ej. `vfs.rs`/`ramfs.rs` — una llamada
`node.clone()`/`node.file_type()`/`entry.insert`); `rdi` será un puntero de
datos coherente (un `RamDirNode`/`Arc<dyn Inode>`) con la vtable rota. Si
`ret_addr` no resuelve a nada sensato → fue un `jmp`/`ret` (no un `call`), lo
que descartaría la llamada virtual y apuntaría a un return-address corrupto en
la pila.

## RESULTADO DEL VOLCADO [BUG2] (KVM a toda velocidad, sin gdb) — MECANISMO CONFIRMADO

Con el canario/cuarentena APAGADOS (layout de referencia) y el volcado en
`page_fault_handler`, el pánico imprime (boot-matrix 6 4, 7/24 con 3 PANIC):

```
=== [BUG2] CORRUPT INDIRECT CALL ===
  CR2=0x2801fec5f58 err=0x11
  sf: rip=0x2801f8ec400 cs=0x2 rflags=0x98 rsp=0x1 ss=0x100000001ed
  guarded ret_addr=[X]=0xdeadbeafdeadbeaf ret2=0x0
  shim: rdi=0x0 rsi=0x0 rax=0x1
```

**Interpretación** (comparada con la expectativa declarada):
- El `sf` lee las palabras del MAP (coherentes, no pisadas por el marco):
  `word0=0x2801f8ec400` (nodo raíz), `word1=0x2` (height), `word2=0x98`
  (length), `word3=0x1`, `word4=0x100000001ed`. → el marco del fault NO es
  legible (el handler lo lee desde la dirección del mapa), igual que antes.
- `sf.rip` (= word0) = **el NODO RAÍZ** → el salto corrupto fue AL nodo raíz
  (ejecutable), y su "código" (datos del nodo) hizo instruction-fetch en el
  map (NX) → el fault 0x11 en CR2 = el map. **Consistente con la hipótesis del
  puntero gordo 8-bytes del supervisor**: un puntero gordo leído desde el base
  del `entries` (0x2801fec5f50) tomaría como vtable el `word0` del map (el
  nodo raíz) → `call [vtable]` = salto al nodo → ejecutar datos del nodo como
  código.
- **`ret_addr` NO es recuperable vía sf** (guarded read saltado: `sf.rsp=0x1`
  basura): el marco legible es el mapa, no el estado real de la CPU. Esto es el
  **caso fallback de la nota**: el transfer no fue un `call` con return address
  en un stack recuperable — la ejecución corrupta corre datos del nodo como
  código tras el salto por el puntero gordo (no hay un call site .text claro en
  la pila legible).
- `shim rdi/rsi/rax = 0/0/1` — lectura de los slots del shim; offsets frágiles
  (posiblemente leen campos del RamDirNode, no los regs guardados). No
  concluyentes.

**Conclusión de esta fase**: la hipótesis del puntero gordo corrupto queda
**CONFIRMADA por el propio kernel** (el salto al nodo raíz = word0 del map, el
fault fetch en el map, el mapa coherente). El call site exacto (qué código
construyó/leyó el puntero gordo desde `&self.entries`) NO se obtiene del return
address (no recuperable vía sf). Zona siguiente: auditar dónde se puede leer un
puntero gordo desde la dirección de `entries` (0x2801fec5f50) — p.ej. un
`&mut BTreeMap` tratado como `&dyn Inode`, o un `Arc::from_raw` con offset
desplazado 8 bytes — con el mapa coherente y el box limpio, el candidato es un
uso-después de un `Arc<dyn Inode>` cuya vtable quedó apuntando al mapa.

## DESCARTE (verificado por el supervisor): `TMP_ROOT_FOR_PANIC` NO es la fuente

`TMP_ROOT_FOR_PANIC` (ramfs.rs:184) es `spin::Once<Arc<RamDirNode>>` — tipo
CONCRETO, no `dyn` — no construye ningún puntero gordo, y solo se lee desde
`dump_tmp_root_entries_layout_for_panic()` (post-mortem, desde el handler de
pánico, con `force_unlock`). Descartado. Nota: el comentario de esa función
afirma layout `[height, node, length]`, pero los volcados (doc y propios)
muestran `word0 = puntero al nodo`, `word1 = 3 (height)` — **el orden empírico
es `[node, height, length]`**; fiarse del dato, no del comentario.

## AUDITORÍA DE UNSAFE SOBRE `dyn` (declarada ANTES de auditar)

En Rust seguro es imposible que la vtable de un `Arc<dyn Inode>` acabe valiendo
`&self.entries` — el puntero gordo se mueve siempre como valor completo. Para
que la vtable sea un puntero a datos hace falta `unsafe`. Patrones a buscar en
`kernel/src/fs/` (ramfs, vfs, mod, devfs, initramfs, procfs, ext2) y
`kernel/src/process/file.rs`:
- `transmute` sobre `&dyn`/`*const dyn`.
- `Arc::from_raw`/`Arc::into_raw`/`Box::from_raw`/`Rc::from_raw` (un `into_raw`
  sin su `from_raw`, o con el tipo cambiado).
- casts de puntero crudo a objeto trait: `as *const dyn …`, `&*(p as *const dyn …)`.
- `core::ptr::read`/`read_unaligned`/`copy_nonoverlapping` sobre algo con `dyn`.
- `MaybeUninit`/`mem::zeroed`/`assume_init` sobre tipos con `Arc<dyn Inode>`.
- `static mut`/`Once` que guarde un `Arc<dyn Inode>` reinicializado.

**EXPECTATIVA DECLARADA**: encuentro un `unsafe` en fs/ o file.rs — el candidato
más probable es un `Arc<dyn Inode>` con refcount desequilibrado (`into_raw` sin
`from_raw`, o un `decrement_strong_count` de más) → el objeto se libera con
alguien conservando el puntero gordo → el slab recicla la memoria para el
RamDirNode/map → la "vtable" pasa a ser un puntero a datos (UAF de dos dueños
vivos, el caso que canario/cuarentena no cubren). Alternativa: un `&mut` a la
vez que un `&` sobre el mismo BTreeMap (aliasing UB). Si el grep sale limpio →
el puntero gordo se corrompe desde FUERA del VFS (escritura salvaje de otro
subsistema), y eso reorienta la caza.

## RESULTADO DE LA AUDITORÍA — LIMPIA (negativo)

**El grep en `kernel/src/fs/` completo (ramfs, vfs, mod, devfs, initramfs,
procfs, ext2) y `kernel/src/process/file.rs` sale LIMPIO.** Los únicos `unsafe`
en fs/ son los 2 de `dump_tmp_root_entries_layout_for_panic` (ramfs.rs:221-234:
`force_unlock` + `read_unaligned` de u64 crudos — diagnóstico post-mortem del
pánico, no construyen punteros gordos). Y en todo el kernel:
- cero `transmute` (solo menciones en comentarios);
- cero `Arc::from_raw`/`Arc::into_raw`/`Box::from_raw`/`Rc::from_raw`;
- cero `decrement_strong_count`/`increment_strong_count`;
- cero casts `as *const dyn`/`as *mut dyn`;
- cero `MaybeUninit`/`mem::zeroed`/`assume_init` sobre tipos con `Arc<dyn Inode>`;
- todos los `from_raw_parts` son buffers de bytes/words (`*const u8`/`*const u32`),
  no fat pointers;
- `Inode::as_any()` usa upcast seguro (`self` → `&dyn Any`), no reconstrucción.

**Consecuencia (reorienta la caza)**: por el razonamiento del supervisor (en
Rust seguro el puntero gordo se mueve como valor completo y el compilador nunca
lo reconstruye), **ningún código del kernel (fs/ ni otro) puede hacer que la
vtable de un `Arc<dyn Inode>` acabe valiendo `&self.entries`** — no hay `unsafe`
que lo produzca. Las dos formas concretas del supervisor (refcount
desequilibrado / `&mut`+`&` alias) requieren `unsafe` y NO están presentes.
**El campo vtable se corrompe por una ESCRITURA SALVAJE desde fuera del VFS**:
un write no consciente de fat pointers que aterriza sobre la memoria donde vive
la vtable de un `Arc<dyn Inode>` vivo. Eso descarta los UAF/alias dentro del
VFS y reorienta hacia: (a) un overflow/uso-después de una asignación GRANDE
(direct-buddy, >2048 bytes — el canario/cuarentena del slab no la cubren), o
(b) un UAF donde el write obsoleto aterriza en un objeto VIVO reciclado (invisible
a la cuarentena, que solo mira freed-dentro-de-la-ventana). Ambos son
consistentes con los 6 negativos y con la sensibilidad al layout.

## FASE "PHANTOM DEL BUDDY" — NEGATIVO LIMPIO (declarado y verificado)

El hilo del doc: los `PhantomEvent` de producción (bitmap dice bloque libre,
free list intrusiva no lo contiene) los provoca algo externo, y el buddy quedó
limpio en aislamiento. Si un phantom precediera al fallo, sería el evento
corruptor ya en disco.

**Expectativa declarada**: phantoms en arranques fallidos, precediendo al
síntoma. **Resultado**: **CERO `[BUDDY] phantom` en TODOS los serial.logs
preservados** (docenas de arranques, sanos y fallidos, PANIC y HANG, de todas
las tandas del boot-matrix). Y cero `[BUDDY]` en absoluto (ni siquiera los
no-phantom): el adaptador no imprimió nada del buddy. El amplificador SÍ ejerce
`allocate_large` (direct-buddy, p.ej. `order 14`, OK at 0x2801fef0000), así que
la ruta grande está activa y no produjo phantoms.

**Interpretación**: los metadatos del buddy (bitmap + free list intrusiva) NO
se corrompen de forma observable durante el repro del amplificador. La
escritura salvaje que pisa la vtable NO aterriza en el bookkeeping del
allocator. (Matiz: un doble-reparto donde ambos dueños siguen VIVOS no produce
phantom hasta que uno libera — la ausencia de phantoms no lo descarta del todo,
pero sí descarta que la corrupción toque el bitmap/free list del buddy de la
manera observable.) Esta vía queda descartada para el repro; los candidatos (a)
overflow/UAF de `allocate_large` (sin canario) y (b) UAF con write sobre objeto
vivo reciclado siguen en pie.

## FASE "¿CUÁL ES LA ENTRADA CORRUPTA?" — recorrido del BTreeMap (declarado)

Los `Arc<dyn Inode>` viven dentro de los nodos del BTreeMap; la entrada rota
tiene una clave (nombre con la iteración). Extiendo
`dump_tmp_root_entries_layout_for_panic` para caminar el árbol desde `word0`
(nodo raíz) con `word1` (altura) y buscar el valor cuya vtable sea un puntero a
datos (≥ `physical_memory_offset`, .rodata es 0x10000… — no hace falta capturar
vtables legítimas). Layout de esta toolchain (alloc/btree/node.rs): `B=6,
CAPACITY=11`; `LeafNode<K,V> = {parent@0, parent_idx@8, len@10, keys@16
(11×24B String), vals@280 (11×16B Arc<dyn Inode>)}`, nodos internos añaden
`edges@456` (12 punteros). String = {ptr@0, cap@8, len@16}.

**EXPECTATIVA DECLARADA**: (a) una entrada corrupta con clave legible →
víctima identificada con su iteración de creación (enorme); (b) varias
corruptas contiguas → escritura de un bloque (memcpy/overflow), no un puntero
suelto; (c) ninguna corrupta pero el salto ocurre igual → el puntero gordo roto
NO está en el mapa (copia temporal en pila/registro) — octavo negativo, y
reorienta. Cualquier inconsistencia estructural aborta el volcado (prudencia:
layout no especificado; se calibrará contra un volcado en vivo de un nodo).

## RESULTADO DEL RECORRIDO — ABORTA (layout del nodo no calibrado)

Implementado el recorrido del BTreeMap en `dump_tmp_root_entries_layout_for_panic`
(guards: len ≤ 11, nodos en rango heap, altura ≤ 12, abort en cualquier
inconsistencia). En el primer pánico capturado:

```
[tmp_root_diag] walk: nodes=1 vals=0 corrupt=0 root=0x2801e9cd000 height=3 aborted=true
```

El walk **abortó por diseño** en el primer nodo: `len @ node+10` leyó `0x1e9c`
(basura), no el `len` del nodo. El header asumido
`LeafNode = {parent@0, parent_idx@8, len@10, keys@16, vals@280}` (del
`node.rs` de esta toolchain, orden de declaración) NO corresponde al layout
real — `repr(Rust)` permite a rustc reordenar campos, y la evidencia de un
volcado en vivo (`node+0 = 0x1`, `node+8 = 0x1e9cc000`) no encaja con ese
orden. La calibración empírica (volcar el nodo raíz real, localizar los bytes
de una clave `/tmp/hhN`, derivar los offsets de keys/vals/len) quedó pendiente:
el attach de gdb en este entorno se volvió intermitente (salidas vacías), y el
nodo raíz cambia de dirección entre boots (0x2801e9cd000 vs 0x2801f8ec400).

**Estado**: el recorrido está en el árbol (aditivo, con guards, aborta antes de
leer a ciegas — cumple la prudencia del supervisor), pero **no produjo la
entrada corrupta** porque el layout no está calibrado. Nota: la pregunta sigue
abierta; la vía de respuesta (identificar la víctima por su clave/iteración)
requiere primero fijar el layout real del `LeafNode` de esta toolchain (p.ej.
un test host con `offset_of!` en el toolchain, o un dump en vivo estable del
nodo). El volcado [BUG2] (mecanismo confirmado) y el walk (abortado, guardado)
quedan como instrumentos permanentes en el árbol.

## PRIORIDAD 1 — PRIMER POSITIVO: `BAD RESUME FRAME` DISPARÓ (fase busybox)

**CORRECCIÓN EXPLÍCITA de la conclusión anterior**: en la fase "FASE C+
VALIDACIÓN DEL FRAME EN EL RESUME" concluí (con el kernel de canario/
cuarentena) que "el frame del resume es SIEMPRE válido" — con 16 intentos gdb
y el check sin disparar en el amplificador. **Esa conclusión queda corregida**:
el check `BAD RESUME FRAME` (timer_preempt.rs) DISPARÓ, en un boot donde el
amplificador COMPLETÓ las 1500 iteraciones (sano) y el pánico llegó en la fase
`busybox --install` (`forks_total: 1, execs_total: 1`).

```
=== KERNEL PANIC ===
  at kernel/src/process/timer_preempt.rs:139:9
  HANGHUNT: corrupt resume frame (bad iretq target)
  forks_total: 1  execs_total: 1  cow_faults_resolved: 1
```

El volcado completo (rip/cs/rflags/rsp/ss, tf, kstack, pid, ramfs_entries) de
ESE disparo se perdió (el estado dir se recreó con el siguiente boot). La
reproducción para recuperarlo y medir la frecuencia está en curso. Datos que
ya aporta el disparo:
- Ocurre en la fase **busybox --install** (fork+exec), NO en el amplificador —
  la firma histórica del doc (`busybox_install_fork_flake`). Puede ser el MISMO
  bug o uno distinto del del amplificador; se comparará cuando se tenga el
  frame completo.
- El check valida: cs∈{0x8,0x23} con ss coherente, rip no-heap/no-nulo,
  rsp en kstack/usuario, rflags bit1. Falta saber cuál falló y si el rip
  corrupto vuelve a ser la zona del BTreeMap.
- Frecuencia y camino (switch/no-switch) pendientes de la reproducción.

Implicación: hay un camino más corto al seed (forks=1, tras el amplificador
sano) que no requiere 800+ iteraciones del amplificador.

**Reproducción (volcados preservados en /tmp/badresume-<n>-<outcome>.log)**: 7
boots — 5 colgados en el amplificador (iter ~810-822, la manifestación
orphan-lock; bloquean llegar a busybox), 2 sanos (amplificador completo + ash),
**0 disparos BADRESUME** hasta ahora. El disparo es raro; la campaña continúa
en lotes cortos con volcados duraderos.

## EL DISPARO — volcado completo y respuestas (boot 12, /tmp/badresume-12-BADRESUME.log)

Contexto crudo:
```
💀 Killed PID 2 (child): exit(0)
  → Process exited, switching immediately (full TrapFrame restore)
=== HANGHUNT BAD RESUME FRAME ===
  tf=0x2801fe7ee40 kstack=[0x2801fe90000,0x2801fea0000)
  cs=0x8 ss=0x10 rip=0x10000278377 rflags=0x10202 rsp=0x2801fe7eee8
  pid=1
  ramfs_entries: acquires=3208 releases=3208 outstanding=0 last_acquirer pid=2 op=symlink
  forks_total: 1 execs_total: 1
```

**1. Frame completo y validación fallida**: `cs=0x8` (kernel, válido),
`ss=0x10` (válido), `rip=0x10000278377` (= `InterruptGuard::drop`,
irq_guard.rs:46 — `.text` VÁLIDO), `rflags=0x10202` (bit1 ok). **Falló la
validación de `rsp`**: `rsp=0x2801fe7eee8 < kstack_lo=0x2801fe90000` — el RSP
interrumpido está **0x1118 bytes por debajo del kstack de PID 1** (y por debajo
también del kstack de PID 2, 0x2801fe80000). PID 1 ejecutaba el epílogo de un
syscall (`InterruptGuard::drop`) **con un RSP basura**.

**2. El `rip` NO es heap**: es `.text` (InterruptGuard::drop). La corrupción de
ESTE disparo es el **RSP**, no el RIP — distinto del amplificador (donde el RIP
era el nodo raíz del BTreeMap).

**3. Camino no-switch**: `tf=0x2801fe7ee40` es el frame del stack (168 bytes
por encima del rsp), no un box (los boxes están en ~0x2801f3…). Un tick sin
cambio de proceso. `pid=1` (shell).

**4. Firma distinta a la del amplificador, misma familia**: el amplificador =
lock huérfano / heap-jump (RIP = heap). Este = **RSP corrupto** (debajo del
kstack), RIP válido, fase busybox (`forks=1, execs=1`) — la firma histórica del
doc (`busybox_install_fork_flake`). Ambas son corrupción de estado de proceso
(trapframe/RSP vs vtable); mecanismo subyacente probablemente el mismo
(escritura salvaje), manifestación distinta. `outstanding=0` (lock ramfs
balanceado, no huérfano aquí).

**5. Frecuencia**: 1/14 boots (~7%).

**CORRECCIÓN ADICIONAL (importante)**: este disparo prueba que el **`rsp` del
trapframe puede estar corrupto** (0x2801fe7eee8 = debajo del kstack) — la
reanudación previa de PID 1 restauró ese RSP desde su box. Mi conclusión
anterior "el box nunca se corrompe" se basó en el watchpoint 80/80 del campo
`rip` del box (offset 120) — **solo cubría el RIP, no el `rsp` (offset 144)**.
La corrupción está en el campo `rsp`. Nuevo candidato a vigilar: el campo
`rsp` del `Box<TrapFrame>` (offset 144), y el propio RSP que el proceso usa
entre reanudaciones.

## CIERRE — la cadena del kstack liberado-diferido (verificada, con código)

La hipótesis externa: el kstack de PID 2 (liberado de forma diferida por
`pending_stack_frees`) seguía en uso cuando el tick lo devolvió al buddy.
**Verificación completa (boot 12):**

1. **La dirección cae dentro del kstack de PID 2, CONFIRMADO**: el SAVE diag de
   pid=2 muestra `kstack_top=0x2801fe80000` (bloque `[0x2801fe70000,
   0x2801fe80000)`), y `tf=0x2801fe7ee40` / `rsp=0x2801fe7eee8` caen dentro
   (0xee40 = 61024 bytes adentro). No es de otro proceso, ni heap, ni hueco.

2. **El free SÍ ocurrió en el mismo tick que disparó la validación**: en
   `timer_preempt_handler`, `scheduler.tick()` (scheduler.rs:892, el
   `pending_stack_frees.retain`) corre **antes** de la validación
   (timer_preempt.rs:196) — así que cuando la validación paniceó, el kstack de
   PID 2 ya estaba de vuelta en el buddy. (`try_free_kernel_stack` libera en
   silencio — process/init/processes.rs:134 — deallocate sin print.)

3. **El mecanismo NO es RSP0 rancio ni un tf guardado — es la ventana entre la
   decisión de conmutar y el cambio real de pila**:
   - `sys_exit` (process_ctl.rs:189) → `kill_and_switch_tf`: encola el kstack
     de PID 2, actualiza `current`/`running` a PID 1, `set_kernel_stack(PID1)`,
     devuelve el box de PID 1 — pero el epílogo sigue **en el kstack de PID 2**.
   - `drop(irq)` (process_ctl.rs:208) hace `sti` → **IF=1 reabierto antes del
     `jump_to_user`** (línea 220, el `mov rsp, rdi` + iretq). Ventana real: CPU
     en el kstack de PID 2, current=PID 1, kstack de PID 2 encolado para free.
   - El tick que cae en la ventana: empuja su marco en 0x2801fe7ee40 (kstack
     de PID 2) y `tick()` libera ese kstack mientras la CPU sigue encima.
   - **El comentario de scheduler.rs:881-885 es FALSO**: "interrupts are off
     from kill_current through that iretq, so no tick can land in between".
     `drop(irq)`/`sti` reabren las interrupciones ANTES del iretq de
     `jump_to_user` — hay una ventana de varios instintos entre `kill_current`
     y el iretq donde un tick sí puede caer.

4. **La corrupción aguas abajo** (si el tick de la ventana toma el camino SWITCH
   en vez del no-switch): `switch_to_next` guarda `*proc.trapframe =
   *current_tf` con `proc` = PID 1 (current) y `current_tf` = el marco sobre el
   kstack de PID 2 → **el box de PID 1 queda guardado con `rsp` = el kstack
   liberado**. La siguiente reanudación de PID 1 restaura RSP sobre esa página
   (ya reciclada como heap/nodo BTreeMap) → pops corruptos → iretq a basura →
   salto al heap / lock huérfano / "el marco no apunta a la pila". Explica de
   una vez: la manifestación 2 (page fault tras "Process exited"), la
   correspondencia con el BTreeMap (la página reciclada ES el nodo), y por qué
   canario/cuarentena/box salieron limpios (la víctima no es un objeto del
   heap: es una pila liberada reciclada como heap).

**MATIZ (no confirmado por esta cadena)**: el orphan-lock del amplificador corre
ANTES de cualquier muerte (shell.rs:110: `hang_hunt()` primero; solo idle +
shell existen al arrancar) — no puede ser esta ventana. El colgado del
amplificador (boot 8, iter 825) termina con el último SAVE en
`InterruptGuard::drop` y luego silencio total (sin más SAVEs ⇒ spin con IF=0,
el timer no puede disparar): el syscall interrumpido mantenía el lock del
scheduler con IF=1 (violación del invariante "cli antes de SCHEDULER") y el
ISR se auto-atranca. Causa raíz distinta o aguas abajo de una corrupción
anterior; requiere su propia investigación.

## Fix propuesto (pendiente de autorización — toca WIP)

1. **Cerrar la ventana (raíz)**: en `sys_exit` (process_ctl.rs:208-220), no
   reabrir IF antes del cambio real de pila — el `drop(irq)` hace `sti` antes
   de `jump_to_user`. Mantener IF=0 hasta el iretq de `jump_to_user` (que
   restaura RFLAGS.IF del destino); el `sti` explícito de la línea 219 es
   redundante por comentario propio.
2. **Hacer el free defendible (red)**: en `timer_preempt_handler` (autorizado),
   antes de `tick()`, comprobar si `current_tf`'s rsp está fuera del kstack del
   proceso actual y en ese caso **aplazar** el free (retener la entrada) o
   conmutar inmediatamente — convierte el supuesto falso del comentario en un
   invariante comprobado.
3. Nota de método (quinto aprendizaje, pedido por el supervisor): *"un
   instrumento calibrado sigue siendo ciego fuera de su cobertura; declarar
   siempre qué campo/rango cubre"* — el watchpoint 80/80 solo cubría el campo
   `rip` (offset 120) del box, no el `rsp` (offset 144), que es el campo
   corrupto aquí.

## FIX APLICADO (2026-08-05, autorizado) + diff exacto + mediciones

**Fix 1 — raíz (`kernel/src/process/syscall/process_ctl.rs`, `sys_exit`)**:
eliminados `drop(irq)` y el `sti` explícito; `cancel_all_waiters` + `jump_to_user`
ahora corren con IF=0. El `_irq` guard nunca hace `sti` (la función diverge en
`jump_to_user`, cuyo iretq restaura RFLAGS.IF del destino). `cancel_all_waiters`
verificado seguro con IF=0: los tres cleanups son secciones críticas de
spin-lock sobre arrays fijos (POLL_WAITERS / EPOLL_FD_MAP / FUTEX_WAITERS), sin
alocación ni espera; con IF=0 ningún ISR puede estar en curso, no hay
contención posible. `notify_child_death` intacto (ya corría con IF=0).

**Fix 2 — red (`kernel/src/process/scheduler.rs` `tick()` + `timer_preempt.rs`)**:
`tick(&mut self, interrupted_rsp: u64)` — el `retain` de `pending_stack_frees`
retiene (aplaza) cualquier kstack que contenga el RSP interrumpido, en vez de
liberarlo. Convierte el supuesto falso del comentario (881-885, ahora corregido)
en invariante comprobado en runtime. Caller único: `timer_preempt.rs:183`
pasa `(*current_tf).rsp`.

**Mediciones (boot-matrix.sh 24 1 --no-build --timeout 120, firmas separadas)**:

| Firma | ANTES (24) | DESPUÉS (48) |
|-------|-----------|--------------|
| Amplifier HANG/PANIC | 17/24 (70.8%) | 35/48 (72.9%) — 1 PANIC (salto a 0x0, orphan idéntico) |
| BAD RESUME FRAME | 0/24 (1/14 en oneboot) | **0/48** |
| OK (amplifier completo → fase busybox) | 7 | 13 |

El BAD RESUME FRAME no reapareció en 48 boots tras el fix, con 13 boots que
sí llegaron a la fase busybox (antes: ~1 BADRESUME por cada ~4 boots sanos).
P(0 en 13) contra una tasa real del 25% ≈ 2.4% — sugestivo, y el cierre se
sostiene además por construcción (la ventana del boot 12 exige IF=1 sobre el
kstack del moribundo, ahora imposible). La matriz completa no puede
confirmarlo estadísticamente de forma tajante: el amplificador (bug abierto,
separado) bloquea ~70% de los boots antes de la fase busybox.

**Un bug cerrado, otro abierto**: el amplificador sigue colgándose (~70-73%,
sin cambio) — es el bug SEPARADO del orphan-lock (corre antes de toda muerte,
solo idle+shell; `sys_exit`/`tick`-free no se ejecutan en esa fase). Su
manifestación varía (spin IF=0 / PANIC salto a 0x0) con la misma firma
(`outstanding=1, last_acquirer op=symlink`). NO cerrar el bug 2 global hasta
resolverlo.

## BUG 3 (separado): orphan-lock del amplificador — (A) refutado, orphan confirmado, medición corregida

### Hipótesis (A) — box con cs=usuario (modo discordante) — REFUTADA
Instrumento pedido por el supervisor: en `tf_note_resume`, comparar el `cs`
del último SAVE (campo nuevo `Process::tf_last_save_cs`) contra `box.cs`; si
SAVE=kernel(0x8) y box=usuario(0x23) → panic "HANGHUNT MODE MISMATCH". Sembrado
automático por `sys_exec` (su propio `tf_note_save(cs=0x23)`); fork/clone crean
Process fresco (0) sin falso positivo. **Resultado: 0 disparos en 21 fallos de
boot-matrix** → (A) refutado por medición. (B) `resolve_signals` verificado
guardado en su propio nivel (scheduler.rs:412, `(*tf).cs != USER_CS` → retorna
sin tocar; cubre a todos los llamantes incluido el tail del syscall) — no puede
corromper un rsp de kernel. Queda (C): camino no enumerado.

### El orphan ES REAL — Phase B dispara por primera vez
En la medición limpia (timeout 600s), el HANG de un boot mostró **5 disparos
del check Phase B** (syscall-tail con `outstanding=1`): syscall 0/11/4/6/1
(read/mmap/stat/lstat/write — los syscalls de libc del println) en iters
1307-1308, con el lock de ramfs retenido. El orphan se creó en iter ~1307
(mkdir/symlink abandonado mid-scope, su propio tail nunca corrió — por eso
Phase B no lo vio en ese syscall), el amp **continuó varias iteraciones** con el
orphan (avanzando a usuario sin el Drop — Fase A confirmada), y colgó cuando un
mkdir posterior intentó `lock_entries`. El camino de retorno a usuario del
syscall abandonado sigue SIN identificar: no es el box (A refutado), no es el
tail (Phase B no disparó en el syscall culpable). Candidato restante: el
**no-switch del timer** (iretq de un current_tf corrupto a modo usuario mientras
el syscall está en vuelo) — no cubierto por `tf_note_resume`.

### Medición corregida: la "tasa ~70%" era un artefacto del timeout
El amplificador corre a ~4 iters/s bajo TCG (QEMU sin `-enable-kvm`; 1500
iters ≈ 5-6 min). boot-matrix con timeout 120s mata a los boots
lentos-pero-sanos en ~iter 320 → "HANG" masivo. Con **timeout 600s** y
disk.img sano, N=8 M=3: **22/24 OK (91.7%), 1 HANG (iter 1307, el orphan), 1
PANIC (iter 407, el [BUG2]) — tasa real ~8.3%**. El [BUG2] (vtable corrupta,
jump al BTreeMap) sigue ABIERTO (1/24). Firmas separadas:
- `[BUG2] CORRUPT INDIRECT CALL` PANIC: ~4%.
- Orphan-HANG (Phase B + spin): ~4%.
- El resto son lento-pero-sano (timeout).

### Lección de método (sexta): los boots gdb con `qemu-debug.sh start` escriben
sobre el `disk.img` REAL (sin overlay) — un panic mid-ext2-write lo corrompe
(fsck: "sistema de ficheros todavía tiene errores"), y la siguiente corrida
arranca sobre disco corrupto (logs serial vacíos, "HANG" masivo). Usar siempre
`QEMU_DEBUG_DISK_IMG` apuntando a una copia (`cp disk.img /tmp/scratch.img`)
para sesiones en vivo. La corrida de 600s que dio 24/24 HANG con logs vacíos
fue exactamente eso — descartada.

### Instrumento 2: check por-lock en todos los retornos a usuario — 0 disparos, gap de cobertura

Implementado (autorizado): `debug::panic_on_ramfs_lock_held_to_user(cs, site)`
— si el frame a iretq es usuario (cs=0x23) y `DirLockDiag::outstanding()!=0` →
panic en el acto. Instalado en: `validate_resume_frame` (timer no-switch y
switch), `jump_to_user`, el camino signal-kill y `start_first_process`.

**Medición (600s, disco sano, N=8 M=3): 23/24 OK (95.8%), 1 orphan-HANG (iter
903), 0 disparos del check, 0 [BUG2] en esta muestra.** Firmas separadas:
- orphan-HANG con Phase B disparado: 1/24 (~4%).
- [BUG2] PANIC: 0/24 en esta muestra (tasa ~4% — 38% de prob. de 0 en 24).

**Gap de cobertura (por qué 0 disparos NO refuta el retorno-a-usuario)**: los
5 disparos Phase B (syscalls 0/11/3/6/1 — read/mmap/close/lstat/write) en iters
903-904 **prueban que el amp retornó a usuario con el lock retenido** (las
entradas a syscall son transiciones ring3→0 reales). El retorno culpable (el
mkdir/symlink abandonado) NO fue por los caminos cubiertos: no es el tail (Phase
B no disparó en 83/88), no es el box (A refutado), no es el timer/jump (0
disparos del check). Entre el abandono y el siguiente syscall hay ventanas de
usuario de ~µs; un tick de 15ms solo aterriza ahí con P≈0.07%/ventana → el
check (que solo observa retornos del timer/jump) lo ve en ~1.5% de los orphans.
Por tanto el fallback del árbol ("el lock se pierde sin retorno a usuario → el
guard") queda **contradicho** por la evidencia Phase B.

**Siguiente instrumento recomendado**: convertir la Fase B (tail del syscall)
de print a PANIC — el siguiente syscall al orphan ocurre a µs del abandono →
disparo garantizado con el diag completo, identificando el orphan en el primer
syscall post-abandono (read/mmap/... del mismo iter), no iteraciones después.
Con eso el orphan-HANG se vuelve un PANIC cazable. (Pendiente de decisión del
supervisor — la Fase B es su instrumento.)

## Fase B = PANIC + anillo de syscalls — CALIBRADO, encuadre cambiado, experimento

### Instrumento (autorizado): Fase B → panic + anillo de últimos 32 syscalls
`syscall_handler_asm` ahora: captura el número de syscall al entrar (antes del
overwrite), escribe el anillo (entrada/salida), y si `outstanding()!=0` al salir
→ volcado del anillo + diag + panic. El anillo: array fijo de 32
`(seq, pid, syscall, entered, exited)`, sin asignar, circular.

### CALIBRACIÓN (prioridad 0) — bug encontrado: `regs.rax` se sobrescribe
`(*tf_ptr).rax = ret` escribe el VALOR DE RETORNO sobre el mismo slot que lee
`regs.rax` ANTES del check → los primeros volcados decían `current_syscall=0`
("read") cuando era el **return value 0 del symlink**. Arreglado: capturar
`current_syscall` al entrar. Verificado: 22/24 OK, y el volcado ahora dice
`syscall=88 (return value=0)`. El anillo es correcto (secuencias contiguas
83/88/1, patrón del amp).

### ENCUADRE CAMBIADO (el veredicto calibrado del anillo)
El orphan ya no es "la syscall nunca retornó" (Fase A era inferencia incorrecta):
**el symlink (88) de la iteración N COMPLETÓ su handler (ring entered+exited=1)
y su `Drop` no soltó el lock** — el panic dispara en el tail DEL PROPIO symlink
culpable (last_acquirer=symlink iter=1430), no en un syscall posterior. El
sospechoso se mueve al Drop del guard en ramfs.rs.

### El MODE MISMATCH (A) DISPARÓ (no estaba refutado — era raro)
1/24 en la misma corrida: `site=switch_to_next`, last SAVE cs=0x8 (mid-syscall),
box al resumir = cs=0x23 rip=0x400084 rsp=0x71000001fdf0 (un frame usuario
COHERENTE, no basura) → el box fue REEMPLAZADO por un frame usuario entre un
SAVE kernel y el RESUME. Si ese resume hubiera procedido, el symlink mid-scope
se habría abandonado → orphan. La medición previa "0/21" era una muestra
pequeña, no una refutación.

### Experimento bare-lock (autorizado, reversible — revertido)
`lock_entries` → `self.entries.lock()` pelado (sin TrackedEntriesGuard, sin
diag). **Resultado: 22/24 OK, 1 HANG (iter 538, firma orphan), 1 [BUG2] PANIC —
el orphan PERSISTE (~4%) con el lock pelado.** El envoltorio del WIP queda
**EXONERADO**: el leak no es el `Drop` de `TrackedEntriesGuard`. El sospechoso
se mueve al camino más profundo del `spin::MutexGuard` (su Drop también se
salta en un retorno normal — UB — o el abandono por el box-flip del mode
mismatch). Dos mecanismos distintos observados dejando el lock retenido:
(1) handler completado + Drop saltado (Fase B, ring exited=1); (2) box volteado
kernel→usuario mid-syscall (mode mismatch). Ambos ~4% cada uno, ~8% combinado,
más el [BUG2] ~4%.

## Discriminador del box-flip + auditoría de la hipótesis (b) — el frame de entrada queda DEBILITADO

### Instrumento (autorizado): dirección del `current_tf` guardado
`Process::tf_last_save_tf` (la dirección del frame copiado en el último SAVE).
El panic MODE MISMATCH ahora imprime: dirección del frame guardado + banda
(entry-frame en `kstack_top-0x200..kstack_top` vs interior del kstack vs FUERA)
+ kstack + box. Discrimina "frame de entrada reutilizado como current_tf" de
"escritura salvaje". **Medición: 21/24 OK, 3/24 ORPHAN, 0 MODE MISMATCH** — el
discriminador aún no ha disparado (el box-flip es ~1/48, raro).

### Auditoría (b) — tres negativos que DEBILITAN la hipótesis del frame de entrada
1. `CURRENT_SYSCALL_TF` se escribe SOLO en la entrada del syscall (mod.rs:231)
   y **nunca se limpia** — pero sus lecturas están todas en handlers en vuelo
   (sync/signal/poll/ipc/fs/process_ctl); el timer y `switch_to_next` NO lo
   leen. No hay camino por el que el timer obtenga el frame de entrada como
   `current_tf` (su `current_tf` = el frame que la CPU acaba de empujar, en el
   asm).
2. El SAVE que disparó el MODE MISMATCH (run previo) grabó `cs=0x8` (kernel) —
   el frame de entrada es `cs=0x23` (usuario). El frame guardado NO era el de
   entrada.
3. El frame usuario del box (`rip=0x400084`) = `push r15` (PROLOGUE de función
   en el shell, resuelto por objdump) — los return-points de syscall reales son
   0x400226/0x40024a/0x400273/0x4002ea. No es un frame de entrada de syscall.

**Interpretación que cuadra con los tres**: el box retiene su frame usuario
ANTERIOR (preempeción usuario normal en 0x400084) — el SAVE kernel posterior
o no llegó a quedar o fue revertido. Apunta a una **escritura salvaje que
restaura/sobrescribe el box con su contenido previo** (el mismo root que el
[BUG2]), no a un frame de entrada reutilizado.

### El orphan dominante NO es el box-flip — contradice la unificación (a)=(b)
Los 3 ORPHAN de la muestra (iters 1052/1150/1379): `syscall=88` (symlink),
`return value=0`, anillo con **0 entradas `entered=1 exited=0`** — el handler
del symlink COMPLETÓ (el ring_exit corrió; la pila no fue abandonada) y el
`Drop` del guard no soltó el lock. La predicción del supervisor ("(a) es la
misma cosa: el Drop no corre porque la pila entera fue abandonada") queda
**contradicha por el anillo**: la pila NO fue abandonada (el handler retornó
normalmente) y el Drop se saltó igualmente. Los dos mecanismos son distintos:
(1) box-flip → abandono (raro, ~1/48); (2) handler completado + Drop saltado
(dominante, ~3/24 esta muestra). El Drop saltado con retorno normal en un
scope Rust simple (ramfs.rs:442-450, sin mem::forget ni moves) es **UB** —
sospechoso común con el [BUG2]: una escritura salvaje corrompiendo la
ejecución del symlink (el retorno salta el drop glue) o el propio box.

## Fase B = counter phantom + calibración de try_lock + detectores de deriva

### La propiedad no usada: el lock NUNCA debe estar en contienda
En `hang_hunt()` solo PID 1 toca el VFS e idle no — cada `lock_entries` debe
encontrar el lock libre. Encontrarlo tomado ES el bug, en su primera
manifestación. Aprovechada con:

- **Instrumento 1 (try_lock + panic)**: `lock_entries` usa `try_lock()`; si
  falla → panic "CONTENDED" con diag + anillo. Sin spin, sin espera.
- **Instrumento 2 (post-scope)**: en mkdir/symlink, tras el scope del guard,
  el lock crudo debe estar libre (try_lock) Y el contador debe volver a su
  valor previo (detección de deriva en el scope).
- **Detector de deriva en `record_acquire`**: si `outstanding != 0` antes de
  registrar (con el lock libre, garantizado por try_lock) → el contador ya
  estaba desviado → panic "COUNTER DRIFT" con el anillo.

### Calibración de try_lock (punto 3, gate de método): PASS
hw_test `try_lock_can_fail_when_held` (spin::Mutex<u64> del kernel): adquirir →
`try_lock()` → None; tras soltar → Some. **El try_lock SÍ puede fallar**, así
que los 0 CONTENDED significan que el lock nunca estuvo tomado en un acquire.

### Mediciones (instrumentos 1+2+drift activos, 600s, N=24 ×2 = 48 boots)
| Corrida | Resultado |
|---------|-----------|
| 2789958 | 23 OK + 1 PANIC **firma [BUG2]** (salto corrupto a 0x100002f3fc8, RIP=0x10001000031f800) — NO orphan |
| 3021069 | **24/24 OK** |

En los 48 boots: **0 FaseB, 0 drift, 0 CONTENDED, 0 STILL-HELD, 0 HANG** — el
contador balanceado todo el boot. El phantom FaseB (~1-3/24 en corridas
previas) NO se ha reproducido con los detectores puestos.

### Reconciliación con la captura gdb (el spin real de Fase A)
La captura viva (spin en SpinMutex::lock, self=0x2801fec5f50) fue del cazado
histórico con la toma BLOQUEANTE. Hoy ese mismo caso → el siguiente
`lock_entries` → try_lock falla → panic CONTENDED. Que CONTENDED no haya
disparado en ~120 boots con instrumentos sugiere: el orphan real es RARO (el
spin gdb fue 1 observación en toda la caza) o algo del instrumento lo previene.

### El phantom del contador sigue sin reproducirse
La pregunta abierta (sexto/séptimo instrumento): el FaseB con `acquires=
releases+1` y lock libre. Los detectores nuevos disparan ANTES que Fase B (el
post-scope en el scope, el drift en el acquire siguiente) — pero necesitan un
boot donde el phantom ocurra. Con ~1-3/24 necesita N grande. Si el drift fuera
en el scope symlink/mkdir → "COUNTER drifted in scope"; si en un op previo
(lookup/open/etc.) → "COUNTER DRIFT at acquire". Aún sin disparo: el contador
estuvo correcto en los últimos 48 boots.

## WATCHPOINT DE VTABLE/LOCK/MAP — RESULTADO (autorizado, calibrado)

Objetivos y hallazgos de diseño (declarados ANTES de mirar):
- **Vtable del root**: NO es persistente. `MountEntry` guarda `Arc<dyn
  Filesystem>` (el `RamFs`), no el inode raíz; la `Arc<dyn Inode>` raíz se
  materializa en cada `root()`. Los únicos vtable-ptrs persistentes viven en
  los VALORES del BTreeMap (16 bytes), dentro de los nodos — cuya dirección
  **varía entre arranques** (0x2801e9cd000 en un boot, 0x2801f8ec400 en otro;
  el struct del BTreeMap en 0x2801fec5f58 y el lock en 0x2801fec5f50 SÍ son
  estables).
- **Lock word (0x2801fec5f50)**: se escribe en cada acquire/release — ruidoso,
  ~4-6 transiciones por iteración.
- **word0 del BTreeMap (0x2801fec5f58)**: se escribe SOLO en rebalance del
  árbol — bajo ruido. **Expectativa: pocos fires, todos con bt dentro de
  `BTreeMap::insert`; un fire con bt externo = corrupción.**

Resultado: watchpoint sobre word0 → **0 fires** en la ventana observada (el
boot, con watchpoints TCG ~100x lentos, no alcanzó ni a poblar la segunda
naturaleza del árbol; no hubo rebalance → no hay datos sobre el writer del
nodo). El watchpoint de objetos del heap es **estructuralmente ruidoso o
demasiado lento** para discriminar "escritura del dueño" vs "escritura
corrupta" en un evento ~15% profundo en el boot. El watchpoint queda como
instrumento agotado para la hipótesis de corrupción de heap (después de la
calibración: el box 80/80 limpio, word0 sin fires).

**Siguiente paso recomendado** (tras el canario, que ya habló — ver arriba):
inspección del estado vivo del HANG con el canario activo (gdb en el giro del
lock huérfano: qué código real gira y con qué stack, ahora con el layout nuevo
y el canario como red de detección de overflow).

### Siguiente paso recomendado

1. **Canario/redzone del slab** (detecta el overflow en el momento, sin gdb):
   requiere permiso para `mm/`. O
2. **Watchpoint sobre la vtable del root de /tmp**: el `Arc<dyn Inode>` raíz
   (`0x2801fec5f50`) se guarda en `/tmp`'s padre; su puntero de vtable vive en
   la entrada del initramfs o en el map. Ver qué escribe/lee ese puntero. O
3. **Test de propiedades host del slab en `mm`** (barato, sin QEMU): demostrar
   si un doble-free del slab produce solapamiento (el slab no tiene detección
   de doble-free a diferencia del buddy, que sí quedó probado limpio).

## TURNO DE RELEVO (2026-08-05) — PROPUESTA DE EXPERIMENTO DISCRIMINANTE (sin implementar)

### Reencuadre: el volcado [BUG2] observa el FINAL de la cadena, no el inicio

El fault que imprime el kernel (CR2=map, error 0x11) NO es el primer evento de
la corrupción — es el segundo (o posterior): la CPU YA saltó al nodo raíz
(`sf.rip` = word0), ejecutó datos del nodo como código, y faultó al fetchear en
el mapa. Para entonces el estado real de la CPU (RSP, GPR, return address del
`call` fallido) está destruido: por eso `ret_addr=[X]`=0xdeadbeaf y `shim
rdi/rsi/rax`=0/0/1 en TODOS los volcados. Los dos únicos datos que localizan al
escritor (el call site y el receptor del puntero gordo) viven en el estado del
PRIMER evento (el salto al word0), no en el segundo.

### Mecanismo: NX-TRAP sobre el physmap (que el primer salto faultée en el acto)

1. Activar **EFER.NXE** (bit 11). El kernel ya hace un RMW de EFER en
   `init_syscall_msrs` (tss.rs:139-140, solo para SCE); añadir el bit es una
   línea. El kernel NUNCA usa NO_EXECUTE hoy (grep: solo 2 comentarios, en
   page_table_manager.rs y elf_loader.rs) → no hay mappings con el bit 63
   puesto que cambien de significado al activar NX.
2. Poner **NO_EXECUTE en todas las páginas hoja del physmap** en el rango
   `[physical_memory_offset, phys_offset+RAM_top)` (el physmap es 1:1; hoy
   mapeado PRESENT|WRITABLE, todo ejecutable). El rango de usuario (0x71…)
   vive en entradas PML4 de usuario SEPARADAS (`new_user()` las salta) → no
   se toca la ejecución de usuario. `.text` (0x10000…) y la pila de boot
   (0x18000…) quedan fuera del rango. Nada legítimo ejecuta código desde el
   physmap (los kstacks, TSS, GDT, IDT son datos).
3. Efecto: el salto corrupto al nodo raíz (una página del physmap hoy
   EJECUTABLE) faultée **inmediatamente**, con el `InterruptStackFrame` REAL
   (antes era basura del mapa).

### Qué discrimina (cada rama mata una familia)

- **sf real**: `cs` (0x8 kernel / 0x23 usuario) → dónde se hizo la llamada
  corrupta; `rip` = word0 (confirma el objetivo); `rsp` = el stack real.
- **`[sf.rsp]`** (lectura guardada ya existente) = el return address del
  `call` fallido → addr2line → **el call site**:
  - resuelve a vfs/ramfs → puntero gordo leído desde código VFS legítimo
    (llamada virtual sobre `Arc<dyn Inode>`);
  - resuelve a otra zona (allocator/scheduler/print) → reorienta fuera de fs/;
  - NO resuelve / rsp basura → fue un `ret`/`jmp` (corrupción de
    return-address/GPR), NO una llamada virtual → mata la hipótesis del
    puntero gordo y apunta a la pila.
- **`shim rdi`** (el receptor = data half del puntero gordo) → identifica la
  VÍCTIMA: slot de un leaf node del mapa (H-WRITE a valor, cazable luego) vs
  ~el SpinMutex base 0x2801fec5f50-like (**confirma la lectura desplazada de
  un puntero gordo desde la zona del lock/mapa** — la hipótesis H-READ del
  supervisor) vs RamDirNode/RamSymlinkNode persistente (H-WRITE a un Arc vivo
  no-churning).

### Por qué NO perturba la reproducción (caveat 2)

No cambia el layout del heap (a diferencia de canario/cuarentena), no añade
prints en el camino caliente, no toca el timer. Solo cambia el bit NX de
páginas que nadie legítimo ejecuta → la tasa (~2-8%) debe quedar igual, y cada
[BUG2] PANIC pasa de imprimir basura a imprimir el estado real del primer
evento.

### Expectativa declarada (antes de medir)

sf real con cs=0x8, rip=word0 (nodo raíz), rsp en kstack, `[rsp]` resolviendo a
un call site en ramfs.rs/vfs.rs (despacho virtual sobre Arc<dyn Inode>). rdi =
puntero de heap que identifica a la víctima. Creo que será un `call` (no un
`ret`); no me comprometo entre "valor del mapa" vs "root/temp" — el volcado lo
resuelve. Negativo posible: la tasa no sube (esperada) pero el sf sigue siendo
basura → el primer salto NO es al physmap (entonces el "word0" del dump es
coincidencia del layout y el salto real es otro) → reorienta.

### Solicitud de permiso (toca fuera del WIP instrumentado)

- `process/tss.rs`: añadir NXE al RMW de EFER (1 línea).
- `memory/` (init o page_table_manager): pase de init que ponga NO_EXECUTE en
  las hojas del physmap (caminando las entradas PML4 del physmap, ~512
  entradas si es 2MiB, 262k si 4KiB; o escribir bit 63 directo en las PTEs).
- `init/devices.rs`: el volcado [BUG2] ya está; solo habría que hacer la
  lectura `ret_addr` con el sf REAL (el código guardado ya la hace vía
  `sf.stack_pointer` — funcionará sin cambios o con un ajuste menor).
- Aditivo, no toca WIP del usuario fuera de esos tres puntos.

### Calibración (regla 2) y medición (regla 3)

- Calibración: (a) run-kernel-tests 3/3 (el kernel de test arranca con NX
  activo; si algo de test ejecuta physmap → false-fault inmediato y se ve);
  (b) un boot sano: amp completa / llega a ash sin faults 0x11 espurios;
  (c) imprimir EFER.NXE en boot para confirmar el bit.
- Medición: `boot-matrix.sh 6 4 --timeout 600`, disco de scratch, N≥24,
  comparar tasa de [BUG2] vs la referencia (~2-8%) y — el dato — el contenido
  nuevo del dump en los pánicos.

### Complemento barato (NO lo hago sin que lo pida)

Scan de vtable de todos los valores del mapa en `lock_entries` (aditivo, en
zona ya instrumentada): discrimina la rama "corrupción dentro de un valor del
mapa" con nombre+iteración, O(n) por acquire (más perturbador). Lo guardo como
segundo paso si el NX-trap muestra rdi apuntando a un leaf node.

## NX-TRAP — IMPLEMENTADO Y CALIBRADO (2026-08-05, aprobado por el supervisor)

### Implementación
- `memory/mod.rs::enable_physmap_nx()`: activa EFER.NXE (ya estaba activo de OVMF) + marca
  NO_EXECUTE en todas las hojas del physmap `[phys_offset, phys_offset+4GiB)`. El physmap usa
  **2MiB** (2048 hojas, verificado: `4k=0 2m=2048 1g=0`). Se llama al final de `init::memory::init_core`.
- `process/tss.rs`: NXE añadido al RMW de EFER (línea autorizada; redundante, el RMW preserva el bit).
- `init/devices.rs`: el volcado `[BUG2]` (error==0x11) ahora reconstruye el marco REAL
  auto-calibrado (abajo). `error==0x11` = presente+IF = el salto corrupto faultéa en el PRIMER evento.
- Nada legitimo ejecuta desde el physmap (auditado: .text en PML4 idx 2, usuario/trampoline en
  PTEs de usuario, physmap en idx 5). Boot sano completo 1500 iters + fase busybox sin faults NX.

### Descubrimiento durante la calibración (3 hallazgos)
1. **El `sf` del handler #PF SIEMPRE fue CR2** (preexistente, no regresión): el volcado `[BUG2]`
   histórico leía `sf.instruction_pointer` = CR2 (el puntero `sf` del ABI x86-interrupt llega
   roto a este handler = el valor de CR2, no RSP+8). Por eso "el marco no apunta a la pila":
   los campos del pánico eran los datos DE CR2. El doc lo había visto; ahora está explicado.
   Los caminos normales no lo usan (`let _ = sf`), por eso nadie lo notó.
2. **El frame de excepción es el de 5 qwords + error** (el doc tenía razón): el fault same-CPL
   empuja [error, RIP, CS, RFLAGS, RSP, SS] (56 bytes); `fault_rsp = [RIP+24]`.
3. **Lecturas del shim sin guardar**: las de `sf-16/-56/-64` pueden cruzar el límite de la pila
   (la calibración en la pila de boot #DF por el push del marco); se guardaron con
   `(sf_addr & 0xfff) >= 0x60`.

### El dump auto-calibrado (clave del experimento)
Para un fault de instruction-fetch, **el campo RIP del marco == CR2** (CR2 es la dirección cuyo
fetch falló = la instrucción que se iba a ejecutar). El volcado escanea hacia arriba desde el
RSP del handler buscando un qword == CR2 cuyo vecino sea un selector CS (8/0x23) con RFLAGS
bit1 → ese es el slot RIP del marco real. De ahí: `cs = [RIP+8]`, `rflags = [RIP+16]`,
`fault_rsp = [RIP+24]`, y **`[fault_rsp]` = el return address del `call` corrupto = EL CALL SITE**.
Auto-calibrado: sin offsets hardcodeados del build (antes intenté 0xac0/0xb60 y cambiaban con
cada rebuild — el shim re-aloca el frame; el escaneo por CR2 no depende de eso).

### RESULTADO DE CALIBRACIÓN (boot calib12, salto sintético a 0x28000101000)
```
[NX-TRAP] REAL frame: rip_slot=0x18000014d68 cs=0x8 rflags=0x10002 fault_rsp=0x18000014d98
  [rsp]ret_addr=0x1000022af94 (rsp_now=0x180000141f8 sf=0x28000101000)
```
- `fault_rsp=0x14d98` COINCIDE con el SP del `-d int` de QEMU → el marco reconstruido es real.
- `cs=0x8` → el fault fue en ring-0 (el salto corrupto en un handler de syscall).
- `[rsp]ret_addr=0x1000022af94` → `addr2line` → **`kernel::memory::calibration_jump`**
  (el call site del `call rax` sintético). El instrumento captura el call site del call corrupto.
- QEMU: `v=0e e=0011` (presente+IF), CR2=el target. El trap dispara como diseñado.

**La prueba positiva del supervisor está completa**: el trap produce el fault 0x11 con el marco
real, y el volcado recupera el call site. En el bug real, CR2 = word0 (nodo raíz) y `[fault_rsp]`
debería resolver al código que leyó el puntero gordo corrupto.

### Calibración siguiente (boot-matrix, CALIBRATION_JUMP apagado)
1. `run-kernel-tests.sh` 3/3 y `cd mm && cargo test` verdes antes de medir.
2. `boot-matrix.sh 6 4 --timeout 600` disco scratch, N≥24. Comparar: tasa de [BUG2] (~2-8%)
   y — el dato — el contenido del volcado en los pánicos (CR2 = word0 + cs + [rsp] call site).
3. Expectativa: los [BUG2] PANIC ahora imprimen el marco real del PRIMER evento (cs=0x8,
   fault_rsp en kstack, [rsp] resolviendo al call site del puntero gordo). Si el call site
   resuelve a vfs/ramfs → el puntero gordo se lee desde VFS legítimo; si a otra zona → reorienta.

## NX-TRAP — PRIMERA CAPTURA DEL BUG REAL (boot-matrix 6 4, boot 23 de 24)

Boot-matrix 3258030: 22 OK + 1 PANIC en 23 boots (hasta ahora). El PANIC es la
firma [BUG2] CAZADA EN SU PRIMER EVENTO por el NX-TRAP:

```
[NX-TRAP] REAL frame: rip_slot=0x2801fe9fa88 cs=0x8 rflags=0x10202
  fault_rsp=0x2801fe9fab0 [rsp]ret_addr=0x100002f6c48 (rsp_now=0x2801fe9ef18)
=== [BUG2] CORRUPT INDIRECT CALL ===
  CR2=0x100002f6c48 err=0x11
  sf: rip=0x100010000322500 cs=0x8 rflags=0x2801f31ef00 rsp=0x5 ss=0x2
  shim: rdi=0x2801fec5fc0 rsi=0x4 rax=0xfee00000
=== KERNEL PANIC ===  Address: 0x100002f6c48  Error: 0b10001  running PID: 1
  forks_total: 0 execs_total: 0 switches_total: 1785  iter=1110
```

**Análisis (el dato que la caza llevaba meses buscando):**
- **CR2 = 0x100002f6c48 = `&MOUNTS`** (kernel::fs::vfs::MOUNTS, la tabla global de
  montajes; símbolo `_RNv...fs3vfs6MOUNTS` a 0x2f6c48, en .bss). El CPU intentó
  **fetchear en la dirección del estático MOUNTS**. addr2line → vfs.rs:205
  (exactamente la línea `static MOUNTS: Once<Mutex<Vec<MountEntry>>>`).
- El .bss del kernel NO está en el physmap (PML4 idx 2): su NX lo pone el
  bootloader al mapear los segmentos RW del ELF. El NX-TRAP (que solo marca el
  physmap) NO hizo falta aquí — el fault 0x11 es del .bss NX preexistente. El
  mecanismo del doc ("vtable = word0/nodo raíz") es UNA manifestación; aquí el
  puntero de código corrupto = **&MOUNTS** (otra dirección de datos). Común a
  ambas: un puntero de código de un objeto vivo quedó con una dirección de
  DATOS (heap o .bss) → fetch NX → 0x11.
- `cs=0x8` → el call corrupto corrió en ring-0 (syscall handler del amp, iter
  1110, sin forks/execs — puro mkdir/symlink).
- `fault_rsp=0x2801fe9fab0` en el kstack de PID 1 (0x2801fe90000..0x2801fea0000).
  `[fault_rsp]=0x100002f6c48` = &MOUNTS (mismo valor que CR2 — ver nota).
- `sf` sigue roto (= CR2 = &MOUNTS): `sf.rip`=0x100010000322500 son los BYTES de
  .bss en &MOUNTS leídos como marco. `shim rdi/rsi/rax` = contenido de .bss
  alrededor de MOUNTS (0x2801fec5fc0 aparece como rdi: un puntero a heap en el
  .bss cercano a MOUNTS — posible clue, o coincidencia).
- `switchs_total: 1785` — el box no se tocó (era el watchpoint 80/80); la
  víctima es otro objeto.

**Nota sobre el transfer (abierto)**: CR2 == [fault_rsp] == &MOUNTS. Un
`call [vtable+off]` no faultéa EN &MOUNTS (saltaría al valor leído de
[&MOUNTS+off]); faultéar EN &MOUNTS sugiere un `jmp/call reg` con reg=&MOUNTS
(no un call vtable con offset), o un return-address = &MOUNTS. La lectura
`[fault_rsp]` puede estar desplazada unos bytes (estimación del marco). Pendiente
de confirmar el mecanismo exacto del transfer con más capturas.

**Siguiente**: (1) terminar la tanda (24) y ver si hay más capturas con otros
destinos; (2) correlacionar: &MOUNTS y word0 (nodo raíz) son ambos direcciones
de datos que aparecen en memoria cerca del objeto corrupto — el escritor salvaje
escribe un puntero a datos en el slot de vtable; (3) candidato a vigilar: qué
objeto recibe el write (necesita el receptor rdi real, aún no capturado).

## REENCUADRE DEL SUPERVISOR + AUDITORÍA DE PILA (2026-08-05)

### Reencuadre: CR2 == [fault_rsp] es la firma de un `ret`, no de una vtable
El supervisor señala: un `call [vtable+off]` NO puede faultear EN &MOUNTS (saltaría al
valor leído de [&MOUNTS+off], no a la dirección del estático). Pero un `ret` sí: toma la
palabra del tope de la pila y salta ahí → la dirección del fallo coincide byte a byte con
`[rsp]`. En la captura: CR2 = [fault_rsp] = &MOUNTS. **Consecuencia: es una dirección de
retorno corrupta en la pila de kernel (RSP desplazado), NO una vtable corrupta en el heap.**
Unifica: (BAD RESUME FRAME) el campo corrupto era `rsp`; (doc manifestación 1) `current_tf`
desalineado; (este) salto a un valor de datos del stack. &MOUNTS es un puntero a estático que
el compilador derrama a la pila en cualquier función de fs::vfs — valor legítimo en un hueco
de pila cercano; un `ret` con RSP desplazado 8 se lleva ese puntero en vez del return address.

### Auditoría (orden/conteo/tamaño campo a campo) — RESULTADO: NEGATIVO (limpia)
Pedida por el supervisor: TrapFrame vs push/pop del asm, simetría prólogo/epílogo del ISR
(switch y no-switch), stub de syscall (alineación 16 + restaura exactamente lo que empuja).
Verificada campo a campo (no por encima):
- **TrapFrame** (repr C, 20×u64 = 160 bytes): r15@0 r14@8 r13@16 r12@24 r11@32 r10@40 r9@48
  r8@56 rbp@64 rdi@72 rsi@80 rdx@88 rcx@96 rbx@104 rax@112 rip@120 cs@128 rflags@136 rsp@144
  ss@152.
- **ISR timer** (timer_preempt.rs): push rax,rbx,rcx,rdx,rsi,rdi,rbp,r8,r9,r10,r11,r12,r13,r14,
  r15 (15) → desde el RSP final: r15@0..rax@112 + marco hw (5 qwords)@120. `current_tf = rsp`
  (apunta a r15). **Coincide EXACTO** con el struct. Epílogo: pop r15..rax + iretq (5) — mismo
  orden. Simétrico en switch y no-switch.
- **jump_to_trapframe** (trapframe.rs): pop r15..rax + iretq — exacto al struct.
- **Stub syscall** (syscall/mod.rs): construye el marco hw de 5 a mano (push $0x1b=user SS,
  user-RSP, user-RFLAGS, $0x23=user CS, user-RIP) y luego push de los 15 GPR en el MISMO orden
  que el ISR. `SavedRegisters` = primeros 15 campos del TrapFrame. Restaura exactamente
  (pop ×15 + iretq). KERNEL_RSP0 = tope del kstack (bloque buddy, 16-alineado).
- **Alineación 16 bytes**: en ambos `call` (timer→timer_preempt_handler, syscall→
  syscall_handler_asm) la entrada queda rsp%16==8 SI la RSP interrumpida es 16-alineada
  (el compilador la mantiene en syscalls y en user). FPU: `#[repr(C, align(16))]` — fxsave/
  fxrstor alineados. El único caso de entrada desalineada (tick en un instante transitorio
  no-alineado) produciría #GP (movaps), no el 0x11 — síntoma distinto.
- Todos los demás `asm!` del kernel son rdmsr/wrmsr con `nostack`/`preserves_flags` — sin
  efecto de pila. Los únicos stubs con stack son los tres verificados.

**Conclusión del negativo**: el desequilibrio de pila de 8 bytes NO vive en el mecanismo
TrapFrame/ISR/syscall — está balanceado. Si el mecanismo del supervisor (ret con RSP
desplazado) es correcto, el desplazamiento viene de OTRO sitio. Candidatos tras el negativo:
(a) el campo `rsp` del Box<TrapFrame> (offset 144) — el fix de BAD RESUME FRAME cerró la
ventana del kstack-liberado, pero una escritura salvaje al box.rsp sigue siendo posible; el
watchpoint 80/80 solo cubría box+120 (rip). (b) un return address de kstack pisado por un
write. (c) un desalineamiento de entrada de call transitorio.

### Siguiente (orden del supervisor)
1. Check barato en el handler del timer: RSP de entrada == RSP de salida (panic si difiere)
   — convierte "1 fallo cada 24" en "el tick N dejó la pila desplazada". O(1), no toca el asm.
2. Si el check no dispara (probable, la auditoría salió limpia) → vigilar el campo `rsp` del
   box (offset 144): el candidato del mecanismo unificado.

## CAMPAÑAS CON NX-TRAP + CHECK RSP (2 tandas, 48 boots)

| Tanda | Boots | OK | PANIC | Detalle |
|-------|-------|----|-------|---------|
| 3258030 | 24 | 23 | 1 | **[BUG2] cazado**: CR2=[rsp]=&MOUNTS, cs=0x8, kstack PID1, iter 1110 |
| 3451966 | 24 | 23 | 1 | **orphan Phase B**: symlink(88) completó handler (ring exited=1, ret=0) pero Drop no soltó lock, outstanding=1, iter 1086 |

Total 48: 46 OK + 2 PANIC (4.2%). Ambos son manifestaciones de la MISMA familia:
- [BUG2] = `ret` derail a &MOUNTS (un .bss de datos, NX del bootloader) → fetch 0x11.
- orphan = `ret` derail que salta el drop glue del guard (symlink completó pero no soltó).

**Check RSP (supervisor): 0 disparos en 48 boots** (incluidos ambos fallos). El handler del
timer es net-zero en cada tick. Otro negativo bien medido: el desplazamiento de pila de 8
bytes NO se introduce en el ISR (Rust) — la auditoría de los stubs también salió limpia.

**El orphan HA REAPARECIDO** (el doc decía "0 en ~120"): 1/24 aquí. No está erradicado;
es intermitente de la misma familia. Los detectores (CONTENDED/STILL-HELD/drift/post-scope)
no lo cazaron porque el handler completó normal (el desvío está en el ret del epílogo que
salta el drop glue, no en el lock).

**Síntesis del modelo unificado (supervisor)**: un resume/ejecución con RSP desplazado 8
hace que el primer `ret` del código reanudado saque la palabra equivocada: si es .text →
salta drop glue → orphan; si es datos (&MOUNTS) → [BUG2]. La auditoría descarta el
mecanismo TrapFrame/ISR/syscall (balanceado) y el check RSP descarta el handler del timer.
**El candidato que queda: el campo `rsp` del Box<TrapFrame> (offset 144)**: el fix de BAD
RESUME FRAME cerró la ventana del kstack-liberado, pero el watchpoint 80/80 solo cubrió
box+120 (rip); una escritura salvaje al box.rsp (8-off, aún dentro del kstack) pasa el
validate_resume_frame (que solo comprueba rango, no desplazamiento de +8).

**Siguiente instrumento propuesto**: vigilar el campo rsp del box (offset 144) — el punto
ciego del watchpoint anterior y el lugar que el modelo unificado señala. (Pendiente de
decisión: ¿watchpoint gdb, o instrumento software?)

## DETECTOR DE INTEGRIDAD DEL BOX (2026-08-05, aprobado + 2 extensiones del supervisor)

### Implementación
- `Process::tf_saved_five: [u64; 5]` (rip, cs, rflags, rsp, ss) — snapshot de los 5 campos
  del marco en el SAVE; cs==0 = nunca guardado (fresh/fork, sentinel).
- **tf_note_save (check SAVE-side, supervisor item 2)**: si `saved_tf != 0`, valida
  `cs ∈ {0x8,0x23}` y `ss ∈ {0x10,0x1b}` del marco FUENTE. Un selector inválido = puntero
  al marco desplazado (BOGUS-RANGE solo comprueba rango del kstack; un desplazamiento de 8
  sigue dentro). Cero falsos positivos: un marco legítimo siempre tiene selectores válidos.
  Al disparar: panic con saved_tf, kstack, box, y 8 palabras alrededor de saved_tf (para ver
  el offset del desvío). Ejec pasa saved_tf=0 (rewrite directo del box) → solo snapshot.
- **tf_note_resume (check RESUME-side, supervisor item 1)**: compara los 5 campos del box
  contra `tf_saved_five`. Cualquier diferencia = el box fue ESCRITO entre SAVE y RESUME.
  Imprime ambos juegos + los deltas (si rsp difiere exactamente ±8 → confirma el modelo del
  desplazamiento de una palabra del supervisor). Es el superset del MODE MISMATCH (cs-only).
  `deliver_pending` modifica el box DESPUÉS de este punto (el resume ya pasó) → sin falso
  positivo por señales.

### Expectativas declaradas (antes de medir, regla 7)
- Dispara el **SAVE-side** → el marco llega desplazado; sospechoso = asm de entrada /
  cálculo de current_tf (caso dinámico, p.ej. anidamiento, aunque la auditoría estática
  salió limpia).
- Dispara el **RESUME-side** → alguien escribe el box entre medias (escritura salvaje); el
  volcado dirá qué campo y con qué valor (rsp ±8 = desplazamiento).
- **No dispara ninguno y los fallos siguen** → el desplazamiento nace DENTRO de la ejecución
  normal, entre el iretq y el `ret` que falla; siguiente paso = canario en la pila de kernel
  (el canario del slab cubría el heap, no las pilas).

### CORRECCIÓN EXPLÍCITA DEL REGISTRO (pedida por el supervisor)
El doc decía "el orphan dejó de reproducirse (0 en ~120 arranques)". **DESMENTIDO**: el
orphan reapareció en la tanda 3451966 (1/24, iter 1086, symlink completó handler pero el
Drop no soltó el lock). No está erradicado; es la otra manifestación de la misma familia
(ret desviado que salta el drop glue). La línea del doc queda corregida aquí.

## DETECTOR DE INTEGRIDAD — PRIMERAS CAPTURAS (tanda 3557098, 24 boots: 22 OK + 2 PANIC)

Ambos PANIC = **`HANGHUNT BOX WRITTEN BETWEEN SAVE AND RESUME`** (detector RESUME-side),
DETERMINISTAS en lo esencial:

```
boot1 (iter 850): saved: rip=0x100002c54b0 cs=0x8 rsp=0x2801fe9eac0   (mid-copy_backward)
                  now:   rip=0x1000027f557 cs=0x8 rsp=0x2801fe9faa8   (InterruptGuard::drop)
                  diff: rsp=+4072, rip/rflags cambian, cs/ss IGUALES
boot4 (iter 1256): saved: rip=0x100002c54a2 (2 bytes antes, mismo memcpy)
                  now:   rip=0x1000027f557 rsp=0x2801fe9faa8   ← IDÉNTICO a boot1
  ramfs_entries: outstanding=1 last_acquirer op=mkdir iter=850/1256
```

**Análisis (mecanismo del orphan CONFIRMADO por instrumento calibrado):**
- El box fue reescrito entre su último SAVE y el RESUME con un marco **distinto y
  determinista**: `InterruptGuard::drop` (irq_guard.rs:46 = el `sti; ret` del tail del syscall),
  rsp=0x...faa8 = offset 64168 del kstack (184 bytes bajo el frame de entrada del syscall).
- El récord (SAVE) era un marco MÁS profundo (mid-`memcpy` en el mkdir, offset 60096). cs=0x8
  en ambos → el **MODE MISMATCH (cs-only) NO podía cazarlo**; el detector de 5 campos sí.
- El resume iretqa el marco stale del tail de un syscall ANTERIOR → el mkdir EN VUELO (con su
  guard de `entries` tomado) queda abandonado → `outstanding=1 op=mkdir` → el orphan.
- La determinismo (mismo marco now en 2 boots) descarta un write aleatorio: es una RUTA
  concreta. El escritor escribe un marco completo (rip/rflags/rsp cambian, cs/ss iguales) →
  es un COPY de un marco, no una corrupción de un campo.

**Pregunta abierta (el escritor)**: ¿qué copia el marco stale del tail al box? El box write
`*proc.trapframe = *current_tf` está pareado con tf_note_save en los 3 sitios del scheduler
(switch/block/stop) — la discrepancia record/box implica que el box se escribió DESPUÉS del
record sin actualizarlo, o el record es de un save cuyo current_tf apuntaba a un marco stale.
Ambas son un write fuera de los saves canónicos. La víctima (box=0x2801f31ed00) es
determinista; el siguiente paso es identificar el writer.

**Nota**: el detector NO dejó pasar ningún [BUG2] en esta tanda (paniquea antes). Las firmas
se separan: esta tanda = 2 orphan (box rewrite), 0 [BUG2]. Tasa 2/24 (8.3%) esta tanda.

## DISCRIMINADOR (i)/(ii) — volcado extendido del detector (2026-08-05)

Implementado (supervisor, opción 1 + discriminador):
- `Process::tf_saved_switches` + `debug::switches_total_count()` — switches totales en el
  instante del SAVE.
- El volcado del disparo ahora incluye: ambos juegos de 5 campos + deltas; `tf_seq`,
  `tf_awaiting_resume`, `tf_last_resumed_seq`; `switches saved_at / now / delta`;
  `tf_last_save_tf` (dirección del marco del récord) + kstack + 8 palabras alrededor.

**Decisión (i)/(ii)**:
- (i) marco stale / escritura salvaje en un box dormant → switches apenas avanzaron desde el
  SAVE, tf_seq sin cambios.
- (ii) el proceso CORRIÓ entre SAVE y RESUME por un camino no registrado (resume sin
  tf_note_resume), llegó al tail del syscall y un tick lo re-guardó → switches avanzaron
  mucho con tf_seq sin cambios (el re-save no-canónico no bumpa seq).

**Expectativa declarada (antes de la tanda)**: el determinismo del marco now y el hecho de
que el detector dispara (record≠box, imposible con un re-save canónico que actualiza el
record) me inclinan a (i) escritura salvaje — pero el dato de switches/seq decide. Si sale
(ii), el objetivo cambia a "qué camino reanuda sin registrar" y el page-protect del box queda
descartado (no habría escritor).
## Tanda 3707088 (volcado extendido): 24/24 OK, 0 disparos
El bug no reprodujo (P(0 en 24) ≈ 35% a la tasa observada ~4%). Sin dato del discriminador
(i)/(ii) aún. Se repite la campaña.

## DISCRIMINADOR (i)/(ii) — RESUELTO: (i) ESCRITURA SALVAJE (tanda 3927183, boot 3, iter 1058)

Volcado completo del disparo:
```
saved: rip=0x100002c5e00 cs=0x8 rflags=0x10606 rsp=0x2801fe9eac0 ss=0x10   (mid-copy_backward)
now:   rip=0x1000027fea7 cs=0x8 rflags=0x10202 rsp=0x2801fe9faa8 ss=0x10   (InterruptGuard::drop)
diff:  rsp=+4072; cs/ss iguales
seq:   tf_seq=1989 tf_awaiting_resume=0 tf_last_resumed_seq=Some(1989)
switches: saved_at=1988 now=1989 (delta=1)
last save frame addr=0x2801fe9ea20 (= saved.rsp - 160 → SAVE fue un switch_to_next,
       current_tf del ISR) kstack=[0x2801fe90000,0x2801fea0000)
```

**Interpretación (decisiva):**
- `inc_switches` SOLO está en `switch_to_next` (línea 1235; kill/block/stop NO incrementan).
  Delta=1 ⇒ el SAVE y el fire están en el MISMO switch_to_next (guarda registra 1988, el inc
  del propio switch → 1989, el fire lee 1989). Como pid=1 (pri 5) siempre supera a idle
  (pri 0), CADA preempción de pid=1 es un save+resume inmediato del mismo proceso.
- ⇒ **PID 1 NO ejecutó entre el SAVE y el RESUME**. El box fue escrito (frame C) durante el
  tramo medio del switch. **Es (i): escritura salvaje sobre el box dormant.**
- `tf_awaiting_resume=0` (ya limpiado por el fire) y `tf_last_resumed_seq=1989` (el fire)
  confirman que no hubo un resume no registrado intermedio.

**El escritor**: copia un marco completo (frame C = InterruptGuard::drop, determinista) al
box=0x2801f31ed00. El tramo medio del switch corre: read_fs_base, fpu::save (fxsave),
push_back run-queue, pop, address_space.activate (CR3), set_kernel_stack, write_fs_base,
fpu::restore (fxrstor), inc_switches. Ninguno escribe el box de forma evidente; el frame C
coherente (rip .text, cs/ss válidos) descarta fxsave (bytes FPU) como fuente directa.

**Consecuencia**: con (i) confirmado, el page-protect del box (opción 2 del supervisor) es
el instrumento correcto: hay un escritor que cazar. Requisitos del supervisor: box en su
propia página (asignación aislada), filtro por RIP del escritor (saves canónicos permitidos),
calibración con escritura sintética.
## Tanda 3927183 completa: 24 boots, 23 OK + 1 PANIC (el fire del discriminador, iter 1058)
Tasa 1/24 (4.2%). El fire aportó el veredicto (i) escritura salvaje. Siguiente: page-protect
del box (opción 2), requisito del supervisor: box en su propia página, filtro por RIP del
escritor, calibración con escritura sintética.

## PAGE-PROTECT DEL BOX — CALIBRADO (2026-08-05)

Implementado según la receta del supervisor: box de PID 1 aislado en su propia página
(fuga deliberada, 4096 B página-alineada vía alloc→buddy; `alloc_trapframe_box`), split de
la página de 2 MiB del physmap en PTEs de 4 KiB (`box_watch_arm`), protect/unprotect en
`switch_to_next` (tras el box copy → antes del resume), y PF handler que caza el write a la
página protegida con el RIP del escritor.

**Calibración (escritura sintética no-canónica en la ventana, BOXWATCH_CALIBRATE)**:
```
[BOXWATCH] armed: box=0x2801ee70000 (página propia, layout cambió — perturbación aceptada)
=== [BOXWATCH] WILD WRITE TO PROTECTED BOX ===  cr2=box
  writer_rip=0x10000247fd6 → addr2line → switch_to_next:1186 (el sitio sintético EXACTO)
```
- El instrumento dispara con el RIP del escritor correcto.
- **Instrumento defectuoso #8 (calibrado)**: el rango del kernel en el scan del marco era
  `0x1000_0000_0000` (2^44), 16× sobre el base real `0x100_0000_0000` (2^40) → todo RIP de
  kernel fallaba el bound → writer_rip=0 silencioso. Corregido y verificado.
- Ventana demostrada (precisión 3): el write sintético dentro del tramo medio se caza.

**Coste (precisión 2)**: protect/unprotect + invlpg por switch (el camino caliente). Se mide
antes/después con el mismo comando; si la tasa cambia, es un dato (perturbación), no un fix.
**Hipótesis 4 (precisión 4)**: si el writer_rip sale como scheduler.rs:715/805/1136 (sitios
canónicos de SAVE) → aliasing de estructuras del scheduler (dos Process con el mismo
trapframe), no corrupción de memoria — lectura distinta, anotada.

## PAGE-PROTECT: INSTRUMENTO CONTRADICTORIO — "pendiente de validar" (supervisor, 2026-08-05)

**El resultado clave que invalida la medición**: tandas con page-protect:
- matrix1 (4104803): 24 boots, 22 OK + **2 BOX WRITTEN** (el box se escribió 2×).
- matrix2 (91446): ~24 boots, 22 OK + 1 BOX WRITTEN, con **`box_page_protected=true`** en el fire.

**Contradicción (el supervisor la cazó)**: el box fue escrito (detector) MIENTRAS su página
estaba write-protected (W claro, `box_page_protected=true`) y **el page-fault NO disparó**.
Con CR0.WP=1 (verificado: `CR0=80010033`), un write de CPU a esa dirección virtual DEBE
faultear. ⇒ **el write NO es una escritura de CPU a la dirección virtual del box.** Rama del
supervisor: (a) alias físico (doble-asignación — otra VA mapea el mismo frame), o (b) DMA de
hardware (el AC97 programa direcciones FÍSICAS en el BDL/ring; el bus-master queda RUNNING
tras init aunque el amp no use audio).

**Pasos del supervisor aplicados**:
1. `new_user()` LEE el clon: copia las entradas PML4 del kernel POR VALOR (punteros a PDPTs
   compartidos) → el split+W-clear del physmap (PML4 idx 5) es COMPARTIDO por todos los
   espacios. Verificado por código, no supuesto.
2. Instrumento nuevo: `box_watch_dump_all_tables()` (W + PT frame por cada CR3/proceso) +
   `box_watch_find_aliases()` (walk completo: ¿alguna otra VA mapea el frame del box?). La
   próxima campaña lo imprime en el fire.
3. Calibración ya hecha (el write sintético faulta) — re-validar en el mismo arranque si
   hace falta.
4. CR0.WP verificado = 1.

**Etiqueta**: las tandas matrix1/matrix2 NO concluyen nada sobre la tasa hasta validar el
instrumento (por qué el write no faulta). El sospechoso actual tras la contradicción: alias
de frame (doble-asignación del buddy) o DMA — el determinismo del contenido (frame C =
InterruptGuard::drop, idéntico en todos los fires) sugiere un copiador determinista, no un
hardware escribiendo basura.

## PAGE-PROTECT: VALIDACIÓN COMPLETA — la contradicción se resuelve (2026-08-05)

Boot con dump multi-tabla + alias (tras processes::init_all):
```
[BOXWATCH] box=0x2801ee70000 page=0x2801ee70000 pte=0x2801ee65380
  current CR3=0x101000: W=true PT_frame=0x1ee65000
  [alias current] 4KiB leaf at 0x2801ee70000 maps box frame 0x1ee70000   ← el PROPIO box
  run_queue (pml4=0x101000): W=true PT_frame=0x1ee65000   ← MISMO PT frame
  run_queue (pml4=0x1ef0c000): W=true PT_frame=0x1ee65000 ← MISMO PT frame
[BOXWATCH] box page W=CLEARED/SET ... en cada switch (0,1,2,3)   ← protect togglea
```

**Checks del supervisor, todos verificados:**
1. `new_user()` copia PML4 por valor → PDPT/PD/PT del physmap COMPARTIDOS → el split+W-clear
   es global. Verificado por código Y por el PT_frame idéntico (0x1ee65000) en todas las tablas.
2. **NO hay alias** del frame del box (el walk solo encuentra la VA del propio box). Sin
   doble-asignación.
3. CR0.WP=1 (dump QEMU `CR0=80010033`) → un write de CPU a una página RO DEBE faultear.
4. El protect togglea en cada switch (W=CLEARED/SET, switches 0-3). La calibración (write
   sintético) faultéa.
5. Los ÚNICOS escritores del box son los 3 saves canónicos (747/837/1168), todos con
   tf_note_save.

**Consecuencia lógica**: el box cambió (detector: box=C vs record=A) sin que ninguna
escritura de CPU a su VA faultéa (RO+WP=1), sin alias, y sin save no-canónico. Un DMA no
puede producir un marco coherente (frame C = InterruptGuard::drop real, de un tick previo en
el kstack de pid=1). ⇒ **el "write" no es una escritura a la dirección virtual del box.**

**Hipótesis que queda (la 4 del supervisor, ahora la más plausible)**: el box CONTENIDO
(frame C) es un SAVE legítimo de pid=1 (fue preemido en el tail del syscall en algún
momento), y el RECORD (frame A) es el que no avanzó — es decir, el box fue re-saveado con
frame C por un save cuyo `tf_note_save` registró en OTRO Process (aliasing de estructuras del
scheduler: dos Process compartiendo el mismo trapframe box, o un running/update_current_fast
desincronizado), NO una corrupción de memoria. El instrumento (page-protect) NO puede cazar
esto (no hay escritura salvaje a la VA del box) — queda invalidado para esta rama.

**Etiqueta**: tandas matrix1 (2/24), matrix2 (1/24), matrix3 (0/24 hasta ahora) con
page-protect — la tasa está contaminada por el layout aislado Y por la rama sin resolver. NO
concluyen. Pendiente: discriminar "box re-saveado por un save de otro Process" (aliasing)
vs "corrupción del record".

## DISCRIMINADOR DE IDENTIDAD — RESULTADO DECISIVO (tanda 423431, HANG iter 1053)

```
=== HANGHUNT BOX WRITTEN BETWEEN SAVE AND RESUME ===
  site=switch_to_next pid=1 box=0x2801f31ed00 box_page_protected=false
  saved: rip=0x100002c8bf0 rsp=0x2801fe9ea10 (mid-memcpy)
  now:   rip=0x10000282cd7 rsp=0x2801fe9faa8 (InterruptGuard::drop)
  seq: tf_seq=1431 tf_last_resumed_seq=Some(1431)   switches: 1430→1431 (delta=1)
  IDENTITY: saved proc=0x2801ef0d000 box=0x2801f31ed00
            resume proc=0x2801ef0d000 box=0x2801f31ed00
            → proc_differ=FALSE box_differ=FALSE
```

**El discriminador del supervisor: `proc_differ=false box_differ=false` — REFUTADA la
hipótesis "no es el mismo box".** El SAVE y el RESUME usan la MISMA instancia de Process
(0x2801ef0d000) y el MISMO box (0x2801f31ed00). El contenido del MISMO box cambió de frame A
a frame C entre el SAVE y el RESUME.

**Reconciliación con el page-protect**: sin el aislamiento (layout de referencia, box en
página normal sin proteger), el write es un write de CPU normal (no faultéa porque no hay
protección). La contradicción del page-protect (RO+WP=1 sin fault) sigue siendo un misterio
separado, PERO la identidad descarta que fuera "otra instancia". El write es al mismo box.

**Qué implica**: delta=1 (mismo-switch: save+resume en el mismo switch_to_next), el box copy
escribe frame A (el récord), y entre el copy y el resume el MISMO box se reescribe con frame C
(un marco stale del kstack de pid=1). El tramo medio corre: read_fs_base, **fpu::save
(fxsave)**, push_back run-queue, pop, activate (CR3), set_kernel_stack, write_fs_base,
fpu::restore, inc_switches. Ninguno escribe el box de forma evidente → se añadió al volcado
la dirección del `fpu_state` + overlap con el box (pendiente del siguiente fire).

**Conclusión intermedia**: el bug es de gestión del scheduler/SAVE, NO una escritura salvaje
de memoria ni un DMA. El write al mismo box ocurre en el tramo medio del switch. El siguiente
dato: si `fpu_state` solapa el box (fxsave escribe 512B encima), o el push_back del run-queue
realloc escribe sobre el box.

## VENTANA ESTRECHADA — el box copy NO escribe current_tf (determinista, 2+ fires)

Checks de ventana añadidos (aditivos): box vs récord tras el box copy, tras fpu::save, antes
de push_back, y en el manejo del proceso entrante.

**Fires deterministas** (tanda 793787: 2 fires idénticos; tanda 988685: 2 fires idénticos):
```
[BOXHUNT] box MISMATCH right after box copy: record_rip=0x100002ca150 box_rip=0x10000284237
  current_tf=0x2801fe9e970 cs=0x8
[BOXHUNT] box MISMATCH right after box copy: record_rip=0x100002ca8a0 box_rip=0x10000284987
  current_tf=0x2801fe9ea80 cs=0x8
record_rip → copy_backward (mid-memcpy) | box_rip → InterruptGuard::drop (tail del syscall)
```

**El box copy `*proc.trapframe = *current_tf` NO escribe el marco del récord al box** —
inmediatamente después, el box contiene un marco del tail (InterruptGuard::drop), no el de
current_tf (copy_backward). El récord (frame A, de tf_note_save leyendo current_tf) y el box
(frame C) divergen EN EL PROPIO COPY.

Esto refuta la rama "escritura salvaje" definitivamente: no hay escritor externo; el problema
es el propio mecanismo SAVE — o el récord es stale (tf_note_save no actualizó) o el copy lee
otra fuente. Pendiente: imprimir `(*current_tf).rip` en el check (si = box_rip → current_tf ya
era frame C y el récord es stale; si = record_rip → el copy escribe desde otra parte).

## EL BOX COPY NO COPIA current_tf — DATO DECISIVO (tanda 1353345, 2 fires idénticos)

```
[BOXHUNT] box MISMATCH right after box copy:
  record_rip=0x100002ca980    ← copy_backward (frame A)
  box_rip=0x10000284a67       ← InterruptGuard::drop (frame C)
  current_tf=0x2801fe9ea20
  current_tf_rip=0x100002ca980 ← **current_tf = EL MISMO marco que el récord (frame A)**
```

**Interpretación (cerrada)**: `current_tf_rip == record_rip` ⇒ el récord es CORRECTO (de
current_tf = frame A). Pero el box, inmediatamente después de `*proc.trapframe =
*current_tf`, contiene frame C (InterruptGuard::drop), NO frame A. Entre el copy y el check no
corre NADA (el check es la siguiente sentencia). ⇒ **el box copy no copia current_tf al box:
o escribe desde otra fuente (register stale / miscompilación), o no escribe y el box retiene
un marco antiguo.** Determinista (2 fires idénticos), fuente == récord.

**Consecuencia**: NO hay escritor externo, NO hay DMA, NO es otra instancia (identity
proc_differ=false box_differ=false), NO es fpu (no solapa). El bug está en el propio
mecanismo SAVE — el `*proc.trapframe = *current_tf` no produce el frame esperado en el box.
Esto explica TODO: el resume iretqa el marco stale del box (frame C) → abandona la syscall en
vuelo → orphan; o el ret salta a datos → [BUG2]. Es un bug del copy (compiler codegen de la
copia de 160 bytes bajo `-Z build-std`, o aliasing no anticipado), no corrupción de memoria.

**Nota de método (supervisor)**: los checks del tramo medio (4 ramas/compares por switch)
perturbaron la tasa (0/24 en una tanda). Eliminados — instrumento silencioso (stores en
memoria, print solo en fire). Se verifica que la tasa vuelve a línea base (~4%) con una tanda
silenciosa; si 0/24 con silencio, es un dato.

## CAMPAÑA SILENCIOSA (1510757): la tasa se recuperó + identidad confirmada

24 boots: 22 OK + 1 HANG (orphan, iter 2). **La tasa volvió a ~4%** con el instrumento
silencioso (los checks del tramo medio ERAN la perturbación — 0/24 en la tanda anterior).
Fire con identidad: `proc_differ=false box_differ=false` de nuevo — MISMO box, MISMA
instancia.

**Estado consolidado del mecanismo** (con los datos de las tandas 1353345 + 1510757):
1. El box del RESUME es el MISMO que el del SAVE (identidad, 4+ fires).
2. `current_tf` contiene el MISMO marco que el récord (current_tf_rip == record_rip).
3. El box, inmediatamente después de `*proc.trapframe = *current_tf`, contiene OTRO marco
   (InterruptGuard::drop), NO el de current_tf. Determinista.
⇒ **el copy de 160 bytes `*proc.trapframe = *current_tf` no escribe el marco de current_tf en
el box.** El bug está en el mecanismo SAVE, no en memoria. Siguiente paso natural: el
desensamblado de ESE copy (el supervisor lo autoriza ahora que la identidad dice que es el
mismo box) — para ver si el compilador genera la copia desde un registro/offset equivocado
(possible miscompilación bajo `-Z build-std` nightly), o si hay aliasing no anticipado.

## VOLATILE+FENCE — el check contra el octavo instrumento defectuoso (supervisor)

El claim "el copy no escribe current_tf" es extraordinario; la explicación mundana es una
lectura del check reordenada por encima del copy (aliasing: &mut Box vs raw pointer). Check
nuevo en el box copy: `compiler_fence(SeqCst)` + `read_volatile` del box a través del MISMO
puntero que escribe el copy, y volcado de los 5 campos del box Y de current_tf en el fire.

**Expectativa declarada**: si la discrepancia sobrevive a volatile+fence → el copy
genuinamente no escribe (desensamblado justificado); si desaparece → era el instrumento
(read reordenada), octavo caveat, y el box copy es correcto (el marco stale llega por otra
vía — el récord/SAVE real). Con volatile no hay reordenación posible que lo explique.

## OCTAVO INSTRUMENTO DEFECTUOSO (el mío) — check roto + observación clave

La versión inicial del check volatile+fence leía `read_volatile(box_ptr)` = offset 0 = **r15**,
comparado con `tf_saved_five[0]` = **rip** (offset 120) → 100% de fires falsos (r15≠rip
siempre). Corregido: `read_volatile((box_ptr as *const u64).add(120/8))` lee el rip real.

**PERO el volcado de los fires falsos es oro**: en TODOS los boots,
`record == box == current_tf` (5 campos idénticos) — **el box copy SÍ escribe current_tf al
box** (volatile, sin reordenación posible). Esto apunta fuertemente a que el "mismatch" de los
checks anteriores (boxhunt-matrix2/4) era una **lectura reordenada** (la hipótesis del
supervisor), no un copy roto. El check corregido (offset 120) decidirá: si dispara ~4% con
box≠current_tf → el copy falla de verdad (sobrevive a volatile+fence); si no dispara en los
boots con bug → octavo caveat confirmado y el marco stale llega por otra vía (el SAVE/récord
real).

## VOLATILE+FENCE CORREGIDO — EL MISMATCH SOBREVIVE (2 fires, determinista)

Tanda 1737406 (check corregido, offset 120): 2 BOXHUNT2 fires (iters 863, 557), idénticos:
```
record=[rip=0x100002ca190 cs=0x8 rsp=0x2801fe9eac0]   ← copy_backward
box=[rip=0x10000284277 cs=0x8 rsp=0x2801fe9faa8]      ← InterruptGuard::drop
current_tf=[rip=0x100002ca190 cs=0x8 rsp=0x2801fe9eac0] ← == record
```

**El box copy `*proc.trapframe = *current_tf` NO escribe current_tf al box, con fence +
volatile** (sin reordenación posible) — en los boots con bug (~4%). En los boots SANOS, el
copy funciona (el volcado de los fires falsos del r15 mostró box==record en todos).

**El claim se sostiene** (criterio del supervisor: sobrevive a volatile+fence → desensamblado
justificado). Pero el matiz importante: la intermitencia (~4%, no siempre) contradice una
miscompilación estática. El box retiene su contenido PREVIO (frame C, determinista). Esto
apunta a que en los boots con bug el copy no se ejecuta / escribe en otro sitio / el
destino cambia — y el desensamblado del copy (autorizado) es el siguiente paso.

## CAVEAT (el más caro de la fase) + SENTINEL TEST (supervisor, 2026-08-05)

**Caveat del offset r15/rip**: el check volatile+fence inicial leyó `read_volatile(box_ptr)`
= offset 0 (r15) comparado contra el récord (rip, offset 120) → 100% de fires falsos, y
**contaminó la conclusión de identidad** que se obtuvo en esa fase. Regla: cualquier lectura de
un campo del TrapFrame por offset crudo debe usar offset 120 para rip (r15..rax ocupan
0..112). Anotado como el instrumento más caro de la fase: invalidó conclusiones previas, no
solo mediciones.

**Operandos intercambiados DESCARTADOS por datos ya en mano**: si `*current_tf = *box`
(invertido), el kstack en current_tf habría quedado pisado con frame C. Pero el volcado
volatile+fence muestra `current_tf == record == frame A` DESPUÉS del copy → el kstack no fue
pisado. Hipótesis muerta, no gastar tiempo.

**SENTINEL TEST (nuevo)**: `write_volatile(box.rip, 0xDEADBEEF)` ANTES del copy, copy,
`compiler_fence`, `read_volatile`. Decisor:
- box_rip == current_tf.rip → el copy FUNCIONA (el problema es la lectura del fire — 9º caveat).
- box_rip == 0xDEADBEEF → el copy NO SE EJECUTÓ (desensamblado es el paso obvio).
- box_rip == frame C → el copy escribió otra cosa (fuente stale).
La identidad (punto 1 del supervisor) se revalida en la MISMA tanda con el check corregido.

## DÉCIMO CAVEAT (el más transferible) + revalidación pendiente (supervisor)

**Caveat de procedencia**: "en Rust, un instrumento que observa memoria escrita por otro
camino debe compartir la PROCEDENCIA del puntero, no solo el orden. `volatile` y
`compiler_fence` ordenan accesos; no hacen que el compilador crea que dos punteros distintos
apuntan al mismo sitio." El centinela anterior derivó el puntero del box de `&*proc.trapframe`
(compartido) mientras la copia real va por el `&mut` del Box → dos procedencias que LLVM puede
tratar como ajenas. El `compiler_fence` no lo arregla (no afecta al análisis de alias). Por
eso "el copy no se ejecuta" era indistinguible de "el compilador cree que son memorias
distintas".

**Nuevo instrumento (BOXHUNT4, procedencia única)**: TODO — sentinel, la copia, y la lectura —
por UN solo puntero derivado de `&mut *proc.trapframe` (Unique). La copia se hace por el mismo
puntero (write_volatile de read_volatile(current_tf)). Resultado:
- mismatch desaparece → era el instrumento (el copy siempre funcionó; el marco stale llega por
  otra vía — volver a identidad/dos instancias con checks de procedencia correcta).
- mismatch sobrevive → real, desensamblado plenamente justificado.

**CONCLUSIONES PENDIENTES DE REVALIDAR** (apoyadas en instrumentos rotos — que nadie
construya encima sin rehacerlas):
1. "Mismo Process, mismo box" (proc_differ=false) — del check de identidad en la fase del
   check r15/rip roto.
2. "El box copy no escribe current_tf" (volatile-matrix2) — del check con procedencia
   compartida.
3. "El copy no se ejecutó" (centinela sentinel-matrix1) — del centinela con procedencia
   compartida.

## PROCEDENCIA ÚNICA — 1 fire (centinela sobrevive) pero perturbación 0/24

- provenance-matrix1 (mismo código, sin box_ptr en el dump): 24 boots, 1 fire BOXHUNT4 —
  `box_rip=0xdeadbeefdeadbeef` (el centinela) tras `write_volatile(box_ptr, read_volatile(current_tf))`
  por un puntero `&mut` Unique. Si es real, el copy no llega al box.
- provenance-matrix2 (con box_ptr en el dump): 24/24 OK, 0 fires — el copy volatile de 160
  bytes por switch perturba y suprime el bug (la enésima vez que un instrumento en el camino
  caliente lo esconde).

**Estado**: falta UN fire con `box_ptr` en el volcado para saber si el destino del copy es
realmente 0x2801f31ed00. El centinela de procedencia única (1 fire) es el dato más fuerte de
que el copy no escribe; pero la perturbación del instrumento bloquea la reproducción. La
elección: (a) más campañas esperando un fire con box_ptr, o (b) el desensamblado (autorizado
por el supervisor si el mismatch sobrevive a procedencia única — y sobrevivió en el 1 fire).

## INSTRUMENTO CORRECTO (supervisor): copy NO volátil por box_ptr + 3 punteros

Corrección del fallo de razonamiento: `box_ptr` se deriva UNA vez y las 3 operaciones usan el
mismo local → un puntero malo sería malo consistentemente (el copy pisaría el centinela). Que
el centinela sobreviva es incompatible con un puntero compartido malo. El copy no escribe.

Instrumento nuevo (BOXHUNT5): `let box_ptr = &mut *proc.trapframe` una vez; centinela
write_volatile u64; **copia NO volátil `*box_ptr = *current_tf`** (la volatilidad de 160 bytes
era la perturbación); fence; read_volatile u64. En el fire: box_ptr + re-derivación `&mut
*proc.trapframe` + `&*proc.trapframe as *const _` (si difieren → el CAMPO puntero del Process
se corrompe). Coste por switch: 2 accesos de 8 bytes + la copia que ya existía.

**Expectativa**: (a) punteros coinciden + centinela sobrevive → el copy no escribe (desensamblado
autorizado); (b) punteros difieren → corrupción del campo proc.trapframe; (c) centinela
desaparece → el copy volátil era la perturbación (10º caveat), copy correcto.

## BOXHUNT5 — VEREDICTO DECISIVO (2 fires idénticos, iters 848/1198)

```
box_ptr=0x2801f31ed00 rederived_mut=0x2801f31ed00 rederived_shared=0x2801f31ed00
box_rip=0xdeadbeefdeadbeef   ← centinela sobrevive al copy *box_ptr = *current_tf
expected=0x100002ca470       ← current_tf.rip
```

- Los tres punteros COINCIDEN → el campo `proc.trapframe` del Process no se corrompe.
- El centinela SOBREVIVE a un copy NO volátil por un solo puntero `&mut` (procedencia única,
  sin perturbación) → el store de 160 bytes no llega al box en esos boots (~4%).
- Ramas descartadas: instrumento (procedencia única), campo puntero, memoria (copy no
  escribe), DMA, alias, dos instancias.
- **Desensamblado del store autorizado y justificado** (criterio del supervisor: punteros
  coinciden + centinela sobrevive).

## DESENSAMBLADO DEL BOX COPY (autorizado; tras el veredicto BOXHUNT5)

El copy `*box_ptr = *current_tf` (switch_to_next, en 0x247b3a+) compila a:
1. 0x247b99-0x247bb5: `memcpy(temp@0x540(%rsp), current_tf, 0xa0)` — corriente_tf a un temp
   de pila (el staging existe porque current_tf, raw ptr, podría aliasing el box).
2. Ramas de UB-check entre las dos etapas (`and $7` / `cmp $0` / `sete` en box_ptr, llamadas
   a `check_language_ub`, paths de panic con strings en 0x2d70b0/0x2d70f8).
3. 0x247c14-0x247c30: `memcpy(box_ptr, temp@0x540(%rsp), 0xa0)` — el write real al box.

**Hallazgo**: el write al box es un memcpy INDEPENDIENTE (0x247c30), y su ejecución depende
del flujo de las ramas de UB-check entre la etapa 1 y la etapa 2. En los boots con bug (el
centinela sobrevive), esa segunda etapa no escribe al box. El candidato: una rama de UB-check
del nightly (`-Z build-std` con ub-checks) que en esos boots toma un camino que salta el
memcpy al box — o los slots de pila que el UB-check usa (0x1a8(%rsp)=box_ptr, 0x268(%rsp))
son los corruptos, no el box.

**Consecuencia**: no es el copy "roto" en abstracto — es que el camino de código que lo
ejecuta no lo ejecuta en los boots con bug. Siguiente: o identificar la rama exacta que salta
(seguir el flujo con un fire de nuevo + step), o probar SIN ub-checks (si el bug desaparece,
es el UB-check del nightly). Decisión del supervisor.

## VOLCADO DEL SLOT DE PILA (supervisor: lectura 2 = corrupción de la pila de PID 1)

El memcpy final del copy recarga `box_ptr` de `0x1a8(%rsp)` (desensamblado 0x247c14). Si ese
slot está corrupto, el copy escribe a otra dirección (el box real conserva frame C, el
centinela sobrevive) mientras el check (que lee el registro) ve el box_ptr correcto. Nuevo
dump en el fire: box_ptr + re-derivados + `field_addr` (dirección del campo proc.trapframe) +
slot[rsp+0x1a8] + slot[rsp+0x268] + 8 palabras alrededor. Decisor:
- slot[rsp+0x1a8] != box_ptr → slot corrupto → el copy escribe a otra parte → corrupción de
  la pila de PID 1 (unifica orphan + [BUG2] + intermitencia).
- slot == box_ptr → el destino era correcto y aun así no escribió → codegen.

## SLOTS DE PILA VERIFICADOS — destino y fuente CORRECTOS (tanda 2809649, 2 fires)

```
box_ptr=0x2801f31ed00 (los 3 punteros coinciden) field_addr=0x2801ef0d490
slot[rsp+0x298]=0x2801f31ed00   ← el slot de box_ptr que el memcpy recarga: CORRECTO
slot[rsp+0x358]=0x2801fe9e970   ← el slot de current_tf: CORRECTO
box_rip=0xdeadbeefdeadbeef      ← el centinela sobrevive
```

Flujo verificado en el desensamblado del build actual: el `jne 247f34` (box_ptr≠0) SÍ lleva al
`memcpy(box_ptr, temp@0x630(%rsp), 0xa0)` que escribe al box. El memcpy es el
`compiler_builtins::mem::memcpy` estándar (rep movsb/movsq). Destino y fuente correctos,
memcpy alcanzado, y el box no cambia.

**Punto máximo alcanzado**: el copy se ejecuta con argumentos correctos y no actualiza el box.
Negativos acumulados (todos verificados): no es corrupción de memoria del box (page-protect
sin fault), no son dos instancias (identidad), no es DMA, no es el campo puntero del Process
(3 punteros coinciden), no es el slot de box_ptr (correcto). La corrupción de la pila de PID 1
(supervisor) sigue siendo la hipótesis unificadora, pero el write al box con args correctos que
no llega a la memoria física no está explicado por ninguna rama.

## CAUSA RAÍZ CONFIRMADA — EL FLAG DE DIRECCIÓN (DF) (supervisor, verificado en código + datos)

**Mecanismo**: `rep movsb` obedece a DF. Con DF=1 copia HACIA ATRÁS (escribe en dest-159..dest).
El memcpy del box copy usa `rep movsb`. Si el kernel entra con DF=1, el memcpy escribe los 160
bytes ANTERIORES al box (no el box) → el centinela sobrevive, el box conserva su marco rancio,
y los vecinos del slab quedan pisados con basura (→ saltos [BUG2] a &MOUNTS/nodo).

**Dos vectores verificados en el código**:
1. `tss.rs:154`: `wrmsr(IA32_FMASK, 1 << 9)` — FMASK solo limpia IF (bit 9). DF (bit 10) NO
   está enmascarado → el DF de usuario entra intacto al kernel por `syscall`. (Linux pone DF
   en FMASK.)
2. **Cero `cld` en todo `kernel/src/`** (grep). El ISR del timer (`timer_preempt.rs`) y
   `syscall_entry_fast` (`syscall/mod.rs`) no lo emiten. (El shim de rustc de los handlers
   x86-interrupt SÍ emite `cld` — verificar.)

**La pistola humeante (ya en los datos)**: el rip del récord = `copy_backward` en TODOS los
fires. El memmove de compiler_builtins para el caso solapado hacia atrás hace `std` → rep
movsb → `cld`. Un tick entre `std` y `cld` entra al kernel con DF=1. Y las rflags de los
marcos grabados confirman: `0x10606`/`0x10602` (bit 10 PUESTO) en TODOS los fires, frente a
`0x10202` (DF limpio) en los marcos "now".

**Encaja con todo**: memcpy con args correctos que no escribe al box ✔; centinela (store
directo) sí ✔; página protegida sin fault (el write fue a otra dirección) ✔; corrupción de
los 160 bytes anteriores al box → punteros basura → [BUG2] ✔; resume del marco rancio →
orphan ✔; intermitencia ~4% (ventana de pocas instrucciones entre std y cld) ✔.

**FIX (dos piezas)**: (1) `wrmsr(IA32_FMASK, (1<<9)|(1<<10))`; (2) `cld` como primera
instrucción del ISR del timer y de `syscall_entry_fast`. Medir antes/después N≥24, firmas
separadas.

## FIX APLICADO (2026-08-05)
1. `tss.rs`: `IA32_FMASK = (1<<9)|(1<<10)` — DF enmascarado en la entrada de syscall.
2. `cld` como primera instrucción del ISR del timer (`timer_preempt.rs`) y de
   `syscall_entry_fast` (`syscall/mod.rs`). (El shim de rustc de los x86-interrupt ya emite
   `cld` — verificado en page_fault_handler.)
El check BOXHUNT5 (centinela) se mantiene: con DF=0 el copy escribe hacia adelante → el box =
current_tf → no dispara. Medición DESPUÉS: N≥24.

## MEDICIÓN DESPUÉS DEL FIX — 24/24 OK (0/24)
Primera tanda post-fix (3054581): **24 boots, 24 OK, 0 PANIC, 0 HANG, 0 BOXHUNT5, 0 BOX
WRITTEN**. El bug desapareció con FMASK(DF) + cld. Se confirma con una segunda tanda de 24.

## CIERRE — EL FLAG DE DIRECCIÓN ERA LA CAUSA RAÍZ (48/48 OK post-fix)

**Medición**: dos campañas de 24 post-fix (3054581, 3237028): **48/48 OK, 0 PANIC, 0 HANG,
0 BOXHUNT5, 0 BOX WRITTEN**. Antes: ~4-8% (1-2/24) en las campañas con instrumento silencioso
y layout de referencia.

**Confirmación en el mecanismo (el listón del supervisor: código + evidencia viva)**:
1. El rip del récord = `copy_backward` en TODOS los fires (el memmove de compiler_builtins
   para el caso solapado hace `std` → rep movsb → `cld`).
2. Las rflags de los marcos grabados = `0x10606`/`0x10602` → **bit 10 (DF) PUESTO** en todos
   los fires; los marcos "now" = `0x10202` (DF limpio).
3. `tss.rs:154` FMASK solo enmascara IF (bit 9) — el DF de usuario entra intacto al kernel;
   cero `cld` en los stubs custom (el shim de rustc de los x86-interrupt sí emite `cld`).
4. `rep movsb` con DF=1 copia HACIA ATRÁS → el memcpy del box copy escribe a box_ptr-159..box_ptr
   → el box conserva el centinela/marco rancio (orphan) y los 160 bytes anteriores al box
   quedan pisados → punteros basura → saltos [BUG2] a &MOUNTS/nodo.

**Fix**: `IA32_FMASK = (1<<9)|(1<<10)` + `cld` en el ISR del timer y `syscall_entry_fast`.
Un solo mecanismo explicó las tres manifestaciones (orphan, [BUG2], BOX WRITTEN).

**Lecciones de método acumuladas**: 10 caveats, los más importantes — el check del offset
r15/rip invalidó conclusiones previas; el de la procedencia del puntero (volatile/fence
ordenan accesos, no hacen que el compilador crea que dos punteros apuntan al mismo sitio);
y esta: cuando un store con argumentos correctos no llega a su destino, sospechar del flag de
dirección antes que del compilador. El candidato `copy_backward` en el récord era la pista
de oro desde el primer volcado.
