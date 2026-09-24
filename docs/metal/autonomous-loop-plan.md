# Plan: bucle autónomo en bare metal

> **Estado (2026-09-24):** fase 1 cerrada y pasos 1-3 de la fase 2 medidos
> en la Ryzen, y el paso 4 (watchdog) respondido con logs: el TCO **no**
> sobrevive al reset, así que la fase 4 hace falta. Fase 3 hecha (QEMU) y
> fase 5 (`scripts/metal-run.sh`) hecha y con su primera ida y vuelta real
> en la Ryzen (`OK`). Fase 4 (watchdog en constanos) hecha y medida en la
> Ryzen: un job colgado vuelve solo a Linux a los ~300 s. Fase 6 montada
> (autologin + `scripts/metal-resume.sh`) y probada en seco; falta su primera
> vuelta real. Ver "Resultados" al final. Es la
> "etapa 0" de la dirección de largo plazo (un SO que un agente LLM pueda
> observar, probar y mejorar; ver la memoria `self-improving-os-direction`).
> No confundir con la etapa 0 de `docs/smp/smp-plan.md`.

## Qué resuelve

Hoy, probar un cambio en la Ryzen es: `cargo build` → `deploy-usb-boot.sh`
→ reiniciar a mano → elegir el USB en el menú del firmware → mirar la
pantalla o sacar una foto → reiniciar a Linux → `usb-log.sh read`. Un
humano en cada paso. El objetivo es un solo comando que haga el viaje de
ida y vuelta sin nadie delante:

```
Linux (Claude Code) ──deploy + job──▶ pendrive
      │  efibootmgr --bootnext <USB>; reboot
      ▼
constanos: arranca, ejecuta el job, escribe el log, reboot(2)
      │  (o cuelga → el watchdog resetea)
      ▼
Linux (BootOrder normal): lee constanos-log, clasifica, sigue el agente
```

Nada de esto necesita red, TLS ni un runtime nuevo dentro de constanos.
Casi todo ya existe: la partición `constanos-log` (con flush periódico, por
`sync` y por pánico), `reboot(2)`, `deploy-usb-boot.sh` y `sync-usb-data.sh`.

## Datos de la máquina (medidos 2026-09-24, desde su propio Linux)

- **Placa:** ASUS PRIME B450M-A II, Ryzen 9 5900X.
- **Entradas UEFI:** `BootOrder = 0001 0000 0003 0004`. `Boot0001 "UEFI OS"`
  arranca Linux (es `BootCurrent`). `Boot0004 "UEFI:  USB, Partition 1"`
  es el pendrive. Esa entrada la genera el firmware para el medio
  extraíble, así que **puede cambiar de número o desaparecer** al
  reconectar el pendrive. Hay que buscarla en cada ejecución (por el
  PARTUUID de la partición `boot`), nunca dejar `0004` fijo.
- **Watchdog:** `sp5100_tco` (FCH de AMD), `/dev/watchdog0`, timeout
  máximo 65535 s, parámetro `action=0` (reset). `bootstatus` existe y puede
  indicar si el último reset lo causó el watchdog.
- **`efibootmgr`** instalado el 2026-09-24 (`core`, 18-4).
- **Pendrive:** `sdb` (SanDisk 3.2Gen1) con `boot` (17M), `constanos-data`
  (2G) y `constanos-log` (64M).

## Fases

Cada fase se cierra **midiendo en la Ryzen**, no razonando. Las dos
primeras son experimentos que deciden el diseño de las siguientes.

### Fase 1: `reboot` vuelve a Linux (experimento)

Hay trabajo sin commitear en `kernel/src/reboot.rs` (`announce()`: deja
en el pendrive qué método de reset se va a probar antes de probarlo).
El `RESET_REG` de la FADT en esta placa apunta al puerto SMI
(`0xB2 <- 0xBE`).

