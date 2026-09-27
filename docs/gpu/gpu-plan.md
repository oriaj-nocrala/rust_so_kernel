# Plan: GPU NVIDIA (display, GSP, canales)

> **Estado (2026-09-26):** fase 0 casi cerrada. Trazas capturadas y
> válidas (ver "Resultados de la fase 0"); D1 fijada en **570.144**, medido.
> Falta segmentar las trazas por paso y aclarar el fallo de display con GSP
> (D6). Ninguna fase se da por hecha sin su criterio medido en la Ryzen.

## Objetivo

Pasar de "framebuffer GOP a 60 Hz, sin vsync, sin saber qué monitor hay" a un
driver propio de la RTX 3050 que:

1. **nombra los monitores** (EDID leído por la GPU, en caliente);
2. da **vblank** (vsync real para el compositor);
3. **arranca el GSP** y habla con GSP-RM;
4. **cambia de modo** a través de RM (1920×1080 a 180 Hz en el DP);
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
- Soporte sin GSP más allá del display de lectura (fases 2–3). Todo lo que
  necesite programar el display o la memoria de la GPU pasa por RM.

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
| Linux `drivers/gpu/nova-core/` | Arranque GSP en Rust (FWSEC, booter, radix3). Ver cómo lo estructuran | GPL-2.0: **leer, no copiar** |
| `envytools` / `rnndb` | Nombres de registros para leer trazas mmiotrace (`demmio`) | MIT |
| Mesa `src/nouveau/` (NVK) | Solo para la fase 7 | MIT |

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
- **D3: el firmware no entra en git.** `kernel/build.rs` lo copia de
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
- **D6: ruta del modeset (abierta, se decide antes de la fase 5).** Medido en
  la fase 0: nouveau **sin GSP** hace un modeset completo y limpio en esta
  tarjeta (fbcon a los 11,7 s, sin errores), y nouveau **con GSP** arranca
  RM bien pero su display falla (ver resultados). Hay dos rutas: modeset vía
  RM, como está escrita la fase 5, o modeset propio con la traza `nogsp`
  como oráculo, portando lo que hace nouveau sin GSP (entrenamiento DP y
  relojes incluidos). Para elegir, primero hay que saber si el fallo con GSP
  es de nouveau r570 en GA106 o un efecto de mmiotrace. Se mide con un
  arranque de nouveau con GSP **sin** mmiotrace.

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

Pendiente para cerrar la fase 0:
1. Segmentar las trazas: anotar aquí los rangos de tiempo de cada paso
   (lectura de VBIOS, AUX/EDID, vblank, FWSEC, booter, RPC de init, modeset).
2. Medir D6: nouveau con GSP sin mmiotrace (un reinicio, solo el dmesg).

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
`DP-1 VG279Q3A` y `HDMI-A-1 HP 2309`, y los 256 bytes de cada EDID leídos
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

### Fase 5: Modeset vía RM

Con RM, el entrenamiento del enlace DP y los relojes de píxel los hace el
GSP. El driver asigna los canales de display y les envía métodos.

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
