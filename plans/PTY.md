# PTY.md — the pseudo-terminal: a real tty line discipline for the GUI terminal

Binding under `AGENTS.md`. This plan takes the graphical terminal
(`userland/apps/terminal`, `tairix-terminal`) from hosting the shell over
two raw kernel pipes — with **no line discipline** between them — to hosting
it over a proper **pseudo-terminal (PTY)** whose slave behaves like a
console, so the shell runs exactly as it does on the hardware console:
local echo, canonical line editing, `Ctrl-C`/`Ctrl-Z` job control, the
raw/cooked/secret mode switch (`stream_input_mode`), a queryable window
size, and correct newline handling. This is the correct, Linux-class fix
for the terminal's broken input/output; it is not optional polish.

Read first, in order: `AGENTS.md` (all of it — especially §2.2 no
duplication, §4 microkernel/memory, §5.4 fail-closed entry points, §9 ABI
versioning, §17 modularity/layering, §20 standard streams, §27 complete
foundational primitives), `plans/APPWIN.md` (AW4 — the terminal's current
pipe wiring this replaces), `plans/SPAWN.md` (SP9 the console line
discipline, SP10 the pipe/`SpawnAttach` wiring the pty create mirrors),
`plans/DISPLAY.md` (D5 console foreground ownership), and
`docs/src/architecture/syscalls.md` (the syscall ABI discipline). Every
rule in all of them applies here.

**Note:** `abi-v1` is *not* frozen (the standing task direction supersedes
the `AGENTS.md`/`PLAN.md` language). Adding the `pty_create` syscall and its
capability today is allowed; it requires regenerating the C header
(`cargo xtask c-header --write`) and updating the syscall table + hashes,
which `cargo xtask abi-check` / `c-header` enforce.

## The defect this fixes

The graphical terminal spawns the shell (`elsh`) with its standard streams
wired to two ordinary kernel pipes (`kernel/core/src/pipe.rs`,
`tairix_terminal::spawned::shell_wires`). A pipe has **no console**, so:

- `stream_input_mode(Raw)` fails on the shell's stdin (the kernel handler
  requires the descriptor to resolve to an installed console device —
  `kernel/core/src/syscalls.rs::stream_input_mode`), so `elsh`'s
  `repl::run` falls to its **plain** loop (`run_plain`), which does **no
  local echo** and expects a *cooked tty* to have cooked its input.
