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
  system program bundle (under `/System/Commands` or `/System/Applications`).
  It shrinks toward nothing over time only in the
  sense that it presents state the kernel already computes; it adds no new
  driver, no new authority.
- **The boot-screen wording is frozen exactly as specced.** The three
  on-screen strings below are byte-exact and are asserted by the QEMU
  vertical (§7). They are the contract; do not "improve" the wording.
  1. `[Press ESC for supervisor]` — shown for 2 seconds.
  2. `ARXFS passphrase: ` — replaces (1) in place after the window.
  3. On ESC: the prompt/message line collapses to `ARXFS`, then a **blank
     line**, then `Supervisor`, then the REPL prompt `*`.
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

1. **Announce.** `write_all(console, b"\r\n[Press ESC for supervisor]")`. The
   leading `\r\n` opens a fresh line, so one blank line separates the boot
   banner (userland `init`'s machine-summary line beneath the
   `TAIRiX <v> <RAM>MiB` counter) from the announcement — the same
   one-blank-line spacing the standalone `FS_UNLOCK_PROMPT` draws, so the
   screen is laid out identically whether the window is entered, skipped, or
   times out.
2. **Timed read window.** Loop until a 2-second deadline
   (`park_for_ns`-style: register a timed wakeup on `CONSOLE_WAITQ`, park,
   re-check the clock), polling `input` for one byte on each wake:
   - `ESC` (`0x1b`) → **enter Supervisor** (step 5). But `ESC` also opens
     CSI sequences (arrow/Delete keys the `LineEditor` consumes), so a lone
     `ESC` must be disambiguated from `ESC [ …` — see §3. A CSI editor
     sequence is drained and ignored (it is a stray key, not passphrase
     content), then falls through to the prompt.
   - any other byte → the operator has begun typing the passphrase before
     the window elapsed, so it is the **first character of the passphrase
     line**: carry it out of the window (`EscWindow::Continue { initial }`)
     and feed it to `read_passphrase_line` as that line's first byte, then
     fall through to the prompt. It must **never** be discarded — dropping it
     silently corrupts a quickly-typed secret and dooms the unlock to endless
     wrong-passphrase retries (the boot-hang the QEMU verticals caught).
   - deadline reached with no byte → fall through with no carried byte.
3. **Redraw in place.** Overwrite the announcement with the passphrase prompt
   on the same line: `write_all(console, b"\rARXFS passphrase: \x1b[K")`. (The
   `\r` returns to column 0 of the announcement line and `\x1b[K` erases the
   longer message's tail — the same in-place technique `FS_UNLOCKED_LINE`
   already uses.) The unlock then proceeds exactly as today (silent blank
   probe, then the interactive loop). Because the announcement already opened a
   fresh line (step 1), the CR-in-place redraw leaves the prompt on that line
   with the blank line still above it — identical to the standalone
   `FS_UNLOCK_PROMPT`'s own-line spacing, so no line gap is lost. Keep both
   spellings built from the single `fs_label!()` macro (§2.2) — extend it, do
   not add a second literal.
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
   Then call `lib/supervisor::run_supervisor(...)`, whose prompt is `*`.
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
- `memtest` — the **only** memory test, and it is the one-way
  whole-RAM takeover specified in §9. It takes no arguments and asks for no
  confirmation: running `memtest` *is* the decision to tear the machine down
  (stop every other CPU, overwrite all of RAM, reset). There is no separate
  bounded / multi-pass mode — an in-kernel RAM test can only ever
  cover the frames it owns, so a partial "safe" test was misleading and has
  been removed. The early-boot banner self-test (`kernel/core::memtest` over
  `ramtest`) is a separate boot-time thing, not this command.
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
- `introspect`/`hw`/`partitions`/`arxfs`/`ls`/`memtest_takeover`/`log` — each
  a narrow seam trait implemented in the kernel over the existing sources
  (`KernelIntrospectSource`, the hardware tree, `lib/partition`, the ARXFS
  descriptor read, the `/System` `FilesystemRead`, the one-way takeover
  drive, the `lib/log` ring). All are read-only except `memtest_takeover`,
  which is the one-way whole-RAM test. `lib/supervisor` depends on **no**
  `kernel/*` crate
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
  `memtest` is the sole exception and is deliberately one-way (§9); even
  so it destroys RAM only through the safe, range-checked
  `ramtest::sweep_pattern` engine over the `BootMemoryMap`, never raw
  pointer arithmetic, and only after the operator invokes it and every other
  CPU has been quiesced.
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
  tokeniser, `help`/`help <cmd>`); the prompt-free `memtest` drives the
  audited takeover seam; fail-closed
  outcomes.
- **`lib/vt` tests**: the `LoneEscape` / `LineFeed::Escape` resolution and
  that arrow/Delete CSI editing still works unchanged.
- **QEMU integration vertical**: drive ESC at *both* trigger points and
  assert the **byte-exact** boot-screen strings (`[Press ESC for
  supervisor]` → in-place redraw to `ARXFS passphrase: ` on timeout; ESC →
  `ARXFS`, blank line, `Supervisor`, `*`). Add the script alongside the
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

## 8. Rich screens: colour and positioning (a `lib/vt` presentation layer)

This section is **immediate work for an AI**, staged. It answers the design
question directly: colour and cursor positioning at the bootstrap floor cost
**nothing new** — the vocabulary, the emitter, and the console seam already
exist and are already paid for. The rule is *reuse, never re-derive* (§2.2).

### 8.0 Why this is cheap and charter-clean

- **The console is already a byte stream that consumes escape sequences.**
  The boot-screen state machine (§2) already emits `\r`, `\x1b[K`, and
  in-place redraws through the same `Report` / `ConsoleWrite` seam. Colour
  (`CSI … m`) and absolute positioning (`CSI row;col H`) are the *same class*
  of output — no new driver, no new authority, no new ABI.
