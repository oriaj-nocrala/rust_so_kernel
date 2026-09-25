# Plan: SMP (varias CPUs)

> **Estado (2026-09-24):** etapa 0 hecha (reglas SMP-ready en el
> CLAUDE.md, `memory::tlb`, `keyboard::DECODER` tras un `IrqMutex`);
> etapa 1 hecha y verificada en QEMU y en la Ryzen (LAPIC timer + I/O APIC,
> `hal::apic`); etapa 2 hecha y verificada en QEMU y en la Ryzen
> (`cpu::percpu`, `swapgs` solo en la entrada de `syscall`); etapa 3 hecha
> y verificada en QEMU y en la Ryzen (`cpu::init_this_cpu`, una GDT con un slot de TSS por
> CPU); etapa 4 hecha y verificada en QEMU y en la Ryzen (`kernel/src/smp.rs`,
> `hal::smp`): los APs arrancan, pasan `init_this_cpu` y se quedan en `hlt`;
> etapa 5 hecha y verificada en QEMU (TLB shootdown por IPI, `memory::tlb`,
> `hal::tlb`, `tlb_selftest`), pendiente de la Ryzen. Los procesos siguen
> corriendo en una sola CPU.

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
| `CR0.WP`, `EFER.NXE` (añadido en la etapa 3) | los pone el bootloader, solo en la BSP | Sin NXE, el bit 63 de cada PTE NX es *reservado* y el primer acceso es un fallo de página; sin WP, una escritura del kernel en una página COW no falla y dos procesos comparten el marco |
| `CR4.PGE`, `CR0.CD/NW` (añadido en la etapa 3) | firmware | Páginas globales y cachés: la API de TLB y el rendimiento dependen de ellos |
| Modo y base de `IA32_APIC_BASE` (añadido en la etapa 3) | `interrupts/apic.rs` | Si la BSP está en x2APIC, cada AP también; en xAPIC, `LAPIC_VIRT` es un solo mapeo para todas y la base física debe coincidir |

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

*(Inventario previo a la etapa 5; hoy hay shootdown, ver la etapa 5.)*
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

**Estado:** hecha en QEMU el 2026-09-24 (`kernel/src/cpu/init.rs`, cuyo
comentario de módulo es la referencia).

- **Una GDT** de `5 + 2·MAX_CPUS` entradas; la TSS de la CPU n en
  `0x28 + 16·n` (`process/tss.rs` lo comprueba con un assert por slot).
  Cada CPU tiene su TSS y su pila IST de double fault (y una pila RSP0 de
  arranque, hasta que su primer proceso ponga la suya). Adiós al
  `static mut TSS`.
- **`cpu::init_this_cpu(cpu)`** hace, en este orden: registros de control
  (copia de la BSP de `CR0.WP/CD/NW`, `CR4.PGE`, `EFER.NXE` — la BSP los
  registra como referencia), GDT + segmentos + TR, `lidt`, GS (`PerCpu`),
  MSRs de `syscall`, PAT, SSE (CR0/CR4) y LAPIC local + timer. **Después lee
  cada registro del hardware** (`verify_*` en cada módulo) y guarda el
  resultado por CPU: `cpu_init:` en `/proc/kdebug` (`cpu0 ok (8)`, o qué paso
  falló y por qué). Devuelve el primer paso que no cuadra; la BSP entra en
  pánico con él, un AP (etapa 4) se quedará fuera.
- **Decisión global y aplicación por CPU, separadas.** `program_pat` y
  `apic::init` siguen corriendo en la BSP y *deciden* (el valor del PAT, la
  cuenta del timer, el modo del APIC, el enrutado del I/O APIC); cada CPU,
  la BSP incluida, *aplica* esa decisión en `init_this_cpu`. El timer del
  LAPIC de la BSP ya no arranca en `apic::init` sino aquí. Dos cosas se
  hacen además antes en la BSP, a propósito: el `lidt` (para que una
  excepción de arranque sea un pánico y no un triple fault) y la escritura
  del PAT (es cómo `program_pat` decide). Los pasos son idempotentes, así
  que repetirlos no cambia nada.
