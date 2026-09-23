# Plan: framebuffer rápido en bare metal (shadow buffer + write-combining)

> **Estado (2026-09-23):** fases 0 y 1 **hechas y medidas en la Ryzen**.
> El shadow bajó S de 875 s a 8,2 s y A1 a 0,32 s (resultados abajo, en
> la fase 1). Siguen las fases 2-3 (PAT + mapeo WC). Las fases 2-3 (PAT + mapeo WC) van después.
> El orden cambió respecto a la primera versión de este plan: la medición
> en metal dijo que el shadow va primero (ver «Por qué este orden»).
>
> Continúa `docs/fb/console-perf.md`, que midió el problema en QEMU e hizo
> el paso de `fill_rect` por scanline.

## Línea base en metal (fase 0)

Ryzen AM4, kernel perfil **`dev`**, 1920x1080, `stride` 2048 px, 4 B/px
(apertura de 8 640 KiB), TSC 3,70 GHz. `fbbench` completo, leído de la
partición de log del pendrive (arranque #3, 2026-09-23 15:37):

| carga | pared | lo que domina |
|---|---|---|
| **S** `seq 1 400` (400 scrolls) | **875 s** | `scroll_up` 99,97 % |
| **A** 400 líneas x 80 col (400 scrolls, 32 000 glifos) | 874 s | `scroll_up` 99,9 % |
| **A1** lo mismo en un `write()` | 872 s | `scroll_up` |
| **B** 100x línea + `\b` + `ESC[J` | 2,3 s | un scroll (2,2 s); sin él, ~0,3 ms por tecla |
| **C** 30 `FBIO_BLIT` 320x200 | 37 s | `blit_scaled`, 1,24 s por frame |

Coste por operación en metal (y en QEMU, que no sirve para medir):

| operación | ciclos/llamada metal | MB/s metal | ciclos/llamada QEMU |
|---|---|---|---|
| `scroll_up` | **8 076 M (2,18 s)** | **8** | 3,7 M |
| `blit_scaled` 320x200 → 1280x800 | 4 593 M (1,24 s) | 5 | 1 100 M |
| `fill_rect` (memset por scanline) | 0,43 M mín. | **391** | — |
| `draw_char` | 18 k | 51 | 145 k |
| `xor_rect` (cursor, 8x9) | 1,1 M (0,3 ms) | — | 65 k |

**Qué dicen los números:**

* **Leer la VRAM es el problema, no escribirla.** Un scroll lee 8,6 MB y
  tarda 2,18 s: ~4 MB/s de lectura, frente a 391 MB/s de escritura con
  `fill_rect`. Cada línea nueva con la pantalla llena, es decir cada Enter
  en el shell, cuesta 2,2 s. Eso es lo que se nota en la máquina.
* **Las escrituras cuestan por transacción.** `fill_rect`, `draw_char` y
  `blit_scaled` escriben lo mismo (píxeles en UC) a 391, 51 y 5 MB/s. Cuanto
  menor es la unidad de store (memset > un span de 32 B > un store de
  3 bytes por píxel), más lento. `blit_scaled` escribe píxel a píxel.
* **Backspace ya no es el problema:** 0,15 ms por `ESC[J` tras el paso de
  `fill_rect`. El «un segundo al borrar» original era esto más el scroll.
* **QEMU sirve para la corrección y no para el rendimiento.** Su `scroll_up`
  es 2 000 veces más barato que el de metal.

## Hechos de la máquina (de `/proc/fbinfo` en metal)

* Apertura en **`phys 0x7c00000000`**, por encima de 4 GiB. Ningún MTRR
  variable la cubre y el tipo por defecto (`def_type 0xc00`) es **UC**. Los
  cuatro MTRR válidos son WB bajo 4 GiB más un UC en `0xcac10000`.
* La PTE del framebuffer es de 4K con índice PAT 0, y el PAT de reset es
  `0x0007040600070406`: **ninguna entrada WC**.
* **El physmap del bootloader no cubre la apertura**
  (`physmap_alias_first/last: not mapped`). En esta máquina no hay alias
  WB, así que el riesgo principal de la fase 3 no existe. En QEMU sí existe
  un alias, pero con MTRR UC, que es el caso benigno.
* **MTRR UC + PAT WC da WC efectivo** (SDM vol. 3A, tabla 11-7). Con el PAT
  reprogramado y la PTE apuntando a la entrada WC basta; no hay que tocar
  MTRRs.
* `instrument_overhead` es de 37 ciclos: el instrumento no contamina nada
  de lo medido.

## Por qué este orden

La versión anterior ponía el WC primero. Los números dicen otra cosa:

1. **El 99,9 % del tiempo son lecturas de VRAM**, y el WC no acelera
   lecturas. Tras las fases de WC, S seguiría tardando ~875 s.
2. **Cada medición en metal cuesta hoy 45 minutos** de `fbbench`. Con el
   shadow, S/A/A1 bajan a segundos y las fases de WC se pueden medir
   rápido.
3. El shadow no toca MSRs ni `CR0.CD`, que son la parte arriesgada sin
   serial.

## Fase 1: shadow buffer en RAM (hecha y medida en metal)

**Progreso:** implementado y verificado en QEMU (2026-09-23). Falta
desplegar y medir `fbbench` en la Ryzen (último punto).

* `hal::fbdirty::DirtyRect`, 7 tests de host.
* `Framebuffer` en modo shadow, `FB_FLUSH` (`fb_flush` en `/proc/fbinfo`
  y en `fbbench`), una línea `shadow:` en `/proc/fbinfo`, lotes en
  `render_bytes`, `kernel_write_bytes`, `kalert!` y `kernel_print`, y
  `attach_shadow()` en `init::boot` justo tras `test_allocators`.
* Test QEMU `framebuffer_shadow_mode_flushes_exactly_what_changed`
  (`hw_tests.rs`). Cubre los puntos (1) a (4) de abajo y además (5): un
  volcado copia solo su rectángulo, lo que se comprueba con un centinela
  plantado en la VRAM. Dos sabotajes, los dos detectados: sin el volcado
  de `end_batch` falla (2); con `touched` marcando la pantalla entera
  falla (5).
* Comprobado a ojo por captura: `vi` con cursor, `less`, DOOM (y la vuelta
  a la consola), un `kalert!` rojo y la pantalla de pánico.
* Modo directo forzado (reserva de 1 GiB inyectada): arranca, dice
  `framebuffer: no RAM shadow` y `/proc/fbinfo` da `shadow: none`.
* `run-kernel-tests.sh` PASS, `hal` 205/205, `mm` 35/35, `boot-matrix.sh
  4 3` 12/12 OK.

**Dos cosas que no estaban en el plan:**

1. **Alineación del shadow (`SHADOW_SKEW`).** El primer shadow volcaba a
   **248 MB/s** en QEMU, 17 veces menos por byte que un `scroll_up`
   directo, que es un `memmove` de VRAM a VRAM del mismo tamaño. El código
   era el mismo (`rep movsq` de `compiler_builtins` en los dos casos). La
   causa es que el bloque del buddy está alineado a su tamaño (4 MiB) y
   la VRAM también, así que el byte `i` del origen y el del destino caían
   en la misma entrada del TLB software de QEMU (mapeo directo por número
   de página) y cada `movsq` expulsaba la del otro lado. Con el shadow
   desplazado `0x2840` bytes: **2 100 MB/s**, mismo build y mismo
   `fbbench`. En metal el análogo es el *4K aliasing*, así que el
   desplazamiento no es un número entero de páginas.
2. **El buddy entraba en pánico con un orden mayor que `MAX_ORDER`** (un
   `debug_assert!`; en release, un índice fuera de rango) en vez de
   devolver `None`, lo que rompía el «best-effort» de `attach_shadow` para
   una petición enorme. Arreglado en `mm::buddy::allocate`, con test de
   host y sabotaje. Además `attach_shadow` reserva ya sin el lock de
   `FRAMEBUFFER`: con el lock tomado, un pánico en el allocator se saltaba
   la pantalla de pánico.

**QEMU, A/B con el mismo `fbbench` (perfil `dev`, 1280x800).** No mide
rendimiento de metal: aquí la VRAM es RAM del host, así que el shadow solo
añade un volcado y todo sale algo **más lento**. Se anota para que nadie
lo lea como una regresión:

| carga | directo | shadow sin skew | shadow |
|---|---|---|---|
| S `seq 1 400` | 455 ms | 7 134 ms | 1 276 ms |
| A 400 x 80 | 1 461 ms | 8 113 ms | 2 285 ms |
| A1 un `write()` | 1 432 ms | 1 409 ms | 1 412 ms |
| B backspace | 249 ms | 276 ms | 250 ms |
| C 30 blits | 6 567 ms | 6 901 ms | 6 866 ms |

Un volcado de pantalla completa cuesta en QEMU 7,3 M ciclos (2 ms). C
sigue dominado por `blit_scaled` píxel a píxel en perfil `dev` (fase 4).
`fb_render_bytes` ya **no incluye** el volcado, que ocurre en `end_batch`,
después de medir; `fb_flush` aparece aparte.

**Resultado en metal** (arranque #4, 2026-09-23 16:32, mismo perfil
`dev`, `shadow: attached (8640 KiB)`):

| carga | antes | predicho | medido |
|---|---|---|---|
| S | 875 s | ~8,5 s | **8,18 s** |
| A | 874 s | ~8,5 s | **8,30 s** |
| A1 | 872 s | < 1 s | **0,32 s** |
| B | 2,3 s | sin cambio apreciable | **68 ms** |
| C | 37 s | < 1 s | **1,26 s** (fallado) |

* `fb_flush`: **412 MB/s**, 75 M ciclos (20 ms) por pantalla completa. La
  predicción era 391 MB/s y ~21 ms, y se cumplió. El `memcpy` por filas
  no escribe peor que el `memset`.
* S y A: el volcado es el 98 % del tiempo. `scroll_up` en RAM cuesta
  1,28 M ciclos (0,35 ms) frente a los 8 076 M de antes.
* B mejoró más de lo previsto porque el cursor (`xor_rect`) ya no lee
  VRAM: de 1,1 M a 4,4 k ciclos por parpadeo.
* **C falla el criterio «< 1 s»:** el volcado cumple (59 M ciclos, 16 ms
  por frame), pero `blit_scaled` cuesta 96 M ciclos (26 ms) por frame en
  RAM porque escribe píxel a píxel en perfil `dev`. Es la entrada de la
  fase 4. El criterio de terminado 2 queda cumplido para S/A y no para C.

**Modelo:** con el shadow enganchado, todas las primitivas dibujan en RAM
WB y marcan un rectángulo sucio. `flush()` copia ese rectángulo a la VRAM
fila a fila (solo el ancho visible, nunca el relleno de `stride`).
**La VRAM no se lee nunca.** `scroll_up` pasa a ser un `memmove` en RAM y
`xor_rect` lee del shadow.

**Correcto por defecto, rápido donde importa:**

* Fuera de un lote, **cada primitiva vuelca su propio rectángulo al
  terminar**. Así cualquier llamante que no sepa del shadow (`panic.rs`,
  `draw_boot_screen`, `FBIO_BLIT`, el cursor desde la ISR) sigue viendo
  la pantalla al día sin cambiar nada. Olvidarse de un sitio cuesta
  velocidad, no una pantalla congelada.
* **`render_bytes` agrupa**: `begin_batch()` al entrar y `end_batch()` al
  salir (con contador de profundidad, porque `kernel_write_bytes` anida
  llamadas). Un `write()` de 400 líneas hace 400 `memmove` en RAM y **un**
  volcado.
* Sin shadow (reserva fallida o antes de enganchar) todo funciona como hoy,
  directo a la VRAM.

**Piezas:**

* **`hal::fbdirty::DirtyRect`** (pura, con tests de host): rectángulo
  `[x0,x1) x [y0,y1)` con `mark` (unión), `take` y recorte a la pantalla.
  Es un rectángulo y no un rango de filas porque el cursor ensucia 8x9 px:
  volcar sus 9 filas enteras serían 69 KB en UC por parpadeo.
* **`Framebuffer`** gana `shadow: Option<NonNull<u8>>`, `dirty` y
  `batch_depth`. Las primitivas escriben en `draw_buffer()` (el shadow si
  existe, si no la VRAM) y llaman a `touched(rect)`.
* **`attach_shadow()`**, llamado justo después de `memory::init_core`,
  antes de `draw_boot_screen`. Reserva `byte_len()` con
  `alloc_zeroed` (un `vec!` fallido sería un pánico), así que un fallo deja la consola en modo
  directo en lugar de provocar un pánico. El shadow empieza a ceros y la
  VRAM visible se pone en negro (un volcado de pantalla completa): **nunca
  se copia la VRAM al shadow**, porque esa lectura costaría 2,2 s en metal,
  y `draw_boot_screen` borra la pantalla de todos modos.
* **`diag::OpStat` `FB_FLUSH`** (`fb_flush` en `/proc/fbinfo` y en
  `fbbench`). Es el único sitio donde se escribe la VRAM en modo shadow, así
  que su MB/s es el ancho de banda de escritura real que las fases de WC
  tienen que mejorar.

**Predicción para metal** (la que hay que contrastar):

* Un volcado de pantalla completa son 1920x1080x4 = 8,3 MB a ~391 MB/s
  (lo que hace `fill_rect` hoy): **~21 ms**.
* **S y A**: 400 `write()` que hacen scroll, cada uno con un volcado
  completo: ~400 x 21 ms ≈ **8,5 s** (antes 875 s, unas 100 veces menos).
* **A1**: un solo volcado, **< 1 s**. La diferencia A/A1 mide el coste del
  volcado por `write()`.
* **C**: el blit va a RAM y el volcado es de 6,4 MB: ~16 ms por frame
  (antes 1,24 s).
* **B**: sin cambio apreciable (ya estaba en ~0,3 ms por tecla).
* Si `fb_flush` sale muy por debajo de 391 MB/s, el `memcpy` usa stores
  más pequeños que el `memset`. Eso se arregla en la fase de WC y no aquí.

**Tests:**

* host: `DirtyRect`;
* QEMU (`hw_tests`): la prueba de primitivas se repite en modo shadow
  sobre dos buffers de RAM, con la «VRAM» rellena de `0xAA`. Afirma que:
  (1) dentro de un lote la VRAM no cambia; (2) tras `end_batch` el área
  visible de la VRAM es idéntica al shadow; (3) el relleno de `stride`
  sigue en `0xAA`; (4) fuera de lote, cada primitiva deja la VRAM al día
  por sí sola;
* sabotaje: quitar el volcado de `end_batch` tiene que hacer fallar (2);
* visual: `vi`, `less`, `doom`, `quake`, el cursor, un `kalert!` y la
  pantalla de pánico (`kdebug panic`), por captura en QEMU;
* `boot-matrix.sh 4 3`, `run-kernel-tests.sh`, y `QEMU_DEBUG_MEM=128M`
  (con poca memoria la reserva puede fallar y hay que caer a modo directo).

## Fase 2: reprogramar `IA32_PAT` (sin cambio visible)

* `hal::memtype::pat_with_entry(pat_msr, index, MemType) -> u64`, pura y
  con tests: la entrada 1 del reset pasa a WC, las otras siete no cambian,
  el resultado es el layout de Linux (`WB WC UC- UC WB WP UC- WT`) y se
  rechazan los tipos reservados (2, 3).
* `memory::memtype::program_pat()`, con la secuencia del SDM §11.12.4 e
  IF=0 de punta a punta: `CR0.CD=1, NW=0` → `wbinvd` → vaciar TLB incluidas
  las globales (conmutar `CR4.PGE`) → `wrmsr` → `wbinvd` → vaciar TLB →
  `CR0.CD=0`. Se llama una vez tras `init_core` y antes del xHCI.
* **Índice 1 (PWT=1, PCD=0, PAT=0)**: está libre (`mmio.rs` usa PWT|PCD, el
  índice 3) y no depende de dónde esté el bit PAT, que cambia con el tamaño
  de página. Antes de reprogramar, un recorrido de las tablas del kernel
  **busca entradas con PWT=1 y PCD=0; si encuentra alguna no reprograma**
  y lo deja en el log.
* Verificación: `pat_has_wc: true` en `/proc/fbinfo`, `boot-matrix`
  limpio, tests en verde y un arranque en metal hasta el shell. No se
  espera ningún cambio de rendimiento; si aparece, desconfiar del
  instrumento.
* SMP (futuro): el PAT tiene que ser idéntico en todas las CPUs, así que
  las APs también llamarán a `program_pat()`.

## Fase 3: mapear el framebuffer como WC

* Recorrer `virt_addr()..+byte_len()` (páginas de 4K en ambas máquinas,
  pero el código acepta hojas grandes): PWT=1, PCD=0, bit PAT sin tocar y
  `invlpg`. Corre en el arranque, antes de que exista ningún proceso, y
  `new_user` comparte las tablas inferiores del kernel, así que todo
  proceso posterior hereda el cambio. `memory::memtype::leaf_for` ya
  devuelve la entrada cruda que hay que modificar.
* Alias del physmap: en la Ryzen **no existe**. En QEMU existe con MTRR UC,
  y UC+WC sobre el mismo marco es la combinación que el SDM tolera. Si una
  máquina futura tuviera un alias WB (MTRR WB), habría que quitarlo con
  `split_physmap_2m` y marcar esas PTEs como no presentes.
* `sfence` al final de `flush()` y de cualquier escritura directa, para que
  `OpStat` mida la salida de los datos y no solo el final de los stores.
* **Predicción:** `fb_flush` pasa de ~391 MB/s a GB/s, y con él S/A (un
  volcado por `write()`) y C. `draw_char` y `xor_rect` ya no tocan la VRAM
  con el shadow puesto, así que no deberían cambiar.

## Fase 4, solo si los números la piden

* Volcado por columnas o por bitset de filas, si `fb_flush` domina B.
* Volcado diferido al tick del timer (añade hasta 10 ms de latencia de
  eco).
* Stores no temporales (`movntdq`) en `flush()`.
* `blit_scaled` componiendo filas enteras en lugar de píxel a píxel (en
  RAM ya no importa tanto; se decide con el número de C tras la fase 1).

## Cómo medir y desplegar

* **Mismo perfil en todas las fases.** La línea base es `dev`, y un cambio
  de perfil movería los números más que cualquier fase.
* Compilar: `cargo build` (`fbbench` está en `DISK_C_PROGRAMS`).
* Desplegar al pendrive (sdX con labels `boot`, `constanos-data`,
  `constanos-log`):
  1. `scripts/sync-usb-data.sh`, que copia `disk-image-root/` a la
     partición de datos.
  2. Kernel: copiar solo la FAT de
     `target/debug/build/so2-*/out/uefi.img` (la más reciente) con
     `sudo dd if=<img> of=/dev/disk/by-partlabel/boot bs=512 skip=34
     count=34816 conv=fsync`, y comprobar con sha256 contra la imagen.
     **Nunca `dd` al disco entero.**
* En la Ryzen: `fbbench; cat /proc/fbinfo; kdebug sync`. Después, en su
  Linux: `scripts/usb-log.sh list` y `scripts/usb-log.sh read` (el último
  arranque). El ring de 64 KiB se desborda con las 800 líneas de A/A1, pero
  el informe de `fbbench` se imprime al final y sobrevive.
* En QEMU: `QEMU_DEBUG_STATE_DIR` corto (el socket unix admite menos de
  108 bytes) y una copia de `disk.img` en `QEMU_DEBUG_DISK_IMG`.

## Criterios de terminado

1. ~~Fase 0: línea base en metal anotada~~ (hecho, arriba y en
   `console-perf.md`).
2. Fase 1: S y A bajan en metal unas 100 veces (de 875 s a ~10 s), C baja
   de 37 s a menos de 1 s, y ninguna pantalla se queda sin actualizar
   (clientes raros comprobados a ojo). **Medido: S/A 107x (cumplido); C
   1,26 s (no cumplido, falta `blit_scaled`, fase 4).**
3. Fases 2-3: `pat_has_wc: true`, PTE del framebuffer en WC y `fb_flush`
   más rápido en metal, con número.
4. En todas: `boot-matrix.sh 4 3` limpio, `run-kernel-tests.sh` PASS, tests
   de `hal` en verde, y `console-perf.md` actualizado con los números
   antes/después, incluidos los que no mejoraron.

## Riesgos

* **Un camino que escriba la VRAM sin pasar por las primitivas** dejaría el
  shadow desincronizado. Hoy no existe (todo va por `Framebuffer`); cualquier
  acceso crudo nuevo tiene que marcarse como sucio o invalidar el shadow.
* **Pánico a mitad de un lote:** el handler hace `try_lock` sobre
  `FRAMEBUFFER`. Si lo tiene el código que entró en pánico, se salta la
  pantalla, como hoy. Si no, `clear` + texto vuelcan por sí solos, porque
  un pánico no está dentro de un lote.
* **Memoria:** el shadow ocupa 8,6 MB en la Ryzen y 4 MB en QEMU. Si la
  reserva falla, la consola sigue en modo directo.
* **`CR0.CD` con interrupciones activas** congela la máquina sin dejar
  rastro: `program_pat()` va con IF=0 de punta a punta y se prueba primero
  en QEMU.
* **La GUI (paso 4 del roadmap):** el compositor será otro cliente de este
  camino (un `FBIO_BLIT` con rectángulo de daño). El `mmap` de `/dev/fb`
  con WC queda fuera de este plan y reutilizaría el índice PAT de la
  fase 2.

## Fuera de alcance

Page flipping, aceleración por GPU, cambios de modo y el `mmap` de
`/dev/fb`.