- **A complete, arch-neutral, `no_std`, allocation-free VT emitter already
  exists in `lib/vt`.** `lib/vt/src/op.rs` (`Op`) + `emit.rs`
  (`encode_into` / `encode_all_into`) already cover **everything** a
  memtest86-style screen needs and round-trip through the parser:
  `Op::CursorPosition { row, col }`, `CursorColumn`, `EraseInDisplay`,
  `EraseInLine`, `Sgr(..)` (colour/attributes), `EnterAltScreen` /
  `LeaveAltScreen`, `HideCursor` / `ShowCursor`, `SaveCursor` /
  `RestoreCursor`, `SetScrollRegion` / `ResetScrollRegion`,
  `ScrollUp`/`ScrollDown`. The encoder needs only an `Extend<u8>` sink; the
  Supervisor's `Report` seam already **is** a byte sink. So a rich screen is
  built by constructing `Op`s and feeding `emit::encode_into` straight into
  `Report`.

### 8.1 Binding decisions

- **Reuse `lib/vt`'s `Op` / `emit`; never hand-roll escape bytes (§2.2).** A
  second copy of the CSI/SGR encoding — an ad-hoc `write_bytes(b"\x1b[...")`
  scattered through a command — is the duplication the charter forbids and a
  review blocker. The one exception already in the tree (`\r`, `\x1b[K`, the
  in-place redraw of the byte-exact boot-screen strings in §2/§3) stays as-is
  because those bytes are the frozen boot-screen **contract** (§2), not a
  presentation layer; everything richer goes through `Op`/`emit`.
- **Target only the universally-safe VT100/xterm subset** — exactly the `Op`s
  enumerated in §8.0. At the bootstrap floor there is **no** `TERM` /
  `lib/termcap` database resolved (that lives on the not-yet-mounted
  `/System`) and **no** way to query the console's size or capabilities (the
  write seam is one-way). So a rich screen must not depend on any capability
  outside that subset.
- **Assume a conservative fixed geometry, threaded not hard-coded.** With no
  size query, a full-screen layout assumes a safe default (80×24). Where a
  geometry value is available from discovery it is *threaded in as data*
  (§18.1), never baked as a per-board constant (§2.20). The layout must clamp
  to the assumed bounds and never position off-screen.
- **Degrade gracefully; colour/position is a nicety, never a correctness
  dependency (§5.4, §2.9).** Every rich screen offers a plain/monochrome
  line-oriented fallback so a genuinely dumb serial line still shows usable
  text. The fallback is selected by a single injected flag on the presenter,
  defaulting to plain; there is no probe. A malformed/oversized coordinate
  clamps, it never panics.
- **No new "stuff" at the floor.** `lib/vt` and `lib/supervisor` are both
  already `no_std` / alloc-free. The presentation helper is a thin
  `Op`-building layer **inside `lib/supervisor`** (arch-neutral), not a new
  crate or subsystem (§2.3). It adds no dependency `lib/supervisor` does not
  already carry (`lib/vt`).

### 8.2 Deliverable (one self-contained stage)

- A small `screen` module in `lib/supervisor` exposing an arch-neutral
  presenter built on `lib/vt`: a typed `Style` (foreground/background/attrs
  mapped to `Sgr`), a `move_to(row, col)` / `clear` / `enter_fullscreen`
  (alt-screen + hide cursor) / `leave_fullscreen` (show cursor + leave
  alt-screen) helper set, and a `plain: bool` mode that emits text only. It
  writes exclusively through the existing `Report` seam via
  `emit::encode_into`; it names no board, MMIO, or `cfg(target_arch)`.
- **Host unit tests** assert the emitted bytes equal the `lib/vt` encoding of
  the corresponding `Op`s (so the "never a second copy" rule is *tested*, not
  merely asserted), and that `plain` mode emits no escape bytes.
- **Docs**: extend `docs/src/architecture/supervisor.md` with a short
  "rich screens" note and rustdoc on every public item; `README.md`
  stability tier unchanged (`experimental`).

This stage stands alone and lands before §9 (the takeover memtest is the
first *consumer* of the fullscreen presenter, but the presenter is useful on
its own and is verified independently).

---

## 9. `memtest` — the one-way takeover RAM test

This section is staged in full stages A–E; each stage is independently
reviewable and must land complete (§27) with its tests and docs (§7, §13).
**`memtest` is the *only* memory test.** There is no separate safe
command: an earlier bounded, in-kernel `memtest [passes]` existed but could
only ever cover the frames it owned (never the live kernel), which was
misleading, so it has been removed. `memtest` now *is* the takeover, and it
runs with **no confirmation prompt** — invoking it is the decision.

### 9.0 Why `memtest` must take the machine over to test all of RAM

Any RAM test that runs **inside the live kernel** can only test frames it
explicitly owns — never the RAM the kernel image, heap, page tables, or
stacks occupy, because corrupting the live map destroys the running kernel.
That is the same wall memtest86 avoids by owning the whole machine, and it is
why the old capped in-kernel test could never be honest about "all of RAM".

So `memtest` takes the machine over: it is a **one-way trip**, exactly like
`reboot`/`poweroff` — there is no "drop back into the system", the only exits
are reset/power-off. That irreversibility is why the decision is audited
before the attempt (Stage C); it is **not** gated behind a typed
confirmation, because a bounded, safe alternative no longer exists
to "fall back" to — running the one memory test there is *is* the intent.

### 9.1 Binding decisions (whole feature)

- **Bootstrap-floor, in-kernel, no new ABI (§18.6).** Like the rest of the
  Supervisor it runs pre-mount, before any app surface; it adds **no
  `lib/abi` type and no syscall**, so `abi-v1` being unfrozen is irrelevant
  here (§0 ABI note). It adds no driver and no new authority.
- **Split arch-neutral vs arch-specific honestly (§2.20 / §2.21 / §17.2).**
  The *pattern algorithm* (walking-ones/zeros, address-in-address,
  moving-inversions) is arch-neutral and already lives in
  `tairix_kernel_mem::ramtest` — extend it with a whole-RAM full-range
  variant shared by all four targets. **Stopping the other CPUs is itself
  architecture-neutral** and lives in one place, `kernel/arch/api::quiesce`
  (the `quiesce_others` stop-request + boot-published liveness/ack tables +
  bounded wait), driven by `kernel/core`'s `drive_takeover` over the neutral
  `SchedulerArch::send_ipi`; only *parking* a stopped CPU is per-silicon
  (each port's IPI-receive path calls `quiesce::stop_requested` →
  `acknowledge` → its masked halt). The remaining *takeover mechanism* (mask
  interrupts + watchdog, relocate/flatten paging so the test can address
  physical RAM, cache maintenance, reset) is irreducibly target-divergent and
  lives behind the **Arch HAL `MachineTakeover` slice**, implemented per
  `kernel/arch/<target>/`. Do **not** `cfg(target_arch)` this into shared
  code (`cargo xtask cfg-check` forbids it).
