# Plan: extraer `fs::ext2` a un crate host-testeable

> **Estado: COMPLETADO** (2026-07-24). Los 6 pasos de la migración están
> hechos: 1-2 en `47f65aa`, 3 en `1e0f22a`, 4 en `0e48928`, 5 en `3c72370`,
> y 6 (consolidar los image builders de test en `ext2::testimg`, borrar
> `TestFs` del kernel, y podar los wrappers de `Ext2Fs` que quedaron sin
> uso) sin commitear todavía al momento de escribir esta nota.
> `kernel/src/fs/ext2.rs` es ya el adaptador VFS puro descrito más abajo en
> "Forma final" — el core (layout on-disk, bitmaps, indirectos, dirents,
> symlinks, mount/repair) vive entero en el crate `ext2/`, con 89 tests de
> host (`cd ext2 && cargo test`). Ver el doc comment de `ext2/src/lib.rs`
> para el detalle línea por línea de qué vive dónde.

Estado original del plan (histórico, ver nota arriba): **plan, sin
empezar.** Escrito 2026-07-23, tras cerrar el seam `hal::block::BlockDevice`
(commit `dd2463d`) y descartar el falso bug de `reclaim_orphans`
(`0f16839`, `fdc9f11`).

## Por qué

`kernel/src/fs/ext2.rs` son ~2.200 líneas de lógica de bitmaps, inodos y
directorios que hoy **solo** se pueden ejercitar arrancando QEMU. El crate
`kernel` no compila para host (ver CLAUDE.md: `-Z build-std` + doble build del
bin target ⇒ colisión de lang items en `core`), que es la razón de existir de
`hal/`.

El precedente que justifica el gasto: el bug de orden en `reclaim_orphans`
liberaba casi todos los bloques vivos en cada montaje fresco y solo se
manifestaba como un pánico de rango en `add_dir_entry`. Un filesystem sin
journal falla así — lejos de la causa y de forma destructiva.

El objetivo real no es "más tests", es **usar `e2fsck` como oráculo**: mutar la
imagen en un `Vec<u8>`, volcarla a fichero, y dejar que la herramienta de
referencia dictamine si quedó consistente. Eso convierte "creo que quedó bien"
en una aserción.

## Forma final

Split clásico: **core puro sin VFS + adaptador delgado en el kernel.**

```
ext2/                        (crate nuevo, no_std + alloc, host-testeable)
  ├─ depende de `hal` (BlockDevice, SECTOR_SIZE) — ya en su sitio
  ├─ layout on-disk, bitmaps, indirectos, dirents, mount/repair
  ├─ NO conoce Inode/Filesystem/FileHandle/Stat/Errno
  └─ habla en números de inodo y rangos de bytes

kernel/src/fs/ext2.rs        (queda solo el adaptador)
  ├─ impl Filesystem / Inode / FileHandle sobre el core
  ├─ EXT2: Once + EXT2_LOCK
  └─ mapeo Ext2Error -> Errno
```

### Tres decisiones de diseño

**1. Error propio, no `Errno`.** El core define `Ext2Error`; el adaptador
implementa `From<Ext2Error> for Errno`. Mover `fs::types::Errno` a un crate
compartido tocaría medio kernel — no lo vale.

**2. El reloj es un seam.** `now_unix_secs()` entra por trait (misma forma que
`PortIo`/`PhysMem`). Bonus: hace **determinista el `i_dtime`**, que ya dio un
bug real (un valor relativo al arranque colisiona con el uso que ext3 hace de
ese campo para su lista de huérfanos, y `e2fsck` lo diagnostica como cadena
corrupta). Hoy eso no es testeable; con un reloj fijo, sí.

**3. El lock se queda en el kernel, y esto es el premio gordo.** El core expone
`&self` para lecturas y `&mut self` para mutaciones; el adaptador lo envuelve en
el `Mutex`. La disciplina actual —"los caminos read-only no toman `EXT2_LOCK`, y
es seguro porque todo método mutador ya lo tiene cogido cuando los llama por
dentro"— es hoy una convención mantenida a mano sobre un `spin::Mutex` no
reentrante. Tras la extracción **la impone el borrow checker**.

> ⚠️ El adaptador no debe llamar al scheduler con `EXT2_LOCK` cogido. Es el
> mismo patrón que ya causó el deadlock de `sys_close`/`sys_dup2` (un `Drop`
> que necesita un `SCHEDULER` fresco).

## Migración: 6 pasos, verde en cada uno

Nunca un big-bang. Tras **cada** paso: `scripts/run-kernel-tests.sh` en verde
(hoy 3 casos) y `cd hal && cargo test` (hoy 71).

| # | Mueve | Nota |
|---|-------|------|
| 1 | Structs on-disk + parsing (superblock, BGD, inodo raw, dirent) | Sin cambio de comportamiento; crea el crate y el andamiaje |
| 2 | Asignación/liberación de bloques e inodos (scan de bitmaps, contadores) | |
| 3 | Direccionamiento indirecto (1/2/3 niveles) + read/write por rango de bytes | |
| 4 | Operaciones de directorio (add/remove/lookup/readdir/rename) + symlinks fast/slow | |
| 5 | `mount` + `reconcile_free_counts` + `reclaim_orphans` | **No tocar el orden del walk** (ver CLAUDE.md) |
| 6 | Lo que queda en `kernel/src/fs/ext2.rs` es solo el adaptador VFS | |

`build_image_with_orphans()` / `TestFs` (hoy `#[cfg(test)]` en el kernel) se
mudan al crate nuevo como helpers de test de primera clase. El problema de
aislamiento del `Once` global desaparece: en host cada test tiene su instancia.

## El oráculo `e2fsck`

En los tests del crate nuevo:

1. Fixtures generadas con `mke2fs` de verdad, no solo la imagen mínima a mano
   (`e2fsprogs` ya es dependencia de build).
2. Mutar vía el core sobre `MemDisk`.
3. Volcar a fichero temporal → `e2fsck -fn` → **exit code 0**.

Saltar con gracia si no hay `e2fsck` en el host, no fallar.

Esto cubre lo que ninguna aserción a mano cubre bien: contadores del BGD contra
bitmaps, link counts, `..` de directorios, `i_dtime`, huérfanos.

## Fuera de alcance

- **`block/ata.rs` → `PortIo`.** Ortogonal: ATA está *debajo* del filesystem.
  Sigue siendo el único driver sin seam (los "6 migrados" de la Fase 2 son
  acpi/ac97/keyboard/mouse/pit/rtc). Vale la pena, pero por separado.
- Cualquier cambio de comportamiento. Esto es un refactor: mismo on-disk
  format, mismas semánticas, misma ordenación de escrituras.

## Riesgos

- **El más grande: mover invariantes sin darse cuenta.** El orden
  "asignar y escribir contenido, luego enlazar" es lo que hace que un crash
  solo pueda filtrar, nunca dejar punteros colgando. Preservar los comentarios
  junto al código que describen, no solo el código.
- `disk.img` debe seguir montando en el arranque real. El caso QEMU es el guard;
  comprobar con `ls /mnt/bin` en un arranque de verdad al cerrar el paso 6.
- Tentación de "arreglar de paso". No. Refactor primero, bugs después, con el
  oráculo ya disponible para probarlos.