- Nothing supplies that discipline: the terminal deliberately never echoes
  (echo is the tty's job), and there is no tty. Result, as reported:
  - keystrokes are invisible (no echo),
  - Return does not break the line (no `CR`→newline echo, no cooked
    line terminator visible),
  - program output "staircases" (a bare `LF` from `ls`/etc. is a line feed
    only — no implicit carriage return — because there is no `ONLCR`
    output translation),
  - the shell prompt shows the anonymous `user@host` fallback (a **separate**
    defect: the terminal forwarded only `TERM`, not the inherited `USER`/
    `HOME`/… — **already fixed**, see "Landed" below).

The kernel **console device** (`kernel/core/src/console.rs`) already
implements the full discipline for console-backed shells: input local echo
with `CR`/`LF`→`CR LF` and backspace/`DEL` erase (`echo_bytes`,
`EchoLine`), `ONLCR` output cooking (`write_output`), `Ctrl-C`/`Ctrl-Z`→
`Signal` for the foreground job (`ConsoleInput::push`), the cooked/raw/
secret mode (`set_input_mode`), foreground ownership
(`grant_foreground`/`release_foreground`), and terminal size. The pipe-
backed GUI terminal has **none** of it. The fix is a PTY whose slave reuses
**that same** discipline — never a second copy (§2.2).

## Design

A PTY is one kernel object joining a **master** end (held by the terminal
emulator) and a **slave** end (wired as the shell's fd 0/1/2). It mirrors
the existing `Pipe` object (`Arc<SpinLock<…>>`, counted ends, non-blocking
`try_read`/`try_write`/`readable` steps driven by the syscall park loop and
the `WaitSourceKind::Stream` wait-set), with a line discipline layered over
two byte rings:

```
terminal (master)                                     shell (slave)
   write  ───────────►  input ring  ──[input discipline]──►  slave read
   read   ◄───────────  output ring ◄─[output discipline]──  slave write
                                     ◄─[echo]───────────────  (echo of input)
```

- **Master write** (the terminal's keystrokes): pushed through the **input
  discipline** — in cooked mode, `Ctrl-C`/`Ctrl-Z` become a queued
  `Signal` for the slave's foreground process group and the bytes are
  echoed (with `CR`/`LF`→`CR LF`, erase handling) back onto the output
  ring; in raw/secret mode, bytes pass straight through with echo
  suppressed. This is the shell's own line editor's precondition (it echoes
  itself in raw mode).
- **Slave read** (the shell reading stdin): drains the input ring, cooked
  or raw per the current `InputMode`, exactly as the console reader does —
  including the **read bound**: at most one line per read, so type-ahead
  meant for the next reader (the foreground child the shell is about to run)
  stays in the pty instead of leaving inside the shell's own event queue.
- **Slave write** (program/prompt output): pushed through the **output
  discipline** (`ONLCR`) onto the output ring.
- **Master read** (the terminal rendering): drains the output ring raw, in
  full — program output is a byte stream, not terminal input, so the
  one-line read bound does not apply to it — and feeds it to the screen grid.

### The shared line discipline (§2.2 — one definition)

Extract the discipline logic now living in `kernel/core/src/console.rs`
into a shared `no_std` component so the console device and the pty share
**one** implementation:

- New crate `lib/tty` (stability `experimental` → `stable`), `no_std`, host
  unit-tested, rustdoc on every public item (§6). It owns the pure discipline:
  the input-echo state machine (today's `EchoLine` + `EraseSeq` assembly),
  the `ONLCR` output transform (today's `ConsoleWrite::write_output`
  default), the cooked-mode `Ctrl-C`/`Ctrl-Z`→`Signal` classifier (today's
  `ConsoleInput::push`), the terminal **read bound** (`read_bounded` /
  `is_line_delimiter` — a terminal read stops after the line delimiter, so
  queued type-ahead belongs to the terminal and not to whichever process
  read first), and the `InputMode` echo/signal predicates. It is
  sink-agnostic: it operates on borrowed buffers / emits typed events, so
  both a `ConsoleWrite` device and a pty ring can drive it.
- `kernel/core/src/console.rs` is **rewritten in place** to call `lib/tty`
  (no behaviour change; delete the now-duplicated code, §2.14). No staged
  migration, no second path.
- Reuses the existing `lib/vt` primitives (`control`, `line::EraseSeq`,
  `secret`) it already builds on — `lib/tty` is the *assembly*, not a
  re-implementation of those.

### Shared foreground (controlling-terminal) ownership (§2.2)

The controlling-terminal ownership rules (who may drain the terminal and
receive its cooked-mode `^C`/`^Z`) are identical for the console and the pty,
so they live in **one** place: `kernel/core/src/foreground.rs`
(`ForegroundOwnership` — the owner + granter atomics, the `grant`/`release`/
`clear_dead`/`current` transitions, and the lock-free `current()` the console
input filter reads from the UART RX ISR). `console.rs` is rewritten onto it
(its private `foreground`/`granter`/`fg` fields and `FOREGROUND_NONE` deleted,
its four methods now thin delegators) and the `Pty` embeds the same type. One
definition, host-tested in `foreground.rs`.

### Kernel PTY object and backings

- `kernel/core/src/pty.rs` — the `Pty` object mirroring `pipe.rs`: two
  bounded rings (flow-control bounds, not scaling capacities — §24.4,
  bounded by the shared `crate::pipe::PIPE_CAPACITY`), the `lib/tty`
  discipline state (`InputMode`, `EchoLine`, `TerminalSize`) plus the shared
  `ForegroundOwnership`, counted `PtyMasterEnd` / `PtySlaveEnd` handles
  (`Clone`/`Drop` wake the peer via `pipe_wake`, exactly as `PipeEnd`), and
  non-blocking steps for each end: `PtyMasterEnd::write` (the input
  discipline — cooked `^C`/`^Z` → `signals` for the foreground job, else
  buffered; returns the signals for the syscall layer to deliver so the
  object stays pure), `PtySlaveEnd::read` (drains input, echoes the consumed
  bytes onto the output ring in cooked mode — echo-at-read, as the console
  does), `PtySlaveEnd::write` (`ONLCR` via `write_cooked`), `PtyMasterEnd::read`
  (drains output raw), and `readable` peeks for the wait-set. Host-tested to
  the same depth as `pipe.rs` (ordering, EOF/broken on last-end drop,
  full-ring park both directions, cooked vs raw echo, `ONLCR`, `Ctrl-C`/
  `Ctrl-Z` signal, intercept gate, mode switch, size, clones, readable).

  **PTY2 lands the object ahead of its backing wiring.** The
  `OpenBacking::PtyMaster`/`PtySlave` variants and the stream read/write +
  wait-set dispatch below are deliberately deferred to **PTY3**, where
  `pty_create` is the variants' first *constructor*: adding the enum arms in
  PTY2 with no creator would be dead code (§2.14). The object is the
  self-contained, complete foundational primitive PTY2 delivers (§27); the
  syscall wiring lands with the syscall.
- `OpenBacking::PtyMaster(PtyMasterEnd)` and `OpenBacking::PtySlave(PtySlaveEnd)`
  in `kernel/core/src/aspace.rs`, joining `Pipe`. Stream `fs_read`/`fs_write`
  and the wait-set `Stream` readiness arm dispatch on them like pipe ends.
- `stream_input_mode`, `terminal_size`, and `console_foreground` recognise a
  pty-slave backing (its discipline lives on the `Pty`, not in the static
  `consoles` list) in addition to a console-index stream — the pty slave is
  a *tty* for these three terminal-control calls. Fail-closed and
  foreground-owner-checked exactly as the console path (§5.4, §17.1
  IRQ-safety unaffected — no ISR shares the pty lock).

### The `pty_create` syscall (ABI addition)

- New `SyscallNumber::PTY_CREATE` in `lib/abi/src/syscalls.rs` (next free
  number), spec + hash; regenerate `kernel/syscall/src/table.rs` and the
  `include/` C header (`cargo xtask c-header --write`); `abi-check` green.
- Signature mirrors `pipe_create`: writes two descriptors (master, slave)
  into the caller's own table via a `UserPtr` out-parameter. Unprivileged to
  *create* (like `pipe_create`), but **capability-gated** by a new
  `CAP_PTY` (or reuse of an existing coarse capability if one fits — decide
  under §5.2 minimalism at implementation time; do **not** add a capability
  without a live holder + enforcement point in the same change). The slave
  is then wired to the shell via the existing `SpawnAttach`/`FdWire::Handle`
  path (no new spawn surface).
- `lib/rt::pty_create` wrapper, `lib/abi-sys` `tairix_sys_pty_create` stub,
  regenerated C header — the full `abi-v1` surface, as every syscall.
- Kernel handler stubs added to the proptest/fuzz `SyscallHandlers` impls
  (`kernel/syscall/tests/proptest_model.rs`, `fuzz_args.rs`) and a fuzz
  harness for the new decoder (§19.6).

### The terminal app rewrite

- `tairix_terminal::spawned`: replace the two `pipe_create`s + `shell_wires`
  with one `pty_create`; the master end backs the `ShellSource`
  (`PipeShellSource` generalised / renamed to a pty-master source), the
  slave is wired to the shell's fd 0/1/2 (stdout **and** stderr share the
  slave, fd 3 closed — unchanged intent).
- The terminal sets the pty window size to its `COLS`×`ROWS` at create (so
  `elsh`'s `terminal_width` prompt sizing and any full-screen app work).
- `elsh` is **unchanged**: over a console-like slave, `stream_input_mode`
  succeeds, so it runs its full interactive editor (history, arrows,
  completion) and switches raw/cooked around each command exactly as on the
  hardware console — the Linux-class experience.
- Env forwarding stays (already landed): the shell sees the real `USER`/…

### QEMU vertical

Extend the `autoload_input` aarch64 vertical (the AW4 terminal path) to
prove the discipline end to end through the graphical terminal. The robust,
serial-observable end-to-end witness is **`Ctrl-C` job control** (it
exercises cooked mode + foreground ownership + the signal path at once): the
AW4 command becomes the *blocking* `sleep 3600`, and once the guest witnesses
its spawn it emits `CTRL_C_ARM_MARKER`, gating a `Ctrl-C` injection whose
cooked `^C` signals the foreground `sleep` dead; the shell — parked in `wait`
until then — recovers and spawns `true`, and that second spawn is a twelfth
PASS witness (`ctrl_c_recovered`), reachable only if the interrupt worked
(else the run times out, fail loud).

Echo and `ONLCR` have **no** robust serial witness on this path: over the pty
slave `elsh` runs its interactive editor and echoes *itself* in raw mode (so
an on-screen dump would witness elsh's echo, not the pty's cooked echo), and
the screendump asserts are coarse colour/region checks, not text OCR. Adding
fragile pixel-glyph heuristics would fail a senior review; echo and `ONLCR`
are covered exhaustively by the `lib/tty` + `kernel/core/pty.rs` host tests
instead. Delivery counts re-base off the new command (round trip = 28,
recovery = 40, `FM9_TYPING_DONE` = 41); `qkeycode_for` gains `\u{3}` →
`ctrl-c` so the seat keyboard can type the interrupt.

## Stages

- **PTY0 — env inheritance. `[x]` landed.** The terminal forwards its own
  inherited environment (with its own `TERM`) to the shell via the shared,
  host-tested `tairix_terminal::spawned::shell_env`, fixing the
  `user@host` prompt. (This stands alone; it does not need the pty.)
- **PTY1 — `lib/tty`. `[x]` landed.** The console line discipline is now the
  shared `no_std` `lib/tty` crate: `write_cooked` (the `ONLCR` output
  transform, preserving the POSIX short-write contract), `EchoLine::echo` /
  `EchoLine::reset` (the local-echo state machine — `CR`/`LF`→`CR LF`, bounded
  Backspace/Delete rub-out, split Delete `CSI 3 ~` recognition over `lib/vt`'s
  `EraseSeq`), and `job_control_signal` (the pure cooked-mode `^C`/`^Z`→
  `Signal` classifier). Every entry point is sink-agnostic (a fallible or
  best-effort closure), so the same code drives a `ConsoleWrite` device, a pty
  ring, or a test recorder. `kernel/core/console.rs` is rewritten onto it in
  place — `ConsoleWrite::write_output`→`write_cooked`, the `line` field and
  `echo_bytes`→`tairix_tty::EchoLine`, `set_input_mode`→`EchoLine::reset`,
  `ConsoleInput::push`→`job_control_signal` — and the duplicated code (the
  kernel `EchoLine`, `INTERRUPT_BYTE`/`STOP_BYTE`, `write_all_bytes`) is
  deleted. 20 `lib/tty` host tests plus the unchanged, still-green console
  tests; §3, jump-sheet, `PLAN.md`, and `docs/src/desktop/apps.md` updated.
- **PTY2 — kernel `Pty`. `[x]` object landed.** The shared
  `ForegroundOwnership` (`kernel/core/src/foreground.rs`) now backs both the
  console (rewritten onto it, duplicated fields deleted) and the new `Pty`
  (`kernel/core/src/pty.rs`): two bounded rings over the `lib/tty` discipline,
  counted master/slave ends, non-blocking steps, and `readable` peeks; 20 pty
  + 9 foreground host tests, the console tests unchanged and green. The
  `OpenBacking::PtyMaster/PtySlave` variants and the stream read/write +
  wait-set dispatch are folded into **PTY3** (they need `pty_create` as their
  first constructor; adding them now would be dead code — §2.14).
- **PTY3 — `pty_create` ABI. `[x]` landed.** `SyscallNumber::PTY_CREATE`
  (**97**, unprivileged/unaudited like `pipe_create` — a pty reaches only
  the caller's own table, so no new capability is warranted under the
  minimalism rule) in `lib/abi` with its spec; `OpenBacking::PtyMaster`/
  `PtySlave` in `kernel/core::aspace` with `open_pty`, the readiness helper
  generalised to `stream_read_member`/`stream_readable`; the read/write park
  loop factored into the shared `parked_stream_read`/`parked_stream_write`
  (pipe rewritten onto it, no second copy) with the four pty steps (master
  write delivers cooked `^C`/`^Z` via `procsignal`); the `pty_create`
  handler + Dispatcher arm + `SyscallHandlers` trait method; pty-slave
  recognition in `stream_input_mode`/`terminal_size`/`console_foreground`
  via the shared `check_terminal_foreground(&ForegroundOwnership)`;
  `tairix_rt::pty_create` + `tairix_sys_pty_create` + regenerated C header +
  table hash (auto) + `abi-check` green; proptest/fuzz stubs. No new byte
  decoder is introduced (rows/cols are register args; the discipline is the
  already-fuzzed `lib/tty`), so the existing `fuzz_args` dispatch harness
  covers it.
- **PTY4 — terminal onto the pty. `[x]` code landed.** `tairix_terminal::
  spawned` rewritten: `shell_wires(slave)` wires fd 0/1/2 to the one slave,
  `PipeShellSource` renamed to `StreamShellSource` (source-agnostic over the
  pty master). `run.rs` `main` creates one `pty_create(ROWS, COLS)` (window
  size set at create), backs the source with the master, wires the slave,
  and closes the parent's slave after spawn; the wait-set Stream member is
  the master. `elsh` is unchanged — over the console-like slave it runs its
  full interactive editor. Host tests updated; docs
  (`docs/src/architecture/syscalls.md`) updated.
- **PTY5 — the QEMU vertical. `[x]` landed (pty mechanism); vertical
  currently RED on an unrelated harness drift.** The `autoload_input`
  aarch64 vertical proves the pty discipline end to end via the `Ctrl-C`
  job-control witness (see "QEMU vertical" above): the AW4 command is the
  blocking `sleep 3600`, the guest arms a `Ctrl-C` injection on its spawn,
  and the recovered `true` spawn is the twelfth PASS witness. Echo/`ONLCR`
  stay covered by the `lib/tty` + `kernel/core/pty.rs` host tests (no
  robust graphical serial witness exists for them; fragile pixel-OCR was
  rejected). `qkeycode_for` gained `\u{3}`→`ctrl-c`.
  - **Terminal-command gate (`plans/OPEN-DEFECTS.md` D19, fixed).** The
    typed command is gated on the guest-emitted `TERMINAL_FOCUSED_MARKER`
    (the terminal window's first focus delivery), not a raw window-event
    *count* the files window satisfies before the terminal exists — the
    former let the command race ahead of the terminal-focus click and land
    on the files window (`0.app`). With the fix `sleep 3600` reaches `elsh`
    and `sleep.app` spawns.
  - **Downstream count drift (`plans/OPEN-DEFECTS.md` D20, open).** The
    FM9/FM10/FM11 stages after the terminal still sequence on cumulative
    delivery counts the FONT-SERVICE speedup shifted, so the vertical does
    not yet reach PASS; the durable fix is to re-sequence them off
    guest-emitted markers too (tracked as D20). The pty `Ctrl-C` mechanism
    itself is correct and host-tested independently.

## PTY6 — the terminal window is resizable (`pty_set_size`)

`done`. The graphical terminal is a resizable window: dragging its frame (or a
maximize/restore) reshapes the character grid and updates the shell's window
size, so the shared kernel geometry both pty ends observe tracks the window
and `elsh`'s prompt sizing follows it immediately. There is no `SIGWINCH`, so
a curses application already running does not track the window directly —
it learns of the change through the `lib/curses` `Event::Resize` (`Tty::size`
polled by `getch`/`read_events`/`doupdate`, `plans/CURSES.md` Stage C5) the
next time it reads input or repaints. What it guarantees:

- **The `pty_set_size` syscall (number 98).** The tty `TIOCSWINSZ` analogue:
  the **master**-end holder sets the pty's `TerminalSize` after create, so the
  shared geometry both ends observe (`terminal_size`) tracks the window.
  Unprivileged and unaudited like `pty_create` (it reaches only the caller's
  own pty); a zero/oversized dimension fails closed `OutOfRange`, a descriptor
  that is not a pty master of the caller `NotFound`. Wired end to end:
  `lib/abi` spec + `SyscallNumber::PTY_SET_SIZE`, the kernel handler over the
  new `AddressSpaceRegistry::pty_master` resolver, `lib/rt::pty_set_size`, the
  `tairix_sys_pty_set_size` C stub + regenerated `include/` header, and
  proptest/fuzz stubs. Kernel host test proves a master-side resize updates the
  slave's `terminal_size` and fails closed on the bad cases.
- **The grid reflows.** `Grid::resize` / `Terminal::resize` reshape the screen
  to a new `cols`×`rows`, preserving the top-left overlap of the contents and
  clamping the cursor, scroll region, saved cursor, and any alternate screen —
  the tty `TIOCSWINSZ` behaviour. Host-tested.
- **The terminal app opts in.** `userland/apps/terminal` creates its window
  `resizable: true` and, on each `WindowEvent::Resized`, re-maps its frame
  region at the new client size (fail-closed, keeping the current surface on a
  refused re-map), derives the new grid from the shared monospace advance /
  line height, `Terminal::resize`s the screen, `pty_set_size`s the pty, and
  repaints. Files was already resizable; the terminal now joins it.

## PTY7 — a command the shell runs writes to the terminal

`done`. A command `elsh` runs inside the graphical terminal shows its output.
What it guarantees:

- **An inheriting spawn wire inherits the *backing*, not just the console
  slot.** A pty-hosted shell's fd 0/1/2 are open pty-slave entries and its
  console slots are therefore closed, so resolving an `FdWire::Inherit` /
  `InheritSlot` from the console table alone handed every child a *denied*
  stdout: the command ran, its writes failed `NotFound`, and the terminal
  never saw a byte. `apply_attach_wires` now clones the parent's own open
  entry behind an inherited standard slot into the child (one more live pty
  end, counted), exactly as a `Handle` wire does — the single resolution both
  forms share (`plans/SPAWN.md` SP10).
- **Only an inherited base reaches the parent's entries.** An explicitly
  selected console index is the whole base, so a child placed on a named
  console never sees its parent's pipe/pty ends; a slot the parent has no
  entry for still inherits the console table as before.
- Covered by `spawn_inherit_hands_a_child_the_parents_pty_backed_streams`
  (a pty-hosted parent spawning `command 2>&1`: both inheriting forms land on
  the same pty, the write reaches the master, fd 3 keeps the console
  fallback) and `stream_readable_peeks_a_pty_master_against_its_slaves_output`
  (the wait-set readiness peek over a pty master/slave pair).

## Status

`done` — **PTY0–PTY7 landed**: env inheritance, the shared `lib/tty` line
discipline, the kernel `Pty` object + shared `ForegroundOwnership`, the
`pty_create` ABI with its backing wiring / stream+wait-set dispatch /
pty-slave `stream_input_mode`/`terminal_size`/`console_foreground`
recognition, the graphical terminal rewritten onto one pty, the
`autoload_input` aarch64 QEMU vertical extended with the end-to-end `Ctrl-C`
job-control witness, and an inheriting spawn wire carrying the parent's own
pty backing into the command it runs. `elsh` runs its full interactive editor over a
console-like slave; echo and `ONLCR` are covered by the `lib/tty` +
`kernel/core/pty.rs` host tests, and `Ctrl-C` job control by both the kernel
host tests and the graphical vertical.
