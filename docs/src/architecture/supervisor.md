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

## Rich screens: colour and positioning

Colour and cursor positioning at the bootstrap floor cost nothing new — the
console is already a byte stream that consumes escape sequences, and `lib/vt`
already has a complete, arch-neutral, allocation-free VT emitter (`Op` +
`emit::encode_into`). The `lib/supervisor::screen` module is a thin
`Op`-building layer over that emitter: a `Screen` presenter that offers
`move_to`, `clear`, `set_style` / `reset_style`, and `enter_fullscreen` /
`leave_fullscreen`, plus a typed `Style` (foreground/background `Color` and the
common attributes) mapped to `Sgr`. It never hand-rolls a second copy of the
CSI/SGR encoding — the charter forbids that duplication, and a host test asserts
the emitted bytes equal `lib/vt`'s encoding of the corresponding `Op`s.

Two properties make it safe at the floor:

- **Plain fallback.** A `Screen` built with `plain` set emits **no** escape
  bytes — only text — so a genuinely dumb serial line still shows usable
  output. The choice is one injected flag; there is no probe (the write seam is
  one-way, and the `TERM`/`lib/termcap` database lives on the not-yet-mounted
  `/System`).
- **Bounded geometry.** With no way to query the console size, a full-screen
  layout assumes a conservative `Geometry` (80×24 by default, threaded in as
  data rather than a per-board constant). Every position is clamped into that
  geometry, so a malformed or oversized coordinate is pinned to the edge rather
  than positioning off-screen, and nothing panics.

The presenter stands alone; the destructive `memtest full` takeover UI
(`plans/NEW-SUPERVISOR.md` §9) is its first full-screen consumer.

## The whole-RAM destructive test engine

The safe `memtest` command tests only RAM the running kernel does not hold: it
allocates free frames, tests each, and frees them, so it can never exercise the
memory the kernel image, heap, page tables, or stacks occupy. Testing *all* of
RAM needs a one-way takeover — the machine is handed to the test and only a
reset follows — which the future `memtest full` command will drive.

The arch-neutral core of that takeover already exists as
`tairix_kernel_mem::ramtest::run_destructive`. Where the boot sanity check
(`run`) samples one word per page and restores every cell it touches, the
destructive engine tests **every** word of every reachable usable region —
a full address-in-address pass plus a two-direction moving-inversions pass —
and, because the machine never resumes, does **not** restore them. It reports
progress as `(tested, total)`, honours an operator `abort` polled between
windows, and returns a `DestructiveOutcome` (`Passed` / `Aborted` / `Faulted`)
so an early abort can never be mistaken for a clean pass. It reuses the same
`WordWindow` / `PhysWindow` primitives and safe, range-checked physical-map
access as the non-destructive path — no raw pointer arithmetic — and is fully
host-tested over a fault-injecting fake. The takeover *mechanism* that quiesces
the machine before it runs (an Arch HAL slice) and the confirmed `memtest full`
command that drives it exist (see *Machine takeover* below); the per-port
takeover bodies and the full-screen progress UI are the remaining stages of
`plans/NEW-SUPERVISOR.md` §9.

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
`memtest` (and its destructive one-way variant `memtest full`, alias
`memtest --takeover`), and the read-only `test disk`.

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

## Machine takeover (destructive whole-RAM test — foundation)

The in-system `memtest` can only test RAM it explicitly owns; it can never
test the frames the kernel image, heap, page tables, or stacks occupy. Testing
*all* of RAM requires owning the whole machine — stopping every other CPU,
masking interrupts, stopping the lockup watchdog, and flattening paging so a
small self-contained test routine can address physical RAM — which is a
one-way trip whose only exits are reset or power-off.

That takeover mechanism is irreducibly per-architecture, so it lives behind an
Arch HAL slice, `MachineTakeover` (`kernel/arch/api/src/takeover.rs`):

- `quiesce_secondaries` stops every other CPU into a bounded, controlled halt
  (a bounded tear-down handshake, never an unbounded spin) and fails closed
  with `CpuQuiesceTimeout` if a core does not acknowledge in budget.
- `prepare_takeover` masks interrupts, stops the watchdog, and
  relocates/flattens paging for the destructive sweep.

Both steps fail closed (`TakeoverError`), never panic, and leave the machine
running and recoverable on any error. `KernelArch::machine_takeover` defaults
to `None`, so a port that has not wired the mechanism (and `wasm32`, which owns
no physical RAM to take over) honestly reports "not supported" and the
Supervisor stays in the REPL.

### The operator command and its supervisor-only gate

The destructive test is driven by a distinct command, `memtest full` (alias
`memtest --takeover`), kept separate from the safe `memtest`. Because it is
irreversible it demands an explicit typed confirmation — the operator must
type `DESTROY` exactly; anything else (a mistyped, blank, or over-long entry)
cancels fail-closed and changes nothing. Only once confirmed is the decision
audited (`4157 supervisor: destructive memtest-full machine takeover
confirmed`, a `Warn` on the hash-chained boot log) — recorded *before* the
attempt, synchronously, because a successful takeover destroys the in-memory
audit ring and never returns.

The takeover handle is reachable **only** from this path. Obtaining it through
`KernelArch::machine_takeover` requires a
`tairix_kernel_core::supervisor_system::TakeoverGrant` — a witness whose
constructor is private to `supervisor_system`, the module that drives the
confirmed `memtest full`. No other kernel subsystem, driver, or userland
caller can mint the grant, so none can obtain the `MachineTakeover` handle or
invoke its `unsafe` steps: the accessor is the single gate and the grant is
its only key. Once the ordered `quiesce_secondaries` → `prepare_takeover`
handshake succeeds, the arch-neutral `run_destructive` sweep runs over the
direct physical map and the machine is reset; it never returns.

The arch-neutral destructive sweep already exists
(`tairix_kernel_mem::ramtest::run_destructive`). Until a port wires the
per-port takeover mechanism, `memtest full` reports "not supported" and stays
in the REPL, and the safe, bounded, interruptible in-system `memtest` is the
only RAM test that actually runs; the full-screen progress UI is the last
remaining stage (`plans/NEW-SUPERVISOR.md` §9).
