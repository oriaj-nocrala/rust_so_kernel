# Plan: texto de verdad en las ventanas (`parley` + `swash`)

> **Estado (2026-09-26):** fase 0 hecha (`d472a3e`: el userspace de Rust
> tiene SSE2). Fases 1-3 hechas: crate `text/` (18 tests en el host),
> fuentes en `disk.img`, `userspace::text` con respaldo bitmap, y
> `textdemo` verificado en QEMU por `scripts/gui-e2e.sh text`. Siguiente:
> fase 4 (metal).

## Por qué, y por qué antes que la librería GUI

La idea era escribir otra aplicación gráfica y extraer de ella y de
`cpumon`/`snake`/`term` lo que tienen en común. Lo primero que salta a la
vista es el texto, y la fuente que hay no da más de sí
(`draw/src/smooth.rs`, sobre `noto-sans-mono-bitmap`):

- **4 tamaños fijos** (16/20/24/32 px), sin nada en medio.
- **Solo monoespaciada**: no sirve para una interfaz con etiquetas, menús o
  botones.
- **Solo Basic Latin**: `á`, `ñ`, `¿` salen como `?`.

El motor va **antes** de la librería GUI porque los tamaños de los widgets
salen de las métricas del texto (alto de línea, ancho de una etiqueta,
dónde se corta). Una librería diseñada alrededor de celdas fijas habría que
rehacerla.

**Qué no cambia:** la consola del kernel y `vt` (el terminal con ventana)
se quedan con la fuente bitmap. Funcionan, son monoespaciados por
naturaleza, y el kernel no tiene por qué cargar fuentes TrueType.

## La elección: no escribir el motor, usar el de Linebender

La primera versión de este plan proponía escribir un rasterizador y un
layout propios sobre `ttf-parser`. Una investigación del 2026-09-26 lo
descartó: el stack moderno de Rust hace todo eso, está mantenido, y
**funciona sin `std`** (`alloc` + `libm`). No es una suposición: las
cuatro candidatas se compilaron para `userspace/x86_64-constanos.json` y se
ejecutaron en constanos (QEMU TCG, programas enlazados con el crate
`userspace`, Noto Sans Regular embebida):

| Librería | Qué hace | Código (sin fuente) | En constanos |
|---|---|---|---|
| `fontdue` 0.9 | parseo + raster + layout simple | ~96 KB | 104 glifos a 24 px: 4 ms (pero 462 ms de parseo: precalcula todos los glifos) |
| `swash` 0.2.10 (`skrifa` + `zeno`) | parseo + raster de calidad, hinting | ~594 KB | 104 glifos: 13 ms; cobertura casi idéntica a `fontdue` (1 688 589 frente a 1 688 470) |
| `parley` 0.11.1 | layout: shaping `harfrust`, cortes de línea ICU4X, bidi; no rasteriza | ~717 KB | `¿Él pidió ñandú y kiwi? ¡Sí! AVATAR Wave To` en 3 líneas de ≤200 px: 26 ms |
| `cosmic-text` 0.19 | todo, con editor | ~1,35 MB | 3 líneas dibujadas: 67 ms |

**Elegido: `parley` + `swash`.** Es el stack de referencia (Xilem/Vello;
egui se está pasando a `parley`), resuelve el kerning GPOS de Noto que
`fontdue` ignora, trae el shaping y los cortes de línea correctos, y
`parley` ya tiene lo que la librería GUI necesitará después: cursor,
selección, hit-testing (`PlainEditor`). Las dos comparten **una sola**
`skrifa` (0.44), así que el tamaño combinado es menor que la suma.
`cosmic-text` queda descartada por tamaño y porque impone su propio modelo
de `Buffer`; `fontdue`, por no tener shaping ni kerning GPOS.

**Coste aceptado: ~1,2 MB por programa con texto**, al no haber enlace
dinámico. Por eso los programas nuevos con texto van **al disco**
(`/mnt/bin`, `DISK_*`), no embebidos en el kernel.

Features usadas en la prueba, que funcionaron tal cual:

