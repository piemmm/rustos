# NEW-SUPERVISOR.md — the pre-boot Supervisor console

This is the staged, **binding** plan (under `AGENTS.md`) for the TAIRiX
**Supervisor**: a small, built-in, pre-mount command monitor — a "quick
busybox, but different" — that an operator can drop into from the boot
screen *before* the encrypted root is mounted, to inspect and control the
machine while it is still at the bootstrap floor.

Read `AGENTS.md` first (especially §1 Rust-only, §2, §4, §5, §9, §16, §17,
§18.6 bootstrap floor, §19, §23, §26, §27), then the plans this work sits
between: `plans/PI.md` (P11 unlock/boot bring-up), `plans/ARCHSUPPORT.md`
(x86_64 unlock/login parity), `plans/WATCHDOG.md` and `plans/FIX-PANICS.md`
(the pre-boot diagnostic records the Supervisor surfaces), `plans/DEVICES.md`
and `plans/CURSES.md` / `plans/CURSES`→`lib/vt` (the line discipline the ESC
trigger extends), and `docs/src/filesystem/drives.md` (the storage model
`ls`/`arxfs` describe). Every rule in all of them applies here without
exception.

> **ABI note.** `abi-v1` is **not** frozen for this work (despite what
> `AGENTS.md` §9 / other plans say in the general case). It barely matters:
> the Supervisor is an **in-kernel bootstrap-floor** facility and adds **no
> user-facing ABI, no syscall, and no `lib/abi` type**. It is not an app
> bundle, so §16.5 does not apply to it.

---

## 0. Scope and binding decisions

- **It is a bootstrap-floor facility, in-kernel by necessity (§18.6).** The
  Supervisor must run *before* the root/`/System` app surface is usable —
  the same layer that already owns the pre-mount console: the compiled-in
  `init` boot floor and the interactive root-unlock policy. It is therefore
  legitimately in-kernel (like `init` and the unlock prompt), **not** a
  `/System/Apps` bundle. It shrinks toward nothing over time only in the
  sense that it presents state the kernel already computes; it adds no new
  driver, no new authority.
- **The boot-screen wording is frozen exactly as specced.** The three
  on-screen strings below are byte-exact and are asserted by the QEMU
  vertical (§7). They are the contract; do not "improve" the wording.
  1. `[Press ESC for supervisor]` — shown for 2 seconds.
  2. `ARXFS passphrase: ` — replaces (1) in place after the window.
  3. On ESC: the prompt/message line collapses to `ARXFS`, then a **blank
     line**, then `Supervisor`, then the REPL prompt `* `.
- **Reuse, never re-derive (§2.2).** Every datum the Supervisor shows is
  already computed elsewhere: `kernel/core::memtest` over
  `tairix_kernel_mem::ramtest` (RAM test), `kernel/core::introspect_source`
  (`KernelIntrospectSource`: version, uptime, memory, mounts, tasks),
  `lib/partition` (MBR/GPT), the `lib/abi` hardware tree (`HwNode`), the
  `lib/log` audit ring (boot events), the `WATCHDOG.md` / `FIX-PANICS.md`
  records. The Supervisor is a **presenter + control surface** over those;
  it computes nothing new. Duplicating any of it is a review blocker.
- **Arch-neutral engine in `lib/*`, device/reset glue stays arch-side
  (§2.20, §2.21, §17.4).** The REPL, the ESC/timeout state machine, and
  every built-in command live in a new `no_std` crate `lib/supervisor`,
  written entirely against the object-safe `ConsoleWrite` / `ConsoleRead`
  seams (exactly as `root_mount.rs` already is) plus injected function
  seams for timing, reboot/poweroff, and continue-boot. No board, SoC,
  MMIO, register, or `cfg(target_arch)` appears in it. The
  device-specific console-0 seam, PSCI/reset, and timer wiring stay in
  `unlock_orchestrate.rs` / `unlock_service.rs` / `kernel/arch/<target>/`.
- **Security is not weakened, ever (§5.4, §2.17, §19.4).** The Supervisor
  runs at full kernel authority at the *physical console before any user is
  authenticated*; its threat model is therefore **physical-console access
  only** — exactly the §19.9 "physical attacker" class already declared out
  of scope. That is a reason to **audit loudly and fail closed**, never a
  licence to weaken anything: it never bypasses the passphrase, never
  reveals key material, never exposes an arbitrary-physical-address poke,
  and every state-changing command is audited (§4).