- **En el arranque**, `init_this_cpu(0)` va justo detrás de `apic::init`, es
  decir, antes que en la etapa 2 (antes de USB y del VFS): un double fault
  desde ahí ya cae en la pila IST de la CPU.
- **Test** `hw_tests::init_this_cpu_restores_what_an_ap_lacks`: en la BSP
  del arranque de tests, estropea uno a uno `CR0.WP`, `KERNEL_GS_BASE`,
  `LSTAR`, el PAT y `CR4.OSXMMEXCPT` y comprueba que la verificación culpa
  al paso correcto; luego los estropea todos a la vez (más `EFER.SCE`,
  `STAR`, `FMASK`) como los tendría un AP y comprueba que `init_this_cpu`
  los devuelve exactamente a sus valores. Probado por sabotaje: quitando el
  paso del PAT, falla con `init_this_cpu did not restore `pat``. Fuera del
  test, a propósito: `EFER.NXE` (borrarlo en una CPU viva es un fallo de
  página inmediato), GDT/TR/IDT y el LAPIC (el arranque de tests no lo usa).

Verificado: `run-kernel-tests` 9/9, `boot-matrix 4 5` = 20/20, 8G con
`fpu_test` ALL_OK y `socket_test` exit 0, `timer_ticks` a 99,9 Hz (dos
lecturas), y la ruta de respaldo (`-cpu max,-apic`) también con
`cpu0 ok (8)`. En la Ryzen (2026-09-24, boot #29, run
20260924-222935-e399b8, `METAL-DONE exit=0`): `cpu_init: cpu0 ok (8)`, PAT
programado (`...0106`, entrada 1 = WC), xAPIC con el timer a la cuenta
calibrada (62494, div 16), `fpu_test` ALL_OK y `socket_test` PASS tras 100
fork+exec. No se tomaron dos lecturas de `timer_ticks` (una sola no mide la
frecuencia); el timer lo programa ahora `init_this_cpu`, y la cuenta y el LVT
los comprueba su propia verificación.

**Para la etapa 4:** `MAX_CPUS` es 8 y la Ryzen tiene 24 CPUs lógicas; la
GDT, las TSS y las pilas crecen solas con la constante (40 KiB de pilas por
CPU).

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

**Estado:** hecha en QEMU el 2026-09-24 (`kernel/src/smp.rs`, cuyo
comentario de módulo es la referencia; `hal/src/smp.rs`, 11 tests, `hal` =
293).

- **Trampolín de cuatro páginas** por debajo de 640 KiB: el código y, detrás,
  PML4/PDPT/PD propios. `hal::smp::pick_trampoline` elige la ventana más alta
  dentro de una región usable y `init::memory::init_core` la recorta *antes*
  de sembrar el buddy (en QEMU cae en `0x9c000`). El PML4 del trampolín es
  una **copia** del del kernel con la entrada 0 sustituida por el mapeo
  identidad de 0–2 MiB: ninguna tabla viva cambia, y el AP carga el CR3 real
  del kernel lo primero en Rust. La entrada 0 del kernel no está vacía (el
  bootloader deja ahí el mapeo identidad de su cambio de contexto, que nadie
  vuelve a usar; `new_user` ya la salta), así que no se exige que lo esté.
- **Real → protegido → largo** en `global_asm!` (AT&T): los datos van al
  principio del blob para que las constantes `T_*` se resuelvan hacia atrás,
  y la BSP parchea antes de cada SIPI las direcciones absolutas (base de la
  GDTR, los dos saltos lejanos) y los campos (CR3, pila, entrada, CPU). El
  trampolín pone `EFER.NXE` junto con `LME`: las PTE del kernel llevan el bit
  63, que sin NXE es reservado. Deja un marcador de progreso (1 = protegido,
  2 = largo) que la BSP lee si el AP no contesta.