```toml
parley = { version = "0.11", default-features = false, features = ["libm"] }
swash  = { version = "0.2",  default-features = false, features = ["scale", "render", "libm"] }
```

Con `default-features = false`, `parley` no busca fuentes del sistema
(`system`): se le registran a mano (abajo).

## Qué escribimos nosotros: el crate `text/`

Una capa fina, testeable en el host, con la forma de `draw`/`vt`/`gui`:
su propio workspace (tabla `[workspace]` vacía), `no_std` + `alloc`,
dependencia por `path` de `userspace`, vigilado por el `build.rs` raíz. Sin
syscalls: recibe los bytes de las fuentes y un `&mut [u32]`.

- **Registro de fuentes**: `FontContext` de `parley` con las fuentes
  registradas a mano (`collection.register_fonts(Blob::new(Arc::new(bytes)),
  None)` devuelve la familia; se elige con
  `StyleProperty::FontFamily(FontFamily::Source(nombre))`).
- **Layout**: `LayoutContext::ranged_builder(...)` → `push_default(...)`
  (tamaño, familia, peso) → `build(texto)` → `break_all_lines(Some(ancho))`
  → alinear. Una API nuestra, pequeña, encima: `measure`, `layout`.
- **Rasterizado**: por cada glifo posicionado del layout, `swash`
  (`ScaleContext` → `scaler` a ese tamaño → `Render::new(&[Source::Outline])`
  con desplazamiento subpíxel) da una máscara de cobertura y su `placement`.
- **Caché de glifos (nuestra)**: clave (fuente, glyph id, tamaño, fase
  subpíxel); memoria acotada con desalojo CLOCK como `hal::blockcache`;
  sin estado global, la posee el programa.
- **Composición**: mezclar la cobertura sobre un `draw::Canvas` con clip
  (la aritmética de `Framebuffer::draw_glyph`).

## Fases

### Fase 1 — `text/` en el host

- Crate con `parley` + `swash` compilando en el host (tests) y para el
  target del userspace.
- **Fuentes**: `scripts/fetch-fonts.sh` baja Noto Sans y Noto Sans Mono,
  Regular y Bold (~2,4 MB, licencia OFL al lado) de una release fijada de
  notofonts, verificadas por sha256, a `disk-image-root/usr/share/fonts/`
  (el patrón de `fetch-freedoom.sh`). Los tests las leen de ahí y, si
  faltan, **fallan diciendo qué script ejecutar**: un test que se salta en
  silencio no prueba nada.
- **Tests:** `measure` coincide con la caja de lo que se dibuja; el corte
  de línea respeta el ancho (una palabra más larga que la línea, espacios
  seguidos, texto vacío, `\n`); `AV`/`To` más estrechos que la suma de
  avances sin kerning (el GPOS funciona); `ñ`/`á`/`¿` no son `.notdef`; la
  caché respeta su tope y un glifo desalojado se vuelve a rasterizar
  idéntico; composición sobre un `Canvas` con clip: nada escrito fuera del
  rectángulo (el truco del relleno `0xAA` de los tests del framebuffer).

### Fase 2 — fuentes en el disco

- Generalizar `sync_disk_terminfo_dir` del `build.rs` raíz para que también
  lleve `usr/share/fonts` a `disk.img`.
- Comprobar que `scripts/sync-usb-data.sh` las copia al pendrive.
- `userspace::text`: leer `/mnt/usr/share/fonts/*.ttf` al heap y construir
  el contexto de `text/`. **Sin fuentes, el programa sigue**: cae a la
  fuente bitmap de `draw`.

### Fase 3 — en pantalla: `textdemo`

- Programa nuevo, en el disco (no embebido), en ventana del compositor:
  tamaños de 10 a 72 px, `¿Él pidió ñandú y kiwi? ¡Sí!`, proporcional
  frente a monoespaciada, regular frente a negrita, un párrafo cortado a un
  ancho, y una regla que marca el ancho que dice `measure`.
