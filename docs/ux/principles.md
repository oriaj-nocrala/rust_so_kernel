# constanos UX principles

Status: agreed direction (2026-10-08), distilled from a research pass over user complaints
about Windows, macOS and Linux desktops, HCI literature and learning science. Every design
doc should say which of these it serves, and must not contradict one without saying so
(see [`audit-2026-10.md`](audit-2026-10.md)).

Related: [`backbones.md`](backbones.md) (the infrastructure these need),
[`ideas.md`](ideas.md) (concrete features, each tied to a principle),
[`../ai/software-on-demand.md`](../ai/software-on-demand.md), [`../ai/education.md`](../ai/education.md).

## How this list was built

- 27 candidate principles were found. Duplicates were merged; principles that are distinct
  but are cases of a broader one became **children** of a **parent**. 9 parents remain.
  Domain principles (education, generated software) live in their own docs and point here.
- **Order = evidence count**: how many independent complaints, studies or precedents from the
  research support the parent (listed under each). Caveat: the count measures what was
  searched, not the whole world; some evidence supports two parents and is counted in both.
- **Why big OSes don't fix these:** almost every one needs two layers to agree (kernel and
  compositor, VFS and file manager, driver and settings) and in Windows/macOS/Linux those
  layers belong to different teams or companies. constanos has every layer in one repo. The
  same risk exists here between coding-agent sessions; P5 is the rule against it.
- Each child is a **rule a test can check**, like the kernel invariants in `CLAUDE.md`.

## When principles pull apart

- **P1 (explain) vs P9 (attention):** everything is explained and available on demand; normal
  state goes to the periphery; only what needs a user action interrupts. A verified backup is
  a green tint at the edge; a failed one is a notification that can't be dismissed until seen.
  Never implement P1 by adding popups.
- **P3.4 (pinned versions last) vs P1/P8 (a known flaw must be visible):** an app may stay pinned
  to old platform libraries, but the system says so when that version has a known problem.

---

## P1. The system explains itself — evidence 15