- **Un AP a la vez**, con INIT, 10 ms, SIPI, 200 µs, SIPI
  (`hal::smp::startup_sequence`), y hasta 200 ms para que termine
  `init_this_cpu`. Un AP que no contesta se registra con la etapa a la que
  llegó y recibe otro INIT para aparcarlo: si despertara tarde leería los
  campos del siguiente y correría en su pila.
- **Numeración:** la BSP es la CPU 0 aunque no sea la primera de la MADT; el
  resto en orden de MADT, sin duplicados, hasta `MAX_CPUS` (subido de 8 a
  32: la Ryzen tiene 24).
- **Inertes de verdad:** el paso `lapic` de `init_this_cpu` deja el timer
  **enmascarado** en toda CPU que no sea la 0 (`apic::runs_timer`, lo que la
  etapa 7 cambia), y su verificación lo comprueba. El AP hace `sti; hlt` en
  bucle: con IF=1, un timer solo "sin programar" podría haber dejado el
  vector 32 pendiente y meter al AP en el planificador.
- `/proc/kdebug`: `smp: madt N cpus, M online: cpu0=apic0/bsp cpu1=apic1/10253us ...`
  (o `NO-RESPONSE(stage n)`, `INIT-FAILED(paso: motivo)`, `IPI-STUCK`), y
  `cpu_init:` con una entrada por CPU. `boot-matrix.sh` apunta
  `cpus_online=M/N` por arranque y lo agrega; `QEMU_DEBUG_SMP=N` en
  `qemu-debug.sh` (y por herencia en `boot-matrix.sh`).

Verificado: `boot-matrix 4 5` con `QEMU_DEBUG_SMP=4` = 20/20, `4/4 x20`, y
sin él = 20/20, `1/1 x20`; `run-kernel-tests` PASS; `-smp 24 -m 8G` (la forma
de la Ryzen) con 24 online, `cpu0..23 ok (8)` y `fpu_test` ALL_OK; `-smp 4`
con `fpu_test` ALL_OK y `socket_test` exit 0; `-smp 40` da 32 online y
`8 beyond MAX_CPUS=32`; `-cpu max,-apic` no intenta nada y lo dice. Probado por
sabotaje (un `hlt` tras llegar a modo largo): `NO-RESPONSE(stage 2)` en ambos
APs y el arranque llega al shell igual. Cada AP tarda ~10,2 ms en QEMU, casi
todo el retardo de INIT. En la Ryzen (2026-09-24, boot #30, run
20260924-225723-534d3f, `METAL-DONE exit=0`, job
`target/metal/smp-stage4-job.sh`): `smp: madt 24 cpus, 24 online`, las 24 con
`cpuN ok (8)`, ~10,1 ms por AP (numeración en orden de MADT: apic 0,2,4…26 y
luego 1,3,5…27), `fpu_test` ALL_OK, `socket_test` PASS tras 100 fork+exec, y
`timer_ticks` 290@3840 ms → 353@4466 ms ≈ 100 Hz: los APs no tocan el tick.
`spurious_irqs: 1` (en QEMU, 0): sin investigar; el vector espurio del LAPIC
se cuenta y se ignora, y ningún AP lo EOIa.

Fuera, a propósito: las entradas x2APIC (tipo 9) de la MADT, que solo aparecen
con IDs de APIC > 255 (la Ryzen llega a 27).

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

**Estado:** hecha en QEMU el 2026-09-24. Referencias: el comentario de módulo
de `kernel/src/memory/tlb.rs` (protocolo) y el de `kernel/src/tlb_selftest.rs`
(la prueba).

