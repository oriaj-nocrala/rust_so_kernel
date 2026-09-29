# Plan de rendimiento gráfico (pendiente)

Anotado el 2026-09-26, tras meter AVX (XSAVE en el kernel, `draw::blend`
AVX2) y los iconos PNG. Nada de esto está hecho: es la lista para después,
en el orden en que conviene atacarla. Regla de todo el plan: **medir en la
Ryzen antes y después** (`metal-run`), no en QEMU, porque TCG emula AVX y
no tiene la memoria write-combining.

## Lo que hay medido

- `gui_demo` bajo el compositor: **50 fps exactos** en todas las corridas
  en la Ryzen (pantalla 1920x1080, stride 2048).
- `fb_flush` (shadow → VRAM WC): ~5,6 GB/s; una pantalla completa (8,3 MB)
  son ~1,5 ms, máximo observado ~2 ms.
- `draw::blend::over_row` en un frame 1280x800 translúcido: AVX2 162 µs,
  escalar 2,6 ms (Ryzen #63/#64).
- `textdemo`: primer frame completo 3 ms (Ryzen #59).

## 1. Subir de 50 fps: el tope es el timer, no la CPU

**Causa.** El compositor compone como mucho cada `FRAME_MS = 16` ms
(`userspace/src/bin/compositor.rs`) y espera con `epoll_wait(timeout)`.
Pero el tick del kernel va a `TIMER_HZ = 100` (`interrupts/apic.rs`) y los
hrtimers solo se revisan en ese tick (`time/hrtimer.rs`): todo timeout se
redondea a múltiplos de 10 ms, así que 16 ms se convierten en 20 → 50 fps.

**Presupuesto a 180 Hz (para cuando haya driver de display): 5,55 ms por
frame.** Con daño a pantalla completa:
componer (copiar ~8 MB en RAM, ~1 ms) + `fb_flush` (~1,5–2 ms) ≈ 3–3,5 ms
en un núcleo. Cabe, con poco margen; con daño parcial (lo normal: un
cursor, una ventana) sobra. Falta sumar lo que tarde cada cliente en
dibujar su frame, que corre en otros núcleos.

**Decidido (2026-09-26): LAPIC en one-shot para el siguiente hrtimer.**
Es lo limpio; subir `TIMER_HZ` a 1000 queda descartado (reparte más
interrupciones a todas las CPUs y obliga a dividir todo lo que hoy asume
100 Hz).

**Pasos:**
1. Kernel: cuando se arma un hrtimer que vence antes del próximo tick,
   programar el LAPIC timer de esa CPU en one-shot hasta ese vencimiento,
   y volver al periodo normal del tick después. Lo que cuelga del tick
   (parpadeo del cursor, sondeo USB, quantum, trabajo global en CPU 0)
   sigue a 100 Hz sin cambios. Primero comprobar si la Zen 3 anuncia
   TSC-deadline (CPUID.1:ECX[24]); si no, one-shot con la cuenta del LAPIC
   ya calibrada en `apic::init`. Invariantes de ISR, IF=0 y CPU 0 en
   `CLAUDE.md`; test de integración (un `epoll_wait` de 3 ms que tarda
   ~3 ms, no 10) y sabotaje.
2. Compositor: marcar el ritmo en ns (no en ms) y con el periodo del
   refresco real como objetivo.
3. Medir `compose_ms_total / frames` y `fb_flush` en la Ryzen.

**Límites que no se arreglan con CPU (confirmado 2026-09-26):**
- **La señal va a 60 Hz.** El menú del monitor dice 60 Hz con constanos
  arrancado: el modo UEFI GOP del firmware, no los 180 Hz del monitor.
  Sin driver de GPU no se puede cambiar el modo. Así que el objetivo
  alcanzable es **60 fps estables** (16,67 ms por frame, que hoy son 20 por
  el tick); 180 Hz exige un driver de display.
- **Sin vsync.** El GOP no da interrupción de vblank; sin driver de GPU no
  hay forma de sincronizar con el barrido, así que habrá tearing a
  cualquier tasa. Solo se arregla con un driver de display.

## 2. Flags de compilación antes que intrínsecos

Experimento barato y medible (`textdemo`, `imgview`, DOOM):

- **`opt-level`.** userspace compila con `opt-level = "s"`. La mezcla
  escalar es ~16x más lenta que AVX2 en la Ryzen pero ~6,5x en el host,
  lo que apunta (sin medir) a mal código de `"s"` en bucles de píxeles.
  Probar `opt-level = 3` solo para los crates de píxeles (`draw`, `vt`,
  `text`, `img`, swash) con `[profile.release.package.X]`.
- **CPU objetivo.** `-C target-cpu=znver3` para Rust (o `+avx2,+fma,+bmi2`
  en `userspace/x86_64-constanos.json`) y `-march=znver3` para C (DOOM,
  Quake, mlibc, BusyBox). LLVM autovectoriza con AVX2 swash, parley,
  `zune-png` (que hoy no usa su SSE4.1: sin `std` elige en compilación) y
  el remuestreo de `img`. Coste: los binarios exigen AVX2 y el XSAVE del
  kernel (ya existe); aceptado, no importan otras CPUs.

## 3. El terminal (`vt`)

Lo que más píxeles de texto mueve en el uso diario: un redibujo completo
son ~8000 celdas mezcladas píxel a píxel en escalar
(`vt/src/render.rs:109`). Primero el algoritmo: cachear cada glifo ya
mezclado por par (fg, bg), y redibujar pasa a ser copiar. AVX2 encima
aporta poco.

## 4. Texto proporcional (`text`)

Poco que ganar hoy (3 ms el primer frame completo; la caché evita volver a
rasterizar). La mezcla de máscaras (`text/src/render.rs:142`) va píxel a
píxel con comprobación de límites: una `draw::blend::mask_row(dst,
cobertura, color)` AVX2, hermana de `over_row`, es fácil, pero los glifos
son estrechos (10–30 px). El coste en frío (swash) lo cubre el punto 2.

## 5. `memcpy`/`memset` de mlibc

Bucles de 8×u64 (`mlibc/options/internal/generic/essential.cpp`). Zen 3
tiene ERMS/FSRM: `rep movsb`/`rep stosb` deberían ganar para todo programa
en C (DOOM, Quake, BusyBox). No es AVX, pero probablemente pesa más. Va
por `mlibc-port/` (nunca editar el submódulo; ver `userspace-programs`).

## Dónde no

- **Composición del compositor y `fb_flush`:** copias limitadas por ancho
  de banda (RAM y PCIe WC); AVX no cambia eso. Lo que sí ayuda es
  componer y volcar solo el daño.
- **`FBIO_BLIT` del kernel (DOOM/Quake en consola, 5,2 ms/frame):** el
  kernel compila sin coma flotante; AVX ahí exige un
  `kernel_fpu_begin/end` que guarde el estado del usuario con IF=0.
  Cambio de diseño delicado para un caso de nicho: mejor que el programa
  escale en userspace, como ya hacen los HIDPI.
