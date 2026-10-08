# Files: the first app built on the UX principles

Status: direction (2026-10-08), not started. Executable path: [`../ux/handoff-capabilities-to-files.md`](../ux/handoff-capabilities-to-files.md). The file manager is where the principles were
first worked out, and the first app that exercises the backbones. Principles:
[`../ux/principles.md`](../ux/principles.md); ideas F1-F16, K1, U7 in [`../ux/ideas.md`](../ux/ideas.md).

## Shape

- Places sidebar (home, devices, network places, trash), a virtualized list, an **inspector
  pane** (metadata, preview, actions). Not a web/Electron app: a `gui` client like `term`.
- **Preview layout per folder** (right for code/PDFs, bottom for video/wide images), decided by
  the folder's dominant content or pinned by the user, **never per selection**: arrowing
  through mixed files must not move the list under the cursor (P2.3).
- **Space** toggles a full-window preview (Quick Look). Multi-selection inspector (F10).
- `/proc`, `/dev` and the log partition are places like any other (K5).

## Architecture

- **The UI only watches** (U7): `filesd` does copies/moves/previews; the window can crash
  without losing an operation.
- **Preview providers** are separate, capability-less processes: they get a read-only fd and
  return a buffer (shared GPU buffer when available). Every format decoder is untrusted code
  (P6.4). Providers are plain programs, so ported C libraries work (mind licences: MuPDF is
  AGPL).
- **Metadata on demand:** `getdents64` for the list, `stat`/preview only for what is visible.

## Steps (each useful alone)

1. List + inspector with PNG (`img` crate) and text/code (`text` engine) previews; providers as
   processes from day one, even before capabilities exist.
2. Operations journal + undo (F6), shell equivalents shown.
3. With B1: live refresh, recents, search (F4), directory sizes (F3).
4. With B3: providers in capability mode; drag-to-grant (K1).
5. More providers (ported decoders), versions (F7), provenance (F8).

## Not now

PSD layers, 5.1 video, PDF rendering: each is a ported decoder later. Audio is AC97 stereo and
there is no video decoder today (NVK exposes no NVDEC).
