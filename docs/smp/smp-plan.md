# Plan: SMP (varias CPUs)

> **Estado (2026-09-24):** etapa 0 hecha (reglas SMP-ready en el
> CLAUDE.md, `memory::tlb`, `keyboard::DECODER` tras un `IrqMutex`);
> etapa 1 hecha y verificada en QEMU y en la Ryzen (LAPIC timer + I/O APIC,
> `hal::apic`); etapa 2 hecha y verificada en QEMU y en la Ryzen
> (`cpu::percpu`, `swapgs` solo en la entrada de `syscall`). El kernel sigue siendo
> de una sola CPU: `cpu::cpu_id()` lee el TR y da `0` hasta que la etapa 3
> dé una TSS a cada CPU, y no hay IPIs ni arranque de APs.

## Por qué ahora

Cada característica que se añade suponiendo una sola CPU es código que
habrá que auditar después. La deuda no crece por igual: casi no crece con
lo que vive en los crates extraídos (`hal`, `ext2`, `usock`, `sched`…,
lógica pura detrás de un lock) ni con programas de userspace. Crece con
lo que toca **estado global del núcleo**, **usa IF=0 como exclusión** o
**cambia tablas de páginas** sin shootdown. Lo siguiente en la lista
(`memfd_create` + `MAP_SHARED`, un compositor) cae de lleno en la tercera
categoría. Por eso SMP va antes.

## Principio de ejecución

**Cada etapa entra en master funcionando con una sola CPU** (salvo la 7,
que es la que enciende las demás) y verificada antes de pasar a la
siguiente:

- `scripts/boot-matrix.sh 4 5`: 20 arranques limpios.
- `scripts/run-kernel-tests.sh` en verde.
- Los tests de host de los crates que se toquen.
- Arranque con `QEMU_DEBUG_MEM=8G` (la forma de la máquina real).
- A partir de la etapa 4, también con `-smp 4`.

Nada de ramas largas medio-SMP. Si una etapa se para a medias, lo que ya
entró tiene que dejar el kernel mejor que antes, no roto.

## Inventario del estado que asume una sola CPU

Medido con grep sobre `kernel/src` el 2026-09-23. Es la lista de trabajo
de la etapa 6. Se amplía si aparece algo más, nunca se recorta sin
justificarlo.

### Entrada al kernel y registros por CPU

| Qué | Dónde | Problema en SMP |
|---|---|---|
| `SYSCALL_USER_RFLAGS` (`static mut`) | `process/syscall/mod.rs:54`, escrito en `:87` | Dos CPUs en `syscall` a la vez se pisan el RFLAGS de usuario |
| `KERNEL_RSP0` (`static mut`) | `process/tss.rs:27`, leído en `syscall/mod.rs:91` | **Todas las CPUs cargarían la misma pila del kernel.** Es el bloqueante número uno |
| `TSS` (`static mut`) + pila IST de double fault | `process/tss.rs:20`, `:39`, `:49` | Cada CPU necesita su TSS (RSP0 propio) y sus pilas IST |
| `GDT` (`Once`) | `process/tss.rs:30` | Contiene el descriptor de *una* TSS; hace falta una GDT (o un slot de TSS) por CPU |
| MSRs `EFER`/`STAR`/`LSTAR`/`FMASK` | `process/tss.rs:132-154` | Son por CPU: cada AP debe programarlos |
| `IA32_PAT` | `memory/memtype.rs:282` `program_pat` | Por CPU. Si un AP no lo programa, el índice 1 no es WC en esa CPU y **el mismo mapeo tiene tipos distintos según la CPU** (el SDM lo prohíbe) |
| CR0/CR4 de SSE (`fpu::init`) | `process/fpu.rs` | Por CPU |
| `IDT` (`Once`) | `init/devices.rs:31` | Se puede compartir, pero cada CPU hace su `lidt` |
| No hay `swapgs` ni `GS_BASE` | — | No existe un sitio donde guardar datos por CPU |

### Interrupciones y tiempo

| Qué | Dónde | Problema en SMP |
|---|---|---|
| EOI al PIC por puerto `0x20` | `process/timer_preempt.rs:155` | Con LAPIC el EOI se hace por MMIO/MSR y es por CPU |
| PIT como reloj de planificación | `pit.rs`, `init/devices.rs` | Es una sola fuente global; cada CPU necesita su tick (LAPIC timer) |
| Calibración del TSC contra el PIT | `cpu/tsc.rs:84` | Sirve; el LAPIC timer se calibra contra el TSC ya calibrado |
| Trabajo global dentro del tick | `timer_preempt.rs:159-186` | `tick_cursor_blink`, `usb::poll`, `hrtimer::tick`: con un tick por CPU correrían N veces por período. Deben quedarse en la BSP |

