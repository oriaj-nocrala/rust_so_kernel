# Plan: extraer `fs::vfs` + `fs::ramfs` a un crate host-testeable

> **Estado: COMPLETADO** (2026-09-09). Los 6 pasos de la migración están
> hechos: 1 (`fs::types` → `vfs::types` + arreglo del orphan rule en
> `fs/ext2.rs`) en `fb91658`; 2 (`FileHandle`/`FileError`/`FileResult`/
> `compute_seek` → `vfs::file`) en `febca7d`; 3 (traits `Inode`/`Filesystem`
> → `vfs::inode`, helpers getdents64 → `vfs::dirent`) en `d098724`; 4
> (`MountTable` + resolución de rutas + mutaciones VFS → `vfs::mount`,
> `normalize_path`/`split_parent` → `vfs::path`) en `c01027b`; 5 (`ramfs`
> entero + seam `DirLockObserver` → `vfs::ramfs`) en `6f73baa`; y 6 (este
> commit: docs, corrección de un comentario de test, verificación final).
> `vfs/` tiene 150 tests de host (`cd vfs && cargo test`), `cargo build`
> compila limpio, y `scripts/run-kernel-tests.sh` da `PASS` (3/3) — línea
> base de `hal`/`ext2`/`mm` (71/90/33+4) sin cambios. Ver el doc comment de
> `vfs/src/lib.rs` para el detalle módulo a módulo de qué vive dónde.

> **Estado: en ejecución.** Escrito 2026-09-09, tras cerrar la extracción de
> `mm` y con `hal`/`ext2`/`mm` ya en su sitio. Es el paso 1 de la línea
> "host-testable extraction: next steps" (vfs+ramfs, luego scheduler con
> reloj falso).

## Por qué

`kernel/src/fs/vfs.rs` (530 líneas) contiene la resolución de rutas del
sistema entero: match de prefijo más largo sobre la tabla de mounts, walk por
componentes, seguimiento de symlinks con guard de 8 saltos para `ELOOP`,
resolución de targets relativos contra el directorio contenedor del symlink,
y las mutaciones a nivel VFS (`mkdir`/`symlink`/`unlink`/`rmdir`/`rename` con
rollback). `kernel/src/fs/ramfs.rs` (438 líneas) es la única implementación
concreta de filesystem que no depende de hardware. Hoy **nada de eso se puede
ejercitar sin arrancar QEMU** (ver CLAUDE.md: `-Z build-std` + doble build del
bin target ⇒ colisión de lang items en `core`).

Los bugs que este código puede tener son exactamente del tipo que un test de
host caza barato y un arranque de QEMU caza tarde: `..` resuelto contra el
mount equivocado, un symlink relativo resuelto contra `/` en vez de contra su
directorio, un ciclo de symlinks que agota la pila en lugar de devolver
`ELOOP`, un `rename` fallido que pierde el fichero porque el rollback no se
ejecutó.

## Forma final

Mismo split que `ext2`: **núcleo puro + adaptador delgado en el kernel.** La
diferencia con `ext2` es que aquí *los traits mismos* son el núcleo, así que
se mudan con él — igual que pasó con `hal::PortIo`/`PhysMem`.

```
vfs/                         (crate nuevo, no_std + alloc, host-testeable)
  ├─ types    — Errno, FileType, OpenFlags, Stat, DirEntry
  ├─ file     — FileError, FileResult, compute_seek, trait FileHandle
  ├─ inode    — trait Inode, trait Filesystem
  ├─ dirent   — getdents64_via_readdir / _from_snapshot
  ├─ path     — normalize_path, split_parent
  ├─ mount    — struct MountTable (NO un static) + resolve/open/mkdir/...
  └─ ramfs    — RamFs y sus nodos, con el seam DirLockObserver

kernel/src/fs/types.rs       `pub use vfs::types::*`
kernel/src/process/file.rs   `pub use vfs::file::*` + FileDescriptorTable
kernel/src/fs/vfs.rs         static MOUNTS: Once<MountTable> + funciones libres
kernel/src/fs/ramfs.rs       RamFs::with_observer(&KernelDirLockObserver)
```

### Cuatro decisiones de diseño

**1. `fs::types` y `FileHandle` se mudan con los traits.** No es opcional:
`Inode::open()` devuelve `Box<dyn FileHandle>` y todo el trait habla en
`Errno`/`Stat`/`DirEntry`. `fs::types` ya declara en su cabecera que no
depende de ningún otro módulo del kernel, así que se muda entero. De
`process/file.rs` se muda solo la mitad pura (trait + errores +
`compute_seek`); `FileDescriptorTable` se queda, porque usa `crate::drivers`
y `serial_println!`. **Ningún `use` del kernel cambia**: los dos archivos
originales quedan como `pub use` de lo movido.

**2. El estado global se queda en el adaptador, la lógica se va.** `MountTable`
pasa a ser un struct normal con métodos; el `static MOUNTS: Once<...>` se
queda en `kernel/src/fs/vfs.rs`, exactamente igual que `EXT2: Once<Ext2Fs>`
se quedó en `kernel/src/fs/ext2.rs`. Las funciones libres del kernel
(`vfs::resolve`, `vfs::open`, …) conservan firma idéntica y solo delegan.

