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

**Siguiente paso:** las dos juntas, en fases medidas por separado —
`docs/fb/wc-shadow-plan.md`.

## La medición en metal (2026-09-23)

`fbbench` (`userspace/c/fbbench.c`) en la Ryzen, kernel `dev`, 1920x1080:
**un `scroll_up` cuesta 2,18 s** (8 076 M ciclos, frente a 3,7 M en QEMU,
unas 2 000 veces más). `seq 1 400` tarda 875 s. Las escrituras van a 391
MB/s (`fill_rect`), pero leer la VRAM va a ~4 MB/s. La predicción de
arriba («en metal la mitad de lectura será mucho peor») se quedó corta
por tres órdenes de magnitud. La apertura está por encima de 4 GiB, es UC
por el tipo MTRR por defecto y no tiene alias en el physmap.

Consecuencia: el shadow buffer pasa por delante del WC. Tablas completas y
plan en `docs/fb/wc-shadow-plan.md`.

## Shadow buffer en metal (2026-09-23, arranque #4)

Mismo `fbbench`, mismo perfil `dev`, misma Ryzen; solo cambia la fase 1
de `docs/fb/wc-shadow-plan.md` (`shadow: attached (8640 KiB)`):

| carga | antes | shadow | factor |
|---|---|---|---|
| S `seq 1 400` | 875 s | **8,18 s** | 107x |
| A 400 x 80 | 874 s | **8,30 s** | 105x |
| A1 un `write()` | 872 s | **0,32 s** | 2 700x |
| B backspace | 2,3 s | **68 ms** | 34x |
| C 30 blits | 37 s | **1,26 s** | 29x |

Todo lo que ahora cuesta es **escribir** la VRAM: `fb_flush` a 412 MB/s,
la misma tasa que el `memset` de `fill_rect` antes (391), en UC. En S el
volcado es el 98 % del tiempo (29 694 de ~30 300 M ciclos). Lo que antes
tocaba la VRAM y ahora va a RAM: `scroll_up` de 2,18 s a 0,35 ms,
`draw_char` de 18 k a 9 k ciclos y el cursor de 1,1 M a 4,4 k ciclos.
C no llegó al «< 1 s» previsto: el volcado cumplió (16 ms por frame), pero
`blit_scaled` en RAM sigue costando 26 ms por frame porque escribe píxel a
píxel en perfil `dev` (fase 4 del plan). Siguiente palanca: el WC (fases
2-3), que ataca directamente esos 412 MB/s.

## Write-combining en metal (2026-09-23, arranque #7)

Fases 2-3 de `wc-shadow-plan.md`: entrada 1 del PAT en WC y las PTEs de la
apertura apuntando a ella (`pte_cache_bits: ... PWT=1 -> pat_index=1`,
MTRR UC). Mismo perfil `dev`, mismo `fbbench`, comparado con el arranque #4
(solo shadow):

| carga | shadow (UC) | shadow + WC | |
|---|---|---|---|
| S `seq 1 400` | 8,18 s | **0,754 s** | 10,8x |
| A 400 x 80 | 8,30 s | **0,862 s** | 9,6x |
| A1 un `write()` | 0,32 s | 0,302 s | sin cambio apreciable |
| B backspace | 68 ms | **35 ms** | 1,9x |
| C 30 blits | 1,26 s | **0,818 s** | 1,5x |
| `fb_flush` | 412 MB/s | **5 486-6 038 MB/s** | ~13,5x |

* Un volcado de pantalla completa (8,3 MB) pasa de 75 M ciclos (20 ms) a
  5,4 M (1,5 ms). Es lo que cuesta cada línea nueva con la pantalla llena.
* Lo que no tocaba la VRAM no cambió, como se esperaba: `draw_char` ~9 k
  ciclos, `scroll_up` en RAM 1,28 M. A1 apenas se mueve porque hacía un
  solo volcado.
* **Qué domina ahora.** En S, `fb_flush` sigue siendo el 80 % (2 237 de
  ~2 790 M ciclos) y `render_bytes` (casi todo `scroll_up` en RAM) el
  19 %. En C, `blit_scaled` en RAM es el 96 %: 96,7 M ciclos (26 ms) por
  frame, sin cambio. C cumple ya el «< 1 s» del plan, pero por el volcado;
  `blit_scaled` píxel a píxel sigue siendo la entrada de la fase 4.

De 875 s (línea base) a 0,75 s para `seq 1 400`: unas 1 160 veces.

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
