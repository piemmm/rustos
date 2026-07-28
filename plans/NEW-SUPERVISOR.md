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
- `memtest [passes]` — the "heavy" test. Reuse `tairix_kernel_mem::ramtest`
  via `kernel/core::memtest`; add a **thorough** multi-pattern mode
  (walking-ones/zeros, address-in-address, moving-inversions) with a
  progress counter and **ESC-to-abort**. Strictly bounded, interruptible,
  fail-loud on a fault. No raw pointer arithmetic — only the safe
  `ramtest::run` over the `BootMemoryMap`. This stays the safe,
  non-destructive test; the destructive whole-RAM `memtest full` takeover
  mode is a distinct command specified in §9.
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

## 9. `memtest full` — the destructive, one-way takeover RAM test

This section is **immediate work for an AI**, staged in well-defined full
stages A–E; each stage is independently reviewable and must land complete
(§27) with its tests and docs (§7, §13). It is **additive**: the existing
non-destructive, bounded, ESC-abortable in-system `memtest` (§4.2) stays
exactly as it is. The takeover mode is a distinct, explicitly-confirmed
command.

### 9.0 Why a takeover mode, and why it is the *only* way to test all of RAM

The in-system `memtest` (`supervisor_system.rs`) runs **inside the live
kernel**: it `alloc()`s free frames, tests each with `ram_test_owned_window`,
frees them, and is deliberately capped (`MEMTEST_MAX_BYTES`, and never more
than half of free RAM) precisely because "a RAM test must confine itself to
memory it explicitly owns … never the live map (that would corrupt the
running kernel)". It therefore can **never** test the RAM the kernel image,
heap, page tables, or stacks occupy — the same wall memtest86 avoids by
owning the whole machine.

A takeover mode is the correct — and only — way to test *all* of RAM. It is a
**one-way trip**, exactly like `reboot`/`poweroff`: there is no "drop back
into the system", the only exits are reset/power-off. That irreversibility is
what makes the confirmation (Stage C) and the pre-jump audit (Stage C)
mandatory, not optional.

### 9.1 Binding decisions (whole feature)

- **Bootstrap-floor, in-kernel, no new ABI (§18.6).** Like the rest of the
  Supervisor it runs pre-mount, before any app surface; it adds **no
  `lib/abi` type and no syscall**, so `abi-v1` being unfrozen is irrelevant
  here (§0 ABI note). It adds no driver and no new authority.
- **Split arch-neutral vs arch-specific honestly (§2.20 / §2.21 / §17.2).**
  The *pattern algorithm* (walking-ones/zeros, address-in-address,
  moving-inversions) is arch-neutral and already lives in
  `tairix_kernel_mem::ramtest` — extend it with a destructive full-range
  variant shared by all four targets. The *takeover mechanism* (quiesce
  secondaries, mask interrupts + watchdog, relocate/flatten paging so the
  test can address physical RAM, cache maintenance) is irreducibly
  target-divergent and lives behind a **new Arch HAL slice**, implemented
  per `kernel/arch/<target>/`. Do **not** `cfg(target_arch)` this into shared
  code (`cargo xtask cfg-check` forbids it).
- **No raw pointer arithmetic without bounds-checked wrappers (§4).** The
  destructive writes go through the safe, range-checked `ramtest` window over
  the `BootMemoryMap` (the `WordWindow` / `PhysWindow` abstraction already
  there), never ad-hoc pointers.
- **Same threat model as the rest of the Supervisor (§0, §19.9).** It runs at
  the physical console *before the root is unlocked*, so **no key material or
  user secret is in RAM yet** — the destruction exposes nothing. That is a
  reason to audit loudly and confirm explicitly, never to relax anything.
- **No panic on any path (§2.9).** A platform that cannot take over (no
  quiesce/relocate primitive) reports "not supported" fail-safe and stays in
  the REPL — exactly like `poweroff` on a port without a power-off primitive
  (`KernelArch::poweroff` returning). It never panics, never half-tears-down
  the machine and wedges.
- **No busy-waiting as steady state (§2.23).** Quiescing the other CPUs is a
  legitimate *bounded handshake* (a documented §2.23 exception — the machine
  is being deliberately torn down, so the secondaries spin-halt under a
  bounded budget and it is documented as such), not a perpetual poll. If a
  secondary does not acknowledge within the budget the takeover **fails
  closed** (report "could not quiesce CPU N", stay in the REPL), it does not
  spin forever.

### Stage A — the arch-neutral destructive full-range pattern engine

- Extend `kernel/mem/src/ramtest.rs` with a **destructive** whole-region
  variant: given the `BootMemoryMap` and a physical-address window
  abstraction, run the full multi-pattern sweep (moving-inversions +
  address-in-address, reusing the existing `address_pass` / `test_window`
  primitives) across a physical range **without** the "leave it zeroed /
  restore" contract the non-destructive `run` keeps — because the machine
  never resumes. Report progress through an injected `on_progress(tested,
  total)` callback and honour an injected `abort() -> bool` between chunks.