- **A quién se avisa** (`hal::tlb`, 6 tests, `hal` = 299): un mapeo de
  usuario, a las CPUs que tienen *ahora* ese PML4 en CR3 — sin PCID, escribir
  CR3 tira toda entrada no global —; uno del kernel, a todas las CPUs
  `READY`. La API cambió de forma, no solo de cuerpo, contra lo que decía el
  plan: `invalidate_page(pml4, addr)` necesita saber *qué* tabla cambió, y
  ahora lo aporta quien llama (decisión 4). `OwnedPageTable` pasa la suya,
  `demand_paging` el CR3 actual. Los tres módulos que no podían decirlo
  (`paging.rs`, `user_code.rs`, `user_pages.rs`) eran código muerto, sin un
  solo uso, y se borraron.
- **Quién tiene qué cargado:** `LOADED[cpu]`, publicado *antes* de escribir
  CR3; todo cambio de CR3 pasa por `tlb::switch_to` (`activate` y el lector
  de la prueba). El emisor lo lee tras un `fence(SeqCst)` posterior a su
  escritura de la PTE: si no ve el valor nuevo de una CPU, la escritura de
  CR3 de esa CPU es posterior y ya ve la PTE nueva. La única excepción es
  el `mov cr3` de `ap_entry`, anterior a cargar TR (`cpu_id()` diría 0);
  `this_cpu_ready` publica ese CR3 después.
- **Entrar en `READY`** (`this_cpu_ready`): la BSP justo antes de arrancar
  los APs, cada AP tras un `init_this_cpu` correcto. Luego vacía su TLB
  entera, globales incluidas (conmutar `CR4.PGE`): lo que cacheó antes de
  entrar pudo perderse un shootdown. Un AP que falla la inicialización nunca
  entra, así que nunca se le espera.
- **Protocolo:** una petición a la vez en una ranura global (`SENDER`), un
  bit por destino en `PENDING`, IPI fijo en el vector `0xF0` (`icr::fixed`,
  con `mfence; lfence` antes: la escritura del ICR en x2APIC no serializa),
  espera acotada a 1 s y pánico nombrando las CPUs que no contestaron. El
  manejador y todas las esperas con IF=0 llaman a `service_pending`: la
  espera por `SENDER` (dos CPUs que se disparan a la vez) y, vía el nuevo
  `diag::IrqControl::relax`, cada espera de un `IrqMutex` (el emisor puede
  tener `BUDDY`: `unmap_page_and_free_2m_with_buddy`).
- **Un segundo IPI, `0xF1`** (`smp::WAKE_VECTOR`): despierta a un AP para que
  mire su buzón (`smp::run_on`). Es cómo la prueba llega a un AP inerte, y la
  forma que tendrá el IPI de reschedule de la etapa 7.
- `/proc/kdebug`: `tlb: ready 0xf, N shootdowns (M IPIs), K serviced ..., wait avg X us max Y us`.
- **La prueba** (`tlb_selftest::run`, compartida por
  `hw_tests::tlb_shootdown_leaves_no_stale_translation` y `kdebug tlbtest`,
  syscall 403 cmd 3): un lector en cada AP lee en bucle una página que la
  BSP mueve entre tres marcos, 200 rondas; la lectura del marco de la ronda
  anterior cuenta como obsoleta. Tres partes: ámbito de usuario (un espacio
  de direcciones de prueba cargado en el AP), ámbito del kernel (una hoja de
  `mmio::map`) y mutua (BSP y AP se disparan 2000 shootdowns cada uno con
  IF=0). `run-kernel-tests.sh` pasa ahora `-smp 4` y el arranque de tests
  hace los pasos de APIC y APs del arranque real.

Verificado: `run-kernel-tests` PASS (10 tests; `3 APs x 200 rounds, stale 0`);
`boot-matrix 4 5` con `-smp 4` 20/20 y con `-smp 1` 20/20; `-smp 24 -m 8G`
4/4 y `kdebug tlbtest` PASS contra 23 APs (13 272 shootdowns, 204 056 IPIs,
espera media 9 µs, máxima 369 µs); con `-smp 4`, `fpu_test`, `socket_test`,
`pthread_test` y `producer_consumer` PASS. **Probado por sabotaje:** sin
enviar el IPI, `stale 34354` y el test falla (el TLB de QEMU TCG sí conserva
la traducción vieja); sin `service_pending` en la espera por `SENDER`, la
parte mutua acaba en `cpus 0x2 never acknowledged`. El `relax` de `IrqMutex`
tiene su propio test de host (`diag` = 39), también probado quitándolo.

