# Driver subsystem — documentation

How devices and drivers are structured in this kernel, and where that structure is
heading.

## Documents

- **[architecture.md](architecture.md)** — the design *as it exists today*: the two-layer
  split (VFS `FileHandle` vs. hardware driver), the `hal` crate and the `PortIo`/`PhysMem`
  hardware seams, the `Driver` trait + boot registry, and the testing story. Read this to
  understand *how the pieces fit and why*.
- **[roadmap.md](roadmap.md)** — the *trajectory*: the phased plan from today's thin trait
  layer toward a Linux-class device model, and the honest long-term ambitions beyond that
  (a stable driver contract, foreign-driver compatibility à la BSD, and — as a north star —
  running proprietary/Nvidia drivers). Read this to understand *what each step is buying and
  where it leads*.

## Related, elsewhere in the repo

- **How to actually write/port a driver:** the `kernel-drivers` skill
  (`.claude/skills/kernel-drivers/SKILL.md`) — the imperative step-by-step playbook.
- **Current subsystem map:** the "Device Driver Framework" and related sections of
  `CLAUDE.md`.
- **Worked reference implementation:** the ACPI driver — `hal/src/acpi.rs` (pure, tested),
  `kernel/src/acpi.rs` (adapter), `kernel/src/hal.rs` (seams + registry).