- It stays pure `lib/*` logic over the `WordWindow` trait, so it is fully
  host-testable with the existing `FakeRam` double. No arch, no board.
- **Host tests**: healthy region passes; each seeded `Fault`
  (`StuckLow`/`StuckHigh`/`Alias`) is caught with the correct reported
  physical offset; abort stops early; the engine touches every word in the
  range (the point of "destructive full-range").

### Stage B — the Arch HAL takeover slice

**Done (the arch-neutral slice):** `kernel/arch/api/src/takeover.rs` follows
the `smp.rs`/`watchdog.rs` pattern — the object-safe `MachineTakeover` trait
(`unsafe quiesce_secondaries` + `unsafe prepare_takeover`, both fail-closed and
non-panicking, with a documented two-step ordering contract), the
`TakeoverError` enum (`CpuQuiesceTimeout { cpu }`, `NotSupported`,
`PrepareFailed(i64)`) with a stable `as_str()`, and the host
`takeover::conformance` vertical (`run_unsupported`) proving the fail-closed
vocabulary via an unsupported double (a genuine takeover has no harmless input,
so — unlike `smp` — a supported port is only proven by the Stage E QEMU
vertical). `KernelArch::machine_takeover` (`kernel/core/src/bootinfo.rs`) is the
exposure seam, `Option<&'static (dyn MachineTakeover + Sync)>` defaulting to
`None` (fail-closed), with no caller yet. Registered in
`kernel/arch/api/src/lib.rs` (module + re-exports + crate-doc slice entry) and
the `plans/WIRING.md` parity matrix (an *optional* slice, not a §17.2 mandatory
primitive — the §17.2 burn-down stays complete).

**Remaining (per-target implementation + conformance):** the real
`MachineTakeover` bodies under `kernel/arch/<target>/` (quiesce the secondaries,
mask interrupts, stop the watchdog, relocate/flatten paging, cache maintenance),
each wiring `machine_takeover()` to return its handle and adding its QEMU
conformance. Staged per port (cross-reference `plans/PI.md` and
`plans/ARCHSUPPORT.md` for bring-up order); `wasm32` stays `NotSupported` (a
sandbox owns no physical RAM to take over).

### Stage C — the confirmation, the pre-jump synchronous audit, and the seam

- Extend the `SupervisorSystem` seam (`kernel/core/src/supervisor_system.rs`)
  with a `memtest_takeover(...) -> !`-shaped control method (mirroring how
  `reboot`/`poweroff` are the state-changing methods), driven by a **distinct
  command** in `lib/supervisor` — `memtest full` (alias `memtest
  --takeover`). The default `memtest` stays the safe, bounded, ESC-abortable
  in-system test.
- **Explicit typed confirmation.** Because it is irreversible and destroys the
  boot, the command requires an explicit, clearly-worded confirmation before
  it does anything (a typed confirmation phrase read through the existing REPL
  line reader — no new input path). A mistyped/blank/aborted confirmation
  returns to the `*` prompt fail-closed and changes nothing.
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
engine's `on_progress(tested, total)` callback and the final `DestructiveOutcome`
(mapped in from the kernel as plain integers, so `lib/supervisor` names no
kernel type); its arithmetic is purely presentational and nothing panics on any
input. `kernel/core::supervisor_system::run_destructive_and_reset` drives it:
it builds the `Screen`/`MemtestUi` over the takeover console, feeds
`ui.progress` from the sweep, and maps `Passed`/`Faulted`/`Aborted` to
`ui.passed`/`ui.faulted`/`ui.aborted` before the reset. Three both-mode `Screen`
helpers (`write_u64`/`write_hex`/`clear_line_tail`) were added for it. Full host
tests over the `VecReport` mock: rich fullscreen emits escapes and the title,
the bar/percentage/figures render, the fault table carries the values, plain
mode emits **no** escape byte, plain progress deduplicates into 10% buckets, a
zero total and a narrow geometry never panic.

### Stage E — tests, docs, and the gate

- **Host tests**: the Stage A engine (above); the confirmation/decision logic
  over mock seams (confirm → takeover requested; decline/blank/abort → no
  takeover, return to prompt, nothing changed); the pre-jump audit id is
  emitted exactly once before the control jump; the fullscreen UI byte output
  over a mock `Report` (and that `plain` mode emits no escapes); the Arch HAL
  `takeover` conformance double.
- **QEMU integration vertical**: because a true full-RAM destructive run
  cannot "return", drive the confirmation, assert the pre-jump audit line and
  the fullscreen output bytes on the serial console, and assert the guest
  **ends in a reset** rather than resuming boot. (A tiny/emulated memory
  window keeps the run bounded in CI.)
- **Docs**: `docs/src/architecture/supervisor.md` gains the takeover section
  (the one-way contract, the confirmation wording, the audit id, the Arch HAL
  slice), rustdoc on every public item, and the `plans/WIRING.md` /
  `plans/WATCHDOG.md` / `PLAN.md` cross-references.
- **Gate**: the same whole-workspace gate as §7 (`cargo fmt --all`,
  `cargo xtask ci` once, `cargo xtask fuzz --secs 5`, `tools/ci/soak.sh both
  --secs 20`), green, before the work is done. The REPL fuzz harness (§7)
  covers the new `memtest full` / confirmation parsing.

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

