# UX ideas inventory

Status: inventory (2026-10-08), none started. 78 raw ideas from the research conversation,
deduplicated to the list below. Each names its principle ([`principles.md`](principles.md))
and backbone ([`backbones.md`](backbones.md)); an idea with no backbone is cheap on its own.
"Old #" is the number used in the conversation, kept so nothing is lost. Effort is a guess:
S (a session), M (a few), L (a project).

Ideas that turned out to be rules moved into `principles.md` as children: never reboot (29,
P2.2), offline install (56, P3.1), the UI doesn't change unasked (36, P2.6), old hardware as a
target (73, P3.3), formats never die (74, P3.4), show-don't-interact (42, P9.3), ask once
then show (30, P2.7). The journal itself (1) is backbone B1, the semantic tree (51) is B5.

## Files and search

| ID | Idea | P | B | Old # | Effort |
|---|---|---|---|---|---|
| F1 | File manager: list virtualized, metadata on demand, inspector pane, preview layout per folder (not per selection), Space for full-size preview | P9, P2.3 | — | conv., 10 | M |
| F2 | Preview providers as sandboxed processes, rendering into shared GPU buffers (`BO_EXPORT`), thumbnails scaled by compute | P6.4, P5.2 | B3 | 5, 6 | M |
| F3 | Directory sizes for free: per-directory totals kept incrementally | P1.3 | B1 | 13 | M |
| F4 | Instant local search from an in-memory index fed by the journal; never the web unless asked; saved searches as live folders | P3.1, P5 | B1 | 11, 50 | M |
| F5 | Command palette over files, settings, windows, processes and plumber actions; natural language becomes a visible, editable filter | P7, P1 | B1, B5 | 12, 55 | M |
| F6 | Operations journal: each file action shows its shell equivalent; Ctrl+Z undoes moves/renames/deletes hours later | P4.1, P7 | B1 | 8, 9 | S |
| F7 | Versions of every small file, diffable in the inspector, searchable | P4.2 | B1 | 15, 59 | M |
| F8 | Provenance of files and fragments (creating program, origin URL, copied-from position); uninstall removes exactly what an app created | P1.5 | B1, B2 | 2, 16, 33, 76 | M |
| F9 | References by identity: shortcuts, recents and links survive moves and renames | P4.5 | B1 | 75 | M |
| F10 | Multi-selection inspector: diff of two texts, contact sheet of images, totals of a mix | P9.3 | — | 10 | S |
| F11 | Content hashes computed on idle CPU: duplicates, shared thumbnails, optional dedup | P1.3 | B1 | 19 | M |
| F12 | Folder README rendered on top; notes on a folder as an attribute | P1 | — | 23 | S |
| F13 | "Explain this folder": the LLM reasons over provenance, access stats and sizes | P1, P7 | B1, B2 | 24 | S |
| F14 | Lifestream view: the journal shown as a timeline of what happened today | P4.6 | B1 | 71 | S |
| F15 | 9P client: the host's or another machine's folders as places | P3 | — | 20 | M |
| F16 | Tear-off preview: a preview becomes a pinned floating window | P9 | B6 | 21 | S |

## Errors, attribution, diagnosis

| ID | Idea | P | B | Old # | Effort |
|---|---|---|---|---|---|
| E1 | Errors with causes: `EBUSY` names the holder (incl. "who has this open", eject), `ENOSPC` names the biggest consumers, `EACCES` the missing capability | P1.1 | B2 | 3, 25 | M |
| E2 | Hangs explained: the compositor labels a stalled window with the syscall/resource it waits on; a thread declared "UI" that blocks > X ms is logged with the culprit (BeOS discipline, measured) | P1.2, P6.3 | B2, B6 | 27, 38 | M |
| E3 | "Where did it go": disk (no "Other"), energy per process from RAPL, network per process with history, every outbound connection visible | P1.3, P8.4 | B2 | 26, 47, 65, 67 | M |
| E4 | Network diagnosis as a ladder (link, DHCP, gateway, DNS, HTTP) + captive-portal URL from DHCP | P1.1, P5 | B2 | 66 | M |
| E5 | Power: every wake and every sleep blocker has a named culprit; on battery with the lid closed, no network wakeups without a capability | P1.3, P2 | B2, B3 | 64 | M |
| E6 | Honest transfers: rate and bottleneck from the block layer; "safe to unplug" per file; dirty bytes left per device | P1.4 | B2 | 17, 35 | S |

## Compositor, input, attention

