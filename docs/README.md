# rust_so_kernel — Documentation

Design documentation for the kernel. This is the **human-facing, narrative** layer
of documentation. It complements two other layers already in the repo:

| Layer | Location | Audience | Purpose |
|-------|----------|----------|---------|
| Quick reference | `CLAUDE.md` (repo root) | Agents + contributors | Small, always-loaded: build/test commands, crate map, boot sequence, design invariants, and an index into `docs/reference/`. |
| Subsystem reference | `docs/reference/*.md` | Agents + contributors | Dense map of each subsystem as it exists *today* (moved out of `CLAUDE.md` 2026-09-26); read on demand. |
| Task playbooks | `.claude/skills/*/SKILL.md` | The coding agent | Step-by-step *how to do* a recurring task: `kernel-drivers`, `kernel-testing`, `qemu-debug`, `metal-run`, `userspace-programs`. |
| **Design docs** | **`docs/` (here)** | **Humans** | **The *what*, the *why*, and the *where we're going*** — architecture rationale and roadmaps that outlive any single change. |

Rule of thumb: if it's "how the system is wired right now," it belongs in `docs/reference/`
(and only in `CLAUDE.md` if it's a rule every change must respect).
If it's "how to perform task X," it's a skill. If it's "why it's shaped this way" or
"the plan for where this is heading," it's a `docs/` design doc.

## Why this exists now

The project has reached an inflection point. Building the kernel with an LLM has been
relatively easy so far, but it has grown large and accumulated debt. The code itself is
decently commented, but two things are missing at the project level: **design
documentation** and **tests**. This directory is the start of closing the first gap; the
[driver roadmap](drivers/roadmap.md) describes how the second gets closed too.

## Organization

Docs are grouped by subsystem, one subfolder each, each with its own `README.md` index.

```
docs/
├── README.md                 ← you are here
├── reference/                ← current-state subsystem notes (the index lives in CLAUDE.md)
├── gpu/gpu-plan.md           ← NVIDIA RTX 3050 plan: EDID, vblank, GSP boot, modeset via RM, copy engine
├── gpu/g5-*.md               ← graphics stack on NVK (plan, layer-4 handoff, and `g5-layer4-session-report-2026-10-01.md`: bugs, dead ends, mistakes, recipes)
├── gpu/hw-cursor-plan.md     ← design (not started): the display engine's cursor channel instead of a quad in the compositor's frame
├── blog/                     ← write-ups for outside readers (`gsp-to-vulkan.md`: the GPU driver, GSP-RM to Vulkan)
├── drivers/                  ← the device-driver subsystem (current focus)
│   ├── README.md             ← driver-docs index
│   ├── architecture.md       ← where we are: the trait/seam design, today
│   └── roadmap.md            ← where we're going: toward a Linux-class driver model
├── gui/files-plan.md          ← the file manager, first app on the UX principles (not started)
├── gui/compositor-visual-plan.md ← vk_comp looks: renderer primitives → themes (Luna 2026 / 9x) → glass → icons → real 3D icons
├── ux/                       ← UX principles (read first for anything user-facing), backbones, idea inventory, design audit
├── ai/                       ← direction: `software-on-demand.md` (north star), `education.md`; handoffs, not started: `llm-as-ui-plan.md` (agent interface layer), `threejs-runtime-plan.md` (unmodified three.js on wgpu/NVK), `capabilities-plan.md` (Capsicum-style, first security model)
├── userland/roadmap.md       ← agreed order: HTTPS client → dynamic linking → Claude Code (scope) → browser; musl-only (mlibc retired)
├── userland/init-plan.md     ← handoff, not started: minimal Rust PID 1 + BusyBox runit (needs mkfifo, flock)
├── busybox-integration.md    ← legacy: BusyBox/ash bring-up log (see note below)
└── *.png                     ← screenshots referenced by the repo-root README.md
```

New subsystems get their own `docs/<subsystem>/` folder as they're documented — "we'll
keep documenting better" is the intent, not a one-time dump. Loose top-level files
(`busybox-integration.md`, screenshots) predate this structure; new docs go in a subfolder.

## Conventions

- **Language:** English, matching the codebase, comments, and `CLAUDE.md`. One legacy doc
  (`busybox-integration.md`) predates this convention and is in Spanish — pending translation;
  not a precedent for new docs.
- **Keep it current or delete it.** A stale design doc is worse than none. When a design
  changes, update the doc in the same change (or remove it and note why in the roadmap).
- **Link, don't duplicate.** Reference `CLAUDE.md`/code/skills instead of restating them;
  docs here carry the *reasoning and direction* the other layers deliberately omit.