- Arrancar constanos a mano, ejecutar `reboot`, comprobar que la máquina
  se reinicia sola y entra en Linux (el `BootOrder` por defecto).
- `usb-log.sh read` → el último `reboot: trying …` dice qué método
  funcionó.
- **Criterio:** 5/5 reinicios completos. Si el SMI no resetea, el
  siguiente candidato es 0xCF9; se decide con el log, no suponiendo.

### Fase 2: `BootNext` al pendrive y watchdog (experimento)

1. `pacman -S efibootmgr`. Localizar la entrada del USB por PARTUUID
   (`efibootmgr -v` muestra el `HD(...,GPT,<partuuid>,...)` de cada una).
2. `efibootmgr --bootnext <USB>` + reboot → ¿arranca constanos sin tocar
   el menú? Riesgo real: el *fast boot* de ASUS puede saltarse la
   enumeración USB. Si pasa, se desactiva en el firmware y se documenta.
3. `reboot` desde constanos → ¿vuelve a Linux? (`BootNext` se consume al
   usarse, así que debería.)
4. **Watchdog:** armar `/dev/watchdog0` desde Linux (timeout de ~180 s,
   escribiendo sin *magic close*), hacer `BootNext` a constanos y **no
   ejecutar `reboot`**. Resultado:
   - **La máquina se resetea sola a los ~180 s:** el watchdog sobrevive al
     reset en caliente y al arranque UEFI. El que lo arma es Linux, y
     constanos no necesita driver. Comprobar después que un arranque
     *correcto* no se lleva un reset a mitad del job (el job termina antes
     del timeout o constanos tiene que alimentarlo).
   - **No se resetea:** el reset o el firmware desarman el TCO. Entonces
     hace falta la fase 4.
   - Leer `bootstatus` en Linux después para ver si distingue "reset por
     watchdog". Si lo hace, es la señal que clasifica `HANG`.

**Criterio:** 5/5 ciclos `BootNext` → constanos → `reboot` → Linux sin
intervención, y una respuesta medida sobre el watchdog.

### Fase 3: el job y el modo autorun (código)

**Cómo sabe constanos que tiene trabajo.** El kernel no lee FAT y la
imagen de `bootloader` no admite línea de comandos, pero `/mnt` (la
partición `constanos-data`) se monta en cada arranque. El host escribe:

```
/mnt/autorun/job      # script de ash
/mnt/autorun/nonce    # identificador único de esta ejecución
```

- **`shell.rs` (PID 1):** después de `install_busybox_symlinks()` y antes
  del bucle de `ash`, si existe `/mnt/autorun/job`, lo ejecuta con
  `busybox ash`. Antes imprime `METAL-BEGIN <nonce>` y al terminar
  `METAL-DONE <nonce> <exit status>`. Después ejecuta `sync` y
  `reboot(RESTART)`. La salida del job llega al log sin trabajo extra: el
  espejo de la consola ya escribe en `klog`.
- **Pánico en modo autorun:** hoy el panic handler hace `hlt` para
  siempre después del flush. En autorun debe reiniciar (tras
  `logpart::on_panic()`), o cada pánico dejaría la máquina parada hasta
  que salte el watchdog, o para siempre si no hay watchdog. El kernel
  necesita un flag `AUTORUN` global, puesto al montar `/mnt` si existe el
  job. El doble fallo (IST) sigue el mismo camino.
- **Es de un solo uso por diseño del host, no del kernel.** `/mnt` es de
  solo lectura para constanos, así que no puede borrar el job. Lo borra el
  host al recoger el resultado. Si alguien arranca constanos a mano con un
  job pendiente, se ejecuta y reinicia: molesto, pero inofensivo. El host
  borra el job pase lo que pase.
- **Timeout dentro del job:** `timeout` de busybox alrededor de cada test
  largo, para que un proceso colgado (sin que el kernel se cuelgue)
  termine con `DONE` y un código de error en vez de esperar al watchdog.