Nothing happens, fails or consumes without a reason the user can see, in plain words, at the
layer where it happened. Seams are shown on purpose, not hidden (Chalmers' *seamful design*).

| Child | Rule |
|---|---|
| P1.1 Failures name their cause | A failed operation says which layer failed and why, with the culprit if there is one ("held by doom, PID 42"), not a bare code. |
| P1.2 Waits name what they wait on | A stalled window or task shows the resource it is blocked on ("waiting for /mnt (USB) for 12 s"). |
| P1.3 Every resource has a visible owner | Bytes on disk, energy, network traffic, wakeups, sleep blockers and open files are attributed to a process or a person. No "System Data"/"Other" bucket. |
| P1.4 Estimates are honest | Progress and time estimates come from measured throughput and name the bottleneck; "done" means the data is really on the device. |
| P1.5 Where things came from is recorded | Files, copied fragments, apps and settings changes carry their provenance (which program, from where, when). |

Evidence: "file in use" without naming the process (Windows); "disk wasn't ejected because one
or more programs *may* be using it" (macOS); "Something went wrong" + 0x8007023f (Xbox);
macOS "System Data" 240 GB with no breakdown; the beachball gives no reason; Time Machine
stopping silently for two months; sleep blockers only via `pmset -g assertions` /
`powercfg /requests`; Modern Standby wakes in a bag with no record; "connected, no internet";
no per-process network history (macOS) or view at all (Linux needs `nethogs`); SMART reporting
OK on a dying disk; copy-time estimates (xkcd 612); uninstall leftovers nobody can attribute;
Chalmers & MacColl (2003) on seamful design; Tognazzini & Norman (2015) on lost
discoverability and feedback.

## P2. Nothing acts without you; your input arrives intact — evidence 14

The user originates every change to their session, and every keystroke and click reaches what
they aimed at, without loss or delay.

| Child | Rule |
|---|---|
| P2.1 Focus has an owner | A new window does not receive the keyboard while the user is typing, unless the user's own action opened it; a fresh dialog ignores Enter for ~500 ms. |
| P2.2 No unrequested restarts | The system never reboots, logs out or closes an app on its own. |
| P2.3 Things stay where you put them | Opening, closing or a monitor waking never moves or resizes other windows; no window is ever off every screen. |
| P2.4 A click on an inactive window only focuses it | It never activates the control under the pointer. |
| P2.5 Input is never lost or late | Keystrokes typed before a surface appears are kept; key-to-photon latency is a measured invariant. |
| P2.6 Change at the user's pace | The UI does not change shape on update; the user adopts a new UI version when they choose (old one stays pinnable). Settings survive every update. |
| P2.7 Ask once, then show | A granted permission is never re-asked; ongoing use is shown (indicator + log), not prompted. |

Evidence: focus stealing (KDE bugs 364689, 324845; Launchpad 54741; r/windows; a 2026 Claude
Code issue); Windows Update forced restarts, incl. a 16-hour computation; macOS Sequoia
screen-recording prompts (daily → weekly → monthly, 20+ cascading in Teams); Windows 11 UI
"changing almost monthly"; Copilot and Start-menu ads pushed in; forced Microsoft account in
setup; windows moving to the wrong monitor after sleep (Intel: an OS design issue); clicks on
inactive windows triggering buttons (OSNews); the Start menu dropping the first keystrokes;
Dan Luu: 30 ms on an Apple IIe vs 100-200 ms on modern machines; dead keys broken per toolkit
(GTK 4.20, Plasma 6); Liquid Glass imposed, later a slider; older adults with MCI: UI change
causes anxiety, a magnifier setting lost after an update; tiling WMs reshuffling every window
(niri's reason to exist).

## P3. Yours, without third parties, for decades — evidence 12

The machine, the data and the system's usefulness belong to the user: nothing requires a
network, an account or a vendor, and none of it expires with a company's support cycle.

| Child | Rule |
|---|---|
| P3.1 Works offline, no account | Install, first boot, search and every base feature work with no network and no account. |
| P3.2 Local first | Personal data (files, memories, the learner model) lives on the machine; anything sent out is explicit and visible (P1.3). |
| P3.3 Old hardware is a target | If an x86-64 UEFI machine works, constanos runs on it; no TPM or CPU-generation gates. |
| P3.4 Formats never die | A decoder the system ever shipped is never removed; an app keeps running unchanged: apps are static except the platform libraries, which are content-addressed and pinned per generation (policy: `docs/userland/roadmap.md` step 2). |
| P3.5 Data survives its owner and its machine | Recovery and inheritance (threshold-shared keys) and whole-system migration need no cloud. |

Evidence: Bing results in local search (fixed only in a 26H2 test build); the Windows 11 Calendar
widget works only with Outlook.com; the forced account in setup; 240-400 M PCs made e-waste
by Windows 11's TPM/CPU gates (Canalys, PIRG); Windows 10 end of support (2025-10-14); printer
drivers abandoned by vendors; Vint Cerf's "digital dark age"; families locked out of a dead
person's accounts and phone; a minor's memories sent to an API (`llm-as-ui-plan.md` caveat);
Canonical-only Snap store; Ink & Switch's local-first essay; permacomputing.

## P4. Nothing is lost — evidence 12

Every change can be undone, every state survives a crash, and every reference survives the
thing moving.

| Child | Rule |
|---|---|
| P4.1 Everything is undoable | File operations, settings changes, installs and system updates can be reversed. |
| P4.2 Versions of everything | Overwritten files keep recent versions, independent of the app, searchable. |
| P4.3 Updates are atomic generations | A system update boots whole or falls back whole; previous generations stay bootable. |
| P4.4 State survives crashes | A compositor or app crash does not lose window layout, work spheres or the clipboard. |
| P4.5 Identity, not location | References (shortcuts, recents, links, monitors) point at identity (inode+generation, EDID serial), so moving or renaming never breaks them. |
| P4.6 Resuming is free | After an interruption the whole working context (windows, files, positions) comes back in one step. |

Evidence: Apple HIG 1987 "forgiveness" and Tognazzini/Norman on its loss; macOS document
versions exist but only per app and unsearchable (Eclectic Light); work lost to forced
restarts; NixOS/Silverblue generations as the loved counter-example; "Working on updates 74%"
for 24 h and forced power-offs; Time Machine restores that fail; Arcan/Durden restoring
layouts after a crash; windows lost off-screen after monitor sleep; Xanadu's "never
overwrite, always version" vs broken web links; Gloria Mark: 23 min to resume interrupted
work; settings lost on update; the X11 clipboard dying with the app.

## P5. One concept, one implementation, at the layer that understands it — evidence 12

Text, input, notifications, file dialogs, settings, clipboard, updates and device classes each
have exactly one implementation, placed in the lowest layer that has the information. This is
also the rule between agent sessions: before adding a mechanism, find the existing one.

| Child | Rule |
|---|---|
| P5.1 No second panel | Every setting is a file; every settings UI is a view over those files. |
| P5.2 System-owned shared UI | File open/save, notifications, the clipboard, input methods and text rendering are system services, not per-toolkit copies. |
| P5.3 Class drivers first | Hardware with a standard class (USB Audio, HID, Mass Storage, IPP printing) gets one generic driver; vendor code is a thin control layer. |
| P5.4 One updater | The system updates itself and every app through one mechanism. |
| P5.5 Design docs cite these principles | A new design doc names the principles it serves and checks [`audit`](audit-2026-10.md)-style conflicts before it is accepted. |

Evidence: Windows Settings vs Control Panel ("one place for all settings" unfulfilled in
2025); save dialogs that don't know the document's folder, per app (macOS, GNOME);
the GTK file picker not making thumbnails (2004-2024); dead keys working in Firefox but not
in Telegram; ClearType vs greyscale per app (Firefox flipping on hover); apps bypassing the
Windows notification system; per-app audio output selection; Flatpak apps ignoring the
desktop theme; mixed DPI blurry under XWayland; "Paste and Match Style" meaning different
things per app; per-app background updaters fighting shutdown; IPP Everywhere and
class-compliant Focusrite audio as the positive case.

## P6. Nothing escapes its box — evidence 10

A bug or a malicious component can only damage what it was explicitly given. No ambient
authority; no layer waits on a lower-trust one.

| Child | Rule |
|---|---|
| P6.1 Least authority by default | Programs start with no capabilities; a user action (drag, open dialog) grants a specific one. |
| P6.2 Risk follows reach, not author | A component's allowed privilege depends on what it can touch (no DMA, DMA+IOMMU, kernel), never on who wrote it. Generated code never runs in the kernel. |
| P6.3 The compositor never waits on a client | A hung client can always be moved, minimized and closed. |
| P6.4 Untrusted parsers run sandboxed | Previews, thumbnails and file decoders run in capability-less processes. |
| P6.5 Crashes are contained | A failing driver, service or compositor restarts without taking others down (supervision, reconnection). |

Evidence: the beachball and mutter #287 ("an unresponsive window freezes the whole desktop");
a lost server freezing Finder for minutes; third-party drivers caused 70% of Windows crashes
(Microsoft, data to 2004) and 85% on XP (Microsoft Research); CrowdStrike 2024 (an
out-of-bounds read in a kernel driver); exclusive fullscreen crashing on Alt+Tab;
Quick Look / preview-handler vulnerabilities; slopsquatting (19.7% of LLM code samples import
a nonexistent package, USENIX Security 2025); Screen Time bypassed by FaceTime and
bugs; Arcan's crash-resilient compositing and BeOS's per-window threads as positive cases.

## P7. Everything can be inspected and changed — evidence 9

Any object on screen or in the system (window, process, file, setting, app) can be opened,
understood and modified by its user, with an LLM's help if needed.

| Child | Rule |
|---|---|
| P7.1 Universal inspect | Any window/process/file can be inspected: its process, fds, memory, GPU buffers, connections, provenance. |
| P7.2 State as files | Apps and system services expose state and settings as readable/writable files. |
| P7.3 Symmetric authoring | Any app, including a generated one, can be opened as source and edited by its user. |
| P7.4 Comprehensible by one person with an LLM | Every subsystem has docs that let a fresh session understand it in one sitting (Wirth's rule, updated). |

Evidence: Wirth, "A Plea for Lean Software" (Oberon's core: 4,623 lines); Pharo's moldable
inspector; Plan 9 (acme buffers are files; the plumber); Ink & Switch's malleable software
(2025); Alan Kay on the iPad "betraying" the Dynabook (no symmetric authoring); Papert's
microworlds, Resnick's low floors/high ceilings, Etoys; Spotlight closed vs Raycast/Alfred
extensible; Finder lacking "copy path"/"open in terminal"; Robin Sloan's home-cooked apps.

## P8. Promises are measured — evidence 8

Whatever the system claims (backed up, fast, legible, learned, safe) it checks by a different
path than the one that did the work, continuously.

| Child | Rule |
|---|---|
| P8.1 Backups are restored | A backup counts only after a sample was really restored and compared. |
| P8.2 Perception metrics are invariants | Input latency, frame pacing and text contrast over translucency are measured and regressions are bugs. |
| P8.3 Measure outcomes, not activity | Learning is measured by delayed unaided recall, never practice scores or streaks; health by observed behaviour, not self-reports. |
| P8.4 Architecture over promises | "No ads", "no telemetry" are enforced structurally (visible connections, capabilities), not by policy. |

Evidence: Time Machine silent failures and the "restore a few files each quarter" advice;
SMART false OK; Dan Luu's camera-measured latency; Liquid Glass contrast complaints and
Apple's rollback; Bastani et al. (practice +48%, exam −17%); Microsoft promising to remove
ads it added; Bloom's unreplicated 2σ; copy estimates.

## P9. Attention is the scarcest resource — evidence 7

The system uses the periphery for status, interrupts only for what needs primary attention,
and never optimises for engagement.

| Child | Rule |
|---|---|
| P9.1 Status lives in the periphery | Ambient state changes slowly at the edge; popups are for what needs action. |
| P9.2 Interruptions have a budget | Notifications have user-owned channels and a per-app budget; excess goes to history and is reported. |
| P9.3 Show, don't make me interact | Information visible without hover or clicks where it fits (Bret Victor, *Magic Ink*). |
| P9.4 No engagement mechanics | No streaks, points, badges or ads anywhere in the system. |

Evidence: Gloria Mark's interruption studies; Weiser & Brown / Amber Case's calm technology;
all-or-nothing notifications vs Android's channels; Duolingo streak burnout; Start-menu
"recommendations"; Sequoia's prompts conditioning users to click Allow; Mercury OS's
design for limited executive function.

---

## Domain principles (own docs)

- Education: [`../ai/education.md`](../ai/education.md) (the LLM authors the material; desirable
  difficulty; adaptive guidance; an adult in the loop). Its other rules are cases of P1, P3, P7,
  P8, P9.
- Software on demand: [`../ai/software-on-demand.md`](../ai/software-on-demand.md). Its rules
  are cases of P1.5, P6 and P8.

## Not principles (yet)

- Personality and delight (Susan Kare: meaningful, memorable, clear; Luna's quirks): real, but no
  testable rule found.
- Audio routing to the wrong device: evidence was only support pages; covered by P5.2 if it
  turns out to be real.
