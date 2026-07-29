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

## End-to-end boot test

The command dispatcher, the REPL, and the ESC/timeout state machine are
covered by host unit tests over mock seams, and a fuzz harness drives the REPL
line/command parser against hostile console input
(`tairix-supervisor`'s `fuzz_repl` target). The boot-screen contract itself is
proven on the *production* boot path by two sibling QEMU verticals,
`tairix-test-supervisor-esc-qemu-aarch64` and
`tairix-test-supervisor-esc-qemu-x86-64`: each boots the real pipeline for its
architecture with a planted encrypted-root disk, presses `ESC` at the boot
screen, runs a read-only command at the `*` prompt, `continue`s, and then
unlocks. Both drive one shared serial script (`tools/xtask`'s
`SUPERVISOR_ESC_SCRIPT` — the frozen boot-screen contract has a single
definition, never a per-arch copy) that walks the screen states in order
(`[Press ESC for supervisor]` → the `Supervisor` banner → the
`Supervisor commands:` header `help` renders → the redrawn `ARXFS passphrase: `
prompt), so reaching each marker is the byte-exact assertion, and the run
passes only once the unlock-service install message proves `continue` resumed
the normal unlock and it mounted the root — i.e. that a Supervisor session is
transparent to boot. The aarch64 sibling drives the announcement-window entry
over the UART and is robust to the 2-second window race; the x86_64 sibling
proves the same contract on the COM1 console, closing the
`plans/ARCHSUPPORT.md` parity gap.

Two further aarch64 verticals cover the *other* two trigger points on the same
production path, **reusing the identical `tairix-test-supervisor-esc-qemu-aarch64`
guest** — only the host-side serial script differs, so there is no second boot
bin. `SUPERVISOR_ESC_AT_PROMPT_SCRIPT` waits for the redrawn `ARXFS passphrase: `
prompt (which appears only after the 2-second window elapses untouched) and then
types a lone `ESC` as the line's first byte, deterministically exercising the
live-passphrase-prompt drop (`read_passphrase_line`'s `Escape` outcome) rather
than the window race. `SUPERVISOR_MOUNT_SCRIPT` runs the `mount` command inside
the REPL — the Supervisor performing the real unlock itself, distinct from
`continue` resuming the normal unlock. Because several enrolments now share one
built binary, the runner (`tools/xtask`'s `backing_image_path`) gives each a
distinct planted-image path keyed on its stable index in the enrolment table, so
the concurrent matrix cannot let one run clobber another's image; a host test
asserts those paths never collide and that all three scripts spell the frozen
boot-screen states through one shared set of markers.

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

The presenter stands alone; the `memtest` takeover UI
(`lib/supervisor::memtest_ui`, `plans/NEW-SUPERVISOR.md` §9) is its first
full-screen consumer — see *Machine takeover* below.

## The whole-RAM test engine

Any RAM test that runs inside the live kernel can test only the RAM the kernel
does not hold — it cannot exercise the memory the kernel image, heap, page
tables, or stacks occupy without destroying the running system. So there is no
"safe" partial test: the single `memtest` command tests *all* of RAM by a
one-way takeover — the machine is handed to the test and only a reset follows.

The arch-neutral core of that takeover is `tairix_kernel_mem::ramtest`'s
`sweep_pattern`. Where the boot sanity check (`run`) samples one word per page
and restores every cell it touches, `sweep_pattern` tests **every** word of
every reachable usable region with one thorough pattern and, because the
machine never resumes, does **not** restore them. A full test *loop* runs the
whole `RamTestPattern` set — own-address, moving inversions (zeros/ones and the
checkerboard), and walking ones/zeros — and the takeover repeats that loop
until the machine is reset. It reports progress as `(tested, total)` and each
mismatch through a `SweepObserver`, never stopping on a bad cell (so one fault
never masks the rest). It reuses the same `WordWindow` / `PhysWindow`
primitives and safe, range-checked physical-map access as the boot path — no
raw pointer arithmetic — and is fully host-tested over a fault-injecting fake. The takeover *mechanism* that flattens
and tears the machine down (an Arch HAL slice) after the arch-neutral caller
has quiesced every other CPU, the `memtest`
command that drives it, the memtest86-style full-screen progress UI it renders
through, and the real per-port bodies + end-to-end QEMU verticals for all three
bare-metal targets are complete (see *Machine takeover* below);
`plans/NEW-SUPERVISOR.md` §9 is done on every Tier-1 target.

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
`clear`. Diagnostics: `log`, `panic-log` (alias `last`), the one-way
whole-RAM `memtest`, and the read-only `test disk`.

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

## Machine takeover (continuous whole-RAM test)

A memory test that runs inside the live kernel can only test RAM it explicitly
owns; it can never test the frames the kernel image, heap, page tables, or
stacks occupy. Testing *all* of RAM requires owning the whole machine —
stopping every other CPU, masking interrupts, stopping the lockup watchdog, and
flattening paging so a small self-contained test routine can address physical
RAM — which is a one-way trip whose only exits are reset or power-off.

**Stopping the other CPUs is architecture-neutral** and lives once in
`tairix_arch_api::quiesce` (a stop request + boot-published liveness/ack
tables + a bounded wait), driven by `kernel/core`'s `drive_takeover` over the
neutral `SchedulerArch::send_ipi`; only *parking* a stopped CPU is per-silicon
(each port's interrupt path). It runs **before** the per-architecture
tear-down, as the last step that can fail closed.

That per-architecture tear-down lives behind an Arch HAL slice,
`MachineTakeover` (`kernel/arch/api/src/takeover.rs`). It is a **single**
operation, `take_over(&self, sweep: &mut dyn FnMut())`, entered only once this
CPU is the sole one running; it owns the rest of the irreversible sequence and
never returns on success: mask interrupts, stop the watchdog, flatten paging,
**switch onto a reserved stack the sweep cannot overwrite**, and run the
caller's `sweep` (the arch-neutral test of every usable frame — every pattern
over all of RAM, looping forever until the operator resets the board). The one
region the sweep cannot test is the memory it runs from — the kernel image and
that reserved stack — which a continuous run must keep intact to keep running,
exactly as a running memtest86 cannot test its own resident code. The takeover
never resets the machine itself, so it needs **no** reset conduit and is
available on every bare-metal port (a spin-table Pi 4 with no `/psci` node
included). A two-step "prepare, then let the caller sweep on its own stack"
split could never be correct: the Supervisor runs on a kthread stack drawn from
RAM the sweep destroys, so the port must own the stack switch itself.

`take_over` **returns** only on a pre-teardown refusal (`TakeoverError`:
`NotSupported`, `PrepareFailed`), leaving the machine running and recoverable
and `sweep` un-run; it never panics. (The `CpuQuiesceTimeout` variant is part
of the same vocabulary but is produced upstream by the arch-neutral quiesce,
not by `take_over` itself.)
`KernelArch::machine_takeover` defaults to `None`, so a port that has not wired
the mechanism (and `wasm32`, which owns no physical RAM to take over) honestly
reports "not supported" and the Supervisor stays in the REPL.

### The operator command and its supervisor-only gate

The whole-RAM test is the one and only `memtest` command; there is no
separate partial test and no confirmation prompt, because invoking the sole
memory test there is *is* the decision to tear the machine down. The decision
is audited (`supervisor: memtest whole-RAM machine takeover confirmed`, a
`Warn` on the hash-chained boot log) — recorded *before* the attempt,
synchronously, because a successful takeover destroys the in-memory audit ring
and never returns. Before the tear-down, the arch-neutral caller stops every
other CPU through the bounded cross-CPU quiesce handshake
(`tairix_arch_api::quiesce_others`); a peer that will not halt makes the
takeover fail closed (`takeover_cpu_quiesce_timeout`) with the machine
unchanged and the REPL still live.

The takeover handle is reachable **only** from this path. Obtaining it through
`KernelArch::machine_takeover` requires a
`tairix_kernel_core::supervisor_system::TakeoverGrant` — a witness whose
constructor is private to `supervisor_system`, the module that drives
`memtest`. No other kernel subsystem, driver, or userland
caller can mint the grant, so none can obtain the `MachineTakeover` handle or
invoke its `unsafe` step: the accessor is the single gate and the grant is
its only key. Once `take_over` succeeds, the arch-neutral `sweep_pattern`
loop runs over the direct physical map — cycling every pattern over all of RAM,
over and over — rendering to the memtest86-style full-screen UI, until the
machine is reset; it never returns.

### The full-screen progress display

Once the test owns the machine it owns the console outright, so the sweep is
presented through `lib/supervisor::memtest_ui::MemtestUi`, built entirely on the
`Screen` presenter above — there is no second escape-emitting path. It renders
*only* from the values the engine hands it: a reverse-video title banner, the
elapsed run time (`HH:MM:SS`), the count of completed test loops, the RAM under
test, the current pattern, the tested-so-far figure, a green progress bar and a
live percentage, and — beneath the bar — a scrolling log of any RAM faults with
a running error count (each faulting physical address, expected and observed
word; no secret was ever in this pre-unlock RAM). The rich figures are
absolute-positioned and redrawn in place as they advance. On a genuinely dumb
serial line the same information
degrades to concise, deduplicated plain-text lines (one injected `plain` flag,
no probe). The UI computes nothing about the RAM; its arithmetic is purely
presentational, and nothing it does panics on any input.

The arch-neutral sweep and this UI already exist
(`tairix_kernel_mem::ramtest::sweep_pattern`,
`tairix_supervisor::memtest_ui`). All three bare-metal ports wire the real
per-port mechanism (`kernel/arch/<t>/src/takeover.rs` + `takeover.s`), entered
after the arch-neutral quiesce has parked every other CPU: mask interrupts
(`sstatus.SIE`/`sie`; `DAIF` plus the `CNTV_CTL_EL0` watchdog cadence; `cli`),
switch to a reserved `.bss` stack, and run the sweep, which loops until the
operator resets the board (if a sweep ever returned, the sole CPU parks in a
masked halt rather than resume kernel code). Each port parks a stopped CPU from
its own IPI-receive path
(`on_ipi_interrupt` / the timer dispatch / `on_software_interrupt`).
**riscv64** and **aarch64** flatten paging (bare mode `satp = 0`; the MMU-off
`SCTLR_EL1` after a cache clean+invalidate — both ports are identity-mapped so
`virt==phys` survives), so the sweep addresses physical RAM directly.
**x86_64** cannot drop paging (long mode requires it), so instead of flattening
the MMU it switches `%cr3` to the reserved boot page tables (`boot_pml4` in
`.boot.bss`), whose higher-half window the sweep reaches physical RAM through.
Because the test never resets the machine itself, no port needs a reset conduit.
Each is proven
end-to-end by `tests/integration/supervisor_memtest_takeover_qemu_<t>`, which
boots the production pipeline — **multi-core** on x86_64 (its CPUs discovered
from ACPI), so the takeover really quiesces its peers, and single-core on the
aarch64 and riscv64 verticals, where the quiesce takes its no-peers path (a
continuous-memtest guest is kept single-core so it stays a light citizen in the
parallel matrix) — and drives the real `memtest_takeover` seam (neither serial
console
has interactive input this early, so the command dispatch is host-tested in
`lib/supervisor`); the guest sweeps all of RAM continuously. Because the test
never stops on its own, each vertical ends it deterministically: once the guest
prints the completed-test-loop marker (`memtest: completed test loop`), the
harness issues a QEMU-monitor `system_reset`. Under `-no-reboot` that reset
exits QEMU with status `0`, and a marker-gated reset-is-success rule
(`tairix_qemu::Spec::with_reset_success_marker`) accepts it only when the serial
also carries that marker — so a crash-into-reset before a completed loop never
printed it and still fails loud. `wasm32` stays `NotSupported` (a sandbox owns
no physical RAM to take over); `memtest` is complete on all four Tier-1 targets
(`plans/NEW-SUPERVISOR.md` §9).
