# Prompt — optimizar la consola de framebuffer para hardware real

Repo: `rust_so_kernel` (kernel bare-metal x86_64, `#![no_std]`, en
`/home/oriaj/rust/rust_so_kernel`). Lee `CLAUDE.md` entero antes de tocar nada.

Esta línea de trabajo existe porque **el kernel ya arranca y es usable en la
máquina física** (AM4/Ryzen, teclado USB, sin PS/2, sin captura de serial) desde
la rama `usb-keyboard` (commits `587a637`, `2bf9bc0`). Ese cambio de escenario es
lo que convierte a la consola en el cuello de botella: en QEMU el framebuffer es
RAM del host y todo lo que sigue es invisible.

Puedes delegar pasos a subagentes como en las líneas anteriores
(`docs/prompt-sched-crate.md` es la referencia de tono y disciplina), pero
**re-corre tú los comandos y lee el diff real antes de aceptar cualquier
reporte**: en este repo los números auto-reportados por subagentes han estado
mal todas las veces.

---

## El síntoma medido

En la máquina real, borrar un carácter con la tecla de retroceso **congela la
consola ~1 segundo**. En QEMU la misma operación es imperceptible.

La causa está identificada y la aritmética cuadra con el segundo observado. No
hace falta re-descubrirla; hace falta arreglarla.

