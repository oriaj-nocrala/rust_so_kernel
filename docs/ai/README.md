# AI direction: handoffs

Three independent plans, none started (2026-10-07). Pick any one; each says what it needs from the others.

- [`threejs-runtime-plan.md`](threejs-runtime-plan.md): run unmodified three.js natively (QuickJS + wgpu on NVK). First step is a host spike plus a benchmark of how many Claude-written apps run unmodified.
- [`llm-as-ui-plan.md`](llm-as-ui-plan.md): a tool-call interface for agents (`agentd`), first over TCP to Claude Code on another machine, later a native harness; the "learn fractions" demo.
- [`capabilities-plan.md`](capabilities-plan.md): Capsicum-style capabilities as the kernel's first security model; good idea, not on the critical path of the other two.
