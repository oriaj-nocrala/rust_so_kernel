# Plan: GPU NVIDIA (display, GSP, canales)

> **Estado (2026-09-26):** **fase 0 cerrada.** Trazas capturadas, válidas y
> segmentadas (ver "Resultados de la fase 0"); D1 = **570.144** y D6 =
> **modeset propio sin GSP**, ambas medidas. **Fase 1 cerrada**
> (a6c221a, Ryzen boot #66; ver "Resultados de la fase 1"). **Fase 2
> cerrada** (Ryzen boot #67; ver "Resultados de la fase 2"). **Fase 3
> cerrada** (Ryzen boot #68: vblank por MSI a 59,995 Hz, sin tearing; ver
> "Resultados de la fase 3"). Siguiente: fase 5 (orden de D6). Ninguna fase se da por hecha sin su
> criterio medido en la Ryzen.

## Objetivo

Pasar de "framebuffer GOP a 60 Hz, sin vsync, sin saber qué monitor hay" a un
driver propio de la RTX 3050 que:

1. **nombra los monitores** (EDID leído por la GPU, en caliente);
2. da **vblank** (vsync real para el compositor);
3. **arranca el GSP** y habla con GSP-RM;
4. **cambia de modo** (1920×1080 a 180 Hz en el DP; sin GSP, ver D6);
5. ejecuta trabajo en la GPU: **canal + motor de copia**.

La pila 3D (Vulkan/OpenGL) **no** forma parte de este plan. La fase 7 es solo
la decisión de cómo abordarla, con sus requisitos.

### No-objetivos

- Otras GPUs o generaciones. Solo GA106 (`10de:2507`) y solo esta placa
  (Zotac, subsistema `19da:c630`). Si algo depende de la placa (la DCB del
  VBIOS), se lee de la tabla, pero no se prueba nada más.
- Correr en QEMU. QEMU no emula NVIDIA. Lo que no sea lógica pura se verifica
  en metal (skill `metal-run`).
- Gestión de energía, relojes de la GPU, ventiladores, reclocking.
- ~~Soporte sin GSP más allá del display de lectura~~: anulado por D6. El
  display (y probablemente el CE) se hace sin GSP; el GSP queda para la
  fase 7.

## Hechos medidos (2026-09-26, desde el Linux de la propia Ryzen)

La máquina de desarrollo **es** la máquina objetivo, con Linux en arranque
dual. Esto es la base del método (ver "Oráculo").

| Qué | Valor |
|---|---|
| GPU | GA106 [GeForce RTX 3050], `09:00.0`, rev a1, `boot_vga=1` |
| BARs | BAR0 `0xf5000000` 16 MiB (MMIO) · BAR1 `0x7c00000000` **8 GiB** (VRAM, ReBAR activo) · BAR3 `0x7e00000000` 32 MiB · I/O `0xe000` |
| VBIOS | `94.06.37.00.40` |
| Interrupciones | MSI/MSI-X (Linux la enruta por MSI) |
| Monitor DP-1 | ASUS `VG279Q3A`, 1920×1080, rango 48–180 Hz, dotclock máx. 430 MHz |
| Monitor HDMI-A-1 | HP `2309`, 1920×1080 a 60 Hz |
| Conectores | DP-1, DP-2, DP-3, HDMI-A-1 |
| Firmware GSP en el host | `/usr/lib/firmware/nvidia/ga106/gsp/`: **535.113.01** y **570.144** (`gsp-*.bin.zst`, `booter_load/unload`, `bootloader`) |
| Driver en el host | `nvidia` 610.57.04 (nouveau en lista negra por la línea de comandos) |
| Kernel del host | 7.2.2-zen, `CONFIG_MMIOTRACE=y`, AMD IOMMU activado en Linux |

1920×1080 a 180 Hz con blanking reducido cabe en DP HBR2 ×4 sin DSC. HDMI
no hace falta para los 180 Hz.

## Principios de ejecución

Se puede hacer en un mes con un LLM **solo** si ninguna sesión adivina. Estas
reglas existen para eso.

1. **Oráculo antes que código.** Cada fase empieza capturando en el Linux de
   la Ryzen lo que el driver debe reproducir: volcados de sysfs, el VBIOS y
   trazas `mmiotrace` de nouveau haciendo exactamente esa fase. Ninguna fase
   escribe un registro que no aparezca en una traza o en un fichero fuente
   citado.
2. **Cada constante cita su fuente.** Offset de registro, bit, clase RM,
   número de RPC, layout de estructura: comentario con `fichero:línea` de la
   referencia fijada (sección "Referencias"). Sin cita, la constante no entra.
   Es la regla que evita que un LLM invente `0x610b20`.
3. **Lógica pura en un crate con tests de host.** Crate nuevo `nvgpu`, con el
   patrón de siempre (nada bloquea, los efectos vuelven como datos). Registros
   detrás de un seam `Mmio` (como `PortIo`/`PhysMem` en `hal`). El adaptador
   del kernel (`kernel/src/gpu/`) es fino: mapea BARs, pide DMA y enruta la MSI.
4. **Tests contra la realidad, no contra la memoria del LLM.** Los fixtures
   son bytes reales de esta máquina (VBIOS, EDIDs, instantáneas de registros,
   RPCs capturados). Un mock `ReplayMmio` sirve lecturas desde una instantánea
   y registra escrituras para compararlas con la secuencia de la traza en los
   pasos clave, no con igualdad total.
5. **Cada test nuevo se prueba por sabotaje** (skill `kernel-testing`).
6. **Metal con red de seguridad.** La GPU es la única salida de vídeo de la
   Ryzen y no hay serie. Por eso:
   - Todo el driver va detrás de un parámetro `gpu=` (`off` por defecto,
     luego `probe`, `disp`, `gsp`, `modeset`, `ce`). El arranque normal no
     escribe en la GPU hasta que la fase correspondiente esté cerrada.
   - Cada fase se prueba con un trabajo autorun de `metal-run.sh` y su
     veredicto se lee de la partición de log. Nunca se depende de la pantalla.
   - Antes de cada fase que escriba en la GPU, se mide que un reinicio en
     caliente devuelve la imagen del firmware. Si no, hace falta apagar y
     encender; se anota y se añade un paso de reset.
7. **Versión de firmware fijada, una sola.** La ABI de RM cambia con cada
   versión. Se fija en la fase 0 (ver decisión D1) y todas las estructuras se
   generan para esa versión. Nada de "compatible con varias".
8. **Una fase por sesión de LLM.** Cada sesión empieza leyendo este plan, la
   sección de su fase y el skill `kernel-drivers`. Termina actualizando el
   estado de este documento con lo **medido**, incluidos los negativos. Los
   resultados de subagentes se vuelven a ejecutar antes de creerlos.
9. **Cada fase entra en master funcionando.** Con `gpu=off`, el kernel se
   comporta igual que antes: `boot-matrix` y `run-kernel-tests.sh` en verde.

## Referencias fijadas

Se clonan fuera del repo (`~/src/gpu-ref/`, script `scripts/gpu-ref.sh` en la
fase 0) en una etiqueta concreta, anotada aquí al fijarla.

| Referencia | Para qué | Licencia |
|---|---|---|
| Linux `drivers/gpu/drm/nouveau/` (etiqueta = kernel del host) | Todo: VBIOS/DCB, I2C/AUX, display, falcon, arranque GSP (`nvkm/subdev/gsp/`), RPC, clases de display | MIT: se puede portar |
| `NVIDIA/open-gpu-kernel-modules` (etiqueta = versión GSP fijada) | **Fuente de verdad de la ABI de RM**: estructuras de control, clases, RPC (`src/common/sdk/nvidia/inc/`) | MIT/GPL: usar como MIT |
| Linux `drivers/gpu/nova-core/` | **Referencia estructural principal de la fase 4.** En v7.2.2 arranca el GSP en GA106 exactamente hasta el hito de la fase 4: `wait_gsp_init_done` y `GET_GSP_STATIC_INFO` → nombre de la GPU (`gsp/boot.rs:154-159`), más el descargado. Unas 13 000 líneas de Rust. **No tiene display.** No está compilado en el kernel del host (`CONFIG_NOVA_CORE` sin activar), así que no hay traza suya | GPL-2.0: **leer, no copiar** |
| `envytools` / `rnndb` | Nombres de registros para leer trazas mmiotrace (`demmio`) | MIT |
| Mesa `src/nouveau/` (NVK) | Solo para la fase 7 | MIT |
| QEMU v11.1.1 `hw/misc/edu.c`, `docs/specs/edu.rst` | Dispositivo `edu` del test de la fase 1 | GPL-2.0: leer, el driver de test sigue la especificación |

## Decisiones

- **D1: versión del firmware GSP (se decide en la fase 0, midiendo).** Se
  fija la versión que carga el nouveau del host, porque es la única que se
  puede trazar. Candidatas: 535.113.01, con r535 en nouveau y la más madura,
  y 570.144, la que usa nova-core. Nada de mezclar ABIs.
  **Fijada: 570.144, medido** (dmesg de `trace-gsp`: `gsp: RM version:
  570.144`, carga `gsp-570.144.bin`, 63 571 696 bytes). Previsión original: el nouveau de v7.2.2 prefiere 570.144
  para GA106 (`nvkm/subdev/gsp/ga102.c:180`, prioridad 1 frente a 535 en
  `:181`), y el GSP está activado por defecto (`NvGspRm`, `tu102.c:460`). Las
  referencias ya están clonadas con esa versión (`~/src/gpu-ref/PINNED`).
- **D2: estructuras de RM generadas, no escritas a mano.** `bindgen` sobre
  los headers de open-gpu-kernel-modules en la etiqueta fijada, filtrado a
  una lista blanca. Se genera un `.rs` que se commitea con
  `assert!(size_of/offset_of)`. El generador vive en `nvgpu/gen/`.
- **D3: el firmware no entra en git.** El `build.rs` raíz (`ensure_firmware`) lo copia de
  `/usr/lib/firmware/nvidia/ga106/` a `disk-image-root/lib/firmware/...`, lo
  descomprime (zstd) en el host y deja el fichero `LICENCE.nvidia` al lado.
  El kernel lo lee de `/mnt/lib/firmware` después de `fs::init`. **El VBIOS
  tampoco entra en git** (es de NVIDIA/Zotac): los tests que lo usan lo leen
  de `$GPU_ORACLE` (por defecto `~/constanos-gpu-oracle`) y se saltan con un
  aviso si no está. Los EDIDs y los extractos de traza sí se commitean.
- **D4: sin IOMMU.** El kernel no activa AMD-Vi, así que la dirección física
  es la de bus. Se comprueba en la fase 1 (IVRS presente pero sin activar). Si
  algún día se activa, la API de DMA es el único sitio que cambia.
- **D5: la consola sigue en el framebuffer GOP hasta la fase 5.** Si una fase
  anterior pierde la imagen (por ejemplo, RM toma el display al arrancar el
  GSP), se registra como hecho medido y la fase 5 pasa a ser la que la
  recupera. No se improvisa un arreglo.
- **D6: ruta del modeset. DECIDIDA: modeset propio, con la traza `nogsp`
  como oráculo.** Medido en la fase 0:
  - Sin GSP, nouveau hace un modeset completo y limpio de las dos pantallas,
    entrenamiento DP incluido (fbcon a los 11,7 s). También levanta el motor
    de copia (`drm: MM: using COPY for buffer copies`).
  - Con GSP, RM arranca, pero el display falla **también sin mmiotrace**
    (`trace-gspnotrace`, mismos síntomas: `DFP_ASSIGN_SOR` devuelve
    `0xffff`, `DP_TRAIN` da -EIO en todas las combinaciones, `core notifier
    timeout`). No lo causa el instrumento: es nouveau r570 en GA106.
  - La única variable que no se aisló es `debug=gsp=trace` (el volcado de
    RPC por printk), que estuvo en las dos ejecuciones con GSP. Un fallo
    devuelto por RM (`0xffff`) no parece de tiempos, así que no se mide más
    salvo que se quiera volver a la ruta RM.

  Consecuencia en el orden: **el GSP deja de estar en el camino crítico del
  display.** Orden de ejecución: 0 → 1 → 2 → 3 → **5** (modeset propio) →
  **6** (canal + CE, si la traza `nogsp` lo confirma sin GSP) → **4** (GSP,
  como puerta a la fase 7). Los números de fase se mantienen para no romper
  referencias.

## Resultados de la fase 0 (2026-09-26)

Capturas en `~/constanos-gpu-oracle/` (fuera de git), con
`scripts/gpu-oracle.sh` en modo desatendido. El primer intento salió vacío:
el `modprobe.d` del host fuerza `nouveau modeset=0`; el script ahora lo anula.

| Captura | Contenido |
|---|---|
| `static/` | VBIOS (148 992 B, igual en los tres arranques), EDIDs, `lspci -xxxx`, `nvidia-smi -q` |
| `trace-nogsp/` | 98 MB: 2 289 019 escrituras y 255 128 lecturas MMIO; dmesg de 1 713 líneas |
| `trace-gsp/` | 364 MB: 2 136 705 escrituras y 7 421 042 lecturas; dmesg de 22 214 líneas con 1 332 RPC volcados (`debug=gsp=trace`) |

Hechos medidos:
- **Los EDIDs leídos por nouveau (con y sin GSP) son idénticos byte a byte a
  los de sysfs bajo `nvidia`.** Son el fixture de la fase 2.
- **El nombre de un mismo conector cambia según la ruta.** El DP del ASUS es
  `DP-3` sin GSP y `DP-1` con GSP y con `nvidia`. Por eso los nombres de
  `/proc/displays` no se copian de Linux: se definen desde la DCB y se
  documentan.
- **Arranque del GSP, completo en la traza:** `fwsec-frts` parchea 3 firmas,
  arranca en unos 235 ms y deja **WPR2 en `0x01ffe000`–`0x01ffee00`**. Son
  los valores crudos de los registros `0x1fa824`/`0x1fa828`
  (`nvkm/subdev/gsp/fwsec.c:369-371`); todavía no se han interpretado las
  unidades. La primera init de RM
  tarda 247 ms, y el resto son RPC. Es el oráculo completo de la fase 4.
- **Display con GSP: falla en la traza.** `NV0073_CTRL_CMD_DFP_ASSIGN_SOR`
  (`0x731152`, `ctrl0073dfp.h:598`) devuelve `0xffff`. `DP_TRAIN` falla en
  todas las combinaciones de carriles y velocidades (-EIO), y aparecen
  `core notifier timeout` y `dotclock = 0`. No sirve como oráculo de la
  fase 5 hasta resolver D6.
- **Display sin GSP: limpio.** La traza `nogsp` contiene un modeset completo
  de las dos pantallas.
- Los relojes de dmesg y de mmiotrace son el mismo (tiempo desde el
  arranque), así que las trazas se pueden segmentar por los mensajes de
  nouveau.

D6 se midió con `trace-gspnotrace` (solo dmesg, 21 111 líneas): el display
falla igual que con mmiotrace.

### Mapa de las trazas

Herramienta: `scripts/gpu-trace.py` (`align`, `segment`, `extract`, `regs`).
Las horas son de dmesg. El reloj de mmiotrace va **+0,1218 s** (`nogsp`) y
**+0,1229 s** (`gsp`) por delante; `align` lo mide con dos anclas (inicio y
fin de la lectura de la PROM), que coinciden al 0,1 ms.

**`trace-nogsp`** (oráculo de las fases 2, 3, 5 y 6):

| Paso | dmesg (s) | Qué hay |
|---|---|---|
| Identificación + VBIOS | 7,9551–8,2382 | `PMC_BOOT_0 = 0xb76000a1`; 141 445 lecturas de la PROM (`0x300000`–`0x3fffff`); imagen de 4 partes, BIT 94.06.37.00.40 |
| Tabla CCB (I2C/AUX) | 8,2386 | ccb 03–09 → auxch 00–06 |
| fb / reset | 8,2527–11,3581 | ~2 M escrituras en `0x7d0000`/`0x7c0000` (sin interpretar; ¿limpieza de VRAM por PRAMIN?) |
| ACR / SEC2 | 11,3581–11,3696 | `gsp(acr)`: ASB, firmas parcheadas |
| Display: constructores | 11,4452 | 8 ventanas, 4 cabezas, 4 SOR |
| Display: DCB y rutas | 11,4489–11,4772 | salidas/conectores (`disp:dcb`); **el DP del ASUS es la salida 04 (SOR-1, conector 2, ccb 06)** y el HDMI del HP es la salida 07 (TMDS, conector 3, ccb 05), sin ruta al arrancar |
| **EDID** | 11,4773–11,6762 | **DP: AUX canal 3**, registros `0xda30`–`0xda58` (`0xd950 + ch·0x50`, `nvkm/subdev/i2c/auxgm200.c:110-120`). **HDMI: I2C por bit-banging, puerto 5**, `0xd0b4` (`0xd014 + drive·0x20`, `busgf119.c:93`), unos 15 000 accesos |
| fbcon | 11,6762 | |
| **Modeset** | 11,8215–11,8789 y 11,9849→ | dos rondas de supervisor 1/2/3; cada una trae en dmesg el volcado de los métodos del canal core (`disp: 0200: 9155b219 -> 00000001`…). Entre las dos rondas, `release SOR-1` |

**`trace-gsp`** (oráculo de la fase 4):

| Paso | dmesg (s) | Qué hay |
|---|---|---|
| VBIOS | 8,0401–8,3289 | igual que sin GSP |
| FWSEC-FRTS | 8,3403–8,5757 | parcheo de firmas; IMEM `0xe100` bytes (bloque `0x110000`, falcon GSP); arranque y sondeo, 218 ms |
| booter-load en SEC2 | 8,5760–8,8377 | IMEM `0x8900` bytes (bloque `0x840000`, falcon SEC2); arranque, 250 ms |
| Arranque de GSP-RM | 8,8377–10,1112 | 505 mensajes: RPC de init |
| Control de RM + objetos | 10,1112–13,6881 | unos 2 M escrituras fuera de BAR0 (BAR3); primeras llamadas `NV2080`, luego objetos de display (`NV0073`) |
| Fallo del display (D6) | 13,6881→ | timeouts de notificador, `DFP_ASSIGN_SOR`, `DP_TRAIN` |

**Fase 0 cerrada.** Los fixtures derivados (EDIDs, extractos de traza por
paso) se sacan con `gpu-trace.py extract` en la fase que los usa, empezando
por la 2.

## Resultados de la fase 1 (2026-09-26)

Estado actual en `docs/reference/gpu.md`. Medido:
- **QEMU:** `hw_tests::edu_mmio_dma_msi` pasa (MMIO, factorial, MSI a la
  CPU 1 por un vector dinámico, DMA de ida y vuelta de 4 KiB, rechazo por
  máscara). Suite completa 11/11; `boot-matrix 4 5` 20/20 OK.
- **Sabotajes que el test detecta:** despacho MSI sin llamar al handler, DMA
  que no escribe (kernel); tamaño de BAR de 64 bits, campo de destino MSI,
  falta del bit de enable final, recorrido de capacidades sin cota (host).
- **Firmware:** los tres ficheros pequeños de 570.144 se leen desde `/mnt`
  con FNV-1a idéntico al del host. Primera palabra `0x10de` =
  `nvfw_bin_hdr.bin_magic`.
- **`gsp-570.144.bin` no entra en `disk.img`** (96 MiB, ~20 MB libres; el
  fichero tiene 63 MB). Se decide en la fase 4: agrandar `disk.img` o
  llevarlo solo al pendrive (2 GB).
- **Configuración PCI de la GA106** (lspci del host, ahora fixture): MSI en
  0x68, 64 bits, sin enmascarado, 1 vector; **sin MSI-X**. La lista de
  capacidades estándar es PM, MSI, Express, vendor.
- **Opciones de arranque:** no había línea de comandos. Ahora son
  `/mnt/etc/kernel.conf` y `/mnt/autorun/kernel.conf` (`metal-run.sh
  --kconf`).
- **IOMMU del host:** la GPU está sola en el grupo 16 junto con su audio
  (`09:00.1`). Paso por VFIO a QEMU: posible (ver la propuesta en la
  conversación de la fase 1; sin decidir).

**En la Ryzen (boot #66, `metal-run.sh --kconf 'gpu=probe'
scripts/metal-jobs/gpu-probe.sh`, veredicto OK):**
- `PMC_BOOT_0 = 0xb76000a1` → chipset 0x176, GA100, impl 6, GA106 (igual
  que nouveau en la fase 0).
- Capacidades `01@60 05@68 10@78 09@b4`, MSI de 64 bits sin enmascarado, sin
  MSI-X: igual que el fixture de Linux.
- **Estado tras el GOP (el que verá el driver):** Command `0x0003` (memoria
  e I/O, **bus master apagado**) y **MSI deshabilitada** (dirección 0). Las
  fases que hagan DMA o usen MSI tienen que encender ambos.
- BARs medidos: BAR0 16 MiB, BAR1 8 GiB, BAR3 32 MiB, I/O 128 B, iguales a
  lspci. BAR0 mapeado UC, 16 MiB de BAR1 mapeados WC.
- Firmware idéntico al del host (los tres FNV-1a coinciden).
- **D4 medido:** IOMMU en `00:00.2`, MMIO `0xfd500000`, control
  `0x0000220000000400`, `IommuEn=0`. Bus = físico.
- La máquina reinició a Linux y retomó la sesión: el sondeo no dejó la GPU
  mal (no hay foto de la pantalla del arranque de constanos).

## Resultados de la fase 2 (2026-09-27)

Estado actual en `docs/reference/gpu.md`. Medido en host y QEMU:
- **Oráculo:** `gpu-trace.py aux trace-nogsp 3` lista las 41+ transacciones
  AUX del ASUS; de ellas salen su DPCD (0x2200: DPCD 1.4, HBR2, 4 carriles)
  y su EDID, idéntico byte a byte al de sysfs. `gpu-trace.py i2c trace-nogsp
  5` decodifica el bit-banging del HP y da su EDID idéntico: el mapa de bits
  del puerto (SCL/SDA en 0/1, lectura en 4/5) está medido, no supuesto.
- **Hallazgos de la traza:** nouveau deja el bit auto-DPCD del canal AUX
  borrado para siempre (`auxch.h:10` pasa `false` en los dos sentidos) y
  enciende el pad del DP sin apagarlo; aquí se restaura todo lo tocado.
  Linux usó `i2c-algo-bit` para el HDMI, no el `bit.c` de nouveau (misma
  semántica de registro, otra secuencia), así que el I2C se prueba contra
  un EEPROM DDC simulado y no contra réplica.
- **VBIOS:** la lectura por PROM reproduce el volcado de sysfs (148 992
  bytes, versión 94.06.37.00.40) sin escribir nada (el bit de sombra ya
  está a 0). La DCB coincide con lo que imprime nouveau (salidas, CCB,
  conectores).
- **El ASUS anuncia 144, 165 y 179,82 Hz** en DTDs de su bloque CTA (no
  solo en el rango): candidatos directos para la fase 5.
- Tests de host de `nvgpu` en verde, con los sabotajes que cada uno detecta
  (tamaño AUX, reintento DEFER, cierre I2C, bits SDA, ACK, offset EDID,
  desplazamiento CCB, EOL, hblank, entrelazado, SVD nativos, NPDE,
  restauración de sombra/pad/auto-DPCD, bloque 1). `run-kernel-tests.sh`
  en verde; `boot-matrix 4 5` 20/20; QEMU con `gpu=disp` y sin GA106 no
  toca nada.

**En la Ryzen (boot #67, `metal-run.sh --kconf 'gpu=disp'
scripts/metal-jobs/gpu-disp.sh`, veredicto OK exit=0):**
- VBIOS por la PROM: **565 760 bytes, 4 imágenes** (00, 03, e0, e0: las
  mismas que nouveau), en 160 ms; los primeros 148 992 bytes idénticos al
  volcado de sysfs; versión 94.06.37.00.40.
- DCB: las 7 salidas, línea por línea, iguales a las de nouveau.
- `DP-1`/`DP-2` desconectados (sin sink en AUX 5/4, 15 ms cada uno);
  **`DP-3` = ASUS VG279Q3A** (DPCD 1.4 HBR2 ×4, 12 ms) y **`HDMI-A-1` = HP
  2309** (47 ms, bit-banging). **Los 256 bytes de los dos EDIDs, idénticos
  a los de sysfs** (hex completo comparado por el trabajo).
- La máquina volvió a Linux sola y la sesión se retomó: la GPU no quedó mal.
  El usuario vio la imagen de constanos durante la ejecución (visible un
  instante, hasta el reinicio del trabajo): la imagen GOP sigue intacta.

**Fase 2 cerrada.**

## Resultados de la fase 3 (2026-09-27)

Estado actual en `docs/reference/gpu.md`. Oráculo (`trace-nogsp`):
- **Cadena completa de la interrupción, medida.** Árbol VFN en `0xb80000`
  (`subdev/vfn/tu102.c`, base en `ga100.c:51`); display = hoja 4, bit 26
  (`ga100.c:30`); rearme MSI escribiendo 0 en `0x088704`
  (`pci/gp100.c:29,34`); resumen del display en `0x611ec0`, estado por
  cabeza en `0x611800` (bit 2 = vblank), MSK `0x611cc0`, EN `0x611d80`
  (`engine/disp/gv100.c`). Unas 1 500 interrupciones por cabeza en la
  traza, una cada ~16,5 ms; el handler de una está en
  `nvgpu/fixtures/vblank-service.txt` y `service` lo reproduce escritura por
  escritura.
- **Lo que la traza no contesta:** las cabezas solo se leen después del
  modeset de nouveau (1920×1080, 148,5 MHz, 2200×1125 → 60,000 Hz), así que
  el estado que deja el GOP no está medido, ni tampoco si el vblank
  interrumpe sin el canal core. En la traza, el bit de vblank de la cabeza
  0 ya estaba latcheado (`0x611800 = 0x7`) antes de que nouveau activara su
  EN: el estado latchea aunque la interrupción esté desactivada. El trabajo
  de metal registra ese estado antes de armar, la tasa por posición del
  barrido (plan B) y si el bit latchea con la configuración del GOP.
- **Decisión:** se intenta el plan A (MSI) sin canal core; el plan B queda
  medido en el mismo arranque como respaldo y diagnóstico.
- **Mecanismo hacia el compositor:** `/dev/vblank` con `epoll` (reutiliza
  la maquinaria de colas de entrada de `poll`, con un número de secuencia
  por handle). El compositor usa la sombra en RAM como búfer trasero: en
  cada vblank vuelca lo compuesto en el frame anterior y luego compone.

Medido en host y QEMU:
- Tests de host de `nvgpu::vblank`, con los sabotajes que detectan: ack de
  vblank omitido, `arm` sin rearme MSI, bit del registro top mal calculado
  por hoja, `disp_other` sin reportar.
- **Encontrado al probar:** ampliar `PollSource` de 16 a 24 bytes provocó
  un DOUBLE FAULT (pila desbordada en `poll_wake_where`, que guarda 8
  waiters en la pila del ISR) en `gui-e2e wm`, que en master pasa. Ahora
  hay un `const assert` de 16 bytes.
- `run-kernel-tests.sh` en verde; `gui-e2e wm/term/text` en verde (el
  compositor cae al temporizador: «pacing by a 16 ms timer»);
  `boot-matrix 4 5` 20/20; el trabajo de metal probado en QEMU (sin GA106
  falla limpio con `exit=1` y no se cuelga).
- **Hueco encontrado (no arreglado aquí):** `kill(pid, 0)` devuelve
  `EINVAL` (`process_ctl.rs:955`), así que el `timeout` de BusyBox se rinde
  al primer segundo. El trabajo usa `&` + `sleep` + `kill -TERM` (SIGQUIT
  llega ignorado a los trabajos `&` de un shell no interactivo).

**En la Ryzen (boot #68, `metal-run.sh --kconf 'gpu=vblank'
scripts/metal-jobs/gpu-vblank.sh`, veredicto OK exit=0):**
- **El GOP enciende solo la cabeza 0**: 1920×1080, total 2200×1125,
  148,5 MHz, 60,000 Hz nominales, profundidad 24 bpp. Las cabezas 1-3 están
  apagadas (reloj 0, total 8×5).
- **Estado que deja el firmware:** árbol VFN sin nada pendiente,
  `disp_intr = 0x1`, `ctrl_disp_en = 0`, EN de todas las cabezas a 0 y MSK a
  `0x3f017f` (nouveau lo deja en 4; `arm` lo sobrescribe). **El bit de
  vblank de la cabeza 0 latchea sin driver** (`0x7`; tras borrarlo, `0x3`,
  y 40 ms después otra vez `0x7`).
- **Plan A funciona sin canal core:** MSI por el vector 0x50 a la CPU 0,
  **1 803 vblanks en 30 s = 59,9953 Hz** (secuencia y marcas de tiempo del
  propio handler). Sin interrupciones espurias, bloqueadas ni «GPU fuera
  del bus». El plan B (posición del barrido) midió lo mismo: 59,995 Hz.
  La diferencia con 60,000 (~80 ppm) es la del cristal del display frente
  al TSC.
- `compositor fire` se ritmó con `/dev/vblank` («pacing by vblank»): 1 217
  vblanks en los 20 s, y la máquina volvió a Linux sola.
- **Sin tearing:** el usuario miró la pantalla durante los 20 s de `fire`
  bajo el compositor y no vio ninguno.

**Fase 3 cerrada.**

## Fases

Calendario orientativo en días de trabajo. El riesgo está en la fase 4.

| Fase | Qué | Días | Salida visible |
|---|---|---|---|
| 0 | Oráculo y fixtures | 1–2 | capturas, D1 fijada |
| 1 | Prerrequisitos del kernel (MSI, DMA, BAR, firmware) | 3–4 | test `edu` de MSI+DMA en QEMU |
| 2 | `nvdisp` de lectura: VBIOS, DCB, EDID | 3–4 | `/proc/displays` con nombres |
| 3 | vblank | 2 | vsync en el compositor |
| 4 | Arranque del GSP hasta RM | 8–10 | RM devuelve "NVIDIA GeForce RTX 3050" |
| 5 | Modeset vía RM | 5–6 | 180 Hz en el menú del monitor |
| 6 | Canal + motor de copia | 3–4 | blit por CE medido |
| 7 | Decisión de la pila 3D | — | documento, no código |

---

### Fase 0: Oráculo y fixtures

**Por qué primero:** todo lo demás se verifica contra esto. Sin trazas, cada
fase es arqueología.

Entregables:
- `scripts/gpu-oracle.sh` (en el host, como root, con pasos idempotentes):
  - VBIOS: `echo 1 > /sys/bus/pci/devices/0000:09:00.0/rom; cat rom`.
  - EDIDs de `/sys/class/drm/card*-*/edid`.
  - `lspci -vvv -xxxx` de `09:00.0` (capacidades: MSI, MSI-X, ReBAR).
  - Instantánea de BAR0 **de solo lectura** (una lista de rangos, no los
    16 MiB enteros) tomada en un arranque de Linux **sin driver de GPU**
    cargado, que es el mismo estado que verá constanos tras el GOP.
- Una entrada de arranque de Linux para trazar: sin `nvidia`, sin la lista
  negra de nouveau y en `multi-user.target`. Con ella, `mmiotrace` de:
  1. nouveau con `NvGspRm=0` (display sin GSP: lectura de DCB, AUX/I2C, EDID);
  2. nouveau con `NvGspRm=1` (arranque completo del GSP + modeset vía RM).

  Se anota qué versión de firmware carga → **D1**.
- `scripts/gpu-ref.sh`: clona las referencias en sus etiquetas.
- Las trazas completas (MB) van en `~/constanos-gpu-oracle/`, fuera de git,
  con un README. En `nvgpu/fixtures/` entran solo los fixtures derivados y
  pequeños: VBIOS, EDIDs, instantánea, extractos de traza por paso.

**Hecho cuando:** existen ambas trazas, se sabe dónde empieza y acaba cada
paso de las fases 2–5 dentro de ellas (offsets anotados en este documento), y
D1 está fijada.

---

### Fase 1: Prerrequisitos del kernel

Genéricos y útiles fuera de la GPU. Se validan en QEMU con el dispositivo
`edu` (`-device edu`, que hace MSI y DMA) antes de tocar la GPU.

1. **MSI/MSI-X.** Recorrido de capacidades PCI (`hal::pci`, test de host con
   el `lspci -xxxx` real), un rango de vectores dinámicos registrado en la IDT
   desde el arranque (la IDT es un `Once`: los vectores se preasignan y
   despachan por tabla) y la dirección/dato MSI hacia el LAPIC de la CPU 0.
   El handler sigue las reglas de ISR de `CLAUDE.md` y dice si su trabajo es
   global o por CPU.
2. **API de DMA** (`memory::dma`): páginas sueltas y bloques contiguos de
   orden N desde el buddy, con phys + virt, y listas de páginas (el firmware
   GSP va por tablas radix3, así que no necesita decenas de MB contiguos).
   Liberación explícita, sin `Drop` implícito en caminos `-> !`.
3. **BARs**: habilitar memoria + bus master, mapear BAR0 UC completo y una
   ventana de BAR1 WC (no los 8 GiB).
4. **Carga de firmware**: `firmware::load("nvidia/ga106/gsp/...")` →
   `Vec<u8>` desde `/mnt/lib/firmware`, con D3 en `kernel/build.rs`.
5. `pci::claim("nvgpu")` de `09:00.0` con `gpu=probe` o más.

**Hecho cuando:** test de integración QEMU con `edu` (factorial por MMIO,
DMA ida y vuelta, MSI recibida) en `run-kernel-tests.sh`; tests de host del
parser de capacidades; en la Ryzen, con `gpu=probe`, el log muestra las
capacidades MSI de la GPU y `PMC_BOOT_0` leído (arquitectura GA10x, impl 6).

---

### Fase 2: `nvdisp` de lectura (VBIOS, DCB, EDID)

Sin GSP y **sin escribir nada que cambie el display**. Solo lecturas y las
transacciones AUX/I2C que nouveau hace en la traza sin GSP.

- `nvgpu::vbios`: localizar la imagen (PROM por BAR0, o la ROM PCI), las
  tablas BIT y la **DCB**: salidas, conectores y qué pad I2C/AUX va con cada
  conector. Tests de host contra el VBIOS volcado.
- `nvgpu::aux` / `nvgpu::i2c`: transacciones DP-AUX (DPCD + EDID por
  I2C-over-AUX) y DDC por HDMI, portadas de nouveau GA10x con cita.
  `ReplayMmio` con el extracto de traza de nouveau leyendo el EDID.
- `nvgpu::edid`: parser propio (nombre, fabricante, DTDs, VICs, límites de
  rango, extensión CTA y DisplayID). Tests con los dos EDIDs reales,
  comparados contra la salida de `edid-decode`.
- `/proc/displays`: conector, estado, nombre, modo nativo y rango. La
  detección de conexión se hace por consulta, sin interrupción todavía.

**Hecho cuando:** en la Ryzen, con `gpu=disp`, `/proc/displays` lista
`DP-3 VG279Q3A` y `HDMI-A-1 HP 2309` (nombres de la DCB, como nouveau; el
`DP-1` de la versión original era el nombre de `nvidia`, ver fase 0), y
los 256 bytes de cada EDID leídos
por constanos son **idénticos** a los de sysfs (el trabajo autorun los
compara con el fixture). Además, la imagen GOP sigue intacta después.

---

### Fase 3: vblank

Todavía sin GSP. Objetivo: la interrupción de vblank de la cabeza que ya
programó el firmware, entregada por MSI.

- Localizar en la traza sin GSP cómo nouveau habilita y atiende la
  interrupción de vblank del display GA10x; portar solo eso.
- **Si** resulta que el vblank no se puede habilitar sin el canal core (no
  medido): usar el plan B de leer la posición del barrido de la cabeza para
  marcar el ritmo, y dejar el vsync real para la fase 5. La decisión se anota
  aquí con la evidencia.
- Exponer al compositor: un evento de vblank por `epoll` sobre `/dev/fb0` o
  sobre un fichero `/dev/vblank`. Se decide al implementar, pero con un
  mecanismo de bloqueo que cumpla "check-then-sleep es un paso".

**Hecho cuando:** contador de vblank en `/proc/kdebug` a 60,0 ± 0,1 /s en la
Ryzen; el compositor presenta en vblank y una foto del fuego/DOOM en
movimiento no muestra tearing.

---

### Fase 4: Arranque del GSP hasta RM

La fase grande. Se trocea en pasos que entran por separado, cada uno con su
extracto de traza como oráculo y su propio `gpu=` intermedio.

- **4a. Falcon genérico**: reset, carga IMEM/DMEM (PIO y DMA), arranque y
  espera por buzones (mailbox). Portado del código falcon de nouveau. Tests de
  la lógica de secuencia con `ReplayMmio`.
- **4b. Parseo del firmware**: la imagen `gsp-<D1>.bin` es un ELF con
  secciones (`.fwimage`, `.fwsignature_*`); `booter_load` y `bootloader`
  tienen sus cabeceras. Todo son tests de host con los ficheros reales.
- **4c. FWSEC-FRTS**: extraer FWSEC del VBIOS (fixture de la fase 0),
  parchear su DMEM como hace nouveau y ejecutarlo en el falcon GSP. Resultado
  medible: el registro de la región WPR2/FRTS queda programado igual que en
  la traza.
- **4d. Memoria para GSP-RM**: tablas radix3 del firmware, `GspFwWprMeta`,
  argumentos de LibOS, colas de mensajes y buffers de log de GSP (LOGINIT,
  LOGINTR, LOGRM). Todo por la API de DMA de la fase 1; los layouts, por D2.
- **4e. Booter en SEC2 → GSP-RM arranca**: `booter_load` en el falcon SEC2
  con la dirección del WprMeta; esperar a que el RISC-V del GSP arranque.
  **Los logs de GSP se vuelcan al klog desde el primer momento**, porque sin
  ellos no hay manera de depurar un GSP colgado.
- **4f. Colas RPC**: la lógica del anillo va en `nvgpu`, con tests de host y
  un "GSP falso" que responde. Recibir `GSP_INIT_DONE` y enviar los RPC de
  arranque que envía nouveau (lista sacada de la traza, en el mismo orden).
- **4g. Objetos RM**: `NV01_ROOT` → `NV01_DEVICE_0` → `NV20_SUBDEVICE_0` y
  una llamada de control que devuelva el nombre de la GPU.

Riesgos específicos: que el arranque del GSP apague el scanout GOP (D5); que
un GSP fallido deje la GPU sin recuperar hasta un apagado completo (medir en
4c/4e y, si pasa, `booter_unload` más reset antes de reiniciar); los tiempos
de espera (acotados, siempre con `service_pending` si hay IF=0).

**Hecho cuando:** en la Ryzen, con `gpu=gsp`, el klog contiene el log de
arranque de GSP-RM y la cadena "NVIDIA GeForce RTX 3050" obtenida por RM; 5
arranques seguidos de `metal-run` sin fallo; y un reinicio en caliente
posterior arranca con imagen.

---

### Fase 5: Modeset (propio, sin GSP; D6)

> **Reescrita por D6.** Lo que sigue en esta sección describe la ruta RM,
> descartada. La ruta vigente porta lo que hace nouveau sin GSP, con la
> traza `nogsp` como oráculo: canal core y de ventana por EVO/NVDisplay
> directo, relojes de píxel desde las tablas del VBIOS y entrenamiento DP
> propio por AUX (portado de `nouveau/nvkm/engine/disp/` y
> `dispnv50/`). Esta sección se reescribe entera al empezar la fase, después
> de segmentar la traza. El criterio de "hecho" no cambia.

Ruta RM (descartada): con RM, el entrenamiento del enlace DP y los relojes de
píxel los hace el GSP. El driver asigna los canales de display y les envía
métodos.

- Asignar por RM el display común (`NV04_DISPLAY_COMMON`) y los canales de
  display de GA10x: core, ventana, ventana inmediata y cursor. Las clases, de
  los headers fijados; nada de copiarlas de memoria.
- Detección y DP por las llamadas de control `NV0073_*`: conector, EDID vía
  RM (se compara con la fase 2), entrenamiento del enlace y asignación de SOR.
- Búfer de scanout en VRAM (ventana de BAR1 WC) con doble búfer y cambio de
  página en vblank. Así queda el vsync real y, si hacía falta, se sustituye el
  plan B de la fase 3.
- Lista de modos desde `nvgpu::edid`, más un modo CVT-RB2 para 180 Hz dentro
  del rango del monitor (dotclock ≤ 430 MHz). Generador CVT con tests de host
  contra tablas conocidas.
- Interfaz: la escritura en `/proc/displays` (o un ioctl en `/dev/fb0`) pide
  el modo, y `/dev/fb0` pasa a apuntar al nuevo scanout. La consola y el
  compositor se enteran del cambio de tamaño (evento).

**Hecho cuando:** el menú del VG279Q3A muestra **180 Hz** (foto); el HP 2309
se enciende a 1920×1080 a 60 Hz con su propio contenido; el compositor
presenta un frame por vblank a 180 Hz (contador en `/proc/kdebug`); y volver
a 60 Hz funciona sin reiniciar.

---

### Fase 6: Canal + motor de copia

- Espacio de direcciones de la GPU por RM (`FERMI_VASPACE_A`), mapear VRAM y
  memoria del sistema en él.
- Canal GPFIFO de Ampere + USERD + doorbell, con la clase del motor de copia
  de GA10x (de los headers fijados).
- Semáforo y interrupción de finalización → fence que los waiters esperan con
  un mecanismo de bloqueo correcto.
- Primer uso: el compositor sube el búfer del sistema a VRAM por CE en vez de
  con la CPU.

**Hecho cuando:** un test en metal copia N MiB sistema→VRAM por CE, verifica
el contenido y mide GB/s; el compositor lo usa y `fb_flush` baja respecto a
la cifra actual (`docs/gui/perf-plan.md`).

---

### Fase 7: Decisión de la pila 3D (puerta, no trabajo)

Documento con la comparación medida entre dos caminos:

- **NVK + Zink sobre una uAPI DRM de nouveau compatible con Linux.** Requisitos
  en el kernel: `clone` con ABI de Linux + hilos, uAPI DRM (`VM_BIND`, `EXEC`,
  syncobj, mmap de BOs, dma-buf) y Mesa estático (sin enlazado dinámico).
- **Módulos abiertos de NVIDIA** (fase 5 de `docs/drivers/roadmap.md`) sobre
  una capa `os_*` propia: da CUDA, pero su espacio de usuario es glibc dinámico.

Cada requisito se lista con su coste estimado y con lo que ya hay (memoria
`rust_std_gaps`).

---

## Registro de riesgos

| Riesgo | Fase | Mitigación |
|---|---|---|
| Perder la única salida de vídeo | 2–6 | `gpu=off` por defecto, veredicto por la partición de log, fotos por adb |
| GPU irrecuperable sin apagar | 4–6 | medir reinicio en caliente por fase; `booter_unload` + reset PCI |
| ABI de RM mal portada (tamaños, offsets) | 4–6 | D2: estructuras generadas + asserts de tamaño y offset |
| El LLM inventa registros | todas | principio 2: sin cita, no entra |
| El GSP cuelga sin decir nada | 4 | logs de GSP al klog desde 4e; tiempos de espera acotados |
| El vblank necesita el canal core | 3 | plan B de posición del barrido; vsync real en la 5 |
| La traza de nouveau no reproduce el estado GOP | 0 | trazar desde un arranque sin driver, igual que constanos |
| DMA por encima de 4 GiB o con IOMMU | 1 | D4 medido; máscara DMA de la GPU (47 bits) |

## Qué se actualiza al cerrar cada fase

- La línea de estado al principio de este documento, con lo medido.
- `docs/reference/gpu.md` (se crea en la fase 1): el estado actual, no el plan.
- `CLAUDE.md`: una fila en el mapa de código cuando exista `kernel/src/gpu/`;
  reglas nuevas solo si son invariantes (por ejemplo, el orden de locks de la
  cola RPC).
- El skill `kernel-drivers`, si el patrón `Mmio` + `ReplayMmio` se generaliza.
