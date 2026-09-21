# La consola de framebuffer: medida, arreglada una vez, y lo que queda

Registro de la línea de trabajo abierta en `docs/prompt-consola-framebuffer.md`.
Lo que sigue son **números medidos**, no estimaciones: el criterio de esa
línea era que cada candidato se pudiera medir por separado contra un
instrumento construido antes de tocar nada.

## Paso 0 — el instrumento (`/proc/fbinfo`)

`cat /proc/fbinfo` reporta, regenerado en cada `open()`:

* **Geometría real** — `width`, `height`, `stride` (que no es `width`:
  el firmware suele rellenar filas), `bytes_per_pixel`, tamaño total,
  rejilla de texto en celdas, y las direcciones virtual **y física**. Antes
  de esto no había forma de saber ninguna de esas cosas en la máquina
  objetivo; el único dato era un comentario en `framebuffer.rs` que decía
  «1280 x 800 en qemu».
* **Tipo de memoria** — los bits PAT/PCD/PWT que la PTE viva selecciona,
  el `IA32_PAT` completo, y qué MTRR cubre la dirección física, con las
  ocho entradas variables listadas. La aritmética vive en
  `hal::memtype` (17 tests host); el kernel solo lee los MSR y camina la
  tabla de páginas (`kernel/src/memory/memtype.rs`,
  `OwnedPageTable::translate_with_flags`).

  **Deliberadamente no** combina MTRR × PAT en un veredicto único. Esa
  tabla (SDM vol. 3A, 11-7) es justo lo que es fácil equivocarse de
  memoria, y no hace falta: la verdad de campo es el `MB/s` medido más
  abajo, que no necesita tabla ninguna.
* **Coste por operación** — `diag::OpStat` (8 tests host) por cada
  primitiva: llamadas, bytes, ciclos TSC, **min y max por llamada**, y el
  throughput derivado.
* **La calibración del propio instrumento** — `instrument_overhead`, el
  coste de una medición vacía, medido en vivo. En QEMU: **74 ciclos**,
  despreciable frente a todo lo demás. Sin esa línea no hay forma de
  distinguir una operación barata de una cuyo coste *es* el instrumento.

`min`/`max` están ahí porque los deltas son reloj de pared: la consola no
corre con interrupciones deshabilitadas, así que una preempción dentro de
una operación medida le carga todo lo que hizo otro proceso. Medido, no
temido: `draw_char` promedia ~115k ciclos en un arranque tranquilo y
~1,5M imprimiendo 400 líneas, para trabajo idéntico. Con `min 103859 max
2022531` a la vista, un lector sabe que la media está cerca del mínimo y
que los outliers son preempciones — que es exactamente el juicio que un
número único le habría quitado.

## Paso 1 — rellenos por scanline

`Framebuffer::fill_rect(x, y, w, h, color)`: un rango de bytes contiguo
por scanline, `memset` cuando el color es negro y copia de un patrón
pre-construido en otro caso. `clear`, `clear_row_from`, `clear_rows` y el
fondo de `draw_char` pasan todos por ahí; `ESC[J` y `ESC[K` dejaron de
recorrer celda a celda.

### La medida A/B

Mismo build salvo esa función, mismo QEMU, mismo `clear` (`ESC[H ESC[J` a
pantalla completa, 1280x800, rejilla 159x88):

| borrado de pantalla completa | llamadas | ciclos | a 3,7 GHz |
|---|---|---|---|
| por celda (`draw_char`) | 14 205 | 1 975 481 800 | **534 ms** |
| por scanline (`fill_rect`) | 2 | 4 811 369 | **1,3 ms** |

**410x**, y la versión nueva además pinta *más* píxeles (incluye la fila
de `LINE_GAP` que `draw_char` nunca tocaba, y que después de un scroll o
un blit crudo no era necesariamente fondo).

Ese es el síntoma con el que empezó todo: en la máquina física, borrar un
carácter congelaba la consola ~1 s, porque `ash` no borra una celda —
redibuja la línea y emite `ESC[J`.

**Advertencia honesta sobre este número:** está medido en QEMU, que es
precisamente el entorno donde el problema estaba escondido. La dirección
es segura en metal (menos transacciones y más grandes, sobre un bus que
cobra por transacción), pero la magnitud real tiene que salir de la
máquina objetivo. Para eso está `/proc/fbinfo`.