- **Verificación:** `scripts/gui-e2e.sh text` (modo nuevo): screendump y
  comprobación de píxeles donde `measure` dice que empieza y acaba cada
  línea. Tiempos (arranque, primer cuadro, Latin-1 completo a 24 px) y
  tasa de aciertos de la caché en el log.

### Fase 4 — metal

- Fuentes al pendrive y un job de `metal-run.sh` con los tiempos de la
  fase 3 en consola (el job no tiene pantalla que leer). En QEMU TCG el
  parseo y el shaping son lentos; los números que valen son los de la
  Ryzen.
- `textdemo` en la Ryzen a mano, con foto de la pantalla.

### Después (otros planes)

- Extraer la librería GUI de `cpumon`/`snake`/`term`/`textdemo` y la
  aplicación nueva, sobre `text/`. `cpumon` es el primer candidato a pasarse
  al motor nuevo.
- `std` para Rust en constanos: está cerca (memoria `rust-std-gaps`) y no
  bloquea nada de esto.
- **Fuentes del sistema, como en Linux.** Allí el kernel no sabe nada de
  fuentes: son archivos en `/usr/share/fonts`, fontconfig (una librería,
  no un servicio) responde "`sans-serif`, negrita, con `ñ`" → archivo, y
  cada aplicación lo abre con `mmap` y lo rasteriza (FreeType + HarfBuzz).
  Lo "gigante" es política (XML de configuración, cadenas de respaldo
  CJK/emoji, hinting por usuario, cada toolkit a su manera), no mecanismo.
  Este plan ya es ese modelo en pequeño: archivos en
  `/mnt/usr/share/fonts`, un índice mínimo en `text/`, `swash`/`parley`
  como FreeType/HarfBuzz. Dos pasos después:
  - **Descubrimiento:** `fontique` (el de `parley`) hace descubrimiento de
    fuentes del sistema, familias y respaldo, pero detrás de su feature
    `system`, que pide `std`. Con `std` en constanos, evaluarlo (y ver qué
    lee exactamente en Linux: su propia configuración o fontconfig).
  - **Compartir la memoria:** en Linux diez programas con Noto Sans
    comparten las mismas páginas (el `mmap` de archivo pasa por la page
    cache). Aquí `mmap` solo es anónimo o de `memfd`, así que cada programa
    lee ~600 KB por fuente a su heap. Dos caminos: **`mmap` de archivos**
    de solo lectura con páginas compartidas (más general: también binarios
    y archivos grandes; necesita algo tipo page cache sobre ext2, hoy solo
    hay la caché de bloques dentro del driver), o **un servicio de
    fuentes** que las carga una vez en `memfd`s y las reparte por
    `SCM_RIGHTS` (piezas que ya existen, las del compositor; Android y el
    sandbox de Chrome hacen algo así). Decidir **midiendo** cuando haya
    varias aplicaciones con texto abiertas a la vez.

## Cómo retomar con el contexto limpio

1. Leer este fichero y las memorias `text-engine-plan` y
   `userspace-sse-target`.
2. Ficheros de referencia: `draw/` (forma de crate a imitar, y
   `smooth.rs`, lo que se reemplaza), `userspace/Cargo.toml` (cómo se
   enlazan `draw`/`vt`/`gui`), `build.rs` raíz (`sync_disk_terminfo_dir`,
   `watch_dir_recursive`), `scripts/fetch-freedoom.sh` (patrón de
   descarga), `scripts/gui-e2e.sh` (patrón del test end-to-end).
3. Las APIs exactas de `parley` 0.11 y `swash` 0.2 que funcionaron están
   resumidas en "Qué escribimos nosotros"; ojo: la API de estos crates
   cambia entre versiones (`FontStack` pasó a `FontFamily`, por ejemplo),
   así que ante la duda leer el código en `~/.cargo/registry/src/`.
4. Los crates extraídos **no los compila el `cargo build` raíz** (ver
   CLAUDE.md): verificar `text/` con `cd text && cargo test`, y el
   userspace con `cd userspace && cargo build --release`.

## Registro

