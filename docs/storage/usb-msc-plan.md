# Plan: leer el pendrive en bare metal (USB Mass Storage sobre el xHCI)

> **Estado (2026-09-23): pasos 1-5 hechos, `/mnt` se monta desde el
> pendrive en solo lectura.** Falta el paso 6 (escritura) y la primera
> prueba en la máquina física. Escrito 2026-09-21; ver "Lo que salió" al
> final para lo que el plan no preveía.

## El problema

El kernel arranca por UEFI desde un pendrive SanDisk de 28,7 GB, pero **no
puede leer ni un sector de él**. Su único dispositivo de bloques es
`kernel/src/block/ata.rs`: ATA PIO sobre los puertos ISA legacy
`0x170`/`0x376`, que solo existen porque la máquina `pc` (i440fx) de QEMU
expone el controlador IDE del PIIX3. La AM4/Ryzen de destino tiene NVMe y
AHCI, ningún IDE legacy, y de todas formas el sistema no vive en el disco
interno sino en el propio pendrive, que es USB Mass Storage.

Tampoco hay una vía lateral: `bootloader` 0.11 sale de los Boot Services de
UEFI antes de entregar el control, así que los servicios de bloque del
firmware no siguen disponibles, y no hay driver FAT para leer la partición
EFI que ya existe.

Por tanto, en bare metal `/mnt` hoy no se monta y `$PATH` pierde
`/mnt/bin` entero — `doom`, `quake` y los tests C no existen en esa
máquina.

La vía es un driver USB Mass Storage (Bulk-Only Transport + SCSI) sobre el
xHCI que ya existe para el teclado.

## Lo que ya está hecho (lado host, 2026-09-21)

El pendrive ya tiene su partición de datos, poblada y verificada:

```
/dev/sdb1   sector 34..34849      17M  EFI System       "boot"   (la imagen UEFI)
/dev/sdb2   sector 36864..4231167  2G  Linux filesystem "constanos-data"
```

Creada con exactamente los mismos parámetros que `build.rs` usa para
`disk.img`, para que el lector ext2 mínimo del kernel la acepte sin
cambios:

```bash
mke2fs -q -t ext2 -b 1024 -O ^resize_inode,^dir_index -L constanos \
       -d disk-image-root /dev/disk/by-partlabel/constanos-data
```

`-O ^resize_inode,^dir_index` deja `s_feature_incompat = 0x0002`
(solo FILETYPE), que es el único bit incompat que
`ext2/src/superblock.rs:101` acepta. Verificado leyendo el superbloque
directamente, no deducido.

La GPT del pendrive estaba rota de antes: el `dd` de la imagen UEFI de 17M
dejó una GPT de respaldo que creía que el disco acababa en el sector 34910.
Se reubicó al final real con `sfdisk --relocate gpt-bak-std` antes de poder
añadir nada. Copia de la tabla original y del primer MiB guardadas al
hacerlo.

`scripts/sync-usb-data.sh` re-sincroniza la partición desde
`disk-image-root/` después de cualquier build que reconstruya binarios.
Busca la partición **por etiqueta GPT, nunca por nodo de dispositivo**: el
`/dev/sdb` de hoy es el `/dev/sdc` de mañana, y escribir 54 MB sobre el
dispositivo equivocado no tiene vuelta atrás.

**Verificado de extremo a extremo, no razonado**: arrancando el kernel en
QEMU contra un overlay qcow2 con la partición real de backing
(`qemu-img create -f qcow2 -b /dev/sdb2 -F raw`, la misma técnica de
`boot-matrix.sh`), el kernel imprime `ext2: mounted /mnt`, `ls /mnt` lista
el contenido completo y `hello` — resuelto por `$PATH` en `/mnt/bin` — se
ejecuta e imprime su salida. Las escrituras del montaje rw (516 KB: las
pasadas de reparación) fueron al overlay; el `e2fsck -fn` posterior sobre
el pendrive da los mismos 88476 bloques y cero errores.