Encontrado por el camino:

- **`unmap_and_remap` deja la PTE a cero entre el `unmap` y el `map`.** La
  primera versión de la prueba lo usaba, y el lector del AP murió con un
  fallo de página "no presente" en esa ventana. En una CPU es inofensivo; con
  dos hilos del mismo proceso en dos CPUs, el otro hilo tomaría un fallo de
  página en mitad de una resolución COW y el manejador podría mapearle una
  página nueva encima. Es la ruta COW del kernel: queda para la etapa 6
  (cada resolución de fallo bajo un lock del espacio de direcciones, y
  cambiar el marco de una sola escritura, como ya hace
  `OwnedPageTable::replace_frame`, que la prueba usa).
- **Toda espera con IF=0 sobre un `spin::Mutex` normal** (el del scheduler,
  entre otros) no atiende shootdowns: si el emisor tiene ese lock, bloqueo.
  Inalcanzable mientras los APs no toman locks; es un requisito de la etapa 7.
- **`rflags = 0x200` en todo `TrapFrame` nuevo** (`process/mod.rs` ×3, `exec`):
  el bit 1 está reservado y siempre a 1, y el validador de marcos de
  `timer_preempt` (2026-08-06) lo rechaza. `pthread_test` entraba en pánico
  en cuanto un hilo nuevo se reanudaba por primera vez desde el tick. No es
  de esta etapa (arreglado aparte: `0x202`).
- **`FUTEX_WAITERS`, `POLL_WAITERS` y `EPOLL_FD_MAP` eran arrays de 32
  entradas indexados por PID** que ignoraban en silencio los PID >= 32: un
  `FUTEX_WAIT` (o un `poll` sin timeout) de esos PID bloqueaba sin
  registrarse y nunca despertaba. El primer job de metal de esta etapa se
  quedó en `pthread_join` (PID 104, tras 100 `exec` de calentamiento) hasta
  que el watchdog reinició; en QEMU, determinista en la 7.ª ejecución
  seguida de `pthread_test`, también con `-smp 1`. No es de esta etapa;
  ahora son `BTreeMap` por PID, sin límite.
- **Coste:** cada página guarda de una kstack (fork, exit) es un shootdown
  del kernel a todas las CPUs: 24 al arrancar al shell con `-smp 4`.

### Etapa 6 — auditoría del estado global

El inventario de arriba, entrada por entrada:

- Los `DECODER` de teclado y ratón, detrás de un lock.
- `cow::FRAME_REFCOUNTS`: refcounts atómicos (`AtomicU8`).
- Los `SCRATCH`: por CPU o detrás de un lock.
- Cada `static Mutex`: documentar su orden de locks respecto a
  `SCHEDULERS` y a los demás, y revisar cada uso desde un ISR.
- Los invariantes de IF=0 del CLAUDE.md (`sys_exit` hasta el `iretq`,
  la kstack diferida, las transferencias USB), reescritos para SMP.
- (De la etapa 5.) La resolución de un fallo COW (`unmap_and_remap`, con su
  ventana de PTE a cero) y cualquier otro cambio de PTE de un espacio de
  direcciones compartido por hilos, bajo un lock de ese espacio, que el
  manejador de fallos también tome.
- (De la etapa 5.) Toda espera con IF=0 que pueda tener al otro lado a un
  emisor de shootdown atiende `memory::tlb::service_pending` — los
  `IrqMutex` ya lo hacen (`relax`); el lock del scheduler y los demás
  `spin::Mutex` tomados con IF=0, no.

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
