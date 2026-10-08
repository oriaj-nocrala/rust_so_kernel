# AI direction: handoffs

North star: [`software-on-demand.md`](software-on-demand.md) (the LLM builds, installs and runs software for its user; drivers by a risk ladder; a shared repo). Hardest case: [`education.md`](education.md). Both follow [`../ux/principles.md`](../ux/principles.md); conflicts in the plans below are listed in [`../ux/audit-2026-10.md`](../ux/audit-2026-10.md).

Three plans, none started (2026-10-07). Capabilities first; each says what it needs from the others.

- [`threejs-runtime-plan.md`](threejs-runtime-plan.md): run unmodified three.js natively (QuickJS + wgpu on NVK). First step is a host spike plus a benchmark of how many Claude-written apps run unmodified.
- [`llm-as-ui-plan.md`](llm-as-ui-plan.md): a tool-call interface for agents (`agentd`), first over TCP to Claude Code on another machine, later a native harness; the "learn fractions" demo.
- [`capabilities-plan.md`](capabilities-plan.md): Capsicum-style capabilities as the kernel's first security model; **the cornerstone** (2026-10-08): previews, the repo, native generated code, userland drivers and kids' accounts depend on it. Do it first.