### Fase 4 (solo si la fase 2 lo pide): driver de watchdog en constanos

Si el TCO no sobrevive al reset, constanos lo arma él mismo al principio
de `init::boot`, solo en modo autorun:

- La lógica de registros va en `hal` (`hal::sp5100_tco`, testeable en
  host con `PhysMem`/`PortIo` simulados), siguiendo la skill
  `kernel-drivers`. Referencia: `drivers/watchdog/sp5100_tco.c` de Linux,
  rama *EFCH* (familia 17h+): activación por los registros PM del FCH y
  MMIO: en esta placa Linux informa `Using 0xfeb00000 for watchdog MMIO
  address` (journal, 2026-09-24). Leer el driver de Linux antes de escribir nada.
  No suponer los bits.
- El timeout es fijo y generoso (p. ej. 300 s). Nadie alimenta el
  watchdog: si el job no ha ejecutado `reboot` antes del timeout, algo ha
  ido mal y el reset es lo que se quiere.
- Esto no protege el tramo firmware → constanos antes de armarlo. Para
  eso queda la opción externa (un enchufe inteligente o un relé USB que
  controle otra máquina), que por ahora no vale lo que cuesta.

### Fase 5: el orquestador del host (`scripts/metal-run.sh`)

```bash
scripts/metal-run.sh JOB.sh      # despliega, programa, reinicia
scripts/metal-run.sh --collect   # tras volver a Linux: resultado + limpieza
```

`metal-run.sh JOB.sh`:

1. **Precondiciones, con error explícito si fallan:** estamos en la Ryzen
   (`board_name`), el pendrive tiene las tres particiones por label,
   `efibootmgr` existe y no hay otro job pendiente.
2. `cargo build` + `deploy-usb-boot.sh` (ya incluye la prueba en QEMU y la
   verificación por relectura) + `sync-usb-data.sh`.
3. Escribir `autorun/job` + un `nonce` nuevo en `constanos-data`, y guardar
   el estado del host en `target/metal/pending` (nonce, hora, commit, job).
4. Si la fase 2 salió a favor de Linux: armar el watchdog.
5. Buscar la entrada USB por PARTUUID, `efibootmgr --bootnext`,
   `systemctl reboot`.

`--collect`:

1. `usb-log.sh read`. **El nonce decide si el slot más nuevo es de esta
   ejecución** (si constanos no llegó a arrancar, el slot más nuevo es de
   una ejecución anterior).
2. **Clasificar** (mismo espíritu que `boot-matrix.sh`):
   - `OK`: `METAL-DONE <nonce> 0`.
   - `FAIL`: `METAL-DONE <nonce> ≠0`.
   - `PANIC`: `METAL-BEGIN` sin `DONE`, con un flush de razón `Panic`.
   - `HANG`: `METAL-BEGIN` sin `DONE` y sin pánico (más `bootstatus` si
     lo distingue).
   - `NO-BOOT`: el nonce no aparece en ningún slot.
3. Borrar `autorun/` del pendrive, mover `pending` a
   `target/metal/runs/<nonce>/` junto con el log completo y el veredicto.
4. **Leer el log guardado antes de creer el veredicto** (la regla de
   `boot-matrix.sh` vale aquí igual).

**Permisos:** escribir en el pendrive, `efibootmgr` y el watchdog necesitan
root. Para que funcione sin nadie delante: una entrada en sudoers
`NOPASSWD` limitada a esos scripts concretos (no a `sudo` en general), o
una regla de udev que dé al usuario acceso a las particiones por
PARTLABEL. Lo decide el usuario. No se configura sin preguntar.

### Fase 6: cerrar el bucle con el agente

Cuando se reinicia la Ryzen, se muere la sesión de Claude Code que corría
en ella. Hay dos niveles:

