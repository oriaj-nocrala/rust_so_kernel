# LLM as UI: an interface layer for agents

Status: **idea, not started** (handoff written 2026-10-07). The user is "a bit obsessed" with
it but has not decided to do it next.
Serves: [`software-on-demand.md`](software-on-demand.md) (the north star) and
[`education.md`](education.md); principles P7, P8, P4, P3.2 ([`../ux/principles.md`](../ux/principles.md)).
Related: [`threejs-runtime-plan.md`](threejs-runtime-plan.md) (the runtime the generated apps
run on), [`capabilities-plan.md`](capabilities-plan.md) (when the agent's authority must be
limited by the kernel), `docs/userland/roadmap.md` (HTTPS, dynamic linking, Claude Code).

## The idea

A layer at the level of GUI, CLI and TUI, but for a model: what a harness receives, so the
model drives the OS through tool calls (models are already trained for this). The end goal:

> A user asks for software ("I need a GarageBand", "teach me fractions"). The OS reads the
> user's local memories (likes football, likes 3D), writes the app, installs it, puts an icon
> on the desktop and opens it. Optionally the user publishes it to a shared repo of generated
> apps. For learning, the app is a microworld that shows errors by itself, not a chat that can
> be asked for answers: [`education.md`](education.md) (unguarded LLM help lowered exam scores
> 17% in Bastani et al., PNAS 2025).

It fits the long-term direction "an OS an LLM agent can observe and improve" (stage 2 of that
direction was already "a daemon in constanos that exposes tools + an MCP server").

## Decisions already taken

- **The model is Claude**, through Anthropic's API. No local LLM. What changes per stage is
  only where the *harness* (the tool loop) runs.
- **Not the Agent SDK / Claude Code inside constanos, for now.** The Agent SDK drives the
  Claude Code CLI, a Bun binary: dynamic linking, JIT, many syscalls (`userland/roadmap.md`
  step 3, far away). A native harness on the Messages API with tool use is the short path.

## Pieces

1. **Tool surface** (the "UI" itself), exposed by a daemon in constanos (`agentd`):
   - see: **the semantic UI tree** (backbone B5, `../ux/backbones.md`: every window's nodes with
     role, name, state, actions), list windows, read `/proc` (pci, kdebug, dmesg), read files;
     a screenshot only as the fallback for what the tree can't say (the compositor has **no
     capture request** today: new `gui` protocol message);
   - act: invoke a node's action by role and name; launch a program (`userspace::launch::spawn`
     path); keys/pointer as the fallback; close. The same tree is how the agent **tests** an
     app it wrote (P8) and what a screen reader reads;
   - create: write an app's files into its directory, install it (a line in
     `/mnt/etc/gui/apps` = `name<TAB>command`, icon in `/mnt/usr/share/icons`; `/mnt` is RW on
     the stick), uninstall;
   - remember: read/write the user's memory files (a profile directory). **They stay on the
     machine** (P3.2): a request carries only what it needs, and what was sent is visible
     (P1.3).
2. **Transport, by stage:**
   - **A. Claude Code on another machine + an MCP server.** `agentd` listens on TCP (works
     today: TCP client and server, `docs/reference/net.md`); the host side is either an MCP
     server in constanos speaking streamable HTTP, or a thin MCP bridge on the host that
     forwards to `agentd`'s own protocol. **Needs no HTTPS**: TCP is enough. First demo:
     "Claude drives my homemade OS".
   - **B. Native harness inside constanos.** A Rust std (musl) program: Messages API + tool use,
     the same tools in-process. Needs the HTTPS client (`userland/roadmap.md` step 1) and an
     API key on the machine.
   - (C. Claude Code itself: only if `userland/roadmap.md` step 3 says it is reachable.)
3. **App format:** defined once in [`software-on-demand.md`](software-on-demand.md) ("One app
   format"): a directory with a manifest (incl. requested capabilities and provenance), the
   entry, assets and the tests the agent ran. The launcher line and icon derive from it.
4. **App runtime:** the three.js runtime (`threejs-runtime-plan.md`). Without a compiler in
   constanos (C programs are built on the host with clang), "the model writes a program"
   means a script. TinyCC against mlibc is the native alternative, untested.
5. **Desktop entry point:** a prompt box (compositor window or panel button) so the user asks
   without a terminal.

## The demo to aim for

60 seconds on the Ryzen: type "teach me fractions", the agent writes a number-line microworld
themed on the user's interest (the flow in [`education.md`](education.md)), installs it with
its icon, opens it on the NVIDIA driver, and drives it once through the semantic tree to check
it works. Gate before building it: step 1 of `threejs-runtime-plan.md` (how many
model-written apps run unmodified).

## Ladder (each step is useful alone)

0. Capabilities first (the cornerstone: [`capabilities-plan.md`](capabilities-plan.md)), so
   `agentd` and the apps it installs start with only what they are granted.
1. `agentd` + transport A (tools: windows, launch, keys, read files; the semantic tree for the
   panel and `term` as the first "see" tool, `../ux/backbones.md` B5).
2. App install/uninstall tools + the app directory format.
3. The three.js runtime (its own plan), then "create an app" end to end through transport A.
4. HTTPS client, then transport B (harness inside constanos).
5. Prompt box in the desktop; memories.
6. Optional: shared app repo (publish/fetch): entries are request + code + tests + manifest,
   checked locally before first launch ([`software-on-demand.md`](software-on-demand.md)).

## Caveats

- **Memories of a student** (possibly a minor) sent to an API: privacy/consent matters for
  anything beyond a demo. Use a fictional user in demos.
- **Trademarks:** a public repo of generated apps with a famous superhero invites trouble; use
  generic characters.
- **Authority:** `agentd` runs with the capabilities its launcher grants; the apps it writes run
  in the runtime, itself in capability mode (`capabilities-plan.md`).
- **Destructive tools are reversible, not confirmed** (P4.1; confirmations train users to click
  Allow, P9): delete/overwrite outside the app dir happen inside a draft the user commits or
  discards (idea U1, `../ux/ideas.md`). Until U1 exists: snapshot the touched files into the
  app's undo directory before acting, and show the undo.
- Outreach rule: claim only what is measured (numbered boots, tests).

## Open questions

- MCP inside constanos (streamable HTTP server in Rust) vs a host bridge: which is less code.
- Screenshot format and size for the model (downscale on the constanos side).
- How the agent sees an app it wrote failing: the runtime's console + JS errors as a tool, plus
  the semantic tree (a button that does nothing is visible there).
