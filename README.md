# 🦀 ConstanOS — Rust Operating System Kernel

Un kernel de sistema operativo x86_64 escrito en Rust desde cero, con multitarea preemptiva, aislamiento de memoria por proceso, un VFS propio, IPC, y soporte para binarios C reales vía un puerto propio de [mlibc](https://github.com/managarm/mlibc).

![ConstanOS shell corriendo en QEMU](docs/screenshot.png)

## 📋 Descripción

Empezó como un proyecto de aprendizaje ("SO2") para explorar desarrollo de sistemas operativos en Rust, y creció hasta tener un scheduler preemptivo real, syscalls compatibles con la ABI de Linux, fork con copy-on-write, un loader de ELF, y un port de libc que permite compilar y correr programas en C sin modificaciones (más allá del target).

## 🚀 Qué tiene implementado

- **Boot UEFI** vía el crate `bootloader`, framebuffer + consola serial.
- **Memoria**: buddy allocator físico (bitmap O(1)) + slab allocator para el heap del kernel; paginación por proceso con demand paging y VMAs.
- **Multitarea preemptiva**: scheduler de prioridades multinivel, quantum dinámico, aging anti-starvation.
- **Procesos**: `fork()` con copy-on-write real, `exec()` vía loader de ELF64, `waitpid()`, aislamiento completo de tablas de páginas por proceso.
- **Syscalls** con números compatibles con Linux (`read`, `write`, `open`, `mmap`, `fork`, `exec`, `futex`, `arch_prctl`, `poll`/`epoll`, `clock_gettime`, ...) entradas por la instrucción `syscall` (MSR LSTAR).
- **VFS propio**: initramfs + devfs (`/dev/null`, `/dev/zero`, `/dev/console`, `/dev/fb`, `/dev/kbd`), `stat`/`getdents64`.
- **IPC**: canales tipo socket (`socket`/`bind`/`connect`/`accept`/`sendmsg`/`recvmsg`) con `poll`/`epoll`.
- **Tiempo**: TSC calibrado contra el PIT, hrtimer, `nanosleep`, `clock_gettime`.
- **Consola con framebuffer** con soporte de escapes ANSI (colores, posicionamiento de cursor).
- **mlibc portado a este kernel** (`mlibc-port/`, ver más abajo): permite compilar programas en C reales (`printf`, `malloc`, TLS, stdio con buffering) contra la ABI de syscalls propia.

### Programas de usuario incluidos

Un pequeño shell interactivo (`shell`) hace `fork`+`exec` de estos binarios:

| Programa | Qué hace |
|---|---|
| `shell` | REPL con `help`, y despacho de comandos a los demás binarios |
| `ls` | Lista archivos vía `getdents64` real sobre el VFS |
| `uname` | Info del sistema |
| `uptime` / `sleep` / `tsc` | Demos de tiempo (hrtimer, `nanosleep`, TSC) |
| `snake` | El clásico, dibujado con ANSI sobre `/dev/fb`, input no bloqueante por `/dev/kbd` |
| `ipc_ping` | Demo de IPC: fork + servidor + cliente, 100 round-trips por canal |
| `mmap_test` / `poll_test` | Ejercitan `mmap`/`munmap` y `poll` end-to-end |
| `hello` | Programa en **C real**, compilado y linkeado contra mlibc — `printf("Hello from user!\n")` pasando por todo el stack de stdio de libc |

## 🏗️ Estructura del workspace

```
.
├── kernel/              # Kernel bare-metal (#![no_std], target x86_64-unknown-none)
│   ├── src/
│   │   ├── memory/       # Buddy/slab allocators, paginación, ELF loader, demand paging
│   │   ├── process/      # Scheduler, syscalls, fork/exec, trapframes
│   │   ├── fs/           # VFS: initramfs, devfs, tipos compartidos
│   │   ├── ipc/          # Canales tipo socket
│   │   ├── drivers/      # /dev/null, /dev/zero, /dev/console, /dev/fb, /dev/kbd
│   │   └── time/         # TSC, hrtimer, clocksource
│   └── embedded/         # ELFs de userspace embebidos vía include_bytes!
├── userspace/            # Programas de usuario en Rust (workspace Cargo separado)
├── mlibc/                # Submódulo git: mlibc upstream (managarm/mlibc)
├── mlibc-port/           # Puerto propio de mlibc a este kernel (sysdeps "constanos")
├── scripts/setup-mlibc.sh # Reconstruye el sysroot de mlibc automáticamente
└── build.rs / src/main.rs # Host: arma la imagen UEFI y lanza QEMU
```

## 🚀 Cómo correrlo

Requisitos:
- Toolchain de Rust **nightly** (fijado en `rust-toolchain.toml`, se instala solo con `rustup`).
- `qemu-system-x86_64`.
- `clang`, `llvm` (para `llvm-ar`/`llvm-strip`/`llvm-objcopy`), `meson`, `ninja` — para compilar el sysroot de mlibc la primera vez.

En Arch:
```bash
sudo pacman -S qemu-system-x86 qemu-img qemu-ui-gtk edk2-ovmf clang llvm meson ninja lld
```

Y listo:
```bash
cargo run
```

Este comando, desde un clon limpio, hace **todo** solo: inicializa el submódulo `mlibc/`, arma su sysroot (`sysroot/`), compila los programas de `userspace/` y el `hello.c`, compila el kernel, arma la imagen UEFI, y levanta QEMU (con ventana gráfica si tenés `qemu-ui-gtk`, si no cae a VNC).

## 🎯 Estado / por implementar

Lo que falta o está a medias, mirando el propio código:

- ⏳ **Sin linker dinámico**: `exec()` solo carga binarios estáticos, no hay `.so`/relocations.
- ⏳ **Un solo core real**: la infraestructura para SMP existe (arrays por-CPU, `MAX_CPUS=8`) pero `cpu_id()` siempre devuelve 0.
- ⏳ **Threads**: `futex`/`clone` son stubs de un solo hilo, no hay pthreads reales todavía.
- ⏳ **`sys_yield`**: no dispara un context switch de verdad (no-op).
- ⏳ **Filesystem persistente**: el VFS es sólido pero todo vive en un initramfs de solo lectura — no hay nada escribible en disco.

---

*Proyecto de aprendizaje personal para explorar el desarrollo de sistemas operativos en Rust — con bastante ayuda de Claude Code en el camino.*
