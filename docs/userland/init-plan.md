# Init and service supervision

Status: **idea, not started** (handoff written 2026-10-07). The user asked for it; not chosen
as the next task.
Related: [`../ai/llm-as-ui-plan.md`](../ai/llm-as-ui-plan.md) (`agentd` becomes a service),
[`../ai/capabilities-plan.md`](../ai/capabilities-plan.md) (services launched with limited fds).

## Today

- PID 1 is `userspace/src/bin/shell.rs` (~200 lines): `busybox --install -s /tmp/bin`, the
  metal autorun job if `/mnt/autorun/job` exists (`docs/reference/metal.md`), otherwise a
  `busybox ash` it respawns forever; it reaps orphans (`wait_reaping_orphans`).
- There is no service concept: the compositor and other daemons are started by hand.
- Networking is not a daemon: DHCP and the stack run in the kernel (`docs/reference/net.md`).

## Design: runit's three stages, our PID 1 + BusyBox's runit

```
PID 1 = constan-init (Rust: shell.rs grown, stays minimal)
  │  stage 1: busybox --install, autorun (unchanged), anything one-shot
  │  stage 2: exec/spawn `runsvdir /etc/service`, respawn it if it dies, reap zombies
  │  stage 3: on reboot/poweroff request: `sv down` all, wait with a timeout, sync, reboot(2)
  ▼
runsvdir /etc/service          (BusyBox applet, not our code)
  ├── runsv console  → run: exec ash on the console   (the respawned ash becomes a service)
  ├── runsv compositor → run
  └── runsv agentd   → run   (later)
```

Why this shape:
- **PID 1 must never die** (that is a kernel panic) and Rust `panic!` is easy to hit, so it stays
  tiny; the complex part (supervision) is a separate, restartable process. That is the reason
  runit and s6 split them, and the part of ChatGPT's diagram worth keeping.
- **Everything goes through the supervisor.** No "system services" started by init directly:
  one place starts processes, one set of rules.
- **Use BusyBox's runit (`runsvdir`, `runsv`, `sv`, `svlogd`, `chpst`) before writing a
  supervisor.** It is proven, standard, and models know it well (`agentd` can expose
  `sv status/up/down/restart` as tools). All are off today in `busybox-config/minimal.config`
  (`CONFIG_RUNSV`, `CONFIG_RUNSVDIR`, `CONFIG_SV`, `CONFIG_CHPST`; `CONFIG_SVLOGD` too).
- **Not systemd-style:** no dependency graph, no unit language. Too much for this system.
- **Our own supervisor only if runit falls short.** The likely reason: start ordering with
  readiness (compositor up before its clients). runit has none; s6's readiness fd
  (`notification-fd`) is the model to copy then.
- **Capabilities fit without a custom supervisor:** a service's `run` script does
  `exec cap-exec --dir ... -- program` (a small wrapper, like `chpst`), see
  `capabilities-plan.md`.

## What runsv needs from the kernel (check first)

From `docs/reference/syscalls.md` as of 2026-10-07; verify against runit's source in BusyBox
(`runit/runsv.c`, `runit/runsvdir.c`, `runit/sv.c`) before trusting this list:

| Need | Used for | Status |
|------|----------|--------|
| `mknod`/`mkfifo` (named FIFOs on ext2/tmpfs) | `supervise/control`, `supervise/ok` | **not in the syscall table**: likely the first job |
| `flock` or `fcntl(F_SETLK)` | `supervise/lock`, one runsv per service | **missing** (`fcntl` does only dup/FD/FL flags) |
| `rename`, `fsync` | atomic `supervise/status` | check |
| `setsid` | `run` scripts, `chpst` | done (`session_test`) |
| `SIGCHLD` + self-pipe + `poll` | runsv's main loop | done (G1) |
| `O_NONBLOCK` open of a FIFO with no writer | `sv` talks to `runsv` | new semantics with the FIFOs |
| a writable directory for `supervise/` | `/etc/service/*/supervise` | `/mnt` is RW on the stick; or symlink to a tmpfs (`/tmp`) |

`mkfifo` and `flock` are useful alone (many programs use them) and are Linux-numbered:
test each with a raw C test proven by sabotage (`linux-abi` skill).

## Steps

1. Read runit's code in BusyBox and turn the table above into a measured list.
2. Kernel: named FIFOs (`mknod` with `S_IFIFO`, `mkfifoat`) reusing the pipe code; `flock`
   (advisory, per open file). C tests + sabotage.
3. Enable the runit applets; rebuild BusyBox (`rm busybox.elf`, `userspace-programs` skill).
4. `/etc/service/console/run` = the `ash` PID 1 respawns today; PID 1 runs `runsvdir`
   instead. **Autorun stays in PID 1, unchanged**, before stage 2 (the metal loop depends on
   it: `METAL-BEGIN/DONE`, `reboot(2)`).
5. Compositor as a service (decide: always on, or `sv up` from the console).
6. Stage 3: a clean shutdown command (`poweroff`/`reboot` applets are off today; PID 1 could
   take a signal, e.g. `SIGTERM` = reboot, `SIGUSR2` = poweroff, as BusyBox init does).
7. Logs: `svlogd` per service into `/mnt/var/log/<name>` or the kernel log (`/dev/kmsg`-like
   path if one exists); decide.

## Tests

- QEMU: kill a service's process (`kill -9`), see runsv restart it; `sv down`/`up`; kill
  `runsvdir`, see PID 1 restart it; the autorun path still produces `METAL-DONE`.
- Metal: one autorun job that does the above (`metal-run` skill).

## Open questions

- Where service dirs live: `disk-image-root/etc/service/` (synced into `disk.img`) vs a list
  compiled into PID 1.
- Whether the console `ash` should stay outside supervision for robustness (if runit is broken,
  you still get a shell). A fallback: PID 1 starts a plain `ash` if `runsvdir` keeps dying.