- **No busy-waiting (§2.23).** The 2-second window and every REPL read are
  genuine timed/interrupt-driven parks on `CONSOLE_WAITQ` (reuse
  `unlock_service::park_for_ns` and the `KthreadConsoleRead` reader), never
  a spin or a `yield_now` loop.
- **Complete, not minimal (§27).** The command dispatcher is a full table
  of `&'static` entries (name, summary, per-command help, handler) with
  prefix-free exact matching, argument tokenisation, and unknown-command
  handling from the first commit — not "the minimum for `help`".

---

## 1. Repository placement (amends `AGENTS.md` §3)

```
lib/
└── supervisor/          # Pre-boot Supervisor REPL engine + built-in commands.
    ├── Cargo.toml        #   no_std; stability tier: experimental (README).
    ├── README.md
    └── src/
        ├── lib.rs        #   Public entry: run_supervisor(...) + seams.
        ├── repl.rs       #   Line read + dispatch loop (parks, never spins).
        ├── dispatch.rs   #   The &'static command table + tokeniser.
        └── commands/     #   One module per command group (info/diag/control).
```

- `lib/supervisor` enters `AGENTS.md` §3 (`lib/*` map) and the §15.18
  jump-sheet in the change that creates it.
- The ESC trigger points and the seam wiring live in the existing
  `kernel/tairix-kernel/src/root_mount.rs`,
  `kernel/tairix-kernel/src/unlock_orchestrate.rs`, and
  `kernel/tairix-kernel/src/unlock_service.rs` — no new kernel module.
- Docs: a new `docs/src/architecture/supervisor.md` (linked from the
  architecture index), written in the same change (§13).

---

## 2. The boot-screen sequence (state machine)

The trigger lives at the **very top** of
`root_mount.rs :: unlock_root_disk_interactively_impl`, *before* the silent
blank-passphrase probe, so the ESC window appears on **every** image —
including the blank-passphrase installer image, which otherwise
auto-unlocks with no prompt at all. (Decision confirmed: show it
everywhere.)

State machine (all timed reads are parks, never spins):

1. **Announce.** `write_all(console, b"[Press ESC for supervisor]")`.
2. **Timed read window.** Loop until a 2-second deadline
   (`park_for_ns`-style: register a timed wakeup on `CONSOLE_WAITQ`, park,
   re-check the clock), polling `input` for one byte on each wake:
   - `ESC` (`0x1b`) → **enter Supervisor** (step 5). But `ESC` also opens
     CSI sequences (arrow/Delete keys the `LineEditor` consumes), so a lone
     `ESC` must be disambiguated from `ESC [ …` — see §3.
   - any other byte → discard it (so it cannot leak into the passphrase)
     and fall through to the prompt.
   - deadline reached with no byte → fall through.
3. **Redraw in place.** Overwrite the message with the passphrase prompt on
   the same line: `write_all(console, b"\rARXFS passphrase: \x1b[K")`. (The
   `\r` returns to column 0 and `\x1b[K` erases the longer message's tail —
   the same in-place technique `FS_UNLOCKED_LINE` already uses.) The unlock
   then proceeds exactly as today (silent blank probe, then the interactive
   loop). **Note:** the existing `FS_UNLOCK_PROMPT` opens with `\r\n`; when
   the ESC window has already drawn the line, use the CR-in-place redraw
   above instead of re-emitting `\r\n`, so the prompt lands on the message's
   line rather than a fresh one. Keep both spellings built from the single
   `fs_label!()` macro (§2.2) — extend it, do not add a second literal.
4. **ESC at the live passphrase prompt.** ESC must *also* drop to the
   Supervisor while the `ARXFS passphrase: ` prompt is up. `read_passphrase_line`
   gains a new outcome: a lone `ESC` as the **first** byte of a line returns
   `LineFeed::Escape` (via a new `PassphraseRead::Escape` result), which the
   caller treats like the ESC-window hit.
5. **Enter Supervisor.** Collapse the current line to the label and open the
   REPL, byte-exact:
   ```
   \rARXFS\x1b[K\r\n     (prompt/message replaced by "ARXFS", end line)
   \r\n                  (one blank line)
   Supervisor\r\n
   ```
   Then call `lib/supervisor::run_supervisor(...)`, whose prompt is `* `.
   On-screen result:
   ```
   ARXFS

   Supervisor
   * 
   ```