- **No raw pointer arithmetic without bounds-checked wrappers (§4).** The
  the whole-RAM writes go through the safe, range-checked `ramtest` window over
  the `BootMemoryMap` (the `WordWindow` / `PhysWindow` abstraction already
  there), never ad-hoc pointers.
- **Same threat model as the rest of the Supervisor (§0, §19.9).** It runs at
  the physical console *before the root is unlocked*, so **no key material or
  user secret is in RAM yet** — the destruction exposes nothing. That is a
  reason to audit loudly, never to relax anything.
- **No panic on any path (§2.9).** A platform that cannot take over (no
  quiesce/relocate primitive) reports "not supported" fail-safe and stays in
  the REPL — exactly like `poweroff` on a port without a power-off primitive
  (`KernelArch::poweroff` returning). It never panics, never half-tears-down
  the machine and wedges.
- **No busy-waiting as steady state (§2.23).** Quiescing the other CPUs is a
  legitimate *bounded handshake* (a documented §2.23 exception — the machine
  is being deliberately torn down, so the boot CPU spin-polls the ack table
  under a bounded budget while each stopped CPU parks on its masked halt),
  not a perpetual poll. If a peer does not acknowledge within the budget the
  takeover **fails closed** (`quiesce_others` returns `Err(cpu)`, the
  Supervisor reports `takeover_cpu_quiesce_timeout` and stays in the REPL),
  it does not spin forever.

### Stage A — the arch-neutral whole-RAM full-range pattern engine

- Extend `kernel/mem/src/ramtest.rs` with a **whole-RAM** whole-region
  variant: given the `BootMemoryMap` and a physical-address window
  abstraction, run the full multi-pattern sweep (moving-inversions +
  address-in-address, reusing the existing `address_pass` / `test_window`
  primitives) across a physical range **without** the "leave it zeroed /
  restore" contract the safe `run` keeps — because the machine
  never resumes. Report progress through an injected `on_progress(tested,
  total)` callback and honour an injected `abort() -> bool` between chunks.
- It stays pure `lib/*` logic over the `WordWindow` trait, so it is fully
  host-testable with the existing `FakeRam` double. No arch, no board.
- **Host tests**: healthy region passes; each seeded `Fault`
  (`StuckLow`/`StuckHigh`/`Alias`) is caught with the correct reported
  physical offset; abort stops early; the engine touches every word in the
  range (the point of "whole-RAM full-range").

### Stage B — the Arch HAL takeover slice

**Done (the arch-neutral slice — single-primitive design):**
`kernel/arch/api/src/takeover.rs` follows the `smp.rs`/`watchdog.rs` pattern.
The object-safe `MachineTakeover` trait is **one** operation,
`unsafe fn take_over(&self, sweep: &mut dyn FnMut()) -> TakeoverError`, that
owns the *entire* irreversible sequence and never returns on success: mask
interrupts → stop the watchdog → flatten paging → **switch onto a reserved
stack the sweep cannot overwrite** →
run the caller's `sweep` (the arch-neutral phase that tests all
*usable* RAM) → test the region the sweep executed from (kernel image + its
stack, never the firmware) with a relocated per-port stub → reset. `take_over`
**returns** only on a pre-teardown refusal (`TakeoverError`: `NotSupported`,
`CpuQuiesceTimeout { cpu }`, `PrepareFailed(i64)`, stable `as_str()`), leaving
the machine unchanged and `sweep` un-run. **Stopping the other CPUs happens
before this, in the arch-neutral caller** (`kernel/core::drive_takeover` →
`quiesce_others`), so `take_over` is only ever entered once this CPU is the
sole one running; the `CpuQuiesceTimeout { cpu }` variant stays part of the
neutral `TakeoverError` vocabulary but is produced by that upstream coordinator,
not by `take_over` itself. The host `takeover::conformance`
vertical (`run_unsupported`) proves the fail-closed vocabulary via an
unsupported double *and asserts the sweep is never invoked* (a genuine takeover
has no harmless input, so — unlike `smp` — a supported port is only proven by
the Stage E QEMU vertical). `KernelArch::machine_takeover`
(`kernel/core/src/bootinfo.rs`) is the supervisor-gated exposure seam,
`Option<&'static (dyn MachineTakeover + Sync)>` defaulting to `None`
(fail-closed). Registered in `kernel/arch/api/src/lib.rs` and the
`plans/WIRING.md` parity matrix (an *optional* slice, not a §17.2 mandatory
primitive).

> **Why one operation, not the earlier `quiesce`+`prepare` pair.** The
> Supervisor REPL (hence `memtest`) runs on a kernel-service **kthread
> stack allocated from usable RAM**. A driver that quiesced/prepared, then swept
> RAM and called `reboot()` on that *same* stack would overwrite its own live
> stack and return path mid-sweep and crash instead of resetting — the two-step
> split could never be correct. The port must therefore switch onto a reserved
> (never-swept) stack and run the sweep itself, which only a single
> owns-the-sequence primitive can express. The `sweep` safety contract (on the
> trait) requires the closure's code and *all* state it reads/writes-through
> (memory map, physmap, console) to live in reserved memory, never a
> frame-allocator frame.

**Per-target bodies + conformance — the takeover mechanism.** The real
`MachineTakeover` bodies under `kernel/arch/<target>/`, each wiring
`machine_takeover()` to return its handle and adding its memtest-takeover QEMU
vertical (Stage E). The `memtest` sweep tests all of RAM **continuously** and
never returns — the operator ends the run by resetting the board — so **no port
resets the machine itself and none needs a reset conduit**. The one region the
sweep cannot test is the memory it runs from (the kernel image + its reserved
stack), which a continuous run must keep intact, exactly as a running memtest86
cannot test its own resident code. Each body's tail is a masked halt-park, taken
only if a (future, finite) sweep ever returned. Precise per-port design:
- **riscv64 (done — the cleanest first port).** `kernel/arch/riscv64/src/takeover.rs`
  + `takeover.s`: every other hart is already parked by the arch-neutral
  quiesce (the caller's `quiesce_others`; each hart halts in
  `preempt::on_software_interrupt`), so the body masks `sstatus.SIE`+`sie` (the
  watchdog is unwired); flattens paging with `satp = 0` (bare mode — the
  identity map still resolves `virt==phys` with no page-table walk); switches
  `sp` to a reserved 64 KiB `.bss` stack (`_takeover_switch_stack`); and runs
  `sweep` over usable RAM (never returns; otherwise parks on `wfi`). Proven by
  `tests/integration/supervisor_memtest_takeover_qemu_riscv64` (guest reports a
  completed test loop; the harness resets it → QEMU `-no-reboot` exit 0).