`ash` no borra una celda: redibuja la línea y emite `ESC[J` ("borra desde el
cursor hasta el final de la pantalla"). El handler está en
`kernel/src/drivers/framebuffer_console.rs`:

```rust
b'J' => { match params[0] {
    0 => { clear_row_from(fb, state, state.row, state.col, cols);
           for r in (state.row + 1)..rows { clear_row_from(fb, state, r, 0, cols); } }
    ...
} }

fn clear_row_from(fb, state, row, start_col, end_col) {
    for c in start_col..end_col {
        fb.draw_char(px, py, b' ', DEFAULT_FG, state.bg, SCALE);   // 64 draw_pixel
    }
}
```

Y `Framebuffer::draw_pixel` (en `kernel/src/framebuffer.rs`) es una escritura
suelta con su comprobación de límites, llamada **una vez por píxel**.

A 1920×1080 con `CHAR_W=8`/`CHAR_H=9` salen ~239 columnas × ~119 filas. Borrar
desde un prompt a un cuarto de pantalla hasta abajo son ~22.000 celdas × 64
píxeles ≈ **1,4 millones de escrituras individuales**. A ~0,5 µs cada una en
memoria de vídeo no combinada, eso es ~1 s.

---

## Lo que cambia en bare metal (el punto de toda esta línea)

Cuatro diferencias, todas ausentes en QEMU:

1. **Escribir en VRAM no es escribir en RAM.** El framebuffer vive detrás de
   PCIe. Si el tipo de memoria efectivo es UC (uncached), *cada* escritura es
   una transacción de bus independiente: no se combinan, no se agrupan en
   ráfagas, y el tamaño de la escritura casi no importa frente al coste fijo.
   Escribir 4 bytes contiguos 64 veces cuesta ~64 transacciones; escribir 256
   bytes contiguos de una vez puede costar muchas menos.

2. **Leer de VRAM es mucho peor que escribir.** Una escritura es *posted* (el
   CPU la suelta y sigue); una lectura es *non-posted* y el CPU se para hasta
   que el dato vuelve por el bus. Todo read-modify-write sobre el framebuffer
   es, en la práctica, el doble de malo que parece.

3. **La pantalla es más grande.** QEMU da 1280×800. La máquina real es
   probablemente 1920×1080 o más — **mídelo, no lo asumas** (ver abajo). Todo
   coste proporcional al área crece con el cuadrado de esa diferencia.

4. **No hay serial.** Cualquier medición tiene que volver por la pantalla o por
   `/proc`. Eso condiciona el diseño del instrumento antes que el de la
   optimización.

---

## Paso 0 — el instrumento, antes que la optimización

Este repo tiene una regla ganada a base de perder tiempo (`measure-the-instrument-first`,
y el hallazgo de la sesión USB de 2026-09-21): **un contador ambiguo es peor que
ninguno**. No optimices a ciegas y no te fíes de tu propia aritmética.

Construye primero, y déjalo permanente detrás de `kernel::debug` (hay precedente
explícito: la instrumentación se generaliza a `ktrace!`/`/proc/kdebug`, no se
borra tras arreglar el bug):

- **Dimensiones y formato reales**: `width`, `height`, `stride`,
  `bytes_per_pixel`, y la dirección física del framebuffer. Exponlo en
  `/proc` (p. ej. `/proc/fbinfo`, o una línea en `/proc/kdebug`). En la máquina
  objetivo esto se lee con `cat`; hoy no hay forma de saberlo.
- **Tipo de memoria efectivo.** El bootloader mapea el framebuffer con
  `PRESENT | WRITABLE | NO_EXECUTE` y **ningún bit PCD/PWT** (verificado en
  `bootloader-x86_64-common-0.11.15/src/lib.rs:303`), o sea índice 0 del PAT =
  WB. El tipo efectivo lo decide entonces la combinación con los MTRR de ese
  rango físico. Lee los `IA32_MTRR_PHYSBASE/PHYSMASK` (MSR 0x200+) y di qué tipo
  cubre la dirección del framebuffer. **Esto es un dato, no una suposición**: si
  ya fuese WC, la mitad de las optimizaciones de abajo sobran.
- **Coste por operación, en ciclos TSC.** Cuenta y acumula (`time::ktime_get` o
  `rdtsc` directo) para: `draw_char`, `clear_row_from`, `scroll_up`, `xor_rect`,
  y el total dentro de `render_bytes`. Report en `/proc`. Con eso, un `ESC[J`
  deja de ser "va lento" y pasa a ser un número comparable antes/después.

Criterio de aceptación del paso 0: puedes arrancar en la máquina real, hacer
`cat` de ese `/proc`, fotografiar la pantalla y tener las cifras. (Para
recuperar la foto: `adb connect <ip>:<puerto>` y `adb pull` de la última de
`/sdcard/DCIM/Camera/`; ver `docs/`/memoria `screen-via-phone-adb`.)

---

## Candidatos, por valor esperado

Ordenados por (ganancia esperada ÷ riesgo). **No los hagas todos de golpe**:
cada uno debe poder medirse por separado contra el paso 0.

### 1. Rellenos por scanline en vez de píxel a píxel — *empieza por aquí*

`clear_row_from`, `Framebuffer::clear` y el fondo de `draw_char` escriben píxel
a píxel. Un rectángulo de celdas es, por cada línea de scanline, un rango de
bytes **contiguos**: se puede llenar con un `copy_from_slice` desde un patrón
pre-construido, o con `write_bytes` si el color de fondo tiene los cuatro bytes
iguales (el caso `DEFAULT_BG` = negro).

Es local, no cambia ninguna interfaz, y es donde está casi toda la ganancia del
síntoma reportado. Añade `fill_rect(x, y, w, h, color)` a `Framebuffer` y haz
que `clear`, `clear_row_from` y el borrado de fondo pasen por ahí.

Ojo: `draw_char` dibuja glifo y fondo entremezclados. Sepáralo en "rellena la
celda" + "pinta los píxeles encendidos del glifo" solo si el paso 0 dice que el
fondo domina; si no, no compliques.

### 2. Quitar las lecturas de VRAM

Dos sitios leen el framebuffer, y en UC eso es lo más caro que hay:

- **`xor_rect`** (`buffer[off] ^= 0xFF`) — es el cursor, y corre en la ISR del
  PIT a 100 Hz vía `tick_cursor_blink`, más una vez por cada `render_bytes`
  (`undraw_cursor_locked`). Alternativa sin lecturas: guardar en RAM los píxeles
  de la celda bajo el cursor y restaurarlos, o directamente repintar la celda
  con el glifo y el color invertido (se conoce el carácter si se lleva un
  shadow del texto — ver 4).
- **`scroll_up`** — `buffer.copy_within(skip..total, 0)` **lee todo el
  framebuffer**. En una pantalla de 8 MB eso es leer 8 MB de VRAM por cada línea
  nueva al final de la pantalla. Este todavía no te ha mordido porque el prompt
  no llegaba abajo, pero en cuanto se use la consola de verdad va a doler más
  que el `ESC[J`.

### 3. Write-combining (PAT)

Si el paso 0 dice que el framebuffer es UC, WC es la ganancia grande para
escrituras secuenciales. **No es gratis y tiene una trampa:**

- El PAT por defecto **no tiene ninguna entrada WC** (0=WB, 1=WT, 2=UC-, 3=UC,
  y 4-7 repiten). Hay que reprogramar `IA32_PAT` (MSR 0x277) para poner WC en
  una entrada, como hace Linux, y luego mapear el framebuffer con la
  combinación PCD/PWT que la seleccione.
- **Los MTRR pueden ganarle al PAT.** La regla de combinación MTRR×PAT no es
  "el PAT manda"; verifica el resultado con una medición real (throughput de
  escritura antes/después), no con la tabla del manual leída de memoria. Si el
  MTRR de esa zona es UC y no se deja, la vía alternativa es un MTRR de rango
  variable WC para el aperture — más invasivo.
- Con WC hacen falta `sfence` en los puntos donde el contenido debe ser visible
  (fin de un frame, antes de un `screendump`), porque las escrituras dejan de
  estar ordenadas.

Ya existe `memory::mmio::map` (uncached, para los registros del xHCI); lo que
falta es el gemelo WC. Compártele la lógica de reserva de rango virtual.

### 4. Shadow buffer en RAM + blit de lo sucio — *el cambio grande*

Mantener el framebuffer en RAM normal (WB, rápida de leer y escribir) y volcar
a VRAM solo los rectángulos modificados. Convierte todo read-modify-write en
operaciones sobre RAM y todas las escrituras a VRAM en secuenciales.

Es el diseño correcto a largo plazo y el que más toca. Hazlo solo si 1-3 no
bastan, y con el paso 0 como juez. Ten en cuenta que duplica el uso de memoria
del framebuffer (8 MB a 1920×1080) y que hay que decidir cuándo se vuelca (por
cada `write`, o por tick).

### 5. El mirror a serial

`mirror_to_serial` escribe **cada byte** de la salida de usuario al puerto
0x3F8, más un prefijo `[fb] ` por línea. En la máquina objetivo no hay nada al
otro lado. No es la causa del síntoma reportado, pero es trabajo por byte en el
camino caliente: mídelo en el paso 0 y decide si merece una condición.

---

## Restricciones duras (no romper)

- **`kalert!`/`kernel_print`/`kernel_write_bytes` usan `try_lock`, nunca
  `lock`**, en `FB_STATE` y `FRAMEBUFFER`. El llamador puede ser un handler de
  excepción con el proceso interrumpido a medio `write`. Si introduces un
  shadow buffer o un lock nuevo, esa propiedad debe sobrevivir.
- **`tick_cursor_blink` corre en la ISR del PIT** con interrupciones
  deshabilitadas: no puede bloquear ni asignar memoria.
- **`FramebufferConsole::new` no debe limpiar la pantalla si el kernel ya
  escribió** (`KERNEL_WROTE`). Eso se arregló en `2bf9bc0` después de que
  borrara el diagnóstico del USB justo antes de que nadie pudiera leerlo.
- **`FBIO_BLIT` y `FB_RAW_DIRTY`** (DOOM/Quake) escriben el framebuffer por su
  cuenta y saltándose el tracking de cursor. Cualquier shadow buffer tiene que
  invalidarse ahí, o el primer texto tras salir de DOOM pintará basura.
- **`TIOCGWINSZ`** reporta filas/columnas derivadas de las dimensiones reales;
  `vi` depende de ello (`CONFIG_FEATURE_VI_WIN_RESIZE`). No cambies el cálculo
  sin comprobar `vi`.
- La consola es también la salida de `stdout`/`stderr` de todo proceso: no
  puede asignar en el camino de `write` de forma que pueda reentrar el
  allocator (ver la invariante de `IrqMutex` en `CLAUDE.md`).

## Verificación

- `cd hal && cargo test` (125 al empezar esta línea) y
  `scripts/run-kernel-tests.sh` deben seguir en verde.
- `scripts/boot-matrix.sh 4 2` limpio.
- `vi`, `doom` y `quake` siguen dibujando bien (son los tres clientes raros de
  la consola).
- **La medición que importa es en la máquina real**: `dd` al pendrive, arrancar,
  `cat` del `/proc` del paso 0, foto, `adb pull`. Un número mejor en QEMU no
  prueba nada aquí — es justamente el escenario que ocultó este problema.