- **Semiautónomo (primero):** al volver a Linux, el humano ejecuta
  `metal-run.sh --collect` y retoma la sesión. Ya elimina todo menos un
  comando.
- **Autónomo:** una unidad `systemd --user` que se ejecuta al iniciar
  sesión y, si existe `target/metal/pending`, ejecuta `--collect` y
  retoma el agente en modo headless (`claude -p --resume <sesión>
  "resultado en target/metal/runs/<nonce>"`) con los permisos acotados en
  `.claude/settings.json`. Requiere inicio de sesión automático o un
  servicio de sistema. **Salvaguarda:** un tope de ciclos por sesión
  (p. ej. `target/metal/budget`) para que un agente que no converge no se
  quede reiniciando la máquina toda la noche.

## Qué queda fuera, a propósito

- **Ejecutar `hw_tests` (el binario de tests del kernel) en metal.** Hoy
  informa por `isa-debug-exit`, que solo existe en QEMU. Queda para
  después: que informe por el log más `reboot` en modo autorun. Encaja
  bien una vez que este bucle funcione.
- **Slots A/B del kernel con rollback.** Aquí no hacen falta: si el kernel
  nuevo no arranca, el watchdog (o `NO-BOOT`) devuelve la máquina a Linux,
  que sigue intacto en el NVMe. Solo hacen falta cuando constanos sea el
  sistema principal.
- **Red, un agente dentro de constanos, hot swap:** etapas posteriores de
  la dirección general.

## Riesgos conocidos

- **La entrada USB de ASUS es dinámica.** Buscarla siempre por PARTUUID.
- **El fast boot puede saltarse el USB** con `BootNext`. Se mide en la fase 2.
- **Flush periódico cada 5 s desde idle:** un proceso al 100 % de CPU lo
  deja sin ejecutarse. Un `HANG` puede perder hasta el final de su log.
  `METAL-BEGIN` debe ir seguido de un `sync` explícito para que al menos
  el inicio del job quede siempre escrito.
- **Desgaste del pendrive:** cada ciclo reescribe la FAT de `boot` (~6 MB).
  Es despreciable para decenas de ciclos al día, pero conviene no
  desplegar el kernel si no ha cambiado (comparar el hash con el último
  despliegue).

## Resultados

**2026-09-24, fase 1 + fase 2 pasos 1-3 (Ryzen):**
- `efibootmgr --bootnext 0004` arranca constanos **sin tocar el menú**; el
  fast boot no molesta. `BootNext` se consume al usarse.
- Entrar al menú del firmware y cancelar **no** consume `BootNext` (arranca
  Linux y la entrada sigue programada).
- `reboot` en constanos vuelve solo a Linux con el **primer** método: la
  última línea del log es `reboot: trying the ACPI reset register (port 0xb2
  <- 0xbe)`. El SMI del firmware resetea; `0xCF9` no llegó a probarse.
- Ida y vuelta (`systemctl reboot` → constanos → `reboot` → Linux arriba):
  ~1,5 min.

**2026-09-24, fase 2 paso 4 (watchdog), respondido sin reiniciar:**
- systemd trae `RebootWatchdogUSec=10min`: cada `systemctl reboot` arma el
  TCO a 10 min (`systemd-shutdown: Watchdog running with a hardware timeout
  of 10min`, en el journal de cada apagado).
- El apagado de Linux de 2026-09-23 21:39:01 lo armó. El arranque #12 de
  constanos empezó justo después (~21:39:14) y seguía vivo a los **901,8 s**
  (último flush 21:54:16). constanos no tiene driver de TCO, así que nadie lo
  alimentó. **Si hubiera sobrevivido al reset, habría reseteado a los 600 s.**
- Al arrancar Linux, el driver encuentra el TCO `inactive`.
- **Conclusión:** el reset o el firmware desarman el TCO. Armarlo desde Linux
  no sirve; la fase 4 (driver en constanos) hace falta. El driver de Linux
  usa MMIO `0xfeb00000` en esta placa.

