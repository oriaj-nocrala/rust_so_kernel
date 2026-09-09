# `ext2`: los dos tests oráculo de `e2fsck` comparten ficheros temporales

> **Estado: diagnosticado, NO arreglado.** Escrito 2026-09-09, encontrado de
> refilón durante la extracción del crate `vfs`
> (`docs/fs/vfs-extraction-plan.md`). Se documenta y se deja como está a
> propósito: no forma parte de aquel refactor y merece su propio commit.

## Síntoma

`cd ext2 && cargo test` falla de vez en cuando con `89 passed; 1 failed` en
lugar de `90 passed`. Con `--test-threads=1` no falla nunca.

## Evidencia

- Observado por primera vez durante el paso 1 de la extracción de `vfs`
  (~1 de cada 3 corridas), en una máquina que a la vez estaba corriendo
  `cargo build`/`run-kernel-tests.sh`.
- **No** se reproduce con `cargo test` a secas: 6 corridas aisladas seguidas,
  90/90 las seis veces.
- **Sí** se reproduce forzando a los dos tests culpables a solaparse:
  `cargo test e2fsck_clean -- --test-threads=2` en bucle dio **1 fallo en 25
  corridas** (una segunda tanda de 40 no volvió a caer — la ventana es
  estrecha). El mensaje de fallo concreto no llegó a capturarse.
- El commit que destapó todo esto (`fb91658`) **no toca `ext2/`** en absoluto
  (`git show --stat fb91658 -- ext2/` sale vacío), así que es preexistente.

## Mecanismo

Los tres helpers de fixture en `ext2/src/repair.rs` construyen sus rutas
temporales con el **PID del proceso** como único discriminante real:

| Helper | Ruta | Línea |
|---|---|---|
| `build_real_ext2_fixture(total_blocks)` | `$TMPDIR/ext2_repair_fixture_{pid}_{total_blocks}.img` | 661-662 |
| `e2fsck_says_clean(bytes)` | `$TMPDIR/ext2_repair_check_{pid}.img` | 758-759 |
| `inject_orphan_file_with_debugfs(bytes)` | `$TMPDIR/ext2_orphan_{fixture,payload,script}_{pid}.*` | 698-701 |

`cargo test` corre los tests **en hilos del mismo proceso**, así que el PID es
idéntico para todos y no discrimina nada. Y hay dos tests que usan esos mismos
helpers con los mismos argumentos:

- `reconcile_free_counts_repairs_drift_on_a_real_mke2fs_image_e2fsck_clean`
  (línea 783) → `build_real_ext2_fixture(256)`
- `reclaim_orphans_repairs_a_real_debugfs_orphan_e2fsck_clean`
  (línea 819) → `build_real_ext2_fixture(256)`

Ambos resuelven a **la misma ruta**, `ext2_repair_fixture_{pid}_256.img`, y
ambos llaman además a `e2fsck_says_clean`, que resuelve siempre a la misma
`ext2_repair_check_{pid}.img` sin discriminante ninguno. Cada helper hace
`File::create` (que trunca), lanza un proceso externo (`mke2fs`/`debugfs`/
`e2fsck`) contra el fichero, lo lee, y termina con `remove_file`. Dos hilos en
esa secuencia sobre la misma ruta se pisan de varias formas: uno trunca la
imagen que el otro está leyendo, uno borra el fichero antes de que el proceso
externo del otro lo abra, o uno escribe su imagen donde el otro acaba de
escribir la suya.

## Por qué importa más de lo que un test flaky normal importa

El modo de fallo interesante no es el rojo intermitente: es que
`e2fsck_says_clean` puede acabar **dictaminando sobre la imagen del otro test**.
Si eso ocurre en la dirección afortunada, el oráculo devuelve "limpio" sobre
una imagen que no es la que el test acaba de reparar, y el test **pasa sin
haber comprobado nada**. Un falso verde en el oráculo de `e2fsck` es
exactamente lo que estos dos tests existen para impedir (ver el bloque de
comentarios "e2fsck oracle" en `repair.rs`, y el bug de orden de
`reclaim_orphans` que motivó todo aquello).

Dicho de otro modo: esto pertenece a la misma familia que el hueco del
observador documentado en `vfs/src/ramfs.rs` y que los "instrumentos
defectuosos" de `docs/hang-hunt-bug2-findings.md` — un detector que puede
mentir sin avisar.

## Lo que NO es

La primera hipótesis fue que el `build.rs` de la raíz, que muta `disk.img` con
`debugfs`, interfería con los tests de `ext2` cuando ambos corrían a la vez.
**Es falsa**: estos tests no tocan `disk.img` en ningún momento, solo ficheros
bajo `$TMPDIR`. Que el primer avistamiento coincidiera con un `cargo build` en
paralelo fue casualidad — la carga extra de CPU cambia el scheduling de los
hilos y hace más probable el solapamiento, nada más.

## Arreglo cuando toque (no hecho)

Cualquiera de estas cierra el agujero; la primera es la mínima:

1. Añadir un discriminante único por llamada a las tres rutas — un contador
   atómico del propio módulo de tests, o `std::thread::current().id()`, además
   del PID.
2. Usar un directorio temporal propio por test (`tempfile::TempDir` como
   dev-dependency), que además limpia solo si el test entra en pánico.
3. Serializar los tests que tocan fixtures externos con un `Mutex` estático del
   módulo de tests. Es lo más barato de escribir, pero renuncia al paralelismo
   y no arregla la causa.

## Cómo reproducirlo

```bash
cd ext2
for i in $(seq 1 25); do
  cargo test e2fsck_clean -- --test-threads=2 2>&1 | grep -E "^test result: "
done
```
Espera un `FAILED` cada ~25 corridas. Con `--test-threads=1`, nunca.
