# LLM as UI: an interface layer for agents

Status: **idea, not started** (handoff written 2026-10-07). The user is "a bit obsessed" with
it but has not decided to do it next.
Related: [`threejs-runtime-plan.md`](threejs-runtime-plan.md) (the runtime the generated apps
run on), [`capabilities-plan.md`](capabilities-plan.md) (when the agent's authority must be
limited by the kernel), `docs/userland/roadmap.md` (HTTPS, dynamic linking, Claude Code).

## The idea

A layer at the level of GUI, CLI and TUI, but for a model: what a harness receives, so the
model drives the OS through tool calls (models are already trained for this). The end goal:

> A student tells the OS "I need to learn fractions". The OS reads its memories (likes a
> superhero, likes 3D games), writes a 3D fractions game, installs it, puts an icon on the
> desktop and opens it full screen. Optionally the user publishes it to a shared repo of
> generated apps ("constanos vibed apps").

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
   - see: list windows, screenshot (the compositor has **no capture request** today: new `gui`
     protocol message), read `/proc` (pci, kdebug, dmesg), read files;
   - act: launch a program (`userspace::launch::spawn` path), send keys/pointer, close;
   - create: write an app's files into its directory, install it (a line in
     `/mnt/etc/gui/apps` = `name<TAB>command`, icon in `/mnt/usr/share/icons`; `/mnt` is RW on
     the stick), uninstall;
   - remember: read/write the user's memory files (a profile directory).
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
3. **App format:** one directory per app (`/mnt/apps/<name>/`): `manifest` (name, icon, entry,
   what it may touch), `main.js` (three.js runtime) or a native binary, assets. The launcher
   line and the icon are derived from the manifest.
4. **App runtime:** the three.js runtime (`threejs-runtime-plan.md`). Without a compiler in
   constanos (C programs are built on the host with clang), "the model writes a program"
   means a script. TinyCC against mlibc is the native alternative, untested.
5. **Desktop entry point:** a prompt box (compositor window or panel button) so the user asks
   without a terminal.

## The demo to aim for

60 seconds on the Ryzen: type "I want to learn fractions with a 3D game", the agent writes
the app, installs it with its icon, opens it full screen on the NVIDIA driver. Gate before
building it: step 1 of `threejs-runtime-plan.md` (how many model-written apps run unmodified).

## Ladder (each step is useful alone)

1. `agentd` + transport A (tools: windows, launch, keys, read files; screenshot added to `gui`).
2. App install/uninstall tools + the app directory format.
3. The three.js runtime (its own plan), then "create an app" end to end through transport A.
4. HTTPS client, then transport B (harness inside constanos).
5. Prompt box in the desktop; memories.
6. Optional: shared app repo (publish/fetch), with a review step.

## Caveats

- **Memories of a student** (possibly a minor) sent to an API: privacy/consent matters for
  anything beyond a demo. Use a fictional user in demos.
- **Trademarks:** a public repo of generated apps with a famous superhero invites trouble; use
  generic characters.
- **Authority:** in stage A/B the agent can do anything the tools allow; the apps it writes are
  limited by the runtime's API (see `capabilities-plan.md`). Keep destructive tools (delete,
  overwrite outside the app dir) behind user confirmation.
- Outreach rule: claim only what is measured (numbered boots, tests).

## Open questions

- MCP inside constanos (streamable HTTP server in Rust) vs a host bridge: which is less code.
- Screenshot format and size for the model (downscale on the constanos side).
- How the agent sees an app it wrote failing (the runtime's console + JS errors as a tool).