**2026-09-24, fase 3 (autorun) hecha y verificada en QEMU:**
- `kernel/src/autorun.rs` (flag, detectado tras `fs::init`), panic handler →
  `reboot::restart_from_panic()` en modo autorun, PID 1 con
  `METAL-BEGIN`/`METAL-DONE <nonce> exit=N|signal=N` + `reboot`.
- Probado: job normal (`exit=3` sale como `exit=3`), `kdebug panic` en un job
  → reset y QEMU termina (`-no-reboot`), 8 tests de userspace como jobs,
  `run-kernel-tests.sh` PASS, `boot-matrix.sh 4 3` 12/12.
- **El primer job encontró dos bugs reales del kernel**, invisibles en uso
  interactivo:
  1. `kill_and_switch_tf` no restauraba `fs_base`: el proceso siguiente
     corría con el TLS del que murió (`ash` fallaba en `get_current_tcb`).
     `sys_fork` copiaba además un `fs_base` desfasado.
  2. `CURRENT_SYSCALL_TF` era un global: una syscall desalojada con IF=1 leía
     al volver el marco de *otro* proceso, y le entregaba SIGCHLD escribiendo
     el marco de señal sobre la pila viva del padre. Solo con carga en el
     host (8/8 fallos en paralelo, 0/8 tras el arreglo). Ahora
     `current_tf_ptr()` sale de la pila de kernel del proceso.
- Estado de `wait` de este kernel: `0x200|code` / `0x400|sig<<24` (no Linux).

**2026-09-24, fase 5 (`scripts/metal-run.sh`), verificada sin reiniciar:**
- `metal-run.sh JOB.sh` / `--collect` / `--abort`, como en el diseño, más
  `--no-deploy` y `--no-reboot`. El despliegue del kernel se salta solo si el
  ELF tiene el mismo sha256 que en el último despliegue.
- **Qué arranque es esta ejecución:** al programarla se guarda el número del
  último arranque del log (`prev_boot_seq`); solo cuentan los arranques con
  número mayor, y entre ellos el que imprime el nonce. Así un nonce viejo no
  confunde, y un arranque nuevo que muere antes del job sale como `NO-JOB`
  (veredicto nuevo) en vez de `NO-BOOT`.
- **Permisos:** este usuario ya tiene `sudo` `NOPASSWD: ALL`, así que no hizo
  falta ninguna regla nueva. El script usa `sudo -n` y falla al principio si
  pidiera contraseña.
- `sync-usb-data.sh` hace `rsync --delete`, así que tiene que ir **antes** de
  escribir `autorun/` (si no, borra el job).
- Probado en la Ryzen sin reiniciar: el job y el nonce llegan al pendrive,
  un segundo intento se rechaza, `--collect` da `NO-BOOT`, borra `autorun/` y
  archiva en `target/metal/runs/<nonce>/`. `e2fsck -fn` limpio tras cada
  escritura.
- Probado en QEMU de extremo a extremo (pendrives de `usb-log.sh mkimage` con
  el job dentro, `-no-reboot`, tres en paralelo): `exit 0` → `OK`, `exit 3` →
  `FAIL exit=3`, `kdebug panic` → `PANIC`; los logs guardados lo confirman.
  `HANG`, `NO-JOB` y la selección por nonce, con logs derivados de esos.
- En QEMU el reset lo hace `0xCF9` (la FADT de i440fx no trae `RESET_REG`).

**2026-09-24, fase 5, primera ida y vuelta real en la Ryzen:** `metal-run.sh
first-job.sh` (compilar, desplegar con la prueba en QEMU, sincronizar,
`BootNext=0004`, reiniciar) → constanos ejecutó el job y volvió a Linux solo
por el SMI (`0xB2 <- 0xBE`) → `--collect`: `OK exit=0 (boot #20)`, `autorun/`
borrado, `BootNext` consumido. Nadie tocó la máquina. El log confirma el
veredicto. Lo único que falló fue el propio job: este busybox no acepta
`head -3` (le falta `FEATURE_FANCY_HEAD`), hay que usar `head -n 3`. Desde
entonces `--collect` muestra solo la salida de consola del job (las líneas
`[fb]`); las trazas del kernel quedan en `boot.log`.