**3. ⚠️ El lock de la tabla de mounts NO se puede sostener a lo largo del
walk.** `initramfs::RootDirInode::lookup` llama a `vfs::direct_children("/")`,
que vuelve a lockear la tabla — y `lookup` se invoca *dentro* de
`resolve_inner`. Hoy el código suelta el lock explícitamente (`drop(table)`)
antes de empezar a caminar por los componentes. `MountTable` debe preservar
eso: el `Mutex` se lockea y suelta **dentro** de `find()`, que devuelve
`(prefix, Arc<dyn Filesystem>)` clonado; ningún otro método sostiene el guard
mientras llama a `root()`/`lookup()`. Un `spin::Mutex` no es reentrante:
romper esto es un self-deadlock inmediato en `ls /`.

**4. El diagnóstico de lock de ramfs entra por seam.** `RamDirNode::lock_entries`
llama a `crate::process::scheduler::current_pid_safe()` y a
`crate::debug::RAMFS_ENTRIES_LOCK`. Se convierte en un trait
`DirLockObserver` (misma forma que `hal::PortIo` / `mm::PhysMap`), con un
`NoopDirLockObserver` por defecto para los tests de host y una impl del lado
del kernel que reproduce el comportamiento actual byte a byte. El orden
importa y está documentado en el código actual: el pid se lee **antes** de
lockear (`current_pid_safe` toma `SCHEDULER` y termina con `sti`), el acquire
se registra **después** de tener el lock (registrar antes nombraría como
holder a quien todavía está girando).

### El coste conocido: orphan rule en `ext2`

Al mudarse `Errno` al crate `vfs`, el `impl From<ext2::Ext2Error> for Errno`
de `kernel/src/fs/ext2.rs` pasa a tener los dos tipos foráneos ⇒ `E0117`. Se
resuelve con un newtype local en el adaptador (`struct ExtErr(ext2::Ext2Error)`)
y `.map_err(ExtErr)?` en los call sites. Se descartó darle al crate `ext2` una
dependencia (aunque fuera opcional) a `vfs`: contradiría la decisión #1 del
plan de extracción de ext2 ("el core NO conoce `Errno`") e invertiría la
dirección de las capas.

## Migración: 6 pasos, verde en cada uno

Nunca un big-bang. Tras **cada** paso: `cd vfs && cargo test`, `cargo build`
en la raíz, y `scripts/run-kernel-tests.sh` en verde (línea base: 3 casos
PASS). Línea base del resto de crates al empezar: `hal` 71, `ext2` 90,
`mm` 33+4.

| # | Mueve | Nota |
|---|-------|------|
| 1 | Andamiaje del crate + `fs::types` → `vfs::types` | Incluye el arreglo del orphan rule en `fs/ext2.rs` (rompe aquí, no después) |
| 2 | `FileError`/`FileResult`/`compute_seek`/`FileHandle` → `vfs::file` | `FileDescriptorTable` se queda en el kernel |
| 3 | Traits `Inode`/`Filesystem` → `vfs::inode`; helpers getdents64 → `vfs::dirent` | La tabla de mounts sigue en el kernel de momento |
| 4 | `MountTable` + `resolve`/`open`/mutaciones + `normalize_path`/`split_parent` | **Ver decisión #3**: el lock se suelta dentro de `find()` |
| 5 | `ramfs` entero + seam `DirLockObserver` | Aquí llegan los tests end-to-end: RamFs montado en una MountTable real |
| 6 | Docs (`lib.rs`, CLAUDE.md, este plan) + verificación de arranque interactivo | `mkdir`/`ls`/`ln -s`/`rm` sobre `/tmp` en QEMU de verdad |

## Fuera de alcance

- **El scheduler contra un reloj falso.** Es el paso 2 de la línea, no este.
- `devfs`/`initramfs`/`procfs`: dependen del registro de drivers, de los ELF
  embebidos y del scheduler respectivamente. No se mudan.
- Cualquier cambio de comportamiento. Esto es un refactor: mismas semánticas,
  mismos valores de `Errno`, mismo orden de escrituras.

## Riesgos

- **El self-deadlock de la tabla de mounts** (decisión #3). Es el único riesgo
  que no se manifiesta como error de compilación. El guard es `ls /` en un
  arranque real, no un test.
- **Mover invariantes sin darse cuenta.** Los comentarios de `lock_entries`
  (orden pid/acquire) y de `resolve_inner` (`..` → `EINVAL` a propósito, no
  un no-op) describen bugs ya pagados. Se mueven con el código.
- Tentación de "arreglar de paso". No. Refactor primero.
- **El test del `DirLockObserver` cubre menos de lo que su nombre sugiere.**
  `ramfs::tests::lock_entries_calls_observer_in_the_exact_required_order`
  fija el orden entre las tres llamadas al observador (`current_pid` →
  `record_acquire` → `record_release`) y que `op`/`pid` lleguen correctos,
  pero NO fija que `record_acquire()` ocurra realmente *después* de tener el
  lock — verificado moviendo esa llamada a antes de `self.entries.lock()`
  en `lock_entries`: los 150 tests del crate siguen pasando, porque sin un
  segundo hilo compitiendo por el lock, el orden real de adquisición no es
  observable desde el observador. Esa mitad del invariante hoy la sostiene
  solo la lectura del código y el doc comment de `lock_entries`, no un test.
  Cerrarlo de verdad requiere un test con un segundo hilo contendiendo de
  verdad por `entries` — trabajo futuro, fuera de alcance de este paso 6
  (que es solo documentación + verificación, no refactor). Ver el comentario
  corregido de ese test en `vfs/src/ramfs.rs` para el detalle completo.