### Estado sin lock que confía en "IF=0 = nadie más"

| Qué | Dónde | Problema en SMP |
|---|---|---|
| `keyboard::DECODER` (`UnsafeCell`) | `keyboard.rs:45` | Su comentario dice "solo desde el ISR del teclado", pero `usb::poll` también llama a `process_scancode` desde el ISR del timer. Con una CPU es seguro por IF=0. Con dos: IRQ1 en una CPU y el timer en otra → carrera sobre el mismo decoder |
| `mouse::DECODER` (`UnsafeCell`) | `mouse.rs:85` | Ídem con IRQ12 |
| `klog::SCRATCH` | `klog.rs:173` | Buffer compartido; revisar quién lo usa y bajo qué exclusión |
| `logpart::SCRATCH` | `block/logpart.rs:96` | Ídem |
| `cow::FRAME_REFCOUNTS` (`static mut` a una tabla de `u8`) | `memory/cow.rs:48` | Los inc/dec de refcount tienen que ser atómicos. `COW_IF_VIOLATIONS_DEC_REF` existe precisamente porque hoy dependen de IF=0 |
| Transferencias USB "con IF=0 y `CONTROLLERS` tomado" | `usb/mod.rs` | El lock sigue excluyendo; lo que ya no vale es el argumento de que IF=0 impide que otro lector del anillo gire sobre un holder expropiado. Con un spinlock real sigue siendo correcto, pero el coste (~1 ms con IF=0 por 64 KiB) ahora lo paga solo una CPU |

`klog::BUF` ya es seguro en SMP: reserva con `fetch_add`.

### Locks globales (`static … Mutex`)

Tomados con `spin::Mutex` y, en su mayoría, con IF=0 alrededor. En SMP
siguen excluyendo; lo que hay que revisar en cada uno es **orden de
locks** (dos CPUs pueden tomar A→B y B→A de verdad, no solo en teoría) y
si algún ISR lo toma sin `try_lock` (con una CPU un ISR no puede
encontrarse el lock tomado por otro código *activo*; en SMP sí, y
esperar es legítimo siempre que el holder no esté en la propia CPU).

`AC97`, `SERIAL`, `TERMIOS`, `FRAMEBUFFER`, `EXT2_LOCK`, `ATA_LOCK`,
`FB_STATE`, `STDIN_WAITER`, `FUTEX_WAITERS`, `QUEUE` (hrtimer),
`EPOLL_INSTANCES`, `EPOLL_FD_MAP`, `POLL_WAITERS`, `CONTROLLERS`,
`logpart::STATE`; y los `IrqMutex`: `BUDDY`, `SLAB_ALLOCATOR`, `SOCKETS`,
`WAITERS`. `MOUNTS` (`vfs::MountTable`) tiene su propio lock interno.

### Scheduler

- `SCHEDULERS: [Mutex<Scheduler>; MAX_CPUS]` (`process/scheduler.rs:283`)
  y `local_scheduler()` (`:300`) suponen que **el scheduler local lo
  contiene todo**: `all_pids()` (`:1091`) itera solo el local, y lo
  mismo `kill`, `waitpid` y los despertares por pid. Con colas por CPU,
  cada una de esas búsquedas tendría que recorrer todas las CPUs y tomar
  varios locks de scheduler a la vez.
- Liberación diferida de kstacks (`sys_exit`, `scheduler.rs:891-913`):
  la guarda compara con el RSP interrumpido *de esta CPU*. En SMP una
  kstack encolada puede seguir en uso en otra CPU.
- Un proceso que otra CPU acaba de dejar puede ser elegido por una
  segunda CPU antes de que la primera haya salido de su kstack (el
  problema clásico de `on_cpu` en Linux).
- Hay un solo proceso idle.

### TLB

Invalidación local, sin shootdown. **Desde la etapa 0 toda pasa por
`memory::tlb`** (`invalidate_page`, `invalidate_kernel_page`,
`invalidate_all_this_cpu`); la etapa 5 cambia esos cuerpos. El inventario
de arriba se quedó corto: eran 21 sitios, no 10 — también
`demand_paging.rs` (3), `user_code.rs`, `user_pages.rs` (2),
`paging.rs` (2) y los `map_to(...).flush()` de `page_table_manager.rs`.
Uno no hacía lo que parecía: `split_physmap_2m` recargaba CR3, que no
tira entradas GLOBAL; ahora es un `invlpg` dentro de la página de 2 MiB.
- `Cr3::write`: `page_table_manager.rs:225`.