- **aarch64 (done).** `kernel/arch/aarch64/src/takeover.rs` + `takeover.s`: every
  other core is already parked by the arch-neutral quiesce (the caller's
  `quiesce_others`; each core halts in `preempt::on_ipi_interrupt`), so the body
  masks `DAIF` (all of debug/SError/IRQ/FIQ) and stops the watchdog cadence
  (`CNTV_CTL_EL0 = 0`); switches `sp` to a reserved 64 KiB `.bss` stack
  (`_takeover_switch_stack`); and runs the arch-neutral `sweep` over usable RAM
  (never returns; otherwise parks on `wfi`). **The MMU stays on.** aarch64 does
  **not** flatten paging: with `SCTLR_EL1.M = 0` an EL1 data access is
  Device-nGnRnE, where an unaligned access faults unconditionally, so an MMU-off
  sweep (the framebuffer console, `memcpy`/`memset`, the sweep's own
  bookkeeping all issue unaligned accesses) takes an alignment fault with
  interrupts masked and wedges the board — the defect that locked a real
  Raspberry Pi 4 while QEMU/TCG, which ignores Device-memory alignment, passed.
  The kernel's identity map is already `virtual == physical` and Normal
  cacheable, so the sweep reaches every frame through it directly; the
  arch-neutral engine still tests DRAM because it flushes each tested word to
  the point of coherency (`PhysMap::clean_invalidate`, backed by real
  `dc civac`) around the read-back. Needs **no** PSCI conduit — it never resets
  the board — so it is available on every aarch64 board, including a spin-table
  Pi 4 whose firmware tree declares no `/psci` node. Wired into
  `Aarch64BinArch::machine_takeover` behind the supervisor-only `TakeoverGrant`.
  Proven by `tests/integration/supervisor_memtest_takeover_qemu_aarch64`.
- **x86_64 (done).** `kernel/arch/x86_64/src/takeover.rs` + `takeover.s`:
  every AP is already parked by the arch-neutral quiesce (the caller's
  `quiesce_others` delivers the stop IPI on `TIMER_VECTOR`; each AP halts in
  `preempt`'s timer dispatch), so the body masks `RFLAGS.IF` (`cli`; no lockup
  watchdog wired). Long mode cannot drop paging, so instead of flattening the
  MMU the port switches `sp` to a reserved 64 KiB `.bss` stack
  (`_takeover_switch_stack`, mapped in every address space), then switches
  `%cr3` to the **reserved** boot page tables (`boot_pml4`, all in `.boot.bss`,
  mapping both the higher-half physmap the sweep writes RAM through and the low
  identity window) so the sweep depends on no page-table frame in the usable RAM
  it destroys; and runs the arch-neutral `sweep` over usable RAM (never returns;
  otherwise parks on `hlt`). Wired into `BinArch::machine_takeover` behind the
  supervisor-only `TakeoverGrant`. Proven by
  `tests/integration/supervisor_memtest_takeover_qemu_x86_64`.
- **wasm32** stays `NotSupported` (a sandbox owns no physical RAM to take over).

### Stage C — the pre-jump synchronous audit and the seam

- Extend the `SupervisorSystem` seam (`kernel/core/src/supervisor_system.rs`)
  with a `memtest_takeover(...) -> !`-shaped control method (mirroring how
  `reboot`/`poweroff` are the state-changing methods). It is driven by the
  **one** `memtest` command in `lib/supervisor`: there is no separate
  safe test and **no confirmation prompt**. `drive_takeover` first
  stops every other CPU (`quiesce_others`, fail-closed
  `takeover_cpu_quiesce_timeout`), then drives the takeover.
- **No typed confirmation.** The earlier design gated the whole-RAM test
  behind a typed phrase because a bounded, safe `memtest` existed to
  fall back to. That safe test has been removed, so there is nothing to
  disambiguate from: the command *is* the whole-RAM test, and prompting for
  a second confirmation of an already-explicit command would be noise.
- **Audit *before* the jump, synchronously.** The in-memory `BOOT_AUDIT_RING`
  is destroyed by the takeover, so a new stable id (extend the Supervisor
  `41xx` range, e.g. `4157 SUPERVISOR_MEMTEST_TAKEOVER`) is flushed to the
  **persistent serial/log sink synchronously** before control leaves the
  kernel — after the jump nothing can be recorded. This is the one audit that
  must not rely on the retained ring.
- **Unsupported platform** (`machine_takeover()` is `None`, or the slice
  returns `NotSupported`/a quiesce timeout) reports the reason fail-safe and
  stays in the REPL (§2.9).

### Stage D — the fullscreen memtest86-style UI (first consumer of §8)

