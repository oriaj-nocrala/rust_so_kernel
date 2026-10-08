# Software on demand: apps, drivers and a shared repo written by the LLM

Status: direction (2026-10-08), not started. The north star of `docs/ai/`: a user says "I need
a GarageBand, make no mistakes" (or a Photoshop, or "my Focusrite's mixer doesn't work") and
the LLM designs, installs and runs software made for them. Education
([`education.md`](education.md)) is its hardest case. Principles: [`../ux/principles.md`](../ux/principles.md).
Mechanisms: [`llm-as-ui-plan.md`](llm-as-ui-plan.md) (agent), [`threejs-runtime-plan.md`](threejs-runtime-plan.md)
(runtime), [`capabilities-plan.md`](capabilities-plan.md) (B3).

## Why other OSes are not ready

They assume every app has a developer, a store and a signature. A generated app for one person
(Robin Sloan's "home-cooked meal") has none. What it needs instead, and where constanos has it:

| Need | Piece |
|---|---|
| Can't hurt anything even if buggy or malicious | B3 capabilities; the script runtime as sandbox (P6.1) |
| Written in what models already know | unmodified three.js; the Linux ABI (G1): a generated program is an ordinary Linux program |
| The model can test what it made | B5 semantic tree: operate the app by role and name |
| Install without fear, undo | U1 draft, U3 generations, apps as directories (U4) |
| Where it came from | provenance (P1.5): request, model, date |
| The user can open and change it | P7.3 symmetric authoring |

## Rules (cases of the parent principles)

- **R1 (P6.1).** A generated app is a first-class citizen with **no privileges by default**: no
  store, no signature, nothing granted until the user grants it, visibly.
- **R2 (P6.2).** **Risk follows reach, not author.** What a component may do depends on what it
  can touch; generated code never runs in the kernel.
- **R3 (P1.5, P8).** A repo entry is **request + code + tests + capability manifest**, never
  code alone.

## One app format (defined here, referenced elsewhere — P5)

`/mnt/apps/<name>/`: `manifest` (name, icon, entry, requested capabilities, provenance:
request text, model, date, parent entry if forked), the entry (`main.js` for the three.js
runtime, or a static binary), assets, `tests/` (semantic-tree scripts the agent ran). Launcher
line and icon derive from the manifest. Native apps are **static**: no name resolution at
install time (no slopsquatting), and they keep running for decades (P3.4).

## Drivers on demand: a ladder by reach

Evidence: third-party drivers caused ~70% of Windows crashes (Microsoft, data to 2004; 85% on
XP per Microsoft Research); CrowdStrike 2024 was an out-of-bounds read in a kernel driver. A
userland driver that can program DMA is effectively root without an IOMMU. No published
evaluation of an LLM writing a whole driver from a datasheet was found (closest: DevGen,
ACL 2026, Linux drivers → QEMU models, 44/50; Termite, OSDI 2014, formal synthesis).

| Level | What | Who writes it |
|---|---|---|
| 0 | Class drivers: USB Audio Class 2, HID, Mass Storage, IPP | Hand-written (with an LLM), reviewed, tested on metal; never generated on demand |
| 1 | Vendor control over USB control transfers (e.g. the Scarlett mixer/routing) | **Generated on demand**, userland, no DMA, a capability for that one device |
| 2 | PCI device with DMA | Userland only **with IOMMU** (AMD-Vi) and replay tests |
| 3 | Kernel code | Never generated on demand |

- The Focusrite case is level 0 + 1: Scarletts are class-compliant (audio works with the generic
  driver; Focusrite confirms), the mixer and routing need vendor messages (reverse-engineered
  by Geoffrey Bennett for Linux). The generic driver does the heavy work.
- **Trust comes from tests, not code:** the `gpu-display` recipe (oracle → fixture → pure code →
  replay test → sabotage → adapter) and the `hal` seams are the model. A driver entry in the
  repo carries its fixtures (recorded device traffic) and replay tests.

## The shared repo

- **Value:** reuse saves tokens; entries are recipes: fetch one as a starting point, the local
  LLM adapts it to this user.
- **Threat, measured:** 19.7% of LLM code samples import a package that doesn't exist; 43% of
  those names recur on every run, so attackers register them (slopsquatting, USENIX Security
  2025); real malicious packages have reached tens of thousands of downloads.
- **Mitigations:** static apps (no install-time resolution); capabilities (a malicious entry
  only does what was granted); the manifest shown before install ("asks for microphone and
  files you give it; **no network**"); tests that run locally before first launch;
  provenance chain of forks.

## Order

1. Agent + app format + runtime (existing plans, steps 1-3 of `llm-as-ui-plan.md`).
2. B5 semantic tree so the agent can test what it made.
3. B3 capabilities (manifest enforced), then the repo.
4. Driver level 1 (needs a USB control-transfer device file + per-device capability), then
   IOMMU, then level 2.