## Lo que el instrumento decidió sobre el resto

Carga de referencia: `seq 1 400` en el shell (330 scrolls, 1616 glifos),
QEMU, TSC a 3,7 GHz.

| operación | ciclos totales | % del tiempo de consola | veredicto |
|---|---|---|---|
| `fb_scroll_up` | 1 328 938 472 | **~83 %** | **lo siguiente que hay que arreglar** |
| `fb_draw_char` | 184 704 592 | ~11 % | aceptable |
| `fb_cursor_xor` | 66 148 193 | ~4 % | **no tocar** |
| `fb_serial_mirror` | 14 203 486 | ~0,9 % | **no tocar** |
| `fb_fill_rect` | 5 734 075 | ~0,4 % | ya arreglado |

Dos candidatos del prompt original quedan **descartados por medición**, no
por opinión:

* **Quitar las lecturas de VRAM del cursor** (candidato 2, mitad
  `xor_rect`): 4 % del tiempo, `min 42254` ciclos por llamada y muy
  estable. El razonamiento «read-modify-write sobre VRAM es lo más caro
  que hay» es correcto en general y falso aquí: una celda son 8x8 píxeles
  y el cursor parpadea a 2 Hz. No vale la complejidad de un shadow de la
  celda.
* **El mirror a serial** (candidato 5): 0,9 %. Es trabajo por byte en el
  camino caliente hacia un UART que en la máquina objetivo no escucha
  nadie, y aun así no aparece en la cuenta. Condicionarlo costaría una
  sonda del 16550 cuyo modo de fallo es «se pierde toda la salida de
  usuario en serial.log», que es la herramienta de depuración de este
  repo. No compensa.

## Lo que queda, con su número

**`scroll_up` es ahora el coste dominante de la consola**, exactamente
como el prompt predijo. Medido: **3 908 865 ciclos mínimos por línea
scrolleada** = **~1,06 ms**, tan caro como un borrado de pantalla entero
después del paso 1. Y eso en QEMU, donde el `copy_within` es un memmove
sobre RAM del host: en metal la mitad de lectura es *non-posted* (el CPU
se para hasta que el dato vuelve por el bus), así que el número real será
mucho peor.

No tiene arreglo local. Un scroll lee el framebuffer entero, y para no
leerlo hace falta una de dos cosas, las dos grandes:

1. **Write-combining (PAT)** — candidato 3. El paso 0 ya da los dos datos
   que hacían falta para decidir: el PAT de reset **no tiene ninguna
   entrada WC** (`pat_has_wc: false`), así que no existe combinación de
   bits que seleccione WC sin reprogramar `IA32_PAT` primero; y en QEMU el
   MTRR que cubre el framebuffer es **UC** (`mtrr[0]: base=0x80000000
   type=UC`), con `wc_supported=true` en `MTRRCAP`. Falta el dato de la
   máquina real, que ahora se lee con un `cat`.
2. **Shadow buffer en RAM + blit de lo sucio** — candidato 4. Convierte el
   scroll en un memmove sobre RAM WB más un volcado secuencial, pero
   duplica el uso de memoria del framebuffer y tiene que invalidarse en
   `FBIO_BLIT` (DOOM/Quake) y sobrevivir a la regla de `try_lock` de
   `kalert!`.

Ninguna de las dos se hizo aquí, por la disciplina de la propia línea: una
a la vez, cada una medida por separado contra el paso 0.

## Verificación de este cambio

* `cd hal && cargo test` — 142 (125 antes + 17 de `memtype`).
* `cd diag && cargo test` — 38 (30 antes + 8 de `OpStat`).
* `scripts/run-kernel-tests.sh` — PASS, 5 casos: el quinto es
  `framebuffer_primitives_touch_exactly_their_own_pixels`, que monta un
  `Framebuffer` sobre RAM (misma técnica que `MemDisk` para ext2) con
  **`stride` mayor que `width`** y el buffer relleno de `0xAA`, para que
  «sin tocar» sea algo que el test pueda afirmar de verdad.
* `scripts/boot-matrix.sh 4 2` — 8/8 OK.
* Los tres clientes raros de la consola, comprobados por captura de
  pantalla: `vi` (pantalla limpia, columna de `~`), `doom` y `quake`
  (ambos dibujan; la consola se recupera limpia al salir, vía
  `FB_RAW_DIRTY`).