Los hilos (`clone`) comparten `AddressSpace`. Si dos hilos corren en dos
CPUs y uno hace `munmap`, resuelve un fallo COW o cambia permisos, la otra
CPU conserva la traducción vieja. **No falla nada visible: corrompe
memoria.** Los mapeos del kernel (`memory::mmio::map`, el WC del
framebuffer) tienen el mismo problema, pero en todas las CPUs.

## Decisiones de diseño

1. **Un solo lock de scheduler para empezar**, no colas por CPU. El
   `Scheduler` pasa a tener `running: [Option<Box<Process>>; MAX_CPUS]`
   y un solo `SchedCore`. Así `all_pids`, `kill`, `waitpid` y los
   despertares no cambian de forma, y el orden de locks sigue siendo
   trivial. Es lo que hizo Linux durante años. Las colas por CPU (con
   balanceo) son una optimización posterior, *medida*: hoy la carga típica
   tiene uno o dos procesos ejecutables.
2. **Datos por CPU vía `GS_BASE`**, con `swapgs` en cada entrada desde
   ring 3 (syscall, timer, excepciones que vengan de usuario). Una
   `struct PerCpu` por CPU con al menos: `self` (para leerse a sí misma
   con `gs:[0]`), `cpu_id`, `kernel_rsp` (sustituye a `KERNEL_RSP0`),
   `user_rflags_scratch` (sustituye a `SYSCALL_USER_RFLAGS`),
   `user_rsp_scratch`, y un puntero a su TSS. `cpu::cpu_id()` lee de ahí.
3. **La BSP hace el trabajo global del tick**: cursor, `usb::poll`,
   `hrtimer::tick`, flush periódico del log. Los APs solo planifican.
4. **Toda invalidación de TLB pasa por una sola API** en `memory`
   (etapa 0). Hoy es local; en la etapa 5 hace shootdown. `memory` no
   importa `process` (invariante del CLAUDE.md): la API recibe "qué CPUs
   pueden tener este espacio de direcciones cargado" como dato, que
   aporta quien llama.
5. **La lógica pura sale a `hal`**, siguiendo el precedente de siempre:
   registros del LAPIC/IOAPIC y la codificación de sus entradas, cálculo
   del divisor del LAPIC timer, secuencia INIT-SIPI-SIPI como máquina de
   estados, y la estructura del trampolín. Todo con tests de host. El
   hardware (MMIO, `wrmsr`, esperas) queda en `kernel`.
6. **Opción de arranque para volver a una CPU** (`nosmp`, o un límite de
   CPUs), para poder comparar al depurar. La etapa 7 no se da por cerrada
   sin ella.

## Etapas

### Etapa 0 — reglas SMP-ready y una API de TLB

Sin cambios de comportamiento. Frena la deuda antes de pagarla.

- Añadir a *Key Design Invariants* del CLAUDE.md:
  - **IF=0 no es exclusión.** Todo estado compartido lleva un lock real;
    `cli` solo evita reentrar en la propia CPU.
  - **Nada de `static mut` ni `UnsafeCell` globales nuevos** para estado
    compartido. Lo que sea por CPU se indexa por `cpu::cpu_id()`.
  - **Todo cambio de PTE pasa por la API de invalidación**, nunca por
    `invlpg`/`flush_all`/`MapperFlush::flush` sueltos.
  - **Nada nuevo cuelga del tick del timer** sin decir si es trabajo
    global (BSP) o por CPU.
- Crear la API (`memory::tlb::invalidate_page`, `invalidate_range`,
  `invalidate_all`, más una variante para mapeos del kernel) y migrar los
  10 sitios de invalidación del inventario (el `Cr3::write` es un cambio
  de espacio de direcciones, no una invalidación, y se queda donde está). `MapperFlush` se consume con `.ignore()` y
  se invalida a través de la API.
- Arreglar ya `keyboard::DECODER`: su comentario de seguridad es falso
  hoy mismo (lo llaman dos ISRs distintos). Con una CPU no hay carrera,
  pero la justificación debe decir la verdad o el decoder debe ir detrás
  de un lock.

**Hecho cuando:** `grep` no encuentra `invlpg`/`tlb::flush`/`.flush()`
fuera de la API, y los tests y `boot-matrix` siguen verdes.