6. **Supervisor exit resumes boot.** `continue` (§4) returns from
   `run_supervisor`; control falls back into the normal unlock path
   (redraw `ARXFS passphrase: ` and carry on), so a Supervisor session is
   transparent to the rest of boot. `mount` performs the real unlock *now*
   and then continues (no second prompt). `reboot`/`poweroff` never return.

---

## 3. ESC-vs-CSI disambiguation (a real design point, in `lib/vt`)

A lone `ESC` (supervisor) must be distinguished from `ESC [ …` (an editor
CSI sequence: arrows, Delete's `CSI 3 ~`). Solve it the classic terminal
way, **in the shared line discipline `lib/vt`, not hacked into the reader**
(§2.2):

- On seeing `ESC`, do a single short bounded re-poll (a small timed park,
  e.g. tens of ms) for a follow-on byte:
  - a follow-on `[` → it is a CSI sequence; hand it to the existing
    `LineEditor` state machine as today.
  - no byte within the window, or a non-`[` byte → it is a lone `ESC`
    (supervisor / cancel).
- Encode this as an explicit state in `lib/vt`'s line/escape parser
  (extend `LineEditor` / its escape sub-state with a `LoneEscape`
  resolution and a `LineFeed::Escape`), with host unit tests for: lone ESC,
  ESC then `[` (arrow/Delete still work), ESC then a printable byte, and ESC
  at end-of-input. `read_passphrase_line` and the REPL reader both consume
  this one definition — never two copies of the timeout logic.
- The re-poll uses the same `CONSOLE_WAITQ` timed-park primitive (§2.23); it
  is bounded and never spins.

---

## 4. Command set (all built-in, read-only unless noted)

The dispatcher is a `&'static [Command]` table; each `Command` is
`{ name, summary, help, handler }`. `help` renders the summary table;
`help <cmd>` renders one command's long help. Argument parsing is
whitespace tokenisation with `--` end-of-options, matching the coreutils
spirit (§16.7) where a command has a coreutils analogue (`ls`, `echo`).

### 4.1 Control / boot

- `help [cmd]` — the help screen; per-command help with `help <cmd>`.
- `continue` (alias `boot`) — leave the REPL and resume the normal
  unlock/boot path. **Audited.**
- `reboot` — clean reset via the port's PSCI/reset seam
  (`kernel/arch/<target>/`), injected as a `fn` seam. **Audited.**
- `poweroff` (alias `halt`) — orderly shutdown where the platform supports
  it (injected seam; on a port without one, report "not supported" fail-safe
  and stay in the REPL). **Audited.** *(Confirmed in scope for first cut.)*
- `mount` — run the **real** `mount_root_disk_and_load_users` under a typed
  passphrase (reuse the exact prompt/collapse/rate-limit/audit path of the
  normal unlock — no oracle, no fail-open), then `continue` on success or
  report fail-closed on failure. **Audited.**

### 4.2 Diagnostics / info

- `version` — kernel version, build hash, target arch, ABI version
  (`introspect_source`).
- `mem` — installed/usable RAM, kernel heap committed size, memory-pressure
  band (reuse memstats / `KernelIntrospectSource`).
- `mem map` — print the `BootMemoryMap` regions (usable/reserved) — pairs
  with `memtest`.
- `memtest [passes]` — the "heavy" test. Reuse `tairix_kernel_mem::ramtest`
  via `kernel/core::memtest`; add a **thorough** multi-pattern mode
  (walking-ones/zeros, address-in-address, moving-inversions) with a
  progress counter and **ESC-to-abort**. Strictly bounded, interruptible,
  fail-loud on a fault. No raw pointer arithmetic — only the safe
  `ramtest::run` over the `BootMemoryMap`.
- `ls [path]` — the "crude" listing. Pre-mount, the only readable volume is
  the always-readable `/System` (the driver store lives there); scope `ls`
  to `/System` and say so clearly when the root is not yet mounted. After a
  Supervisor `mount`, `ls` sees the mounted tree. Read-only.
- `cpu` — CPU/core count and features (reuse the `cpufeatures` / hardware-tree
  data, `plans/FIX-HARDWARE-FEATURES.md`).
- `hw` (alias `lsdev`) — dump the discovered hardware tree (`HwNode`s, bind
  keys) — answers "why didn't my disk/keyboard show up" before boot.
- `disk` — list block devices + geometry.
- `partitions <dev>` — parse and show the MBR/GPT table (reuse
  `lib/partition`).
- `arxfs` — show the root volume's descriptor/label/UUID and status (present?
  is it ARXFS? unlocked?) **without** unlocking.