Eso valida el filesystem y su contenido. Lo que **no** valida es nada de lo
que sigue: en esa prueba la partición llegó por IDE, no por USB, y sin
pasar por la GPT.

## Lo que ya sirve del xHCI actual

Más de lo que parece. El driver del teclado dejó hecha la parte cara:

| Pieza | Dónde | Estado |
|---|---|---|
| `Trb::normal()` — el TRB de las transferencias bulk | `hal/src/xhci.rs:334` | Ya existe, con test |
| `Trb::configure_endpoint`, `build_input_control`, `build_endpoint_context` | `hal/src/xhci.rs:378/552/579` | Genéricos sobre `ep_type`, no atados a interrupt |
| `endpoint_dci(ep_number, is_in)` | `hal/src/xhci.rs:222` | Ya maneja cualquier endpoint, testeado hasta el 15 |
| `Ring`, `Dma`, `doorbell`, `wait_for_command` | `kernel/src/usb/xhci.rs:270/201/391/752` | Reutilizables tal cual |
| Lectura del event ring con el cycle bit primero | `kernel/src/usb/xhci.rs:727` | **La lección cara ya aprendida** — ver CLAUDE.md |
| Recuperación de stall (Reset Endpoint + Set TR Dequeue) | dentro de `control_transfer` | Existe; hay que factorizarla, MSC la necesita igual |
| Enumeración, Address Device, lectura de descriptores | `configure_device` | Ya recorre la configuración; falta una rama por clase |

El pendrive **ya se está enumerando y direccionando en cada arranque** —
CLAUDE.md lo dice explícitamente: un dispositivo que no es un teclado HID
boot "se direcciona (para que salga en el log de arranque) y se deja en
paz". Aparece en el log; solo falta actuar sobre él.

Lo que el xHCI no tiene es **endpoints bulk**. Y eso son dos constantes
(`EP_TYPE_BULK_OUT = 2`, `EP_TYPE_BULK_IN = 6`), no una reescritura.

## El punto de diseño que hay que resolver ANTES de escribir código

**El event ring es uno solo, y hoy tiene un único consumidor.**

`usb::poll()` corre desde el ISR del PIT a 100 Hz, hace `try_lock` sobre
`CONTROLLERS`, drena el event ring y da por suyo todo lo que encuentra;
`handle_async_event` loguea (acotado) lo que no reconoce. En cuanto haya
una lectura de disco en vuelo, ese `poll()` se tragará su Transfer Event y
la lectura expirará por timeout un segundo después — exactamente el modo de
fallo que costó tres ciclos de bare metal la vez anterior, y de nuevo
disfrazado de `Timeout`.

La forma correcta, y la generalización natural de `handle_async_event`: **un
único `service_events()` que drena el ring y despacha cada evento por
(slot, dci) a su destino** — el teclado a su decoder, el almacenamiento a
una celda de completion — en lugar de que cada cliente lea el ring por su
cuenta. Ningún cliente vuelve a tocar `next_event` directamente.

El segundo filo del mismo problema: `CONTROLLERS` es un `Mutex` y el ISR
del timer lo toma con `try_lock`. Una lectura de disco que retenga ese
mutex milisegundos no causa deadlock (el ISR se salta su turno, la
estrategia de `tick_cursor_blink`), pero **sí pierde pulsaciones de
teclado mientras dura la I/O**. Aceptable al arrancar; hay que medirlo con
tráfico real antes de darlo por bueno.

Y la regla que no se puede romper: la espera de completions es un
busy-wait (como `ac97::write_pcm`), así que **jamás con IF=0 ni bajo el
lock del `SCHEDULER`**. Hay que comprobar desde dónde entra `fs::ext2` a
`BlockDevice` por sus rutas de syscall, no solo en el arranque.

## Pasos

Cada paso es verificable por sí solo. Los dos primeros no tocan el kernel.

