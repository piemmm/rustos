# The pre-boot Supervisor console

The **Supervisor** is a small, built-in command monitor an operator can drop
into from the boot screen *before* the encrypted root is mounted — a "quick
busybox, but different" for inspecting and controlling the machine while it is
still at the bootstrap floor. Its staged design is `plans/NEW-SUPERVISOR.md`.

It is an **in-kernel bootstrap-floor** facility (like the compiled-in `init`
and the interactive root-unlock policy), because it must run before the
root/`/System` app surface is usable. It is **not** an application bundle, and
it adds no user-facing ABI, syscall, or `lib/abi` type.

## The boot-screen contract

The on-screen wording is fixed and byte-exact:

1. `[Press ESC for supervisor]` is shown for two seconds on its own line, one
   blank line below the boot banner — the same one-blank-line spacing the
   `ARXFS passphrase: ` prompt uses, so the screen is laid out identically
   whether or not the operator enters the console.
2. If no key is pressed, the announcement is replaced **in place** by the
   `ARXFS passphrase: ` prompt (the blank line above it is preserved) and boot
   proceeds normally.
3. If `ESC` is pressed — during the window or at the passphrase prompt — the
   prompt/message line collapses to `ARXFS`, then a blank line, then
   `Supervisor`, and the REPL opens with the prompt `*`:

   ```
   ARXFS

   Supervisor
   *
   ```

The REPL echoes what the operator types like an ordinary command prompt: the
`[input active…]` secret-entry marker is shown **only** while a passphrase is
being typed (the `mount` command or the live unlock prompt), never for the
command line itself.

Leaving the REPL with `continue` resumes the normal boot (the passphrase
prompt is redrawn); `mount` performs the real unlock now and continues
without a second prompt.

## Where the code lives

The arch-neutral engine — the REPL, the command dispatcher, and every
built-in command — is the `no_std` crate `lib/supervisor`. It names no
architecture, board, or device and talks to the world only through
object-safe seams:

- `Report` — the output sink.
- `SupInput` — the keyboard byte source (an interrupt-driven, parking reader
  in the kernel; scripted bytes in a host test).
- `SupervisorHost` — the data and control seam the kernel implements over its
  existing subsystems (introspection, the memory-map RAM test, the partition
  reader, the hardware tree, the audit-log ring, the real unlock path, the
  port reset primitive).

The engine is a **presenter + control surface**: it computes nothing itself,
so the one source of truth for each datum stays in the subsystem that already
owns it. The ESC-vs-CSI disambiguation (`ESC` alone versus `ESC [ …` for the
arrow/Delete keys) is resolved in the shared line discipline `lib/vt`
(`LineEditor::resolve_escape` / `LineFeed::Escape`), driven by a bounded,
timed re-poll — never a busy-spin.

## The retained boot audit log

The `log` command tails the boot audit trail even after the serial console has
scrolled it away, so the recent security-relevant decisions are still
inspectable at the bootstrap floor. That retention is a **fan-out** on the
audit channel: each Tier-1 boot binary passes `BootInfo`'s audit sink a
`tairix_log::TeeSink` that delivers every record both to the port's serial
console (unchanged) *and* to a retained, tail-able in-memory ring
(`tairix_kernel_core::boot_audit_ring::BootAuditRing`). The ring keeps the most
recent `BOOT_AUDIT_RING_CAPACITY` records, overwriting the oldest, and a viewer
reads them back **non-destructively** by sequence — unlike the drain-once
`tairix_log::BootRing` the journal import consumes.

The fan-out is defined once (`TeeSink`); only the ring `static` is per-port,
because its interrupt-safe lock is parameterised by the architecture's
`InterruptControl` (`RflagsIrqControl` on x86_64, `DaifIrqControl` on aarch64,
`SstatusIrqControl` on riscv64) so a record copy masks the current CPU's
interrupts for its short, allocation-free duration — the ring is a `Sink` and
may be written from an interrupt handler that logs. Each record is stamped from
the one arch-neutral monotonic since-boot clock (`boot_audit_clock`, reading the
wait-queue timer); the earliest records, emitted before that clock is installed,
carry an honest zero rather than a fabricated instant, and the tail is ordered
by each record's strictly-increasing sequence regardless. The QEMU boot
verticals substitute their own audit sink, so retention is a production-only
wiring that never disturbs a test's audit interception.

## Command set

Control: `help`, `continue` (alias `boot`), `mount`, `reboot`, `poweroff`
(alias `halt`). Information: `version`, `mem` / `mem map`, `cpu`, `hw` (alias
`lsdev`), `disk`, `partitions`, `arxfs`, `ls`, `uptime`, `date`, `echo`,
`clear`. Diagnostics: `log`, `panic-log` (alias `last`), the interruptible
`memtest`, and the read-only `test disk`.

## Security

The Supervisor runs at full kernel authority at the physical console before
any user is authenticated, so its threat model is **physical-console access
only** — the physical-attacker class already out of scope for the charter.
The response is to audit loudly and fail closed, never to weaken a defence:

- `mount` runs the real passphrase unlock (no oracle, no fail-open); the
  typed passphrase lives in a fixed on-stack buffer wiped the instant the
  attempt resolves and never renders.
- No command reveals key material; every command is read-only unless it is an
  explicit, audited control action.
- Entering the console and every state-changing command emit a stable audit
  event on the hash-chained boot log; no event carries a secret.