### Etapa 1 — LAPIC + IOAPIC, todavía con una CPU

- Deshabilitar el 8259 (enmascarar todo tras remapear, para que sus
  espurias caigan en vectores manejados, no en 7/15; ver la memoria de
  la IRQ7 espuria de la Ryzen).
- Mapear el LAPIC (`memory::mmio::map`, uncached) o usar x2APIC si CPUID
  lo ofrece. Registrar el vector espurio del LAPIC.
- LAPIC timer en modo periódico a 100 Hz, calibrado contra el TSC.
  Sustituye al PIT como fuente de `timer_interrupt_entry`; EOI al LAPIC.
- IOAPIC desde la MADT (ya parseada en `acpi.rs`), con los
  *Interrupt Source Overrides* aplicados. Teclado (IRQ1), ratón (IRQ12),
  y lo que haga falta, rutados a la BSP.
- `/proc/acpi` o `/proc/kdebug` muestran qué controlador está en uso.

**Riesgos:** los overrides de la MADT (en QEMU la IRQ0 va al GSI 2),
polaridad y disparo de cada entrada, y que el PIT siga haciendo falta
para calibrar el TSC al arrancar (se usa antes de apagarlo).

**Hecho cuando:** arranca en QEMU y en la Ryzen con el LAPIC timer,
teclado PS/2 y USB funcionan, DOOM mantiene su velocidad (el tick sigue a
100 Hz), y `boot-matrix` sigue limpio.

**Estado:** hecha en QEMU el 2026-09-24 (`kernel/src/interrupts/apic.rs`,
`hal/src/apic.rs`, sección *Interrupt Controllers* del CLAUDE.md).
Verificado: `boot-matrix 4 5` = 20/20, `run-kernel-tests` PASS, `hal` 272
tests; teclado PS/2 y USB (`QEMU_DEBUG_NO_PS2=1 QEMU_USB_KBD=1`), 8G,
`timer_ticks` ≈ 100/s (523 en 5,26 s), y la ruta de respaldo al 8259 con
`-cpu max,-apic`, y el ratón PS/2 por IRQ12 (`mouse-move`/`mouse-button`
del monitor → `cat /dev/input/event1 | wc -c` da los bytes exactos). Pendiente:
la Ryzen (`/proc/kdebug`: `irq_controller`, `timer_ticks`, teclado USB).

Esto además abre la puerta a MSI para el xHCI (dejar de sondear el USB a
100 Hz), pero eso es un plan aparte.

### Etapa 2 — datos por CPU y entrada con `swapgs`

- `struct PerCpu` (decisión 2) con una instancia para la BSP, cargada en
  `KERNEL_GS_BASE`/`GS_BASE` en el arranque.
- `syscall_entry_fast`: `swapgs`, guardar el RSP de usuario y R11 en
  `gs:`, cargar la pila del kernel desde `gs:`. Desaparecen
  `SYSCALL_USER_RFLAGS` y `KERNEL_RSP0`.
- `timer_interrupt_entry` y los stubs `x86-interrupt`: `swapgs`
  condicional según el CPL del frame (si vino de ring 3). Los shims
  `x86-interrupt` de rustc no hacen `swapgs`, así que cualquier handler
  que lea datos por CPU desde un fallo de usuario necesita resolverlo;
  la forma más simple es que `cpu_id()` lea el `IA32_TSC_AUX`/`rdpid`
  o el ID del LAPIC en vez de `gs:`, y reservar `gs:` para los stubs
  propios. Decidirlo aquí, midiendo el coste.
- `jump_to_trapframe` y `jump_to_user`: `swapgs` antes del `iretq` a
  ring 3.
- `cpu::cpu_id()` real.

**Riesgo:** es la etapa más delicada de todas. Un `swapgs` de más o de
menos no falla en el momento: deja el `GS_BASE` del usuario en el kernel,
o al revés, y el síntoma aparece lejos. Los fallos con el TLS de mlibc
(que usa `fs`, no `gs`, así que no debería tocarse) serían un síntoma.
Añadir un detector permanente a `diag`: comprobar en cada entrada que
`gs:[0]` apunta a sí mismo.

**Hecho cuando:** `grep` no encuentra `SYSCALL_USER_RFLAGS` ni
`KERNEL_RSP0`, y `boot-matrix` con 20 arranques, `socket_test`,
`fpu_test`, DOOM y Quake funcionan.