**1. `hal/src/msc.rs` — el protocolo, puro y host-testeable.**
CBW (31 bytes, firma `0x43425355`) y CSW (13 bytes, `0x53425355`):
construcción, parseo y validación (tag que coincide, `dCSWDataResidue`,
status 0/1/2). CDBs SCSI: `TEST UNIT READY` (0x00), `REQUEST SENSE` (0x03),
`INQUIRY` (0x12), `READ CAPACITY(10)` (0x25), `READ(10)` (0x28),
`WRITE(10)` (0x2A). Aquí va el grueso de los tests nuevos.

**2. `hal/src/gpt.rs` — la tabla de particiones, pura y host-testeable.**
Cabecera en LBA 1 (firma `EFI PART`, CRC32 de la cabecera y del array),
entradas de 128 bytes, nombre en UTF-16LE. Búsqueda por nombre de partición
(`constanos-data`) con respaldo por GUID de tipo Linux. Necesita un CRC32
(pequeño, bitwise, sin tabla). El fixture de regresión es el volcado real
de los 34 primeros sectores de este pendrive (regenerable con
`sudo dd if=/dev/sdb bs=512 count=34 of=<fixture>`, 17408 bytes), además de
tablas construidas a mano en el propio test, al estilo de
`ext2::testimg::build_minimal_image`.
**Validar los CRC de verdad**: una GPT primaria corrupta debe hacer caer a
la de respaldo, no montar basura.

**3. Refactor del despacho de eventos, sin funcionalidad nueva.**
`service_events()` como se describe arriba. Se verifica con lo que ya hay:
el teclado debe seguir funcionando idéntico — tecleo, backspace, historial
con flechas, `usb_key_reports` subiendo en `/proc/kdebug`, y
`boot-matrix.sh 4 3` limpio. Un paso que no añade nada y solo puede
romper cosas es exactamente el que hay que aislar en su propio commit.

**4. Endpoints bulk y `MassStorage` — sin montar nada todavía.**
Las dos constantes de tipo de endpoint en `hal::xhci`; reconocer en
`hal::usb` la interfaz clase 0x08 / subclase 0x06 / protocolo 0x50 y
devolver sus dos endpoints bulk; `kernel/src/usb/msc.rs` con el Configure
Endpoint, los anillos IN/OUT, las páginas DMA y `bulk_transfer()`
—**filtrando el Transfer Event por slot + dci, nunca por puntero de TRB
solo**, que es el error ya cometido en `control_transfer`. Se verifica
haciendo solo `INQUIRY` + `READ CAPACITY(10)` y escupiendo el modelo y la
capacidad en el log de arranque. En QEMU:

```
-drive if=none,id=stick,format=raw,file=<imagen> \
-device usb-storage,bus=xhci.0,drive=stick
```

Que el log diga "SanDisk, 60088320 sectores" es la prueba de que el
camino bulk entero funciona, y se consigue sin tocar el VFS.

**5. `BlockDevice` y la partición — `/mnt` en solo lectura.**
`kernel/src/block/usb.rs` (`UsbBlockDevice`) y
`kernel/src/block/partition.rs` (`Partition { inner, first_lba, sectors }`,
que suma el offset y valida los límites — un LBA fuera de la partición debe
ser un error, no una lectura del disco vecino). `fs::ext2::init()` intenta
USB → GPT → partición y, si algo falla, cae a `AtaBlockDevice` como ahora,
para que QEMU siga funcionando igual.

**Montar primero read-only**, aunque el driver ya sepa escribir: sin
journal, un cuelgue a mitad de las pasadas de reparación deja el pendrive
inconsistente, y el pendrive es también la llave de arranque. Escribir es
el paso 6, cuando leer ya sea aburrido.

**6. Escritura.** `WRITE(10)` es simétrico a `READ(10)`; el trabajo real es
la confianza, no el código.

## Riesgos anotados, no resueltos

