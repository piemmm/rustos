# tairix-supervisor

The pre-boot **Supervisor** console engine: the arch-neutral, `no_std`
command monitor an operator drops into from the boot screen *before* the
encrypted root is mounted — a "quick busybox, but different" for inspecting
and controlling the machine while it is still at the bootstrap floor
(`plans/NEW-SUPERVISOR.md`).

The boot path draws a brief `[Press ESC for supervisor]` window; pressing
`ESC` there (or at the passphrase prompt) enters this REPL, whose prompt is
`*`.

## What it is

A **presenter + control surface**, not a source of truth. Every datum it
shows — the kernel version, the memory map, the hardware tree, the partition
table, the boot audit log — is already computed by an existing kernel
subsystem. This crate computes none of it: it renders through the
`SupervisorHost` seam, which the kernel implements over those subsystems, so
the one source of truth stays where it already lives (`AGENTS.md` §2.2) and
this crate stays tiny and arch-neutral (§2.20, §17.4).

It names no architecture, board, device, or `kernel/*` type. It talks to the
world only through object-safe seams:

- `Report` — the output sink (the console in the kernel, an in-memory buffer
  in a test), with allocation-free integer/hex formatting helpers.
- `SupInput` — the keyboard byte source (the interrupt-driven, parking
  console reader in the kernel; scripted bytes in a test).
- `SupervisorHost` — the data and control seam: `version`, `mem`/`mem map`,
  `cpu`, `hw`, `disk`, `partitions`, `arxfs`, `ls`, `log`, `panic-log`,
  `uptime`, `date`, the interruptible `memtest`/`test disk`, plus the audited
  control actions `mount`, `reboot`, `poweroff`.

Nothing here allocates — the bootstrap floor cannot assume a heap — and
nothing panics on any input (`AGENTS.md` §2.9).

## Rich screens

`screen::Screen` is a thin colour/positioning presenter over the shared
`lib/vt` `Op`/`emit` vocabulary — `move_to`, `clear`, `set_style`,
`enter_fullscreen`/`leave_fullscreen`, and a typed `Style` — that never
hand-rolls a second copy of the terminal encoding (§2.2) and offers a plain
(escape-free) fallback for a dumb serial line. `memtest_ui::MemtestUi` is its
first full-screen consumer: the memtest86-style progress display the
destructive `memtest full` takeover renders through, driven only from the
engine's `(tested, total)` progress and final outcome.

## Security

The Supervisor runs at full kernel authority at the physical console before
any user is authenticated, so its threat model is **physical-console access
only** — the physical-attacker class the charter already places out of scope
(§19.9). That is a reason to audit loudly and fail closed, never to weaken a
defence:

- `mount` runs the **real** passphrase unlock — no oracle, no fail-open — and
  the typed passphrase lives in a fixed on-stack buffer wiped the instant the
  attempt resolves; it never renders and never reaches the output.
- No command reveals key material; every command is read-only unless it is an
  explicit, audited control action.
- Entering the console and every state-changing command emit a semantic
  `SupervisorEvent` the kernel maps onto the hash-chained audit log; no event
  ever carries a secret.

## Stability

`experimental`.

## Testing

The whole engine is host-testable through the in-memory mocks in
`src/commands/test_support.rs`: every command, the dispatcher and tokeniser,
the REPL loop, the `mount` secret hygiene, and the `memtest`/`test disk`
abort path. Run `cargo test -p tairix-supervisor`.