**Estado:** hecha en QEMU el 2026-09-24, **con un diseño distinto al de
arriba** (`kernel/src/cpu/percpu.rs`, cuyo comentario de módulo es la
referencia):

- **`gs` solo existe dentro de `syscall_entry_fast`.** El stub hace
  `swapgs`, guarda el RSP de usuario y carga el del kernel por `gs:`, empuja
  el RSP de usuario y vuelve a hacer `swapgs`: cuatro instrucciones, con
  IF=0. Ni el stub del timer, ni los shims `x86-interrupt`, ni
  `jump_to_trapframe` tocan GS. El diseño "GS_BASE es por CPU mientras
  corre el kernel" de Linux exigía un `swapgs` en cada entrada y salida a
  ring 3, y los shims de rustc no hacen ninguno de los dos: un fallo de
  página desde usuario habría corrido con el GS del usuario, y la ruta de
  kill sale por `jump_to_trapframe`, que no puede saber si su entrada hizo
  `swapgs`. Con el par a cuatro instrucciones no hay nada que desincronizar,
  y lo que el usuario deje en GS_BASE no puede afectar al kernel, porque el
  kernel nunca lo lee.
- **`SYSCALL_USER_RFLAGS` desapareció sin reemplazo:** R11 ya no se
  reutiliza como temporal, así que el RFLAGS de usuario se empuja
  directamente. `PerCpu` tiene `self_ptr`, `kernel_rsp`,
  `user_rsp_scratch` y `cpu_id`.
- **`cpu_id()` lee el task register (`str`)**, no `gs:` ni RDPID: vale en
  cualquier camino, sea cual sea el estado de GS, y no depende de ninguna
  característica de CPUID. Coste medido en la Ryzen (boot #28, `percpu:`
  en `/proc/kdebug`, que lo mide en cada arranque; bucle de 1000 llamadas
  a `opt-level 0`, con su propio overhead incluido): **37,6 ciclos de TSC
  por llamada frente a 33,0 de RDPID** — unos 4,5 ciclos de diferencia, que
  no justifican depender de RDPID. (En QEMU/TCG: 677 frente a 546, sin
  valor para el metal.) **Impone una condición a la
  etapa 3:** una sola GDT con un slot de TSS por CPU, en
  `FIRST_TSS_SELECTOR + 16·n` (con una GDT por CPU y la TSS en el mismo
  índice, `str` daría lo mismo en todas). `tss::init` comprueba el selector
  con un assert.
- **Detector permanente:** `check_gs_invariant` (en `percpu.rs`, no en
  `diag`: entra en pánico, no cuenta nada) comprueba que
  `IA32_KERNEL_GS_BASE == &PERCPU[cpu]` en cada syscall y en cada tick.
  Probado con sabotaje: borrando el segundo `swapgs` del stub, sin la
  comprobación por syscall el síntoma era un DOUBLE FAULT pelado; con ella,
  `per-CPU GS invariant broken on cpu 0: KERNEL_GS_BASE=0x0 ...`.