- **Rendimiento del montaje.** `reclaim_orphans` recorre los bitmaps de los
  256 grupos de bloques de una partición de 2 GiB con bloques de 1 KiB: del
  orden de 512 lecturas más el recorrido del árbol de directorios. Si la
  espera de completion se hace por ticks de PIT de 10 ms en vez de por
  sondeo activo del event ring, eso son más de 5 segundos de arranque. Hay
  que **medirlo**, y si duele, el `count: u8` del trait ya permite 255
  sectores por transferencia (127 KiB), que `READ(10)` soporta de sobra.
- **Hubs.** El driver solo ve puertos del root hub. En muchas placas AM4 los
  puertos frontales cuelgan de un hub interno: si el pendrive va ahí, no
  aparece. Usar un puerto trasero directo, y si aun así no aparece, esa es
  la primera hipótesis, no la última.
- **LBA de 32 bits.** `BlockDevice::read_sectors(lba: u32, count: u8)` es
  LBA28. El último sector de esta partición es el 4231167 y el pendrive
  entero son 60088320, así que ambos caben; se rompe por encima de 2 TiB.
  No hay que tocar el trait ahora, pero conviene saberlo.
- **Sin hot-plug.** El pendrive es el disco de arranque, así que siempre
  está presente al enumerar. Pero si se desconecta en caliente, el driver
  no se entera y las lecturas empiezan a expirar.
- **`usb-storage` de QEMU no es un pendrive real.** El modelo de QEMU
  escribe respuestas de forma atómica respecto al invitado y no produce los
  stalls, los `REQUEST SENSE` ni los reintentos que produce el hardware de
  verdad. Que el paso 4 pase en QEMU no dice nada sobre la AM4 — es
  exactamente la asimetría que escondió el bug del cycle bit. Las
  recuperaciones hay que escribirlas aunque QEMU no las ejercite nunca.

## Lo que salió (2026-09-23)

- **Pasos 1-2**: `hal::msc`, `hal::gpt`, más `hal::usb::find_mass_storage`
  y `hal::block::Partition`. Fixtures: una tabla de `sfdisk` y las regiones
  GPT reales del pendrive. Tests de CRC comprobados por sabotaje — el
  primero no mordía (la corrupción caía en `my_lba`, que otra comprobación
  ya valida) y se movió al GUID del disco.
- **Paso 3** encontró un bug que ya existía: una pulsación que llegaba
  mientras la enumeración esperaba la transferencia de control de *otro*
  dispositivo se descartaba junto con la única transferencia pendiente del
  teclado, que ya nunca se rearmaba. `service_events` lo cierra.
- **Otro bug previo**: `describe()` tenía los códigos de completion
  desplazados en uno a partir del 18 (19 salía como "BandwidthOverrun"
  siendo Context State Error, justo el que produce una recuperación de
  endpoint mal elegida).
- **Bloqueo**: se resolvió con IF=0 + `CONTROLLERS` por cada transferencia
  de ≤64 KiB, en vez de medirlo después. Las teclas pulsadas durante una
  lectura las decodifica la propia lectura y las entrega el siguiente
  `poll` (`usb_keys_dropped` en `/proc/kdebug`).
- **Solo lectura**: `write_lock()` en `fs::ext2` (todas las mutaciones ya
  pasaban por `EXT2_LOCK`, así que un único punto las cubre) y `open()` en
  escritura → `EROFS`.
- **Rendimiento del montaje**: el riesgo anotado no aplica todavía — sin
  pasadas de reparación, montar son unas pocas lecturas.
- **Primera escritura USB real, en una partición sin metadatos**: antes que
  el paso 6 (ext2 en escritura), el pendrive recibe el log del kernel en
  una tercera partición en crudo, `constanos-log` (`block::logpart`,
  `hal::logpart`, `scripts/usb-log.sh`). Ejercita `storage_write` sin
  arriesgar la partición de datos: probado en QEMU con una imagen de la
  misma forma (`scripts/usb-log.sh mkimage`), con huella de todo lo que
  queda fuera de la partición de log idéntica antes y después (GPT
  primaria, `boot`, `constanos-data`, GPT de respaldo). En metal, pendiente
  de crear la partición (`scripts/usb-log.sh mkpart`) y marcarla (`init`).