**Done.** `lib/supervisor::memtest_ui::MemtestUi` — the memtest86-style
fullscreen presenter, built entirely on the §8 `Screen` (alt-screen + hidden
cursor via `enter_fullscreen`, a reverse-video title banner, the RAM-under-test
and tested-so-far figures, a green absolute-positioned progress bar, a live
percentage, and a coloured pass line / red fault table), with the §8 plain-text
fallback (one injected `plain` flag, no probe) that degrades to concise,
deduplicated lines for a dumb serial line. It reuses the §8 `screen` module —
no second escape-emitting path (§2.2) — and renders **only** from the Stage-A
engine's `on_progress(tested, total)` callback and the final `SweepObserver`
(mapped in from the kernel as plain integers, so `lib/supervisor` names no
kernel type); its arithmetic is purely presentational and nothing panics on any
input. The kernel-side `sweep` closure the port runs (built in
`kernel/core::supervisor_system::drive_takeover`) builds the `Screen`/`MemtestUi`
over the takeover console and, **before any write**, copies the frame
allocator's currently-**free** runs
(`tairix_kernel_mem::FrameAllocator::for_each_free_region` via
`ram_snapshot_free_regions`) into a reserved-memory snapshot on the takeover
stack — carving out the live console framebuffer
(`KernelArch::console_framebuffer`) — so it depends on no RAM it is about to
overwrite. This is the memtest86 model applied honestly, with the frame
allocator as the **single authority** on what is safe to write: a tester cannot
test its own resident working set, and here the allocator already marks *every*
in-use frame used — the kernel image, takeover stack, and identity page tables
(reserved), the kernel heap (the takeover's live console cell grids, audit ring,
and everything allocated past the bootstrap arena), a DMA buffer a device may
still map non-cacheably, and all driver and userland memory. Only free RAM is
swept. This whack-a-mole-proof design is what finally fixed a real Raspberry
Pi 4: earlier attempts snapshotted the boot map's "usable" regions and tried to
enumerate and exclude the tester's working set piece by piece (framebuffer, then
grown heap regions) and kept missing one — a live, non-cacheable DMA buffer at a
fixed physical address the sweep then wrote, wedging the SoC (QEMU never exercises
that exact live-DMA layout, so CI stayed green throughout). Testing only free
frames excludes the *entire class* of in-use memory by construction. The
framebuffer is the one in-use range the allocator may not know about (firmware
carves it out of usable DRAM), so it is excluded explicitly to keep the progress
display alive. A free set so fragmented it could not fit the reserved snapshot is
refused fail-closed *before* the quiesce; a snapshot overflow post-quiesce only
truncates surplus *free* runs (never sweeps an in-use frame). The closure then feeds
`ui.progress`/`ui.record_fault`/`ui.loop_complete` and the per-window
`ui.set_current` (the live physical address under test) from the continuous
sweep, which never returns (the operator resets the machine to end the run);
`ui.set_environment` shows the reserved framebuffer extent, the region count,
and the number of excluded ranges as on-screen diagnostics. Three both-mode `Screen`
helpers (`write_u64`/`write_hex`/`clear_line_tail`) were added for it. Full host
tests over the `VecReport` mock: rich fullscreen emits escapes and the title,
the bar/percentage/figures render, the fault table carries the values, plain
mode emits **no** escape byte, plain progress deduplicates into 10% buckets, a
zero total and a narrow geometry never panic.

### Stage E — tests, docs, and the gate

- **Host tests**: the Stage A engine (above); the prompt-free `memtest`
  command over mock seams (invoking `memtest` audits the decision and drives
  the takeover seam once, with no confirmation and regardless of trailing
  args); the quiesce coordinator (`kernel/arch/api::quiesce` — peer selection,
  the all-acknowledged path, and the fail-closed bounded-timeout path over
  plain slices); the fullscreen UI byte output over a mock `Report` (and that
  `plain` mode emits no escapes); the Arch HAL `takeover` conformance double.
- **QEMU integration vertical**: because a true full-RAM takeover run
  cannot "return", boot **multi-core** (so the takeover really quiesces its
  peers), assert the fullscreen output bytes on the serial console, and assert
  the guest **ends in a reset** rather than resuming boot.
- **Docs**: `docs/src/architecture/supervisor.md` gains the takeover section
  (the one-way contract, the cross-CPU quiesce, the audit id, the Arch HAL
  slice), rustdoc on every public item, and the `plans/WIRING.md` /
  `plans/WATCHDOG.md` / `PLAN.md` cross-references.
- **Gate**: the same whole-workspace gate as §7 (`cargo fmt --all`,
  `cargo xtask ci` once, `cargo xtask fuzz --secs 5`, `tools/ci/soak.sh both
  --secs 20`), green, before the work is done. The REPL fuzz harness (§7)
  covers the prompt-free `memtest` dispatch.

---

## 10. Charter housekeeping (do in the same change)

- Add `lib/supervisor` to `AGENTS.md` §3 (`lib/*` map, one-line label).
- Add the jump-sheet row to `AGENTS.md` §15.18:
  `Pre-boot Supervisor console (ESC-at-boot REPL, pre-mount diagnostics/control) | plans/NEW-SUPERVISOR.md`.
- Add a `docs/src/architecture/supervisor.md` entry to the mdBook `SUMMARY`.
- Update `PLAN.md` if this advances a stage.

---

## 11. Status

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
  (`src/dispatch.rs`); the `*` REPL + line reader/echo (`src/repl.rs`); every
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
  **safely**. `kernel/core::audit` is only the id catalogue and
  `lib/log::BootRing` is a drain-once FIFO, so neither serves the
  Supervisor's `log`: this ring is that missing store.
  - **Home is `kernel/core`, not `lib/log` (deliberate).** It lands where both
    its producer (audit-sink composition) and its consumer (the
    `SupervisorHost`) live, and `lib/log` stays the pure record vocabulary
    rather than gaining a retained store only the kernel uses. It is a
    `tairix_log::Sink`, exactly as the seam requires.
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

**Done (the boot audit-log composition — the producer half of the item-1 wiring):**

- The `BootAuditRing` is composed into the boot **audit** channel of every
  Tier-1 boot binary. Each arch's `main.rs` now passes `BootInfo`'s audit sink
  a `tairix_log::TeeSink` fan-out that delivers each record both to the port's
  serial sink (unchanged) *and* to a retained per-arch `BOOT_AUDIT_RING`
  `static` in that arch's `boot.rs`.
  - **Fan-out defined once (`lib/log::TeeSink`, §2.2).** A `const`-constructible,
    alloc-free `TeeSink<'a, N>` over `[&'a (dyn Sink + Sync); N]`; only the ring
    `static` is per-port, because its `IrqSafeSpinLock` is parameterised by the
    arch `InterruptControl` (`RflagsIrqControl` / `DaifIrqControl` /
    `SstatusIrqControl`). riscv64 gained its first `InterruptControl`
    (`SstatusIrqControl`), and the port's existing kheap CSR adapters now
    delegate to it (one definition of the mask discipline).
  - **Arch-neutral monotonic stamp.** `kernel/core::boot_audit_ring::boot_audit_clock`
    reads the one arch-neutral wait-queue clock (`waitq::wait_now_ns`), so no
    per-arch clock code exists; records emitted before that clock is installed
    carry an honest zero (ordered by sequence regardless). The plan's earlier
    "per-arch monotonic-clock seam / none wired on x86_64" note is moot — the
    shared clock covers every port.
  - **Capacity is one shared constant** (`BOOT_AUDIT_RING_CAPACITY`, a diagnostic
    tail bound, not a scalable capacity). Host tests cover the tee fan-out and
    the clock mapping; `docs/src/architecture/supervisor.md` documents it. The
    QEMU boot verticals inject their own audit sink, so retention is
    production-only and never disturbs a test's audit interception.