- `log [n]` — tail the in-memory boot audit log (the hash-chained `lib/log`
  events) — the single most useful pre-boot diagnostic.
- `panic-log` (alias `last`) — surface a previous boot's recorded
  panic/lockup record if one exists (the `WATCHDOG.md` + `FIX-PANICS.md`
  records).
- `uptime` — monotonic since boot; `date` — wall clock, both `Time64` (§21).
- `test disk <dev>` — a bounded, read-only surface scan (fits §26.5 "a disk
  may be failing"): reports read errors/timeouts, never writes, ESC-abortable.
- `echo [args…]` — print its arguments.
- `clear` — clear the screen.

Staged/optional (each justified in this plan before it lands): `loglevel <n>`
/ `boot verbose` to raise verbosity of the *continued* boot.

---

## 5. Seams and wiring (§17.4 layering)

`lib/supervisor::run_supervisor` takes only object-safe/`fn` seams — no
kernel or arch types:

- `console: &dyn ConsoleWrite`, `input: &dyn ConsoleRead` — the console-0
  halves the unlock already holds (write + the gated interrupt-driven read).
- `audit: &dyn Sink` — the same `lib/log` sink the unlock uses.
- `timer`/`delay` — the timed-park seam (a `&dyn Fn` or small trait) backed
  in-kernel by `unlock_service::park_for_ns` on `CONSOLE_WAITQ`; host tests
  pass an immediate no-op.
- `reboot: &dyn Fn() -> !`, `poweroff: &dyn Fn() -> !` — port reset seams
  from `kernel/arch/<target>/`.
- `mount: &mut dyn FnMut(&[u8]) -> MountResult` — invokes the real
  `mount_root_disk_and_load_users` (the kernel wires the disk `Block`,
  install cells, and audit behind this closure so `lib/supervisor` never
  names them).
- `introspect`/`hw`/`partitions`/`arxfs`/`ls`/`memtest`/`log` — each a
  narrow read-only seam trait implemented in the kernel over the existing
  sources (`KernelIntrospectSource`, the hardware tree, `lib/partition`,
  the ARXFS descriptor read, the `/System` `FilesystemRead`, `memtest`, the
  `lib/log` ring). `lib/supervisor` depends on **no** `kernel/*` crate
  (§17.4): the seams are defined in `lib/supervisor` (or `lib/abi` where a
  type already exists) and implemented kernel-side.

The trigger in `root_mount.rs` threads these through; the device-specific
console-0 acquisition, RX-interrupt arming, and reset live where they do
today (`unlock_orchestrate.rs` / arch crates).

---

## 6. Security (non-negotiable — §5.4, §2.17, §19.4)

- **Never bypasses the passphrase.** `mount` runs the real unlock under a
  typed passphrase in a zeroized on-stack `Zeroizing` buffer; there is no
  command that unlocks without the secret and none that reveals key
  material. No fail-open path exists.
- **Fail closed & audited.** Entering the Supervisor and every
  state-changing command (`mount`, `continue`, `reboot`, `poweroff`) emits a
  stable `lib/log` event id — extend the `41xx` root-unlock range (e.g.
  `SUPERVISOR_ENTERED`, `SUPERVISOR_CONTINUE`, `SUPERVISOR_REBOOT`,
  `SUPERVISOR_POWEROFF`, `SUPERVISOR_MOUNT_*`). **Zero** secrets, keys, or
  memory contents ever reach the audit log or any output.
- **Read-only by construction.** `ls`, `hw`, `disk`, `partitions`, `arxfs`,
  `log`, `mem`, `cpu`, `version`, `test disk` never write to disk and never
  expose an arbitrary physical-address read/poke that could break isolation.
  `memtest` uses the safe `ramtest` engine over the `BootMemoryMap`, not raw
  pointer arithmetic.
- **Threat model stated, not glossed.** Pre-auth at the physical console =
  the §19.9 physical-attacker class (already out of scope). The response is
  loud auditing + fail-closed, never a weakened defence.
- **No panic on any error path (§2.9).** Every command handler returns a
  `Result`/typed outcome the REPL renders; a bad argument, an unreadable
  disk, or a missing record is a message, never a panic.

---

## 7. Tests, docs, and the validation gate (§7, §13, §23)

Land in the **same change**:

- **Host unit tests** (`lib/supervisor`): every command handler over mock
  seams; the full ESC/timeout/disambiguation state machine (lone ESC, ESC
  then `[`, ESC then printable, ESC at EOF, window-timeout fall-through,
  stray-byte discard); the dispatcher (exact match, unknown command,
  tokeniser, `help`/`help <cmd>`); `memtest` abort path; fail-closed
  outcomes.
- **`lib/vt` tests**: the `LoneEscape` / `LineFeed::Escape` resolution and
  that arrow/Delete CSI editing still works unchanged.
- **QEMU integration vertical**: drive ESC at *both* trigger points and
  assert the **byte-exact** boot-screen strings (`[Press ESC for
  supervisor]` → in-place redraw to `ARXFS passphrase: ` on timeout; ESC →
  `ARXFS`, blank line, `Supervisor`, `* `). Add the script alongside the
  existing `UNLOCK_PASSPHRASE_LINE` script in
  `tools/xtask/src/commands/qemu_tests.rs`. Assert `continue` resumes to a
  normal boot and `mount` unlocks and boots.
- **Docs**: `docs/src/architecture/supervisor.md` (command reference,
  security model, the boot-screen contract), rustdoc on every public item,
  `lib/supervisor/README.md` with the stability tier (`experimental`).
- **Gate**: `cargo fmt --all`, `cargo xtask ci` (once), `cargo xtask fuzz
  --secs 5`, `tools/ci/soak.sh both --secs 20` — all green over the **whole**
  workspace before the work is done. A fuzz harness over the REPL line/
  command parser (untrusted console input) enters `cargo xtask fuzz`.

---

## 8. Charter housekeeping (do in the same change)

- Add `lib/supervisor` to `AGENTS.md` §3 (`lib/*` map, one-line label).
- Add the jump-sheet row to `AGENTS.md` §15.18:
  `Pre-boot Supervisor console (ESC-at-boot REPL, pre-mount diagnostics/control) | plans/NEW-SUPERVISOR.md`.
- Add a `docs/src/architecture/supervisor.md` entry to the mdBook `SUMMARY`.
- Update `PLAN.md` if this advances a stage.

---

## 9. Status

`in progress`.

**Done (the arch-neutral engine layer):**

- `lib/vt` lone-ESC resolution — `LineFeed::Escape`,
  `LineEditor::resolve_escape`, `EraseSeq::holding_escape`/`reset`, additive
  and behaviour-preserving (`push` is byte-for-byte unchanged; escape only
  surfaces via the reader-driven `resolve_escape`), with unit tests. The three
  existing `LineFeed` consumers (both `elsh` readers, the `root_mount`
  passphrase reader) handle the new arm inertly.
- `lib/supervisor` — the complete crate: the `Report` / `SupInput` /
  `SupervisorHost` seams and the `SupervisorExit` / `TestOutcome` /
  `MountOutcome` / `SupervisorEvent` vocabulary (`src/lib.rs`); the `&'static`
  command table + tokeniser + case-insensitive dispatcher + `help`
  (`src/dispatch.rs`); the `* ` REPL + line reader/echo (`src/repl.rs`); every
  built-in command over the seams (`src/commands/{control,info,diag}.rs`); and
  full host tests via the in-memory mocks (`src/commands/test_support.rs`).
  `no_std`, alloc-free, clippy-clean under `-D warnings`.
- Registered in the workspace and `AGENTS.md` §3; `README.md`,
  `docs/src/architecture/supervisor.md`, and the `docs/src/lib/supervisor.md`
  page + SUMMARY entries; §15.18 jump-sheet row already present.

**Done (the per-arch machine-control seam):**

- `KernelArch::reboot()` / `poweroff()` (`kernel/core::bootinfo`) — the
  arch-neutral control seam the Supervisor's `reboot`/`poweroff` drive, typed
  `()` (never `!`) so an unsupported/refused platform **returns** and the
  caller reports it fail-safe rather than wedging. Fail-safe defaults
  (unsupported → return), overridden per port:
  - aarch64: PSCI `SYSTEM_RESET` / `SYSTEM_OFF` (`psci::system_control`) over
    the conduit discovered from `/psci`, threaded into `Aarch64BinArch` via
    `with_psci_method` (recovered from `secondary_start()`).
  - riscv64: SBI System-Reset (SRST) extension (`sbi::system_reset`, cold
    reboot / shutdown).
  - x86_64: `reset::reboot` (8042 pulse then `0xCF9`); power-off is left the
    honest default-unsupported until an ACPI S5 subsystem exists (documented,
    not a guessed control-port write).
  - Host unit tests + compile-time id asserts; builds clean on all four
    Tier-1 targets. The seam has no caller yet — its consumer is the
    `SupervisorHost` below (do not land these alone once past this staging).

**Done (the retained tail-able boot audit-log ring — the item-1 prerequisite):**

- `kernel/core::boot_audit_ring::BootAuditRing<N, I>` — a bounded, SMP-/ISR-safe
  `tairix_log::Sink` that keeps the most recent `N` boot events (level, id,
  monotonic time via an injected `MonotonicClock`, message) readable
  **non-destructively**. `kernel/core::audit` is only the id catalogue and
  `lib/log::BootRing` is a drain-once FIFO, so neither serves the
  Supervisor's `log`: this ring is that missing store.
  - **Home is `kernel/core`, not `lib/log` (deliberate).** The ring needs the
    IRQ-safe lock from `lib/sync`, but `lib/sync`'s `epoch` module is the sync
    crate's only `alloc` user, and the multi-crate `--target …-none` build
    unifies features — so any crate depending on `lib/sync` forces `alloc`
    into *every* binary in that build, including the minimal no-allocator
    QEMU fault-test binaries that link `tairix-log`. Putting the ring in
    `kernel/core` (which already depends on `lib/sync`+`lib/log`+`alloc` and
    is never linked by those minimal binaries) keeps `tairix-log` allocator-
    free while landing the ring where its producer (audit-sink composition)
    and consumer (the `SupervisorHost`) both live. It is a `tairix_log::Sink`,
    exactly as the seam requires.
  - Guarded by the shared `IrqSafeSpinLock` (generic over the arch's
    `InterruptControl`, supplied by the bin crate's `static`); the lock is
    held only for one record copy, never across rendering, so a slow console
    write never masks interrupts.
  - Records carry a strictly-increasing global sequence; a viewer walks
    `seq_range()` and fetches each by `record(seq)` (one lock hold each), so
    a tail read stays consistent under a live writer (an evicted sequence
    reads back `None`, never a different record). `total()` counts every
    record ever written for a "last k of N" view.
  - Fixed-capacity inline array (no alloc); a full ring overwrites the
    oldest; an over-long message truncates on a UTF-8 boundary, so no input
    makes a read panic. Full host tests, rustdoc, and `const`-constructible
    for a `static`.

**Remaining (the kernel consumer):**

1. Compose the `BootAuditRing` into the boot audit path (a tee alongside the
   per-arch serial audit sink) with the kernel's monotonic since-boot clock,
   and wire the reader into `SupervisorHost::log_tail`. The ring primitive
   above is landed; this is its kernel producer + reader wiring, which needs
   the per-arch monotonic-clock seam (the serial sink notes none is wired on
   x86_64 yet) threaded through. `panic-log` reads the `WATCHDOG.md` /
   `FIX-PANICS.md` records, not this ring.
2. A kernel `SupervisorHost` wiring each command to its existing source
   (introspect/version/mem/uptime/date, `tairix_kernel_mem::ram_selftest`
   for `memtest`, `lib/partition`, the `lib/abi` hardware tree, the boot
   audit ring above, the ARXFS descriptor read, the `/System` `FilesystemRead`
   for `ls`, `KernelArch::reboot`/`poweroff` for control, and the real
   `mount_root_disk_and_load_users` for `mount`), plus the `41xx`
   `SupervisorEvent` audit ids.
3. The ESC boot-screen window at the top of
   `root_mount.rs :: unlock_root_disk_interactively_impl` (byte-exact
   `[Press ESC for supervisor]` → 2 s timed park → in-place redraw to
   `ARXFS passphrase: `; ESC → `ARXFS`, blank line, `Supervisor`, `* `),
   including the timed-read/non-blocking-poll primitive and the ESC-vs-CSI
   re-poll driving `LineEditor::resolve_escape`, threading the
   `SupervisorHost` through the unlock path from `unlock_orchestrate.rs`.
4. The QEMU integration vertical driving ESC at both trigger points with the
   byte-exact boot-screen assertions, and a fuzz harness over the REPL parser.

The engine layer and the machine-control seam are complete and green; items
1–4 are the remaining kernel wiring, to land behind a green whole-project
gate (item 1 first, as items 2–3 depend on it).