| ID | Idea | P | B | Old # | Effort |
|---|---|---|---|---|---|
| C1 | Focus ownership: typing guard, Enter delay on fresh dialogs, click on an inactive window only focuses | P2.1, P2.4 | B6 | 28, 45 | S |
| C2 | Key-to-photon latency measured end to end (USB HID → scanout) as a `/proc/kdebug` counter and a regression test | P2.5, P8.2 | — | 37 | M |
| C3 | Input methods and compose in the compositor; apps receive composed text | P5.2, P2.5 | B6 | 68 | M |
| C4 | Notifications: compositor-drawn only, user-owned channels, per-app budget, deferred while typing or fullscreen | P9.2 | B6 | 52 | M |
| C5 | Peripheral status channel (edge band/tint changing slowly): backup verified, dirty writes, energy hog | P9.1 | B6 | 72 | S |
| C6 | Per-monitor window memory by EDID serial, sleep-vs-unplug debounce, never off-screen | P2.3, P4.5 | B6 | 53 | S |
| C7 | Contrast measured by the compositor under translucency; glass darkens itself below a minimum | P8.2 | B6 | 48 | M |
| C8 | Clients survive a compositor crash and reconnect; layout restored; boot back into the last session | P4.4, P6.5 | B6 | 39, 41 | L |
| C9 | Work spheres: a project groups windows, files and terminals; one step restores it after an interruption | P4.6 | B1, B6 | 70 | M |
| C10 | Fullscreen = borderless window scanned out directly (instant Alt+Tab) | P2, P6 | — | 54 | S |
| C11 | Type-to-leap anywhere (Raskin's LEAP), the same in every app | P5.2 | B5 | 41 | S |
| C12 | One text engine; subpixel AA only on opaque 1x surfaces, chosen from EDID subpixel order | P5.2 | — | 69 | M |
| C13 | Opening a window never resizes the others (compare with GUI phase 4 decisions) | P2.3 | B6 | 44 | S |

## Capabilities and sharing

| ID | Idea | P | B | Old # | Effort |
|---|---|---|---|---|---|
| K1 | Powerbox: dragging a file to an app or picking it in the system open/save dialog grants that fd; the dialog defaults to the document's own folder and accepts drops from visible file windows | P6.1, P5.2 | B3 | 7, 32 | M |
| K2 | Clipboard as a system folder with history and several representations; default paste keeps semantics (emphasis, links), drops presentation | P4.4, P5.2 | — | 22, 34, 63 | M |
| K3 | Plumber: user-written rules route text (paths, `file:line`, PIDs) to actions; rules are files | P7 | B5 | 40 | M |
| K4 | Apps and settings as files (one settings app = a view; history and undo of settings) | P5.1, P7.2 | B1 | 31, 46 | M |
| K5 | Universal inspect: any window → process, fds, memory, GPU buffers, connections; `/proc` objects as first-class items | P7.1 | B2, B5 | 4, 43 | M |
| K6 | Kids' accounts: restrictions are missing capabilities, not bypassable overlays | P6.1 | B3 | 24 (edu) | S |

## Storage, updates, longevity

| ID | Idea | P | B | Old # | Effort |
|---|---|---|---|---|---|
| U1 | Run in draft: execute an installer/script in a VFS overlay, review the diff, commit or discard | P4.1, P6 | B1 | 14 | L |
| U2 | Backups verified by real sample restores; status = "last verified restore" | P8.1 | B1 | 58 | M |
| U3 | Atomic system generations with automatic fallback (from the metal loop) | P4.3 | B4 | 61 | M |
| U4 | Apps as self-contained directories, static except the pinned platform libraries; one updater for system and apps | P5.4, P3.4 | B4 | 60, 62 | M |
| U5 | Pinned UI version; a new one is tried in a window a few times before adoption | P2.6 | B4 | 77 | M |
| U6 | Inheritance and migration: threshold-shared disk key among trusted people; whole-system transfer | P3.5 | B4 | 78 | L |
| U7 | Daemon does the work, UI only watches (`filesd`): copies survive a UI crash and resume after reboot | P6.5, P4.4 | B1 | 18 | M |
| U8 | Driverless printing: IPP Everywhere over TCP + mDNS, IPP-over-USB | P5.3 | — | 57 | M |

## Accessibility and the agent

| ID | Idea | P | B | Old # | Effort |
|---|---|---|---|---|---|
| A1 | AccessKit-schema tree from every `gui` client; screen reader; the agent acts by role and name | P7, P8 | B5 | 51 | L |
| A2 | Command palette and agent share the tree (see F5) | P5 | B5 | 55 | — |

## Recorded but parked

- Mercury OS-style intent modules instead of apps (49): never shipped, no evidence; antecedent of `agentd`.
- Scrollable tiling (niri/PaperWM) as a layout: users split; only its rule (P2.3) is adopted.
