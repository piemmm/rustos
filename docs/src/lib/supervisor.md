# `tairix-supervisor`

The arch-neutral, `no_std` engine for the **pre-boot Supervisor console** —
the command monitor an operator drops into from the boot screen before the
encrypted root is mounted. The full design, boot-screen contract, command
reference, and security model live in
[The pre-boot Supervisor console](../architecture/supervisor.md); this page
records the crate's shape.

## Responsibility

`lib/supervisor` is a **presenter + control surface**, not a source of truth.
It holds the REPL, the command dispatcher, and every built-in command, and it
computes nothing itself: every datum it shows is rendered through the
`SupervisorHost` seam, which the kernel implements over its existing
subsystems (introspection, the memory-map RAM test, the partition reader, the
hardware tree, the audit-log ring, the real unlock path, the port reset
primitive). Keeping the crate a pure presenter means the one source of truth
for each datum stays where it already lives.

## Seams

The engine names no architecture, board, device, or `kernel/*` type. It talks
to the world only through object-safe seams:

- `Report` — the output sink, with allocation-free integer/hex formatting.
- `SupInput` — the keyboard byte source (a parking, interrupt-driven reader in
  the kernel; scripted bytes in a host test).
- `SupervisorHost` — the data and control seam (`version`, `mem`, `cpu`, `hw`,
  `disk`, `partitions`, `arxfs`, `ls`, `log`, `panic-log`, `uptime`, `date`,
  the interruptible `memtest`/`test disk`, and the audited control actions
  `mount`, `reboot`, `poweroff`).

`run_supervisor` drives the `*` REPL to a `SupervisorExit` (`ContinueBoot` or
`Mounted`) the boot path acts on. Nothing allocates and nothing panics on any
input.

## Stability

`experimental`. The whole engine is host-testable through the in-memory mocks
in `src/commands/test_support.rs`.