Verificado: `grep` no encuentra ni `SYSCALL_USER_RFLAGS` ni `KERNEL_RSP0`;
`boot-matrix 4 5` = 20/20 (dos veces), `run-kernel-tests` PASS,
`fpu_test` ALL_OK, `socket_test` PASS, DOOM y Quake corriendo sus demos.
En la Ryzen (2026-09-24, boots #27 y #28 por `metal-run.sh`): 200
fork+exec, `fpu_test` ALL_OK, `socket_test` PASS, `METAL-DONE exit=0`, sin
pánico del detector. El primer intento se clasificó NO-JOB porque el bucle
desbordó el ring de `klog` (64 KiB) y borró `METAL-BEGIN`; el clasificador
ya reconoce el job también por `METAL-DONE` (51abd1d).

### Etapa 3 — TSS, GDT, IST y MSRs por CPU

- Una TSS y un juego de pilas IST por CPU, dentro o al lado de `PerCpu`.
- GDT con un slot de TSS por CPU, en `FIRST_TSS_SELECTOR + 16·n`. **No**
  una GDT por CPU con la TSS en el mismo índice: `cpu::cpu_id()` lee el TR
  y dejaría de distinguir CPUs (ver el estado de la etapa 2).
- Una función `cpu::init_this_cpu()` que haga todo lo que es por CPU:
  GDT/TSS, `lidt`, MSRs de syscall, `IA32_PAT`, CR0/CR4 de SSE, LAPIC y
  su timer. La BSP la llama en el arranque; los APs la llamarán en la
  etapa 4. **La lista de lo que hace esta función es la del inventario de
  registros por CPU de arriba**: si falta algo, un AP lo tendrá mal.

**Hecho cuando:** la BSP arranca a través de `init_this_cpu()` y nada por
CPU se inicializa fuera de ella.

### Etapa 4 — arrancar los APs, inertes

- Trampolín en memoria baja (< 1 MiB): real mode → protected → long mode
  con el CR3 del kernel, luego salto a Rust con una pila propia. Reservar
  la página del trampolín antes de que el buddy la reparta.
- INIT-SIPI-SIPI a cada LAPIC de la MADT, con los tiempos del SDM y
  tiempo de espera acotado: un AP que no responde se registra y se
  ignora, nunca cuelga el arranque (misma regla que el ratón y el AC97).
- Cada AP: `init_this_cpu()`, marca que está vivo y queda en
  `loop { hlt }` con interrupciones activas pero sin tick de scheduler.
- `/proc/kdebug`: CPUs detectadas en la MADT, CPUs arrancadas.
- QEMU: añadir `QEMU_DEBUG_SMP=N` a `qemu-debug.sh` y `boot-matrix.sh`.

**Hecho cuando:** con `-smp 4`, las cuatro CPUs arrancan y el sistema se
comporta exactamente igual que con una. En la Ryzen (5900X) se leen las 24 CPUs
lógicas en el log del pendrive.

### Etapa 5 — TLB shootdown

- IPI de invalidación: la CPU que modifica publica la petición (espacio
  de direcciones, rango), envía el IPI a las CPUs afectadas y espera su
  confirmación. Con IF=0 en quien espera, hay que atender las peticiones
  entrantes mientras se espera, o dos CPUs que se hacen shootdown
  mutuamente se bloquean.
- "Afectadas" = las CPUs que tienen cargado ese CR3 (una máscara por
  `AddressSpace`, actualizada en cada cambio de contexto), o todas para
  mapeos del kernel.
- La API de la etapa 0 es el único sitio que cambia.

**Hecho cuando:** un test con dos hilos en CPUs distintas (uno lee en
bucle una página, el otro la desmapea o fuerza COW) no ve nunca la
traducción vieja. Todavía no hay procesos en los APs, así que el test
dispara el shootdown a mano desde los APs inertes.

### Etapa 6 — auditoría del estado global

El inventario de arriba, entrada por entrada:

- Los `DECODER` de teclado y ratón, detrás de un lock.
- `cow::FRAME_REFCOUNTS`: refcounts atómicos (`AtomicU8`).
- Los `SCRATCH`: por CPU o detrás de un lock.
- Cada `static Mutex`: documentar su orden de locks respecto a
  `SCHEDULERS` y a los demás, y revisar cada uso desde un ISR.
- Los invariantes de IF=0 del CLAUDE.md (`sys_exit` hasta el `iretq`,
  la kstack diferida, las transferencias USB), reescritos para SMP.

**Hecho cuando:** cada entrada del inventario tiene una resolución
escrita en este documento, y el CLAUDE.md está actualizado.

### Etapa 7 — los APs ejecutan procesos

- Scheduler con un solo lock y `running` por CPU (decisión 1). Un idle
  por CPU.
- Un proceso no se puede elegir mientras otra CPU siga en su kstack:
  bandera `on_cpu` que se limpia al terminar el cambio de contexto.
- Liberación diferida de kstacks: una kstack encolada se libera solo
  cuando ninguna CPU está en ella.
- Tick por CPU; trabajo global solo en la BSP (decisión 3).
- IPI de reschedule: despertar un proceso de mayor prioridad cuando la
  CPU destino está en `hlt`.
- `sched`: extender las propiedades y el comprobador de invariantes con
  varios `running`, en los tests de host.
- Opción `nosmp` (decisión 6).

**Hecho cuando:** `boot-matrix` con `-smp 4` y con `-smp 1`, los dos
limpios con el mismo número de arranques; `pthread_test` y
`producer_consumer` con hilos en CPUs distintas; DOOM y Quake jugables;
y la Ryzen llega al shell con todas las CPUs.

## Qué queda fuera

- Colas de ejecución por CPU y balanceo de carga (decisión 1).
- Afinidad (`sched_setaffinity`).
- Interrupciones rutadas a otras CPUs que no sean la BSP.
- MSI para el xHCI (abierto por la etapa 1, pero es otro plan).
- Locks más finos que los actuales (`EXT2_LOCK` global, etc.). Primero
  que sea correcto; después, medir contención con `LockDiag`.