**Done (the kernel consumer — the `SupervisorHost` and the ESC boot-screen):**

- `kernel/tairix-kernel/src/supervisor_host.rs :: KernelSupervisorHost<'a, B>`
  — the binding kernel's `SupervisorHost`. Rendering/control/`memtest`
  delegate to the boot-published `SupervisorSystem`
  (`tairix_kernel_core::supervisor_system()`); `log_tail` reads the retained
  `BOOT_AUDIT_RING` through `boot_log_tail()` (`seq_range()`/`record(seq)`);
  `hardware` reads `HW_TREE.snapshot()`; `disks`/`partitions`/`arxfs`/`ls`/
  `scan_disk` read the shared boot disk through independent `store.window()`
  windows + `lib/partition` + `with_system_volume`; `mount` runs the **real**
  `mount_root_disk_and_load_users` + `finish_install` (no oracle, no
  fail-open) through the *same* `UnlockInstall` cells the interactive prompt
  fills. Every state-changing decision audits a stable `41xx` id
  (`4150..=4157`). `panic-log` honestly reports "no persisted record" until a
  cross-boot panic store exists (`FIX-PANICS.md`/`WATCHDOG.md` record live,
  not persistently) — it never fabricates one.
- The ESC boot-screen window at the top of
  `root_mount.rs :: unlock_root_disk_interactively_impl`, byte-exact:
  `[Press ESC for supervisor]` → 2 s timed park (`ConsoleRead::read_timeout`,
  a genuine bounded park) → in-place redraw `\rARXFS passphrase: \x1b[K`; a
  lone `ESC` → `\rARXFS\x1b[K\r\n\r\nSupervisor\r\n` then `run_supervisor`.
  ESC-vs-CSI disambiguation is a bounded re-poll (`esc_is_lone_escape` /
  `drain_csi_sequence`). ESC **also** drops in at the live passphrase prompt
  (`read_passphrase_line` returns `PassphraseReadError::Escape` on a
  first-byte lone `ESC`); both entry points share one `enter_supervisor`
  banner+REPL definition. The host is built in `unlock_orchestrate.rs`'s
  unlock body and threaded as `Option<&mut dyn SupervisorHost>`
  (`None` in host tests, so their behaviour is unchanged); on
  `SupervisorExit::Mounted` the unlock returns `Installed` with no further
  prompt. `install_boot_log_tail(&BOOT_AUDIT_RING)` is published on each
  Tier-1 `kernel_main`.
- Compiles clean on the host and all three freestanding Tier-1 targets;
  clippy `-D warnings` clean; the 27 `root_mount` host tests pass with the
  new signature.

**Done (§8 rich screens — the colour/positioning presenter):**

