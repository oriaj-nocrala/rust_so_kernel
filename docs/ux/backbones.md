# UX backbones: the infrastructure the principles need

Status: direction (2026-10-08), none started. Almost every idea in [`ideas.md`](ideas.md)
hangs from one of these six. Build a backbone once, in the layer that owns the information
(P5), and the ideas on top become small.

| Backbone | Layer | Serves | Unlocks (ideas) |
|---|---|---|---|
| B1 VFS change journal | kernel (`vfs` crate) | P1, P4, P3 | search, recents, undo, versions, provenance, live folders, verified backup, lifestream |
| B2 Attribution ledger + error causes | kernel | P1, P8 | errors with causes, hang reasons, "where did it go" (disk/energy/network), wake/sleep reasons |
| B3 Capabilities | kernel | P6, P2.7, P3 | powerbox, sandboxed previews, generated apps, repo, userland drivers, kids' accounts |
| B4 System generations | boot + updater | P4.3, P2.2, P2.6 | atomic updates with rollback, pinned UI versions, app updates, migration |
| B5 Semantic UI tree | `gui` + compositor | P7, P8, accessibility | screen reader, the agent's view and tests, universal inspect, learner-action logs |
| B6 Compositor policy | compositor | P2, P9, P5.2 | focus ownership, notifications, periphery, monitor memory, IME, contrast checks |

## B1. VFS change journal

- **What:** a global ring of VFS events (create, write, rename, unlink, setattr) with a
  sequence number, persisted so readers can catch up after downtime. Same mechanism as NTFS's
  USN journal, which is what makes voidtools' Everything instant.
- **Where:** `vfs` crate (host-testable: pure "operation → event" logic), adapter in
  `kernel/src/fs/`. Effects as data, nothing blocks (CLAUDE.md crate pattern).
- **First step:** an in-memory ring + `/proc/journal` reader; prove ordering and catch-up with
  host tests and a sabotage.
- **Open:** persistence format on ext2; per-directory aggregate sizes (idea F3) as a consumer.

## B2. Attribution ledger + error causes

- **What:** (a) per-process accounting the kernel already half has (CPU time, RSS) extended to
  bytes written per device, network bytes per socket owner, wakeups, sleep blockers, RAPL
  energy; (b) an extended error channel: the failing syscall leaves a structured "why"
  (`EBUSY → holder pid, fd`), readable next to `errno` (e.g. `/proc/self/lasterror` or a
  constanos-range call), so the Linux ABI is unchanged.
- **Where:** `kernel/src/process/`, `kernel/src/network/`, `kernel/src/time/` (RAPL), syscall
  layer. Per-CPU vs global work stated per the timer-tick rule.
- **First step:** `EBUSY` on unmount/eject naming the holder (who-has-it-open walk over fd
  tables, IF-safe per the lock order).
- **Visible from outside the process:** foreign Linux programs will never read the extended
  channel and print a bare "Permission denied". So the kernel also keeps each process's recent
  failures with their causes (syscall, path, missing capability, holder) where the inspector,
  `/proc` and `agentd` can read them. The cause must not depend on the app cooperating.
- **Shared memory has owners too:** pages shared through the page cache (platform libraries)
  are attributed proportionally (like Linux's PSS) and listed by name ("NVK: 15 MB shared by
  vk_comp, snake3d"), never as an anonymous "shared"/"other" bucket.
- **Unknowns:** cost of per-socket accounting on the hot path; where the "why" lives so threads
  don't clobber each other's.

## B3. Capabilities

- **What:** Capsicum semantics: rights masks on fds, capability mode, `openat2` +
  `RESOLVE_BENEATH`. Plan: [`../ai/capabilities-plan.md`](../ai/capabilities-plan.md).
- **Priority:** the cornerstone (decided 2026-10-08): previews (F2), the shared repo, native
  generated code, userland drivers and kids' accounts all need it.
- **Plus:** NX / `PROT_EXEC` (any security claim is weak without it); IOMMU (AMD-Vi) before any
  userland driver that does DMA.

## B4. System generations

- **What:** a system version = kernel + `/bin` + system apps as one unit; boot the new one once,
  mark it good after `watchdog::settle`, else the firmware falls back, **and the next boot says
  why** ("the update to generation 13 never reached a healthy state: watchdog at X; you are back
  on 12"), with the log at hand (P1.2). The metal loop already
  does this: `BootNext` one-shot boot + TCO watchdog (`docs/reference/metal.md`,
  `docs/metal/autonomous-loop-plan.md`).
- **First step:** write down the generation layout on the ESP and the "good" marker; reuse the
  metal-loop code paths rather than writing new ones (P5).
- **Platform libraries** (musl, NVK) live in an immutable store per generation, addressed by
  hash; native apps pin the store path they were tested against (`docs/userland/roadmap.md` step 2).
- **Open:** where user-pinned UI versions live (P2.6); how apps (directories, static except the platform) join a
  generation.

## B5. Semantic UI tree

- **What:** every `gui` client publishes a tree of nodes (id, role, name, state, bounds,
  actions) using AccessKit's schema (Rust, platform-neutral); the compositor exposes it
  (as files, P7.2). Consumers: a screen reader, `agentd` (the agent sees and acts by role
  and name, not pixels), UI tests that survive restyling, the universal inspector, the learner
  model (structured actions instead of free text).
- **Where:** `gui` crate (host-tested), compositor, `userspace::gfx`; the three.js runtime's UI
  helper must emit nodes too (otherwise generated apps are invisible to the agent).
- **First step:** a tree for the panel and `term`; `agentd` reads it.
- **Status:** the mechanism exists (`gui::semantic`, `semantics_node`/`get_semantics`, `docs/reference/graphics.md`): the `ui` widgets
  publish it and `gui-tree` reads it. Not yet: the panel, `term` and `gfx` clients, files under P7.2, actions (only listed).
- **Why now:** retrofitting accessibility is the classic mistake of the big OSes; the GUI
  libraries are still small.

## B6. Compositor policy

- **What:** the rules of P2 and P9 enforced in one place: focus ownership (typing guard,
  Enter delay, click-to-focus), notification channels and budget, the peripheral status
  channel, per-monitor window memory keyed by EDID serial with sleep-vs-unplug debounce,
  input methods/compose, contrast measurement under translucency, never waiting on a client.
- **Where:** `gui/src/compositor/` (host-tested state machine) and `vk-comp`.
- **First step:** P2.1 (typing guard) and P6.3 (a hung client stays movable/closable,
  with a P1.2 "waiting on" label once B2 exists).