**2026-09-24, fase 4 (driver del TCO), medida en la Ryzen:**
- `hal::sp5100_tco` (protocolo de registros, 8 tests en host) +
  `kernel/src/watchdog.rs` (busca el SMBus 0C/05, mapea las ventanas PM
  `0xFED80300` y WDT `0xFEB00000` sin caché, arma a 300 s). Solo la variante
  `efch_mmio` de Linux, la de esta placa (SMBus `1022:790b` rev `0x61`);
  las demás se detectan y se informan como no implementadas. Solo en modo
  autorun, justo después de `autorun::detect()`. Nadie lo alimenta.
- Job de prueba: mostrar la cuenta, `sleep 5`, mostrarla otra vez,
  `kdebug sync` y un bucle infinito. Resultado: `armed 300 s, 300 s left`
  → `295 s left`; Linux se apagó a las 14:41:26, constanos hizo el `sync` a
  las 14:41:45 y Linux volvió a arrancar a las 14:46:54, sin tocar nada.
  `--collect`: `HANG last flush: sync (boot #21) [watchdog reset:
  bootstatus=32]`.
- **`bootstatus=32` distingue el reset del watchdog:** el bit `WatchDogFired`
  sobrevive al reset y el driver de Linux lo lee al cargar. `--collect` lo
  guarda y lo añade al veredicto.
- Al arrancar constanos, `DECODEEN_WDT_TMREN` estaba a 0 (`decode enabled
  here`): el reset borra también lo que Linux activó, igual que el timer.
- En QEMU (sin FCH de AMD) el driver dice `no AMD FCH SMBus` y sigue;
  `run-kernel-tests.sh` PASS, `boot-matrix.sh 4 2` 8/8.
- Sigue sin cubrirse el tramo firmware → `fs::init`: un kernel que muere
  antes de armar el watchdog no se rescata.

**2026-09-24, fase 6 (reanudar el agente solo), montada y probada en seco:**
- **Autologin:** esta máquina no tiene gestor gráfico; tty1 la sirve kmscon
  (`login` → `zsh`). Drop-in `/etc/systemd/system/kmsconvt@tty1.service.d/
  autologin.conf`: `ExecStart=kmscon --vt=%I --no-switchvt --login --
  /usr/bin/login -f oriaj`. Elegido por el usuario (único usuario real, máquina
  en su casa) frente a un servicio de sistema.
- **Gancho:** `~/.zlogin` ejecuta `scripts/metal-resume.sh` si `XDG_VTNR` es
  1. `.zlogin` y no `.zprofile`, porque zsh lo lee después de `.zshrc`, que
  es donde `~/.local/bin` (el `claude`) entra en el `PATH`.
- **`scripts/metal-resume.sh`:** sin `target/metal/pending` no hace nada. Si
  hay una ejecución pendiente: `--collect`, espera a la red, y
  `claude --resume <sesión> "<veredicto + dónde está el log>"` **en primer
  plano en tty1**, así que quien esté delante ve lo que hace y puede cortarlo.
  `metal-run.sh` guarda la sesión (`CLAUDE_CODE_SESSION_ID`) en `pending`.
- **Frenos:** `target/metal/budget` (reanudaciones automáticas que quedan;
  sin el fichero o a 0 solo recoge) y `target/metal/stop` (solo recoge).
  Cada reanudación resta 1.
- Probado en seco: con `budget`=3 → «would collect, then resume the agent»;
  con `stop` → «then stop»; sin pendiente → nada.