- `lib/supervisor::screen` — the arch-neutral `no_std` presenter over
  `lib/vt`'s `Op`/`emit`: a `Screen { move_to, clear, set_style,
  reset_style, enter_fullscreen, leave_fullscreen }` writing through a
  bounded stack flush-sink (`ReportSink: Extend<u8>` over the `Report`
  seam), a typed `Style` (foreground/background `Color` + bold/underline/
  reverse) mapped to a deterministic `Sgr` run, and a fail-closed
  `Geometry` (default 80×24, dimensions clamped ≥1, positions clamped into
  bounds). A `plain` flag emits **no** escape bytes (text only) for a dumb
  serial line. It hand-rolls no escape encoding: host tests assert the
  emitted bytes equal the `lib/vt` encoding of the corresponding `Op`s, that
  `plain` mode emits no `\x1b`, and that the flush-sink crosses its chunk
  boundary losslessly. Re-exported as `Screen`/`Style`/`Geometry`; rustdoc on
  every public item; `docs/src/architecture/supervisor.md` "Rich screens"
  section. This is §9's Stage-D dependency and now stands complete on its own.

**Done (§9 Stage A — the arch-neutral whole-RAM full-range engine):**

- `kernel/mem::ramtest::sweep_pattern` (+ `RamTestPattern`, `SweepObserver`,
  `sweep_window`, `usable_frame_bytes`) — the whole-RAM, full-coverage
  whole-RAM test the takeover mode drives. It reuses the existing
  `WordWindow`/`PhysWindow`/`check`/`address_marker` primitives (§2.2): a
  full address-in-address pass over **every** word plus a moving-inversions
  pass (fill `PATTERN`, ascending verify→write `ANTIPATTERN`, descending
  verify→write `PATTERN`), touching every cell (not the sampling `run`'s one
  word per page) and — unlike `run`/`test_owned_window` — **never restoring**
  them, because the machine never resumes. It reports progress as
  `(tested, total)` and honours an injected `abort()` between windows,
  returning a distinct `Passed`/`Aborted`/`Faulted` outcome so an operator
  abort can never read as a clean pass. Pure `lib/*` logic over `WordWindow`,
  fully host-tested via `FakeRam`/`SimPhysMap`: healthy full pass leaves the
  pattern behind (proving every word written), each seeded fault
  (`StuckLow`/`StuckHigh`/`Alias`) caught at the correct physical offset
  including the lone-cell gap the sampling test trades away, abort stops
  early, and the progress denominator is honest. No arch, no board, no
  `cfg(target_arch)`; Stages B–E build on it.

**Done (§9 Stage C — the `memtest` command + the supervisor-only takeover gate):**

- **`memtest`** — the single `lib/supervisor` command (`commands/diag.rs`).
  There is **no** separate safe test and **no confirmation prompt**:
  invoking `memtest` audits the decision through
  `SupervisorEvent::MemtestTakeover` and immediately drives the
  `SupervisorHost::takeover_memtest` seam. Host-tested: `memtest` audits + drives
  the seam once, ignores trailing arguments, and stays fail-closed in the REPL
  when the seam returns. The REPL fuzz harness covers the dispatch.
- **Supervisor-only gate on the takeover handle.** `KernelArch::machine_takeover`
  requires a `kernel/core::supervisor_system::TakeoverGrant` — a witness with a
  private field whose only constructor (`TakeoverGrant::mint`) is module-private
  to `supervisor_system`, the module that drives `memtest`. Holding a
  `&dyn KernelArch` is therefore not enough to obtain the `MachineTakeover`
  handle: no other kernel subsystem, driver, or userland caller can mint the
  grant, so the takeover mechanism is reachable **only** from the
  Supervisor. Ports keep their takeover `static` private and hand it back only
  through this gated accessor.
- **The cross-CPU quiesce is here, in the neutral caller.** `drive_takeover`
  mints the grant, reads the gated handle and the direct physical map, then
  **stops every other CPU** — `tairix_arch_api::quiesce_others(current,
  |peer| SchedulerArch::send_ipi(arch, peer))` over the boot-published
  liveness/ack tables — as the *last fallible step* before the irreversible
  tear-down. On a bounded-timeout miss it renders `CpuQuiesceTimeout`
  fail-closed (stable cause `takeover_cpu_quiesce_timeout`, no payload value)
  and stays in the REPL, having changed nothing. Only once every peer is parked
  does it build the `sweep` closure (the Stage-A `sweep_pattern` sweep + the
  Stage-D `MemtestUi`, all `'static`/reserved per the `take_over` contract) and
  drive the **single** `MachineTakeover::take_over(&mut sweep)` operation, which
  on a supported port never returns (mask, flatten paging, sweep on a reserved
  stack, test the kernel-image region, reset).
  `KernelSupervisorHost::takeover_memtest` wires it in and audits id
  `4157 SUPERVISOR_MEMTEST_TAKEOVER` (`Warn`) synchronously before the attempt.
  The wired riscv64/aarch64/x86_64 ports return `Some` from `machine_takeover`
  and are proven end-to-end by the per-port **multi-core** QEMU verticals
  (Stage E); `wasm32` stays `NotSupported`.

**Done (the REPL fuzz harness — the item-2 prerequisite):**

- `lib/supervisor/tests/fuzz_repl.rs` — a deterministic, seeded harness
  (`tairix_fuzzseed` LCG + budget/seed seam) driving `run_supervisor` over
  hostile console scripts: mutated real-command-word streams, pure noise, and
  over-long unterminated lines. Its invariant is "never panics, always
  terminates"; host std mocks back the seams (`mount` varies its outcome by
  passphrase length to reach every branch without echoing the secret).
  Registered as target `fuzz_repl` in `tools/xtask/src/commands/fuzz.rs`
  (`cargo xtask fuzz`) with a registry unit test.

**Done (§9 Stage D — the fullscreen memtest86-style UI):**

- `lib/supervisor::memtest_ui::MemtestUi` — the memtest86-style fullscreen
  presenter built entirely on the §8 `Screen` (no second escape path, §2.2):
  alt-screen + hidden cursor, a reverse-video title banner, the RAM-under-test
  and tested-so-far figures, a green absolute-positioned progress bar and a
  live percentage redrawn in place as the whole percent advances, and a
  coloured pass line / red fault table for the final outcome, with a plain
  line-oriented fallback (one injected `plain` flag, no probe) that
  deduplicates into 10% buckets on a dumb serial line. It renders **only** from
  the Stage-A engine's `on_progress(tested, total)` and the final
  `SweepObserver` faults (mapped in as plain integers, so the crate names no
  kernel type); the arithmetic is purely presentational and nothing panics on
  any input. The kernel-side `sweep` closure built in
  `kernel/core::supervisor_system::drive_takeover` drives it over the takeover
  console (the port runs that closure on a reserved stack); three both-mode
  `Screen` helpers
  (`write_u64`/`write_hex`/`clear_line_tail`) were added for it. Full host
  tests, rustdoc on every public item, and the `docs/src/architecture/supervisor.md`
  full-screen-display section.

**Done (the ESC boot-screen QEMU verticals — aarch64 + x86_64 siblings):**

- Two sibling verticals prove the byte-exact ESC boot-screen contract on the
  **production** boot path, one per PC-class arch, each booting its pipeline
  over the shared planted encrypted-root image (so the real `SupervisorHost`
  is installed and the boot screen is drawn) and swapping only the audit sink
  (§2.2): `tests/integration/supervisor_esc_qemu_aarch64`
  (`tairix-test-supervisor-esc-qemu-aarch64`, over the UART) and
  `tests/integration/supervisor_esc_qemu_x86_64`
  (`tairix-test-supervisor-esc-qemu-x86-64`, over COM1 — the
  `plans/ARCHSUPPORT.md` parity sibling, reusing the x86_64 admission
  vertical's `boot_x86_64::boot` bin body).
- Both drive the **one** shared serial script `SUPERVISOR_ESC_SCRIPT` in
  `tools/xtask/src/commands/qemu_tests.rs` (the frozen boot-screen contract
  has a single definition, never a per-arch copy, §2.2): `ESC` at the
  `[Press ESC for supervisor]` window → `help` at the `Supervisor` banner's
  `*` prompt → `commands:` (the dispatcher's `Supervisor commands:` header) →
  `continue` → the typed passphrase at the redrawn `ARXFS passphrase: `.
  Reaching each marker in order is the byte-exact assertion (the run fails
  loud if the guest exits before every step is sent), and PASS keys on the
  unlock-service install witness (`EventId(4139)`), which can only follow
  `continue` resuming the normal unlock and that unlock mounting the root —
  proving a Supervisor session is transparent to boot.
  `docs/src/architecture/supervisor.md` documents both.
- **Enabling fix — the unlock kthread reader has a timed backstop.**
  `unlock_service::KthreadConsoleRead::read` now bounds an empty park with a
  one-shot re-poll deadline (`CONSOLE_READ_REPOLL_NS`) when no secret-marker
  animation is scheduling a wake, instead of parking solely on the RX
  interrupt's `console_wake`. The bootstrap-floor console is poll-backed and
  its receive interrupt is not guaranteed to be routed this early on every
  port (it is a fail-closed no-op when the console IRQ is unresolved, as on the
  x86_64 QEMU PC target), so a read whose only wake would be that interrupt —
  the echoed REPL prompt, or the first byte of a passphrase before the marker
  animates — would otherwise hang forever. The fix is the one shared reader
  definition (§2.2), so it covers both the REPL and the passphrase read on
  every port; the fast `console_wake` path is unchanged and it is a tickless
  one-shot park, never a busy-spin (§2.23).

**Done (the *other*-trigger-point QEMU verticals — item 1):**

- Two further aarch64 QEMU verticals now drive the two remaining ESC trigger
  points on the production boot path, **reusing the exact same
  `tairix-test-supervisor-esc-qemu-aarch64` bin** as the announcement-window
  vertical — the guest is byte-identical, only the host-side serial script
  differs, so no bin is duplicated (§2.2):
  - `SUPERVISOR_ESC_AT_PROMPT_SCRIPT` deterministically enters at the **live
    passphrase prompt** (it waits for the redrawn `ARXFS passphrase: `, which
    only appears once the 2 s window has elapsed untouched, then types a lone
    `ESC` as the line's first byte, exercising `read_passphrase_line`'s
    `PassphraseReadError::Escape` drop — the path the window-race-robust
    original reached only incidentally), then `help` → `continue` →
    passphrase.
  - `SUPERVISOR_MOUNT_SCRIPT` drives the **`mount`-from-REPL** path (the
    Supervisor performing the real unlock itself, distinct from `continue`
    resuming the normal unlock): enter at the window, `mount`, then satisfy
    `cmd_mount`'s own `ARXFS passphrase: ` prompt. The interactive unlock then
    resolves to `Installed` and logs the install witness with no second
    prompt.
  Both key PASS on the same unlock-service install witness (`EventId(4139)`),
  which the interactive unlock logs whenever it resolves to `Installed`
  (including the `mount`-from-REPL path). All three scripts spell the frozen
  boot-screen states through one shared set of markers (host-tested for
  drift). Enabling this reuse, the runner's `backing_image_path`
  (`tools/xtask`) now disambiguates the planted backing images of enrolments
  that share one built binary by their stable `TESTS` index, and `TESTS` was
  made a `static` so that index lookup (via `std::ptr::eq`) is sound — a
  `const` need not have a single address, which would have silently collapsed
  every shared entry onto one image path. Host tests cover the frozen-marker
  consistency and the no-collision invariant; the three aarch64 guests pass
  end to end under `cargo xtask test --qemu`.
- **x86_64 sibling not added deliberately.** These two verticals exercise
  *arch-neutral engine* paths (`read_passphrase_line`'s `Escape` arm and the
  REPL `mount` command), which are already host-tested; the arch-sensitive
  part — the byte-exact boot-screen contract over the port's console (UART vs
  COM1) — is already proven on **both** PC-class arches by the landed
  announcement-window ESC vertical. A future x86_64 parity pair is now
  trivial: register two entries against the existing
  `tairix-test-supervisor-esc-qemu-x86-64` bin with the same shared scripts.

**Done (§9 Stage B/E — the per-target takeover bodies + QEMU verticals).**
The real `MachineTakeover::take_over` bodies live in
`kernel/arch/<target>/src/takeover.{rs,s}`, each a private `static` reached only
through `machine_takeover_handle()` and wired into that port's
`KernelArch::machine_takeover` behind the supervisor-only `TakeoverGrant`. Every
other CPU is already parked by the arch-neutral `quiesce_others` before the body
runs; the body then masks interrupts, stops the watchdog where wired, brings RAM
into direct reach — riscv64 flattens to bare mode (`satp = 0`), aarch64
clean+invalidates the kernel image to PoC then writes the MMU-off `SCTLR_EL1`
(`paging::SCTLR_MMU_OFF`; both are identity-mapped so `virt==phys` survives),
and x86_64 (which cannot drop long-mode paging) instead installs the reserved
boot page tables (`%cr3 = boot_pml4` in `.boot.bss`) — switches onto a reserved
64 KiB `.bss` stack (`_takeover_switch_stack`) the sweep cannot overwrite, and
runs the caller's arch-neutral `sweep` (Stage-A `sweep_pattern` + Stage-D
`MemtestUi`). The sweep tests all of RAM continuously and never returns; the
operator ends the run by resetting the board, so **no port resets the machine
itself and none needs a reset conduit** (a spin-table Pi 4 with no `/psci` node
is fully supported). Each body's `-> !` tail is a masked halt-park, reached only
if a future finite sweep ever returned. The one region a continuous run cannot
test is the resident kernel image + its reserved stack, exactly as a running
memtest86 cannot test its own resident code. `wasm32` stays `NotSupported`.

Each is proven end to end by
`tests/integration/supervisor_memtest_takeover_qemu_<target>`, which boots the
production pipeline and drives the real
`supervisor_system().memtest_takeover(...)` seam directly (no console this early
has interactive input, so the command dispatch is host-tested in
`lib/supervisor`). The x86_64 vertical boots **multi-core** (its CPUs
discovered from ACPI), so the takeover genuinely quiesces its peers; the
aarch64 and riscv64 verticals boot single-core and take the no-peers quiesce
path (a continuous-memtest guest is kept single-core so it stays a light
citizen in the parallel matrix). Because the guest never resets itself, each vertical
ends it deterministically: once the guest prints the completed-test-loop marker
(`memtest: completed test loop`), the harness issues a QEMU-monitor
`system_reset`. A marker-gated reset-is-success rule
(`tairix_qemu::Spec::with_reset_success_marker`) accepts the resulting status-`0`
`-no-reboot` exit as a pass **only** when the serial also carries that marker, so
a crash-into-reset before a completed loop still fails loud.

The arch-neutral engine, the machine-control seam, the boot audit-log
composition, the `SupervisorHost`, the ESC boot-screen (both entry points),
the §8 rich-screen presenter, the REPL fuzz harness, the §9 Stage-A
whole-RAM full-range RAM engine, the §9 Stage-B `MachineTakeover` Arch HAL
slice (arch-neutral surface + conformance + supervisor-gated `KernelArch`
seam), the §9 Stage-C `memtest` command, the §9 Stage-D fullscreen
memtest86-style UI, the aarch64 + x86_64 ESC boot-screen QEMU verticals and
the two further aarch64 trigger-point verticals, and the **§9 riscv64,
aarch64, and x86_64 takeover bodies + their Stage E QEMU
verticals** are complete and compiling on every Tier-1 target. `wasm32` stays
`NotSupported` (a sandbox owns no physical RAM to take over). **§9 is
complete on all four Tier-1 targets.**