- **2026-09-26 — fase 0:** target `userspace/x86_64-constanos.json` con
  SSE2; marco de señal con FXSAVE; `sse_test`. Verificado en QEMU y en la
  Ryzen (boot #58). Commit `d472a3e`.
- **2026-09-26 — elección de librerías:** `fontdue`, `swash`, `parley` y
  `cosmic-text` compiladas para el target del userspace y ejecutadas en
  constanos (tabla de arriba). El plan anterior (rasterizador y layout
  propios sobre `ttf-parser`) quedó descartado.
- **2026-09-26 — fase 1:** crate `text/` (`Fonts`, `Style`, `TextLayout`,
  `GlyphCache`; `cache::ClockCache` genérica y testeada aparte) y
  `scripts/fetch-fonts.sh` (Noto Sans v2.015 y Noto Sans Mono v2.014 de
  notofonts/latin-greek-cyrillic, las `unhinted`, zip verificado por
  sha256; 1,6 MB). `cd text && cargo test`: 6 unitarios + 12 contra las
  fuentes, todos los del plan. Cada uno probado por sabotaje: quitar
  `OverflowWrap::Anywhere`, medir con los espacios finales, una clave de
  caché sin la fase subpíxel, una caché sin desalojo, la máscara desplazada
  3 px y la negrita ignorada hacen fallar al menos un test cada uno.
  Medido de paso: el GPOS de Noto acerca `AV` 1,6 px, `To` 2,8 y `Va` 0,8
  a 40 px. Decisiones: posiciones horizontales a **cuarto de píxel** (4
  máscaras por glifo y tamaño como mucho), verticales enteras (`parley`
  cuantiza la línea base); una palabra más larga que la línea se corta
  dentro (`OverflowWrap::Anywhere`); los espacios seguidos se conservan;
  `ScaleContext::builder_with_id` con `Blob::id()` para que `swash` no
  reanalice la fuente en cada ejecución. Compila `no_std` para
  `x86_64-constanos.json`; en el userspace entra en la fase 2.
- **2026-09-26 — fase 2:** el `sync_disk_terminfo_dir` del `build.rs` raíz
  pasó a ser `sync_disk_tree(disk, rel)`, recursivo y verificando el tamaño
  de **cada** archivo (el de terminfo solo miraba el último directorio);
  lo usan terminfo y `usr/share/fonts`. `ensure_fonts()` ejecuta
  `fetch-fonts.sh` en cada build si faltan (sin red, avisa y sigue).
  `sync-usb-data.sh` hace `rsync` de todo `disk-image-root/`: las fuentes
  van al pendrive sin tocarlo. `userspace::text::Text::load()` lee las
  cuatro al heap; sin ninguna, `draw`/`measure` usan la Noto Mono bitmap de
  `draw::smooth` al tamaño más cercano, con el mismo corte de línea (voraz,
  por celdas) — verificado en QEMU con una copia de `disk.img` sin fuentes.
  Los binarios que no usan `text` no cambian de tamaño (LTO).
- **2026-09-26 — fase 3:** `textdemo` (`DISK_RUST_PROGRAMS`, nuevo en
  `kernel/build.rs`: programas Rust que van a `/mnt/bin`); **1,66 MB**,
  no los ~1,2 MB previstos. Se lanza con `compositor /mnt/bin/textdemo`
  (el compositor solo busca en `/bin`). `scripts/gui-e2e.sh text`: 18 cajas
  de `measure` contra la tinta del screendump — tinta en el borde izquierdo
  y derecho de cada caja y ninguna en los 10 px a su derecha; el margen de
  los bordes es un décimo del alto de línea, porque el *side bearing* del
  último glifo crece con el tamaño (la primera versión, con 3 px fijos,
  falló en `!`, `ñ` y `l` a 32-72 px). Probado por sabotaje: `measure`
  12 px corto hace fallar las 18. Tiempos en QEMU (TCG, 4 CPUs): fuentes
  276 ms, primer cuadro 77 ms (532 glifos rasterizados, 96 KiB de caché),
  Latin-1 completo a 24 px 10-11 ms en frío y 2 ms en caliente.
  `gui-e2e.sh term` sigue en PASS con el terminfo sincronizado por la
  función nueva.
