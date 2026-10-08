# Capabilities: the kernel's first security model

Status: **not started; cornerstone** (handoff written 2026-10-07; promoted 2026-10-08 to backbone B3
of [`../ux/backbones.md`](../ux/backbones.md), ahead of the other `docs/ai/` plans).
Principles served: P6 (nothing escapes its box), P2.7 (ask once, then show), P3
([`../ux/principles.md`](../ux/principles.md)).

Executable path (steps 1-5 + `cap-exec`, then the file manager): [`../ux/handoff-capabilities-to-files.md`](../ux/handoff-capabilities-to-files.md).

## Verdict: the cornerstone

- **Today the kernel has no security model at all.** Credentials are bookkeeping only: files
  have no owners (`stat` says 0/0), nothing refuses an open, kill or exec for an id, everything
  runs as root (`docs/reference/syscalls.md`, 102-123). NX is off and `PROT_EXEC` is ignored
  (`docs/userland/roadmap.md`). Isolation is address spaces and nothing else.
- So the choice is not "capabilities instead of Unix permissions": it is **which model to build
  first**. Building Unix DAC (owners, modes) first would add exactly the ambient authority
  that the critique of Linux is about. Capabilities first is the cleaner order.
- **Why it comes first** (decided 2026-10-08): the script runtime can sandbox three.js apps, but
  almost everything else the direction needs depends on kernel capabilities:
  - sandboxed preview providers and thumbnailers (untrusted parsers, P6.4; `docs/gui/files-plan.md`);
  - native code the agent writes or fetches, and the shared repo
    ([`software-on-demand.md`](software-on-demand.md): the manifest is enforced here);
  - userland drivers (the driver ladder, `docs/drivers/roadmap.md` phase 4), with the IOMMU;
  - the powerbox: drag-to-grant and the system open/save dialog (idea K1);
  - children's accounts as missing capabilities, not bypassable locks
    ([`education.md`](education.md));
  - the agent's own tools running inside constanos with a shell; more than one user.
- The three.js runtime is still a sandbox of its own; it runs **in** capability mode with only
  its app directory and the compositor socket (see "Unknowns").

## Where the idea came from

LaurieWired on Windows NT (reported by Windows Latest, 2026-09-26; the article was read in an
earlier session, the video was not watched): in NT every resource is a typed object under one
Object Manager, reached through handles that carry their granted access, while Linux layers
UIDs, modes, ACLs, cgroups, SELinux and namespaces with no single coherent model; and the 2026
question is "what exactly may this AI agent do?". Papers proposing OS-level capabilities for
agents were found in that session (AgenticOS; "Lingering Authority: revocable
resource-and-effect capabilities for coding agents") but not read.

## Options

| Model | Verdict |
|-------|---------|
| NT Object Manager + handles + ACLs | No: rewrites half the kernel, and NT still has ambient authority (the token + ACLs) |
| Unix DAC (owners, modes) | No as the first model: ambient authority. Maybe later for compatibility |
| **Capsicum (FreeBSD, Cambridge, 2010)** | **Yes, its semantics**: fds are already almost capabilities; incremental; keeps the Linux ABI (only adds calls) |
| Landlock / seccomp (Linux) | Linux-numbered (Landlock 444-446), but path rules and syscall filters, not capabilities. A compatibility layer later, not the model |
| seL4 / Fuchsia (pure capabilities) | Reference for ideas; not reachable without a different kernel |

## Design sketch (Capsicum-style)

- **Rights per fd:** each fd-table entry gets a rights mask (read, write, seek, mmap, ioctl,
  lookup-under, create-under, accept, connect, ...). `cap_rights_limit(fd, rights)` only
  narrows; `dup`, `SCM_RIGHTS`, fork and exec keep the mask. Checked in the syscall layer before
  `FileHandle::read/write/...` (the fd table is in `kernel/src/process/`, locks per CLAUDE.md:
  fd-table lock before `SCHEDULER`).
- **Capability mode:** `cap_enter()` sets a per-process flag inherited by fork/clone/exec, never
  cleared. In it, every syscall that names a *global* namespace fails (`ECAPMODE`):
  `open`/`openat` with `AT_FDCWD` or an absolute path, `bind`/`connect` to a path, `kill` of a
  pid (use `pidfd`, which exists), opening `/dev`, `chdir`, mount. Allowed: anything on fds it
  holds.
- **Lookups beneath a directory fd:** `openat(dirfd, rel)` must not escape `dirfd` (no `..`
  above it, no absolute symlinks). Linux has this as **`openat2` (437) + `RESOLVE_BENEATH`**:
  implementing `openat2` first is useful alone, Linux-numbered, and is the core of cap mode.
- **Process launch:** a launcher (the agent's `agentd`, or the compositor) opens what the app
  may use (its own directory, a compositor socket, `/dev/nvgpu`), limits rights, spawns the
  app, and the app calls `cap_enter` before running untrusted code (or the launcher sets it via
  an exec flag).
- **Already capability-shaped:** `SCM_RIGHTS` (`usock`), `pidfd_open`, `memfd`, GPU sessions on
  `/dev/nvgpu` and `BO_EXPORT`/`SYNC_EXPORT` fds (`gpu-g5` skill).
- **Numbers:** Linux has no Capsicum calls. Use a documented constanos range (or `prctl`
  options) and say so in `docs/reference/syscalls.md`.
- **Credibility prerequisite:** turn NX on and honour `PROT_EXEC` (W^X). Not part of
  capabilities, but any security claim is weak without it.
- **DMA:** a process that can program a device's DMA reaches all physical memory, whatever its
  fd rights. Userland drivers need the **IOMMU** (AMD-Vi on the Ryzen; QEMU emulates
  `amd-iommu`/`intel-iommu`) so a device sees only the buffers its driver was granted.

## Steps

1. `openat2` with `RESOLVE_BENEATH` (+ `RESOLVE_NO_SYMLINKS`), raw C test proven by sabotage
   (`linux-abi` skill).
2. NX / `PROT_EXEC`.
3. Rights masks on fds + `cap_rights_limit`, host-testable where the logic allows (`vfs`).
4. `cap_enter` + `ECAPMODE` on every global-namespace syscall: audit the syscall table
   (`kernel/src/process/syscall/`) and list each one as allowed / denied / fd-relative.
5. Launcher integration (`agentd`, compositor, `filesd`'s preview providers).
6. IOMMU: per-device DMA domains, before the first userland driver that does DMA
   (`docs/drivers/roadmap.md` phase 4).

**Test that proves it** (outreach rule: claim only what is tested): a process in capability
mode tries to open `/etc/...`, `/dev/...`, `..` from its dir fd, `kill` another pid, connect to
a socket path, and every attempt fails; sabotage each check and see the test fail.

## Unknowns

- How many syscalls need a cap-mode decision (count from the table).
- Whether procfs (`/proc/self`) is allowed in cap mode (FreeBSD has no procfs there).
- How the runtime-level sandbox (the three.js runtime's API) and kernel caps compose: probably
  the runtime runs in cap mode with only its app dir and compositor socket.
