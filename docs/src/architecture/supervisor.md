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

1. `[Press ESC for supervisor]` is shown for two seconds.
2. If no key is pressed, it is replaced **in place** by the `ARXFS
   passphrase: ` prompt and boot proceeds normally.
3. If `ESC` is pressed — during the window or at the passphrase prompt — the
   prompt/message line collapses to `ARXFS`, then a blank line, then
   `Supervisor`, and the REPL opens with the prompt `* `:

   ```
   ARXFS

   Supervisor
   *
   ```

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
