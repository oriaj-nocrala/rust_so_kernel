# 🦀 ConstanOS — Rust Operating System Kernel

Un kernel de sistema operativo x86_64 escrito en Rust desde cero, con multitarea preemptiva, aislamiento de memoria por proceso, un VFS propio, IPC, y soporte para binarios C reales vía un puerto propio de [mlibc](https://github.com/managarm/mlibc).

![ConstanOS: comando `demo` corriendo en QEMU — uname, VFS con ext2 real, threads, IPC, mmap y condvars en una sola sesión](docs/screenshot.png)

*Captura del comando `demo` (`userspace/src/bin/demo.rs`) — un recorrido guiado por varias capacidades del kernel en una sola corrida real, no texto estático.*

![DOOM corriendo de verdad en ConstanOS — partida real en curso, HUD con vida/armadura/munición, "PICKED UP A STIMPACK."](docs/doom-screenshot.png)

*Sí, corre DOOM. Captura real de una partida en curso — ver la sección de abajo.*

![Quake corriendo de verdad en ConstanOS — partida real en curso, HUD con vida/armadura/munición, "You receive 25 health / You get 2 rockets"](docs/quake-screenshot.png)

*Y también corre Quake — motor de software rendering real, de punta a punta. Ver la sección de abajo.*

## 📋 Descripción

Empezó como un proyecto de aprendizaje ("SO2") para explorar desarrollo de sistemas operativos en Rust, y creció hasta tener un scheduler preemptivo real, syscalls compatibles con la ABI de Linux, fork con copy-on-write, un loader de ELF, y un port de libc que permite compilar y correr programas en C sin modificaciones (más allá del target).

## 🚀 Qué tiene implementado

- **Boot UEFI** vía el crate `bootloader`, framebuffer + consola serial.
- **Memoria**: buddy allocator físico (bitmap O(1)) + slab allocator para el heap del kernel; paginación por proceso con demand paging y VMAs.
- **Multitarea preemptiva**: scheduler de prioridades multinivel, quantum dinámico, aging anti-starvation.
- **Procesos**: `fork()` con copy-on-write real, `exec()` vía loader de ELF64, `waitpid()`, aislamiento completo de tablas de páginas por proceso.
- **Threads reales**: `clone()` crea un `Process` que comparte el `AddressSpace` (`Arc<AddressSpace>`, sin COW) *y* la `FileDescriptorTable` (`Arc<Mutex<..>>`) del padre en vez de aislarlos — soporta `pthread_create`/`pthread_join`/`pthread_cond_*` de mlibc de punta a punta. Un thread que sale se reap-ea inmediatamente en el kernel (no queda zombie: `pthread_join` de mlibc es 100% futex-based y nunca llama `waitpid()` sobre el tid).
- **Pipes** (`pipe(2)`): IPC anónima con ring buffer, lectura/escritura bloqueante, `EOF`/`EPIPE`, fds heredados por `fork()` (refcount por extremo vía `FileHandle::dup()`).
- **Señales POSIX**: `kill`, `sigaction`, `sigprocmask`, `sigreturn` — `SIGKILL`/`SIGTERM`/`SIGSEGV`/`SIGPIPE`/`SIGUSR1`/`SIGUSR2` (default: terminar) y `SIGCHLD` (default: ignorar). El kernel arma el frame de la señal en la propia pila de usuario y lo redirige a través de una página trampolín mapeada de forma transparente (mlibc no necesita instalar `sa_restorer`). La entrega se engancha en cada retorno a modo usuario: fin de syscall, preempción por timer, y cada wakeup de una syscall bloqueante.
- **Syscalls** con números compatibles con Linux (`read`, `write`, `open`, `mmap`, `fork`, `clone`, `exec`, `futex`, `arch_prctl`, `poll`/`epoll`, `clock_gettime`, `pipe`, `kill`, `sigaction`, `sigprocmask`, `sigreturn`, `mkdir`/`rmdir`/`rename`/`unlink`/`symlink`, `access`/`chmod`, `dup`/`dup2`/`fcntl`, `statvfs`, ...) entradas por la instrucción `syscall` (MSR LSTAR).
- **`exec()` con argv/envp reales**: `sys_exec` arma un stack ABI (SysV) real y dinámico — strings + tabla de punteros + auxv — en vez de un `argc=0` fijo. Un `main(int argc, char **argv, char **envp)` de C recibe argumentos reales sin ningún cambio del lado de mlibc.
- **VFS propio, con escritura y symlinks reales**: initramfs + devfs (`/dev/null`, `/dev/zero`, `/dev/console`, `/dev/fb`, `/dev/kbd`) + ramfs en `/tmp` (árbol recursivo real — `mkdir`/`unlink`/`rmdir`/`rename`/**`symlink`** de verdad, no un namespace plano) + **ext2 de lectura y escritura real en `/mnt`**, sobre un driver ATA PIO propio (canal secundario IDE) — el disco (`disk.img`, sembrado con `mke2fs -d`) sobrevive entre corridas de `cargo run`. `stat`/`lstat`/`readlink`/`getdents64`, `chdir`/`getcwd` reales (paths relativos andan). `/proc` enumera procesos vivos de verdad (`ls /proc`, `/proc/<pid>/stat` con el formato clásico de Linux) — es lo que hace andar `ps`/`top` de BusyBox.
- **ext2 con escritura real, no solo lectura**: allocation real de bloques e inodos (bitmaps + contadores libres), bloques directos/indirectos/doblemente/triplemente indirectos (archivos de ~16GB+), symlinks reales (representación "fast", target inline en el inodo, y "slow", como archivo normal — igual que un ext2 real) y `chmod`/`fchmod` reales (persisten los bits de permiso, el único filesystem acá con permisos por-inodo de verdad). Dos pasadas de reparación al montar (antes de exponer `/mnt` a la VFS) recuperan del `disk.img` cualquier bloque/inodo que un apagado sucio haya dejado a medio asignar — sin journal, así que esto reemplaza lo que haría un `e2fsck` real. En el camino se encontró y arregló un bug serio: el orden de esas dos pasadas hacía que el filesystem completo pareciera "huérfano" en el primer mount de cualquier imagen (el propio inodo de la raíz caía dentro del rango que se pre-marcaba como reservado, cortando el recorrido de alcanzabilidad antes de visitar un solo archivo real) — cada corrida terminaba reciclando silenciosamente bloques todavía en uso.
- **`dup`/`dup2`/`fcntl(F_DUPFD)`** con semántica POSIX real — dos fds duplicados comparten el mismo offset de lectura/escritura (`Arc<Mutex<usize>>`, no una copia independiente). Habilita **redirección real en la shell** (`>`, `>>`, `<`, `2>`, `2>>`, `2>&1`, `1>&2`).
- **`waitpid` con exit status real**: `WIFEXITED`/`WIFSIGNALED`/`WEXITSTATUS` reflejan el código de salida o la señal real del hijo, no un `exited(0)` fijo.
- **IPC**: canales tipo socket (`socket`/`bind`/`connect`/`accept`/`sendmsg`/`recvmsg`) con `poll`/`epoll`.
- **Tiempo**: TSC calibrado contra el PIT, hrtimer, `nanosleep`, `clock_gettime`. Reloj de pared real vía un driver de RTC CMOS propio (`kernel/src/rtc.rs`, puertos `0x70`/`0x71`) leído una vez al bootear — `CLOCK_REALTIME` ahora devuelve la hora real, no "boot = epoch".
- **Consola con framebuffer** con soporte de escapes ANSI real (colores, posicionamiento de cursor, clear screen/line) — suficiente para que aplicaciones full-screen como `vi`/`less` dibujen bien. `ioctl` da termios real (`TCGETS`/`TCSETS`), tamaño de terminal real vía `TIOCGWINSZ` (calculado del framebuffer, no un 80×25 fijo), y control de grupo de terminal (`TIOCGPGRP`/`TIOCSPGRP`) — suficiente para job control real.
- **mlibc portado a este kernel** (`mlibc-port/`, ver más abajo): permite compilar programas en C reales (`printf`, `malloc`, TLS, stdio con buffering) contra la ABI de syscalls propia. En el camino se encontró y parchó un bug real de mlibc *upstream* (no de este puerto): `sscanf`/`fscanf` cortaban de raíz en la primera conversión suprimida (`%*s`) — afecta a cualquier programa que use ese patrón, se descubrió porque rompía en silencio el parser de `/proc/<pid>/stat` de BusyBox `ps`/`top`. El parche vive en `scripts/setup-mlibc.sh` (se re-aplica solo, sobrevive a un reset del submódulo), no en el checkout.
- **BusyBox real corriendo, con uso real**: BusyBox 1.36.1 (fuente oficial sin modificar, submódulo git) compila y corre contra el `sysroot/` propio. Ya no es solo `busybox echo hello`: `ash` (con job control real) es la shell interactiva de PID 1 en adelante, y hay ~60 applets reales — `vi` (editor full-screen), `grep`/`sed`/`awk`/`find`/`sort`/`diff`, `tar`/`gzip`/`gunzip`, `ps`/`top` (vía `/proc` real), `df` (vía `statvfs`), `du`, `chmod`, `id`/`hostname`, `md5sum`, `od`/`hexdump`, `less`/`more`. Al bootear, PID 1 corre `busybox --install -s /tmp/bin` de verdad — `symlink()` real por cada applet, la misma mecánica que usa una instalación real de Linux (un binario multicall + symlinks reales + dispatch por `argv[0]`), no algo calculado por el kernel.
- **🎮 DOOM corre y se juega de verdad, con mouse-look y sonido**: [doomgeneric](https://github.com/ozkl/doomgeneric) (submódulo git) + un puerto propio (`doom-port/doomgeneric_constanos.c` + `doomgeneric_sound_constanos.c`) sobre las primitivas del kernel — un ioctl `FBIO_BLIT` en `/dev/fb` (el juego manda su propio buffer offscreen, el kernel lo escala y bliteá directo al framebuffer real), `/dev/input/event0`+`/dev/input/event1` wire-compatible con el evdev real de Linux (`event1` es un driver PS/2 mouse real: IRQ12, secuencia de habilitación del controlador 8042, decodificación de paquetes de 3 bytes — mouse-look de verdad: girar con X, avanzar/retroceder con Y, disparar con el botón izquierdo), y `/dev/dsp` sobre un **driver PCI AC97 real** (`kernel/src/ac97.rs` + `kernel/src/pci.rs`, el primer código PCI de este kernel — enumeración de config-space desde cero, DMA bus-mastering por buffer-descriptor-list, todo por *polling* en vez de IRQ ya que el IDT es un `Once` poblado antes de que exista memoria dinámica). El puerto de sonido decodifica los lumps DMX de los efectos, los mezcla (hasta 16 canales, resample 16.16 de punto fijo) y escribe PCM de 48kHz estéreo real — verificado capturando la salida real con el backend `wav` de QEMU e inspeccionando el `.wav` resultante (picos de -3dB, no silencio). Sin música (este fork de doomgeneric no trae ningún sintetizador MIDI/OPL, algo aparte del driver de audio en sí). El IWAD (Freedoom, licencia libre) se lee desde `/mnt/freedoom1.wad` (ext2). Escribir "doom" desde `ash` — probado de punta a punta: pantalla de título, menús, partida real, movimiento, giro, disparo con mouse y efectos de sonido reales. En el camino aparecieron y se arreglaron 3 bugs reales y preexistentes del kernel: `ext2` solo soportaba bloques directos + indirectos simples (tope ~268KB por archivo, ahora soporta doblemente indirectos), un `read()` escribiendo en un buffer de usuario recién `malloc`-eado (nunca tocado en modo usuario) paniqueaba el kernel por tratar todo fault en modo kernel como bug irrecuperable, y `lseek()` era un stub completo que siempre devolvía `ESPIPE` desde una época en que no existían archivos reales. La causa raíz real de los síntomas "el WAD se vacía" terminó siendo un mismatch de ABI: `SEEK_SET` valía `3` en un header de mlibc copiado de un puerto no-Linux, en vez del `0` que esta ABI espera — una vez arreglado, el WAD se sirve directo desde ext2 sin necesidad de un device kernel-embedded como workaround.
- **🎮 Quake también corre y se juega de verdad**: [quakegeneric](https://github.com/erysdren/quakegeneric) (submódulo git, un puerto minimalista al estilo doomgeneric del código GPL de WinQuake de id Software) + un puerto propio (`quake-port/quakegeneric_constanos.c`) sobre las mismas primitivas del kernel que DOOM: `FBIO_BLIT` en `/dev/fb` (acá el motor entrega un buffer paletizado de 8bpp a 320x240, así que el puerto hace la conversión índice→RGB él mismo antes de blitear) y `/dev/input/event0`+`event1` (mismo evdev real, pero acá el motor los pide — `QG_GetKey`/`QG_GetMouseMove` — en vez de que el puerto empuje eventos como hace DOOM). El propio README de quakegeneric dice que "solo compila para 32 bits" — antes de meterle tiempo real, lo clonamos y lo compilamos con `-m64` en el host: compila limpio, con dos warnings inofensivos nada más (ningún puntero real truncado) — la advertencia del upstream resultó ser más conservadora de lo necesario para este caso. El shareware `id1/pak0.pak` (licencia de redistribución libre de id Software, misma lógica que el WAD shareware de Doom) se baja de un espejo en archive.org y se siembra en `disk.img` (ahora 96MB, no 48MB, para entrar junto con el WAD de Freedoom). En el camino apareció un gap real de este kernel: el motor de Quake (C de 1996, nunca escrito contra una pila chica) desbordaba la pila de usuario fija de 64KB que todo proceso recibe. La solución real es una **pila que crece sola** (`VmaKind::GrowableStack` — arranca en 64KB y el page fault handler la extiende hacia abajo bajo demanda, hasta un tope de 8MB tipo `RLIMIT_STACK`), no adivinar cuánta pila va a necesitar cada programa de antemano. En el camino de diagnosticar esto se encontró, por separado, un bug real y preexistente: `busybox --install` cuelga o hace double-fault en su propio `fork()` más o menos 1 de cada 3-4 boots — reproducible tal cual en el código sin tocar (sin Quake, sin cambios de pila), así que no tiene nada que ver con el tamaño de la pila (un diagnóstico intermedio culpó al tamaño de la pila; estaba mal). Sigue sin diagnosticar del todo. Sin sonido todavía (el motor linkea el backend silencioso `snd_null.c` de upstream — un `/dev/dsp` real reusando el mismo driver AC97 de DOOM es candidato natural para después). Escribir "quake" desde `ash` — probado de punta a punta: demo de la pantalla de título, menú principal navegable, partida real jugándose sola en el demo, HUD completo.
- **Salida limpia de clientes de framebuffer crudo**: `FBIO_BLIT` (usado por DOOM y Quake) escribe píxeles directo al framebuffer sin pasar por el tracking de cursor/ANSI de la consola de texto — al salir, el siguiente `write()` de texto (el prompt de la shell) detecta el framebuffer "sucio" y limpia la pantalla antes de dibujar, en vez de quedar el último frame del juego debajo del texto nuevo.

### Programas de usuario incluidos

`shell` es PID 1 — no una shell interactiva en sí misma, sino un loop mínimo de init: instala symlinks reales de BusyBox (`busybox --install -s /tmp/bin`) y después hace `fork`+`exec` de `busybox ash` en loop, relanzándola si sale (por `exit`, Ctrl-D, o un crash). `ash` es la shell interactiva real; todo lo demás se lanza a demanda desde ahí (por nombre, si es un applet de BusyBox, o por ruta/`$PATH` para el resto de estos binarios).

| Programa | Qué hace |
|---|---|
| `shell` | PID 1: instala symlinks reales de BusyBox y mantiene `ash` corriendo (ver arriba) — ya no es un REPL propio |
| `busybox` | **BusyBox 1.36.1 real**, sin modificar — `ash` (shell interactiva con job control), más ~60 applets (`vi`, `grep`, `tar`, `ps`, `top`, `df`, ...). Config en `busybox-config/minimal.config`, ver `scripts/build-busybox.sh` |
| `demo` | Recorrido guiado por varias capacidades en una corrida: VFS (initramfs/devfs/ramfs/**ext2 real**), threads con `meminfo` antes/después, IPC, mmap, condvars — la captura de arriba |
| `uname` | Info del sistema |
| `uptime` / `tsc` | Demos de tiempo (hrtimer, TSC) — `sleep` real ahora lo da BusyBox |
| `snake` | El clásico, dibujado con ANSI sobre `/dev/fb`, input no bloqueante por `/dev/kbd` |
| `ipc_ping` | Demo de IPC: fork + servidor + cliente, 100 round-trips por canal |
| `mmap_test` / `poll_test` | Ejercitan `mmap`/`munmap` y `poll` end-to-end |
| `hello` | Programa en **C real**, compilado y linkeado contra mlibc — `printf("Hello from user!\n")` pasando por todo el stack de stdio de libc |
| `pthread_test` | Programa en **C real**: 3 threads (`pthread_create`) incrementando un contador bajo mutex, `pthread_join`, verifica el resultado — ejercita `clone()` de punta a punta |
| `producer_consumer` | Programa en **C real**: productor/consumidor con `pthread_cond_t` (`pthread_cond_wait`/`broadcast`) sobre un ring buffer — ejercita el path de condvars (dos futex words por hilo) |
| `pipe_test` | `pipe()` + `fork()`: el hijo escribe un mensaje y cierra, el padre lee hasta `EOF` y compara |
| `signal_test` | ABI cruda del kernel: `sigaction(SIGUSR1)`, `fork()`, el hijo hace `kill()` al padre, verifica entrega + retorno vía `sigreturn`, y que `SIGCHLD` llegue al salir el hijo |
| `mlibc_signal_test` | Programa en **C real**: lo mismo que `signal_test` pero pasando por `pipe()`/`fork()`/`kill()`/`sigaction()` reales de mlibc |
| `stat_test` / `argv_test` / `jobctl_test` | Programas en **C real**: ejercitan `stat`/`fstat`/`lstat`, argv/envp reales de `exec()`, y job control (`tcgetpgrp`/`tcsetpgrp`, señales de terminal) respectivamente |
| `kdebug` | Prende/apaga en caliente los subsistemas de tracing del kernel (`kernel::debug`) sin recompilar — ver `kdebug_ctl` en la tabla de syscalls |
| `doom` | **DOOM real, jugable, con mouse-look y sonido** — [doomgeneric](https://github.com/ozkl/doomgeneric) + puerto propio sobre `FBIO_BLIT` (`/dev/fb`), `/dev/input/event0` (teclado), `/dev/input/event1` (mouse PS/2, evdev real) y `/dev/dsp` (driver PCI AC97 real), IWAD Freedoom leído de `/mnt/freedoom1.wad` (ext2). Ver la entrada de arriba |
| `quake` | **Quake real, jugable** — [quakegeneric](https://github.com/erysdren/quakegeneric) + puerto propio sobre `FBIO_BLIT` (con conversión índice→RGB propia, el motor entrega paletizado), `/dev/input/event0`+`event1` (evdev real, pull-based), shareware `id1/pak0.pak` leído de `/mnt` (ext2). Sin sonido todavía. Ver la entrada de arriba |

## 🏗️ Estructura del workspace

```
.
├── kernel/              # Kernel bare-metal (#![no_std], target x86_64-unknown-none)
│   ├── src/
│   │   ├── memory/       # Buddy/slab allocators, paginación, ELF loader, demand paging
│   │   ├── process/      # Scheduler, syscalls, fork/exec, trapframes
│   │   ├── fs/           # VFS: initramfs, devfs, ramfs (symlinks reales), ext2 (lectura/escritura real), procfs, tipos compartidos
│   │   ├── ipc/          # Canales tipo socket
│   │   ├── block/        # Driver ATA PIO (canal secundario IDE)
│   │   ├── drivers/      # /dev/null, /dev/zero, /dev/console, /dev/fb, /dev/kbd, /dev/input/event0+1 (evdev: teclado+mouse), /dev/dsp (AC97)
│   │   └── time/         # TSC, hrtimer, clocksource
│   └── embedded/         # ELFs de userspace embebidos vía include_bytes!
├── userspace/            # Programas de usuario en Rust (workspace Cargo separado)
│   └── c/                 # Programas de usuario en C real (hello, stat_test, argv_test, ...)
├── doomgeneric/          # Submódulo git: doomgeneric (ozkl/doomgeneric)
├── doom-port/            # Puerto propio de doomgeneric a este kernel (video/input + sonido en archivos separados)
├── quakegeneric/         # Submódulo git: quakegeneric (erysdren/quakegeneric)
├── quake-port/           # Puerto propio de quakegeneric a este kernel
├── mlibc/                # Submódulo git: mlibc upstream (managarm/mlibc)
├── mlibc-port/           # Puerto propio de mlibc a este kernel (sysdeps "constanos")
├── busybox/              # Submódulo git: BusyBox oficial (git.busybox.net), pineado en 1_36_1
├── busybox-config/        # .config mínimo versionado para BusyBox
├── scripts/setup-mlibc.sh    # Reconstruye el sysroot de mlibc automáticamente
├── scripts/build-busybox.sh  # Compila BusyBox contra el sysroot automáticamente
├── scripts/build-doom.sh     # Compila doomgeneric + el puerto propio contra el sysroot
├── scripts/fetch-freedoom.sh # Descarga el IWAD de Freedoom (no versionado, ~29MB)
├── scripts/build-quake.sh        # Compila quakegeneric + el puerto propio contra el sysroot
├── scripts/fetch-quake-shareware.sh # Descarga el shareware pak0.pak de Quake (no versionado, ~18MB)
├── disk-image-root/      # Contenido semilla del disco ext2 (/mnt) — mke2fs -d
└── build.rs / src/main.rs # Host: arma la imagen UEFI + disk.img, lanza QEMU
```

## 🚀 Cómo correrlo

Requisitos:
- Toolchain de Rust **nightly** (fijado en `rust-toolchain.toml`, se instala solo con `rustup`).
- `qemu-system-x86_64`.
- `clang`, `llvm` (para `llvm-ar`/`llvm-strip`/`llvm-objcopy`), `meson`, `ninja` — para compilar el sysroot de mlibc la primera vez.
- `make` — para compilar BusyBox (submódulo `busybox/`) contra ese sysroot.
- `e2fsprogs` (`mke2fs`) — para armar `disk.img` (el ext2 que se monta en `/mnt`) la primera vez. Opcional: sin esto el build sigue, simplemente no hay `/mnt`.
- `curl`, `unzip` — para bajar el IWAD de Freedoom (~29MB) y el shareware de Quake (~18MB) la primera vez, ninguno versionado. Sin esto el build sigue, simplemente `doom`/`quake` no tienen con qué correr.

En Arch:
```bash
sudo pacman -S qemu-system-x86 qemu-img qemu-ui-gtk edk2-ovmf clang llvm meson ninja lld e2fsprogs
```

Y listo:
```bash
cargo run
```

Este comando, desde un clon limpio, hace **todo** solo: inicializa los submódulos `mlibc/`, `busybox/`, `doomgeneric/` y `quakegeneric/`, arma el sysroot de mlibc (`sysroot/`), compila los programas de `userspace/` (Rust y C), compila BusyBox, DOOM y Quake contra ese sysroot, baja el IWAD de Freedoom y el shareware de Quake, compila el kernel, arma la imagen UEFI, y levanta QEMU (con ventana gráfica si tenés `qemu-ui-gtk`, si no cae a VNC). BusyBox, DOOM y Quake solo se recompilan si sus `.elf` faltan en `kernel/embedded/` — a diferencia del resto, esos pasos tardan de verdad.

## 🎯 Estado / por implementar

Lo que falta o está a medias, mirando el propio código:

- ⏳ **Sin linker dinámico**: `exec()` solo carga binarios estáticos, no hay `.so`/relocations.
- ⏳ **Un solo core real**: la infraestructura para SMP existe (arrays por-CPU, `MAX_CPUS=8`) pero `cpu_id()` siempre devuelve 0. Todo el modelo de concurrencia actual (`cli`/`sti` + `spin::Mutex`) asume esto — el día que haya un segundo core de verdad, cada sitio que usa `cli` como si fuera exclusión mutua (no solo el propio lock) necesita auditoría, no es un cambio aislado.
- ✅ **FPU/SSE guardado en los context switches, arreglado**: `Process::fpu_state` (imagen FXSAVE de 512 bytes) ahora se guarda/restaura de verdad (`fxsave`/`fxrstor`) en cada punto de cambio de contexto — antes `TrapFrame` solo tenía registros de propósito general, así que una preempción en medio de una computación de punto flotante corrompía silenciosamente `xmm0`-`xmm15`. `fork()` copia los registros *en vivo* del padre (semántica real de `fork()`); `clone()` (threads) arranca con el estado default; `exec()` lo resetea, igual que hace un `execve()` real. Verificado con `fpu_test`: carga un patrón de 128 bits en `xmm0` vía asm inline, gira en un loop entero puro el tiempo suficiente para atravesar cientos de preempciones reales (confirmado con un contador nuevo, `switches_total` en `/proc/kdebug`, no solo tiempo transcurrido), y chequea que sobrevivió intacto. Esto era el bloqueante real para portar algo como Quake (motor 100% en punto flotante) — ya no lo es.
- ✅ **ext2 con escritura real, arreglado**: `/mnt` ahora soporta `create`/`mkdir`/`unlink`/`rmdir`/`rename`/`symlink`/`chmod` de verdad (allocation real de bloques/inodos, ver más arriba) — ya no hace falta pasar por `/tmp` (ramfs, sin persistencia entre reboots) para tener escritura real.
- ⏳ **`mmap` solo anónimo**: no hay mmap de archivos/devices (`fd` tiene que ser `-1`). Bloquea, por ejemplo, un framebuffer mapeable directamente en vez de escrito por syscall.
- ✅ **Leak de stack por hilo, arreglado en su mayor parte**: el `mmap()` de 2MiB que mlibc arma para la pila de cada `pthread_create` nunca se liberaba — es un gap de mlibc *upstream* (`pthread_exit`/`thread_join` tienen TODOs/FIXMEs propios admitiéndolo), no específico de este puerto. El kernel ahora lo libera solo al morir el hilo (mismo patrón de liberación diferida que `kernel_stack`, evitando el mismo peligro de liberar la pila mientras el hilo todavía corre sobre ella). Con `meminfo`: bajó de ~8.9MB a ~2.7MB perdidos por corrida de `pthread_test` — probablemente el TCB en sí (`thread_join`'s FIXME: "destroy tcb here, currently we leak it"), sin investigar todavía.

---

*Proyecto de aprendizaje personal para explorar el desarrollo de sistemas operativos en Rust — con bastante ayuda de Claude Code en el camino.*
