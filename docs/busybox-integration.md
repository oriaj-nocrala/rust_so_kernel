# Integración de BusyBox — estado, bugs arreglados, y el que queda pendiente

Este documento es la bitácora técnica completa de cómo se llegó a correr BusyBox
real contra este kernel, qué bloqueadores hubo que resolver para que `ash`
arranque con job control real, y el bug que quedó sin resolver al final de la
última sesión de trabajo. Está pensado para que quien retome esto no tenga que
redescubrir nada de lo que ya se investigó.

Fecha de la última sesión cubierta acá: **2026-07-18**.

## Índice

1. [Contexto y objetivo](#contexto-y-objetivo)
2. [Los tres bloqueadores de ash/hush (resueltos)](#los-tres-bloqueadores-de-ashhush-resueltos)
3. [Habilitar `ash`: los 4 bugs reales que expuso](#habilitar-ash-los-4-bugs-reales-que-expuso)
4. [El bug que queda: ash muere después de 2 caracteres](#el-bug-que-queda-ash-muere-después-de-2-caracteres)
5. [Técnica de debugging usada (y por qué hace falta)](#técnica-de-debugging-usada-y-por-qué-hace-falta)
6. [Cómo reproducir todo esto](#cómo-reproducir-todo-esto)
7. [Próximos pasos sugeridos](#próximos-pasos-sugeridos)

---

## Contexto y objetivo

El objetivo de fondo (declarado por el usuario a lo largo de varias sesiones)
es correr BusyBox real sobre este kernel — no solo los programas de usuario
hechos a mano — y específicamente llegar a tener un shell interactivo (`ash`
o `hush`) funcionando de punta a punta: banner, prompt, ejecución de
comandos, Ctrl-C interrumpiendo un foreground job, etc.

El trabajo se hizo con la metodología "vamos de a poco, con lo mas facil"
— cada sesión atacó un bloqueador concreto, lo verificó de punta a punta en
QEMU real (nunca solo `cargo build`), y lo commiteó antes de seguir.

Se habían identificado tres bloqueadores concretos para que `ash`/`hush`
funcionen como shell interactivo real:

1. `waitpid()` con exit status real (no hardcodeado a 0)
2. `chdir()`/`getcwd()` reales (rutas relativas, `..`)
3. termios/job control real (Ctrl-C, Ctrl-Z, grupos de proceso, `SIGSTOP`/`SIGCONT`)

Los tres están resueltos. Lo que sigue es la crónica de cómo se resolvió el
tercero, y qué apareció al intentar prender `ash` con eso ya en su lugar.

---

## Los tres bloqueadores de ash/hush (resueltos)

### 1. `waitpid()` con exit status real — commit `810233d`

Antes de esto, `waitpid()` siempre reportaba `exited(0)` sin importar cómo
terminara el hijo. `Process` ganó `waiting_status_ptr`/`pending_wait_status`/
`killed_by_signal`; el estado real del hijo muerto se guarda en el `Process`
del *padre* (seguro sin importar qué tabla de páginas esté activa) y se
vuelca a la memoria real del padre la próxima vez que el padre mismo vuelve a
modo usuario, vía `Scheduler::resolve_wait_status()`.

### 2. `chdir()`/`getcwd()` reales — commit `c68c834`

`Process` ganó un campo `cwd: String` (siempre absoluto y normalizado),
heredado por `fork()`/`clone()`, preservado por `exec()`. Nueva función
`fs::vfs::normalize_path()` que resuelve `.`/`..` léxicamente — antes de
esto, `..` en el VFS era directamente un no-op en todos lados, no solo para
`chdir`. Nuevos syscalls `chdir(80)`/`getcwd(79)`; **todos** los syscalls que
toman un path (`open`/`stat`/`mkdir`/`rmdir`/`unlink`/`rename`) ahora pasan
por un helper `resolve_path()` que normaliza contra el cwd del que llama.

### 3. Termios + job control real — commit `bc2151c`

Este fue el más grande. Piezas nuevas:

- **`kernel/src/tty.rs`** (nuevo): un `struct Termios` global que matchea
  byte a byte el `struct termios` del port (`mlibc-port/constanos-sysdeps/
  include/abi-bits/termios.h` — ojo, `cc_t`/`tcflag_t` son `unsigned int`
  ahí, no `unsigned char` como en POSIX real). Detrás de `ioctl`
  `TCGETS`/`TCSETS`/`TCSETSW`/`TCSETSF` reales — antes `TCGETS` solo
  zeroeaba 60 bytes fijos y nunca guardaba nada.
- **Grupos de proceso reales**: `Process` ganó `pgid` (heredado en
  `fork()`/`clone()`, nunca tocado por `exec()`). Nuevos syscalls
  `setpgid(109)`/`getpgid(121)`/`setsid(112)`.
- **`tcgetpgrp()`/`tcsetpgrp()`** vía `ioctl` `TIOCGPGRP`/`TIOCSPGRP` contra
  un `FOREGROUND_PGID` global (este kernel tiene una sola tty, así que un
  solo global alcanza).
- **`kill()` extendido** a `pid==0` (grupo propio) y `pid<-1` (grupo
  `-pid`) — antes solo aceptaba un pid exacto.
- **Tecla Ctrl en el driver de teclado** (`kernel/src/keyboard.rs`) — no
  existía en absoluto antes de esto.
- **Line discipline ISIG real** (`tty::feed_input`, enganchado en el ISR de
  teclado y en el de serial COM1): cuando `ISIG` está activo, un byte que
  matchea `VINTR`/`VQUIT`/`VSUSP` se convierte en `SIGINT`/`SIGQUIT`/
  `SIGTSTP` entregado al grupo foreground en vez de encolarse como input.
  Verificado con Ctrl-C real tecleado en QEMU matando un `sleep` en
  foreground.
- **`SIGSTOP`/`SIGTSTP`/`SIGCONT` reales**: `ProcessState` ganó `Stopped`
  (parqueado en `wait_queue`, nunca en las run queues),
  `Scheduler::stop_and_switch_tf`/`wake_stopped`. `waitpid()` reescrito para
  soportar `pid` `>0`/`0`/`-1`/`<-1`, flags `WNOHANG` y `WUNTRACED`, y
  `ECHILD` real cuando el target no matchea ningún hijo (antes bloqueaba
  para siempre en ese caso).

Verificado con un test C nuevo, `userspace/c/jobctl_test.c` (ver más abajo —
sigue siendo la herramienta principal para regression-testear todo esto).

**No incluido a propósito** (no es que falte, es una decisión de scope):
`WCONTINUED`, `waitpid(-1)` con modelo de permisos real (no hay modelo de
permisos), y `ECHO`/`ICANON` reales del lado del kernel — el line editing
sigue siendo responsabilidad de userspace, tal como lo hace el propio line
editor de `ash` una vez que pone la tty en raw mode.

---

## Habilitar `ash`: los 4 bugs reales que expuso

Con los tres bloqueadores resueltos, el siguiente pedido concreto del
usuario fue: **"habilitá ash en minimal.config y probalo"**. Esto es la
crónica de esa sesión — commit `879ff7b`.

### Cambio de config

En `busybox-config/minimal.config` (que antes solo tenía `CONFIG_STATIC` +
`CONFIG_TRUE` + `CONFIG_ECHO`):

```
CONFIG_ASH=y
CONFIG_ASH_JOB_CONTROL=y
CONFIG_FEATURE_EDITING=y
CONFIG_FEATURE_EDITING_MAX_LEN=1024   (default real de BusyBox, antes 0 porque estaba deshabilitado)
CONFIG_FEATURE_EDITING_HISTORY=255    (idem)
```

`CONFIG_SH_IS_ASH=y` y `CONFIG_SHELL_ASH=y` ya estaban prendidos de una
corrida anterior de `make oldconfig` (son el "esqueleto" que hace que
`/bin/sh` use la implementación de ash, aunque el applet `ash` en sí — el
nombre — no estuviera compilado). `CONFIG_ASH` es el que agrega el applet
"ash" de verdad y activa todo el bloque de opciones `ASH_*` en el
`.config`.

Para regenerar: editar `busybox-config/minimal.config` a mano y correr
`scripts/build-busybox.sh` (copia el config a `busybox/.config`, corre
`make oldconfig` no-interactivo, compila). Es idempotente.

### Bug 1 — `sys_getuid`/`geteuid`/`getgid`/`getegid` faltaban del todo

**Síntoma**: al ejecutar `busybox ash` por primera vez, panic de mlibc
inmediato:

```
In function geteuid, file ../mlibc/options/posix/generic/unistd.cpp:1268
__ensure(Library function fails due to missing sysdep) failed
In function geteuid, file ../mlibc/options/posix/generic/unistd.cpp:1269
__ensure(!"Cannot continue without sys_geteuid()") failed
mlibc: panic!
```

**Causa**: `mlibc/options/posix/include/mlibc/posix-sysdeps.hpp` declara
`sys_getuid`/`sys_geteuid`/`sys_getgid`/`sys_getegid` como símbolos
`[[gnu::weak]]` — si el port no los implementa, quedan sin resolver y
`unistd.cpp`'s `geteuid()` hace `__ensure(!"Cannot continue...")` (un abort
duro, no un ENOSYS silencioso) apenas alguien los llama. `ash` los llama en
su arranque (probablemente para setear `$UID`/`$EUID` o para el chequeo
"¿soy setuid?").

**Fix** (`mlibc-port/constanos-sysdeps/generic/generic.cpp`, cerca de
`sys_getppid`): este kernel es single-user, así que las cuatro son triviales:

```cpp
uid_t sys_getuid() { return 0; }
uid_t sys_geteuid() { return 0; }
gid_t sys_getgid() { return 0; }
gid_t sys_getegid() { return 0; }
```

Con `uid==euid==0` en todos lados, `ash` nunca dispara su lógica de "soy
setuid, hago algo especial" — no hace falta implementar `setuid`/`setgid`
tampoco (no se llegaron a necesitar).

### Bug 2 — `sys_ioctl`'s "es esto una tty" era por número de fd, no por identidad

**Síntoma**: con el bug 1 arreglado, `ash` arranca, imprime el banner, pero
inmediatamente después escribe a stderr:

```
ash: can't access tty; job control turned off
```

y sigue andando en modo no-job-control (banner sí, pero sin negociación de
terminal real).

**Causa**: `kernel/src/process/syscall.rs::sys_ioctl` tenía:

```rust
// fds 0,1,2 are the console — a tty.  All others: not a tty.
let is_tty = fd <= 2;
```

`ash`'s `setjobctl()` (`busybox/shell/ash.c`, función homónima) hace, en
este orden:

1. `open("/dev/tty", O_RDWR)` — falla (ENOENT, este kernel no tiene
   `/dev/tty`), así que cae al fallback: `fd=2; while(!isatty(fd)) fd--;`
   → usa fd 2 (nuestro `/dev/console`, un tty de verdad).
2. `fd = fcntl(fd, F_DUPFD_CLOEXEC, 10)` — duplica fd 2 a un fd nuevo,
   **≥ 10**.
3. `pgrp = tcgetpgrp(fd)` sobre *ese* fd nuevo (≥10), en un loop
   `while(1) { ...; if (pgrp == getpgrp()) break; killpg(0, SIGTTIN); }`.

El paso 3 hace `ioctl(fd_nuevo, TIOCGPGRP, ...)`. Como `fd_nuevo >= 10 > 2`,
el chequeo `fd <= 2` decía "no es una tty" → `ENOTTY` → `tcgetpgrp()`
devuelve -1 → dispara exactamente el mensaje de arriba (`ash_msg("can't
access tty..."); mflag = on = 0; goto close;`).

Este es un bug real e independiente de ash: cualquier programa que dupe un
fd de tty a un número más alto (algo perfectamente normal y esperado en
POSIX) se topaba con esto.

**Fix** (`kernel/src/process/syscall.rs::sys_ioctl`): en vez de mirar el
número de fd, mirar la identidad real del handle detrás de ese fd (todos
los `FileHandle` tienen un método `name()`; `SerialConsole::name()` →
`"serial"`, `FramebufferConsole::name()` → `"fb"`):

```rust
let is_tty = {
    unsafe { core::arch::asm!("cli"); }
    let result = {
        let mut sched = super::scheduler::local_scheduler();
        sched.running_mut().map(|proc| {
            proc.files.lock().get(fd as usize).ok()
                .map(|f| matches!(f.name(), "serial" | "fb"))
                .unwrap_or(false)
        }).unwrap_or(false)
    };
    unsafe { core::arch::asm!("sti"); }
    result
};
```

### Bug 3 — fd 0 (stdin) atado a `/dev/null` (el bloqueador de fondo real)

**Síntoma**: con los bugs 1 y 2 arreglados, ya no aparece "can't access
tty" — pero tampoco aparece el banner de BusyBox, ni ningún prompt. `ash`
queda mudo en stdout, aunque **sigue procesando comandos** si se le escribe
a ciegas (se pudo confirmar tipeando `echo hi` a ciegas y viendo
`ash: echo: not found` por stderr, y `exit` terminando el proceso con el
exit status correcto).

**Causa**: `ash` decide si es interactivo (`iflag = 1`, lo que gatilla
imprimir el banner y el prompt) con esta condición en
`busybox/shell/ash.c` (línea ~14563):

```c
if (iflag == 2                 /* no explicit -i given */
 && sflag == 1                 /* -s given (or implied) */
 && !minusc
 && isatty(0) && isatty(1)     /* we are on tty */
) {
    iflag = 1;
}
```

`isatty(0) && isatty(1)`. `isatty(1)` (framebuffer) siempre daba `true`.
Pero `isatty(0)` daba **siempre false**, porque `fd 0` estaba atado a
`/dev/null`:

```rust
// kernel/src/process/file.rs — FileDescriptorTable::new_with_stdio()
// FD 0: stdin (for now, /dev/null)
table.files[0] = Some(drivers::open_device("/dev/null")...);
```

Este comentario ("for now") venía de sesiones anteriores. Nunca importó
para *leer*, porque `sys_read`'s rama para `fd==0` bypasea completamente la
tabla de descriptores y lee directo del keyboard buffer global
(`crate::keyboard::read_key()`), sin mirar qué handle está instalado en el
slot 0. Pero sí importa para `isatty()`/`ioctl(TCGETS)`, que consultan el
handle real. Con `/dev/null` ahí, **ningún** shell podía considerarse
interactivo jamás, en silencio, sin ningún error visible.

Encontrar esto costó bastante: primero pareció un problema de buffering de
stdout (el banner "se perdía"), después pareció que ash colgaba
indefinidamente en el loop de `setjobctl()`. Terminó aislándose escribiendo
un test C que imprime `isatty(0)`/`isatty(1)`/`isatty(2)` explícitamente
(agregado a `userspace/c/jobctl_test.c`) y viendo `isatty(0)=0` en el log.

**Fix**: hay que arreglarlo en **dos lugares independientes** — hay dos
funciones separadas que inicializan la tabla de fds:

1. `FileDescriptorTable::new_with_stdio()` (usada al crear un proceso desde
   cero — el `shell` en boot, o cualquier `Process::new_user`).
2. `FileDescriptorTable::clone()` (usada por `fork()` — **esta es la que de
   hecho importa para `busybox ash`**, porque se lanza vía `fork()`+`exec()`
   desde el shell, nunca desde `new_with_stdio()` directamente).

```rust
// ambos: /dev/null -> /dev/console
table.files[0] = Some(drivers::open_device("/dev/console")...);
```

Es fácil arreglar solo uno de los dos y pensar que ya está — el síntoma
(banner ausente) desaparece recién con los dos arreglados, porque
`new_with_stdio()` es la que corre en boot para el shell mismo, pero
`clone()` es la que corre cada vez que el shell hace `fork()` para lanzar
algo, que es el camino real por el que se llega a `busybox ash`.

### Bug 4 — leak de waiters de poll/epoll/futex en la muerte por señal

**Síntoma**: encontrado mientras se investigaba el bug de "ash muere
después de 2 caracteres" (ver sección siguiente) — **no se confirmó que
esto fuera la causa de ese bug específico**, pero es un bug real e
independiente que se encontró y arregló en el camino.

**Causa**: `sys_exit` (`kernel/src/process/syscall.rs`) limpiaba
explícitamente las tablas de espera de un proceso al morir:

```rust
poll_cancel_waiter(dead_pid);
clear_epoll_fd_all(dead_pid);
futex_cancel_waiter(dead_pid);
```

pero esto **solo pasaba en `sys_exit`**. Un proceso que muere por una señal
no atrapada (el path `Scheduler::resolve_signals`'s `Terminate`, o el path
de fallo de hardware en `kernel/src/init/devices.rs::kill_current_user_process`)
nunca corría esta limpieza. Antes de la feature de job control esto era
raro de disparar (pocas cosas mataban procesos por señal en la práctica);
con `kill(-pgid, SIGTERM)` matando rutinariamente varios procesos de una,
se volvió mucho más fácil de gatillar.

**Fix**: se extrajo a un helper público,
`syscall::cancel_all_waiters(pid)`, y se lo llama desde los tres lugares
donde un proceso puede morir: `sys_exit`, `Scheduler::resolve_signals`'s
rama `Terminate`, y `kill_current_user_process` en `init/devices.rs`.

**Importante para quien retome esto**: este fix quedó en el commit, pero
**no resolvió** el bug de "ash muere a los 2 caracteres" (se probó
explícitamente después de aplicarlo y el síntoma fue idéntico). Se dejó
igual porque es un bug real por derecho propio, no porque se haya
confirmado como la causa de nada. Ver la hipótesis descartada más abajo.

---

## El bug que queda: ash muere después de 2 caracteres

Este es el bug sin resolver. Documentado acá con el máximo detalle posible
para no tener que re-investigar desde cero.

### Síntoma exacto

Con los 4 bugs de arriba arreglados, `busybox ash` arranca perfectamente:
banner real, prompt `#`, sin "can't access tty". Pero al tipear en el
prompt interactivo, **exactamente después del segundo carácter**, ash
imprime lo que se alcanzó a tipear y termina limpiamente:

```
# pw
💀 Killed PID 6 (child): exit(0)
```

(el ejemplo es tipeando "pwd" — solo "pw" llega a procesarse; "d" y el
Enter nunca llegan, van a parar al shell padre una vez que ash ya murió).

Reproducido también tipeando "exit" (muere después de "ex") y con varias
otras combinaciones. **Es consistentemente el segundo carácter**, sin
importar cuáles sean los dos caracteres.

### La secuencia de syscalls exacta que precede a la muerte

Tracing exhaustivo (ver sección de técnica de debugging) mostró esta
secuencia, siempre idéntica, arrancando justo después de que se lee y hace
echo del segundo carácter:

```
read(fd=0, buf, count=1)        → devuelve el 2do carácter (ej. 'w')
write(fd=1, buf, count=1)        → eco del carácter a stdout (normal)
poll(fds, nfds=1, timeout=-1)    → arranca esperando el 3er carácter
ioctl(fd=0, request=TCSETS)      → ash restaura el termios "cocido" original
write(fd=2, buf, count=1)        → 1 byte a stderr (probablemente un \n de limpieza visual)
ioctl(fd=10, request=TIOCSPGRP)  → ash devuelve el foreground pgrp al original
setpgid(pid=0, pgid=1)           → ash vuelve a su pgid original
close(fd=10)                     → cierra el fd duplicado de la tty
exit(0)                          → sale
```

Esta secuencia (`TCSETS` restaurando cooked mode + `TIOCSPGRP` +
`setpgid` + `close` + `exit`) es **exactamente** el cuerpo de
`setjobctl(0)` ("turning job control off") en `busybox/shell/ash.c`,
seguido de la salida normal del shell. Es decir: no es un crash, no es un
signal matando al proceso — es **ash decidiendo, por su cuenta, terminar
limpiamente**, como si hubiera llegado a una condición de salida normal
(EOF en stdin, o el builtin `exit` ejecutándose).

El misterio es: **¿qué le hace pensar a ash, después de solo 2 caracteres
normales, que tiene que terminar?**

### Hipótesis probadas y descartadas

1. **¿Es específico de correr `jobctl_test` antes?**
   Descartado. Se probó lanzando `busybox ash` dos veces seguidas desde
   cero, sin `jobctl_test` de por medio en absoluto — el bug reprodujo
   igual, muriendo a los 2 caracteres cada vez. Esto también descartó
   cualquier hipótesis sobre estado global corrompido específicamente por
   `jobctl_test` (grupos de proceso, señales, etc.).

2. **¿Se entrega alguna señal a ash justo antes de morir (p. ej.
   `SIGTTIN`/`SIGTTOU` mal manejadas)?**
   Descartado con evidencia directa: se instrumentó
   `kernel/src/process/signal.rs::deliver_pending` con un
   `serial_println_raw!` imprimiendo cada señal entregada a cada proceso.
   En la corrida que reprodujo el bug, **no hay ninguna entrada de
   `deliver_pending` para el pid de ash** antes de su muerte — cero señales
   entregadas. La muerte es un `exit()` (syscall 60) genuino llamado
   directamente, no una consecuencia de `resolve_signals`.

   (De todos modos, mientras se investigaba esto se encontró y arregló un
   bug real: `SIGTTIN`/`SIGTTOU` no estaban en la lista de "señales que
   paran el proceso por default" — caían al default de `Terminate`. Esto
   importa porque `ash` se manda `killpg(0, SIGTTIN)` a sí mismo como parte
   de su protocolo normal de negociación de terminal cuando *no* es el
   grupo foreground — con el bug, eso lo hubiera matado en vez de pararlo.
   Se agregó `SIGTTIN`/`SIGTTOU` a la rama de `Stop` en
   `kernel/src/process/signal.rs::deliver_pending`, junto a `SIGTSTP`. Este
   fix quedó en el commit `879ff7b` — es correcto y necesario para POSIX
   real, pero **no** resolvió el bug de los 2 caracteres.)

3. **¿Es un leak de waiters de poll/epoll/futex (bug 4)?**
   Se implementó el fix completo (`cancel_all_waiters` en los 3 death
   paths) y se re-probó la secuencia exacta que reproduce el bug. **El
   síntoma no cambió en absoluto.** Además, cabe notar que los hijos que
   crea `jobctl_test` (los que se matan con `kill(-pgid, SIGTERM)`) nunca
   llaman a `poll`/`epoll`/`futex` — solo hacen `setpgid`/`kill`/
   `nanosleep` — así que la teoría de que ESE leak específico afectara a
   `ash` era poco probable desde el principio. El fix se mantuvo en el
   commit igual porque es un bug real e independiente.

4. **¿Hay algún mismatch de `pgrp`/`FOREGROUND_PGID` que dispare el loop
   de `killpg(0, SIGTTIN)` de `setjobctl()`?**
   Descartado con evidencia directa: se instrumentó el handler de
   `TIOCGPGRP` para imprimir `FOREGROUND_PGID` y el `pgid` del que llama en
   cada consulta. En la corrida que reprodujo el bug:
   `[DBG TIOCGPGRP] fg=1 caller_pgid=Some(1)` — coinciden exactamente, el
   loop de `setjobctl()` rompe en la primera iteración como corresponde, no
   hay ningún `killpg` de por medio.

5. **¿Está relacionado con el conteo de caracteres per se, o es en
   realidad un tema de tiempo transcurrido (algún timeout de ~3-4
   segundos)?** No se llegó a aislar con un experimento directo (p. ej.
   tipear los 2 caracteres muy rápido vs. muy lento y ver si el timing
   cambia el punto de falla). Queda como hipótesis abierta — ver
   "Próximos pasos".

### Lo que se sabe con certeza

- No es un crash ni un panic del kernel (no aparece "KERNEL PANIC" en el
  log serial en ningún momento).
- No hay corrupción de memoria visible — el `write()` del carácter #2 se
  ejecuta y su contenido es correcto (el eco en pantalla siempre muestra
  exactamente los caracteres tipeados, nunca basura).
- El `read()` de ambos caracteres siempre usa el fast-path de
  `sys_read` (`crate::keyboard::read_key()` encuentra el dato
  inmediatamente, sin necesidad de bloquear) — al menos en los tracings
  hechos, aunque esto puede depender del timing exacto del `sendkey` usado
  para simular el tipeo.
- `ash` llama a `exit()` (syscall 60) explícitamente — no hay señal, no hay
  fault de hardware.
- El path exacto (`TCSETS` → `write(fd=2)` → `TIOCSPGRP` → `setpgid` →
  `close` → `exit`) matchea línea por línea el cuerpo de `setjobctl(0)` +
  salida normal en `busybox/shell/ash.c`.

---

## Técnica de debugging usada (y por qué hace falta)

Ver también la memoria `debugging_technique_qemu_monitor.md` (si estás
leyendo esto como Claude con acceso a esa memoria) — acá va el resumen para
quien no tenga acceso a eso.

### Por qué no se puede simplemente usar la terminal interactiva

Este entorno no tiene una terminal gráfica real disponible para QEMU —
todo el testing se hace headless, así que hace falta simular el tipeo vía
el monitor de QEMU y leer el resultado del log serial (más una captura de
pantalla ocasional cuando hace falta ver píxeles reales).

### Cómo lanzar QEMU en modo debug

```bash
UEFI=<repo>/target/debug/build/so2-<hash>/out/uefi.img
OVMF_CODE=/usr/share/edk2/x64/OVMF_CODE.4m.fd
OVMF_VARS=<repo>/target/debug/build/so2-<hash>/out/OVMF_VARS.fd
qemu-system-x86_64 \
  -drive if=pflash,format=raw,readonly=on,file=$OVMF_CODE \
  -drive if=pflash,format=raw,file=$OVMF_VARS \
  -drive format=raw,file=$UEFI \
  -m 512M -cpu max \
  -serial file:/tmp/qemu-serial.log \
  -monitor unix:/tmp/qemu-mon.sock,server,nowait \
  -display none
```

(el hash exacto de `so2-<hash>` sale de `find target -iname "so2-*" -path
"*build*"` o de correr `cargo build` una vez y mirar el output).

Lanzar esto con el parámetro `run_in_background` de la herramienta Bash (o
equivalente) es más confiable que `nohup ... & disown` en este entorno —
se vio bastante flakiness con el segundo método en esta sesión.

### Cómo tipear

```bash
echo "sendkey p" | socat - UNIX-CONNECT:/tmp/qemu-mon.sock
```

Un `sendkey` por carácter, con nombres de tecla especiales para símbolos:
`spc` (espacio), `ret` (Enter), `shift-minus` (guión bajo `_`),
`ctrl-c` (para probar señales reales). Conviene esperar 0.15-0.3s entre
teclas — mandarlas demasiado rápido puede perder eventos.

**Importante para reproducir el bug de este documento**: el buffer de
teclado de este kernel es **global**, no por-proceso. Si se manda una
secuencia de teclas y el proceso que las iba a consumir muere antes de
leerlas todas (como en este mismo bug), los caracteres sobrantes quedan
en el buffer y los recibe el **próximo** proceso que lea de stdin — típicamente
el shell padre, que los intenta correr como comando. Esto generó bastante
ruido/confusión durante la investigación (ver los "💀 Killed PID N (shell):
exit(0)" en los logs, que son el shell padre recibiendo un "exit" que en
realidad estaba destinado a la instancia de `ash` ya muerta). Al reproducir
el bug, conviene rebootear QEMU entre intentos para empezar con el buffer
limpio, o al menos ser consciente de esto al leer los logs.

### Cómo ver stdout real de procesos de usuario

Desde el commit `42c7b7f` de esta misma sesión, el stdout de los procesos
de usuario (que antes solo iba al framebuffer, invisible en modo headless)
también se espeja al puerto serie con el prefijo `[fb] `
(`kernel/src/drivers/framebuffer_console.rs::FramebufferConsole::write`).
Ya no hace falta `screendump` para ver texto — solo para verificar
renderizado real (colores, cursor, etc.). stderr (fd 2) está atado al mismo
driver que stdout — antes iba solo a `/dev/console` (serie), así que errores
como `ash: clear: not found` quedaban invisibles en pantalla y solo se veían
grepeando `serial.log`; ahora se ven en ambos lados.

### Cómo agregar tracing temporal

Para este tipo de investigación, agregar `crate::serial_println_raw!(...)`
(la versión lock-free, segura desde cualquier contexto incluyendo ISRs) en
puntos clave y sacarlo después es más rápido que tratar de razonar todo
estáticamente. Los puntos que se instrumentaron en esta sesión (y que
probablemente valga la pena reinstrumentar si se retoma esto):

- `syscall_handler` (en `kernel/src/process/syscall.rs`, al principio) —
  loguea `(pid, nr_syscall, arg1, arg2, arg3)` de cada syscall. Ya existe
  un bloque comentado ahí mismo con el esqueleto, solo hay que
  descomentarlo/adaptarlo.
- `sys_read`'s rama `fd==0` — loguea si tomó el fast path (con qué
  carácter) o si se registró para bloquear.
- `stdin_wakeup()` — loguea qué carácter entrega al despertar a un
  proceso bloqueado en `read()`.
- `deliver_pending` (en `kernel/src/process/signal.rs`) — loguea cada
  señal que se entrega a cada proceso.
- El handler de `TIOCGPGRP` en `sys_ioctl` — loguea `FOREGROUND_PGID` y el
  `pgid` del que consulta.

**Todos estos se sacaron antes de commitear** — el código en el repo está
limpio de tracing. Si se retoma la investigación, hay que volver a
agregarlos (son ediciones chicas, no debería tomar mucho).

---

## Cómo reproducir todo esto

1. `cargo build` desde la raíz del repo (rebuildea kernel + userspace +
   busybox si falta — si `kernel/embedded/busybox.elf` ya existe con la
   config vieja, correr `scripts/build-busybox.sh` a mano primero para
   regenerarlo con la config nueva).
2. Lanzar QEMU en modo debug (ver arriba).
3. Esperar el prompt `$` del shell (`ConstanOS shell`).
4. Tipear `busybox ash` + Enter (con delays de ~0.15-0.2s entre teclas).
5. Esperar ~2.5s a que aparezca el banner + `#`.
6. Tipear un carácter, esperar ~1-2s, tipear otro. El segundo dispara la
   secuencia de muerte descrita arriba.

Para regression-testear que **no** se rompió nada de lo que sí funciona:

```
$ jobctl_test
```

corre `userspace/c/jobctl_test.c`, que ejercita `tcgetattr`/`tcsetattr`,
`setpgid`/`getpgid`, `fork`+`SIGSTOP`+`waitpid(WUNTRACED)`+`SIGCONT`+exit
real, y `kill(-pgid, SIGTERM)` matando dos procesos de un mismo grupo.
Debería terminar imprimiendo `jobctl_test: OK`. También correr `stat_test`
(ejercita `waitpid`/`chdir`/`dup`/`mkdir`/`poll` — para confirmar que nada
de lo viejo se rompió).

---

## Próximos pasos sugeridos

En orden de lo que probablemente sea más productivo:

1. **Aislar si `CONFIG_FEATURE_EDITING` es realmente el trigger.**
   Deshabilitarlo (volver a `# CONFIG_FEATURE_EDITING is not set`),
   rebuildear busybox, y ver si `ash` sigue muriendo a los 2 caracteres en
   modo no-interactivo/lectura simple de líneas. Si el bug desaparece,
   confirma que es específico del code path de `lineedit.c`
   (`read_line_input()`), no de algo más general en la negociación de
   terminal. Esto no se llegó a probar en la sesión anterior.

2. **Mirar `busybox/libbb/lineedit.c` directamente**, específicamente
   `read_line_input()` (línea ~2457 en la versión 1.36.1) y todo lo que
   corre entre dos llamadas a `read_key()`/`safe_read()` sucesivas. Buscar
   cualquier chequeo de "caracteres leídos" o de retorno de valores de
   `poll()`/`read()` que pudiera interpretar mal algo específico de la
   ABI de este kernel (p. ej., si `lineedit.c` espera que `read()` pueda
   devolver >1 byte en una sola llamada bajo ciertas condiciones, y nuestro
   `sys_read` SIEMPRE devuelve exactamente 1 byte por llamada para fd=0 —
   ver `kernel/src/process/syscall.rs::sys_read`, el comentario de
   `stdin: try to read from keyboard buffer`).

3. **Revisar si el "byte único" que se escribe a `fd=2` justo antes de
   morir (paso 2 de la secuencia de syscalls) tiene contenido
   diagnóstico.** No se llegó a inspeccionar qué byte es exactamente — se
   asumió que era un `\n` cosmético de `setjobctl(0)`, pero valdría la pena
   confirmarlo con un trace que imprima el contenido real del buffer en
   `sys_write` cuando `fd==2 && count==1`.

4. **Probar con un buildeo de BusyBox con símbolos de debug** y correrlo
   bajo algo que permita inspeccionar el estado de `ash` en el momento
   exacto de la decisión de salir (no hay debugger remoto configurado para
   userspace en este kernel actualmente — sería trabajo aparte armarlo, o
   alternativamente instrumentar `ash.c` mismo con prints de diagnóstico
   temporales, recompilando con `scripts/build-busybox.sh` cada vez).

5. **Confirmar/descartar la hipótesis de timing** (¿es realmente sobre
   *cantidad* de caracteres, o coincide con un tiempo transcurrido fijo?)
   tipeando los dos caracteres deliberadamente muy rápido (sin esperar
   entre `sendkey`) vs. muy lento (varios segundos entre uno y otro) y
   viendo si el punto de falla se mantiene en "carácter #2" o se corre.