**Done (§9 Stage A — the arch-neutral destructive full-range engine):**

- `kernel/mem::ramtest::run_destructive` (+ `DestructiveOutcome`,
  `destructive_window`, `usable_frame_bytes`) — the whole-RAM, full-coverage
  destructive test the takeover mode drives. It reuses the existing
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

**Done (§9 Stage C — the `memtest full` command + the supervisor-only takeover gate):**

- **`memtest full` (alias `memtest --takeover`)** — a distinct `lib/supervisor`
  command (`commands/diag.rs`), separate from the safe `memtest`, that demands
  an explicit typed `DESTROY` confirmation (a mistyped/blank/over-long entry
  cancels fail-closed and changes nothing), audits the confirmed decision
  through the new `SupervisorEvent::MemtestTakeover`, then drives the
  `SupervisorHost::takeover_memtest` seam. Host-tested: confirm → audited +
  seam driven; decline/blank → no audit, no drive; the alias; and that plain
  `memtest` never triggers it. The REPL fuzz harness covers the parsing.
- **Supervisor-only gate on the takeover handle.** `KernelArch::machine_takeover`
  now requires a `kernel/core::supervisor_system::TakeoverGrant` — a witness
  with a private field whose only constructor (`TakeoverGrant::mint`) is
  module-private to `supervisor_system`, the module that drives the confirmed
  `memtest full`. Holding a `&dyn KernelArch` is therefore no longer enough to
  obtain the `MachineTakeover` handle: no other kernel subsystem, driver, or
  userland caller can mint the grant, so the destructive mechanism is
  reachable **only** from the Supervisor. Ports keep their takeover `static`
  private and hand it back only through this gated accessor.
- **The kernel seam** `SupervisorSystem::memtest_takeover`
  (`kernel/core::supervisor_system`) mints the grant, reads the gated handle,
  and drives the ordered `quiesce_secondaries` → `prepare_takeover` handshake
  through the host-tested, fail-closed `prepare_machine_takeover` helper; on
  success it runs the Stage-A `run_destructive` sweep over the direct physical
  map and resets (never returns), and on any refusal (unsupported / quiesce
  timeout / prepare failed) it renders the reason and returns so the REPL
  stays. `KernelSupervisorHost::takeover_memtest` wires it in and audits id
  `4157 SUPERVISOR_MEMTEST_TAKEOVER` (`Warn`) synchronously before the
  attempt. Because every current port's `machine_takeover` is `None`, the
  command reports "not supported" fail-closed on all Tier-1 targets today; the
  supported path is proven end-to-end by the per-port QEMU vertical (Stage E).

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
  `DestructiveOutcome` (mapped in as plain integers, so the crate names no
  kernel type); the arithmetic is purely presentational and nothing panics on
  any input. `kernel/core::supervisor_system::run_destructive_and_reset` drives
  it over the takeover console; three both-mode `Screen` helpers
  (`write_u64`/`write_hex`/`clear_line_tail`) were added for it. Full host
  tests, rustdoc on every public item, and the `docs/src/architecture/supervisor.md`
  full-screen-display section.

**Remaining:**

1. The QEMU integration vertical driving `ESC` at both trigger points with
   the byte-exact boot-screen assertions (alongside the existing
   `UNLOCK_PASSPHRASE_LINE` script in `tools/xtask/src/commands/qemu_tests.rs`),
   asserting `continue` resumes a normal boot and `mount` unlocks and boots.
2. **§9 `memtest full` takeover** — the destructive, one-way whole-RAM test.
   Stage A (the arch-neutral destructive full-range engine), the **arch-neutral
   half of Stage B** (the `MachineTakeover` Arch HAL slice + `TakeoverError` +
   `takeover::conformance` + the now supervisor-gated
   `KernelArch::machine_takeover` seam), **Stage C** (the confirmed
   `memtest full` command + the `TakeoverGrant` gate + the pre-jump audit +
   the `memtest_takeover` seam), and **Stage D** (the fullscreen memtest86-style
   UI on the §8 `screen` presenter) are **done** (above). Remaining: the
   per-port `MachineTakeover` bodies + their QEMU conformance (rest of Stage B);
   and the destructive-run QEMU vertical (Stage E) (`planned`).

The arch-neutral engine, the machine-control seam, the boot audit-log
composition, the `SupervisorHost`, the ESC boot-screen (both entry points),
the §8 rich-screen presenter, the REPL fuzz harness, the §9 Stage-A
destructive full-range RAM engine, the §9 Stage-B `MachineTakeover` Arch HAL
slice (arch-neutral surface + conformance + supervisor-gated `KernelArch`
seam), the §9 Stage-C `memtest full` command (confirmation + supervisor-only
`TakeoverGrant` gate + pre-jump audit + `memtest_takeover` seam), and the §9
Stage-D fullscreen memtest86-style UI are complete and compiling on every
Tier-1 target; the per-port takeover mechanisms and the QEMU verticals remain.
