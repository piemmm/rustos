# CURSES.md — First-party terminal vocabulary, termcap database, and curses library

This is a staged build plan for TAIRiX's text-mode / TUI stack. It is
**binding under `AGENTS.md`**; read `AGENTS.md` and `PLAN.md` first. Every
rule in both applies here without exception. This plan exists because the
charter requires a `lib/*` crate proposal to be written and approved in a
plan file *before* any API is invented (`AGENTS.md` §6, §15.2).

## 0. Scope and decisions (binding for this plan)

- **We adopt the ANSI / xterm escape vocabulary as canonical.** It is what
  every real terminal, terminal multiplexer, and remote Linux host already
  speaks, so TAIRiX speaks it too — emitter (curses) and consumer (our
  emulator) share *one* definition (`AGENTS.md` §2.2). Our terminal emulator
  is upgraded to a full xterm-class consumer as part of this work; the
  current subset parser in `userland/apps/terminal` is replaced by the shared
  vocabulary, never left as a second divergent definition.
- **We roll our own, comprehensively** (`AGENTS.md` §2.12): a first-party
  escape/attribute vocabulary crate, a first-party **compiled-in termcap
  database** (no external terminfo/termcap files — there is no `/etc`,
  `/usr`, or `/proc`, §16.1), and a first-party curses library. No ncurses,
  no terminfo binary DB, no external TUI dependency.
- **It is text-only infrastructure, so it lives outside `userland/gui/*`**
  and has zero dependency on the window manager/compositor; it must work in a
  headless image (`AGENTS.md` §17.3/§17.4).
- **Remote support is a first-class requirement.** Both the text-mode and the
  GUI terminal will ultimately drive *remote Linux systems* (serial / SSH).
  The termcap database therefore describes real foreign terminals, not just
  our own emulator, and the curses library must produce output a real
  `xterm-256color`/`alacritty`/`vt220` host accepts.
- **Fail closed** (`AGENTS.md` §2.9, §5.4): unknown/missing `TERM` degrades to
  a safe baseline (`dumb`, then `vt100`-class), never a panic, never a file
  read derived from `TERM`.
- **No stubs** (`AGENTS.md` §15.1): each stage ships code **plus** tests
  **plus** docs, and is only "done" when the whole-project gate (§7) is green.

## 1. Target terminal set (closed, versioned)

The recognised `TERM` values, frozen as an ABI-disciplined closed set
(`AGENTS.md` §2.4). Each entry is a capability record in the compiled-in
database:

`xterm`, `xterm-color`, `xterm-16color`, `xterm-256color`, `alacritty`,
`xterm-kitty`, `dumb`, `vt100`, `vt220`.

`dumb` and `vt100` are the fail-closed fallbacks. Adding a terminal later is a
data entry plus a capability test, not new control flow.

## 2. Layering (one-way edges, `AGENTS.md` §17.4)

```
lib/vt        → lib/* only (no kernel/driver/userland deps)   [vocabulary]
lib/termcap   → lib/vt, lib/* only                            [capability DB]
lib/curses    → lib/termcap, lib/vt, lib/* only               [TUI/screen model]
userland/apps/terminal → lib/vt (+ existing geometry/theme/raster/font)
userland (shells/apps) → lib/curses (dynamically-linked OS library, §16.4)
```

`lib/curses` (with `lib/termcap` + `lib/vt`) is **part of the OS**: it is the
curated `/System/Libraries/` "Terminal / TUI client" shared-library class
(`AGENTS.md` §16.4), so OS apps and third-party apps **dynamically link** it
rather than compiling it in — one security update to the library then covers
every consumer. (At the in-tree build level the crate is an ordinary cargo
path dependency; dynamic-vs-static is the OS-image / runtime linking model the
charter governs, not a cargo setting.) All three crates are `no_std` +
`alloc`, I/O-injected (an abstract byte source/sink, mirroring the terminal's
existing `ShellSource` seam) so the screen model and renderer are fully
host-testable without a kernel (§7).

## 3. Work breakdown (stages)

Each stage is a reviewable chunk. **At the end of each stage, replace this
plan's status with what landed and the exact next work** (overwrite it each
time — git is the history, §13), in the style of the sibling `plans/*.md`. Do
not start a stage before its predecessor is green on the whole-project gate
(§7).

### Stage C1 — `lib/vt`: the shared escape/attribute vocabulary

**Status: done** (see `PLAN.md`, "CURSES Stage C1").

**Deliverables**
- New `no_std` crate `lib/vt` (the canonical ANSI/VT/xterm vocabulary):
  - CSI/OSC/DCS introducers and the C0/C1 control set as typed constants.
  - SGR attributes: bold/dim/italic/underline/blink/reverse/strike, the
    16-colour, 256-colour (`38;5;n`), and 24-bit truecolour (`38;2;r;g;b`)
    models — emit and parse.
  - Cursor movement/positioning, erase-in-line/display, scroll region,
    alt-screen enter/leave, cursor show/hide, save/restore.
  - A shared `Cell`/attribute representation reused by both consumer and
    emitter.
  - An emitter (vocabulary → bytes) **and** a streaming parser (bytes →
    typed events) over the *same* tables, so they provably agree (§2.2).
    Bounded parameter accumulation, no `unwrap`/`expect`/`panic!` (§2.9).
- Register `lib/vt` in the workspace `Cargo.toml`, `AGENTS.md` §3, and
  `PLAN.md` (§6 requires the §3 + PLAN.md update); stability tier in the
  crate `README.md`.

**Tests** — round-trip emit→parse identity for every SGR/movement/erase op;
property test that any byte stream is consumed without panic or OOB; fuzz
target for the parser (untrusted input, §19.5/§19.6) registered in
`tools/xtask/src/commands/fuzz.rs` with a registration test.

**Docs** — `docs/src/lib/vt.md` + rustdoc on every public item; add to
`docs/src/SUMMARY.md`.

### Stage C2 — Refactor `userland/apps/terminal` onto `lib/vt`

**Status: done** (see `PLAN.md`, "CURSES Stage C2").

**Deliverables**
- Replace the terminal's private `parser.rs` control set with `lib/vt`'s
  parser; `Grid`/`Cell` consume `lib/vt`'s attribute representation. The
  emulator becomes a full xterm-class consumer: SGR colour (16/256/truecolour)
  rendered through the theme, scroll region, alt-screen, cursor visibility —
  pair every newly *advertised* capability with real parser support so the
  advertised `TERM` is not a lie (§2.2 honesty).
- Update `userland/apps/terminal/README.md` and its docs page to describe the
  shared-vocabulary consumer and the honest `TERM` it advertises for itself.

**Tests** — existing terminal tests migrated and extended for colour/scroll/
alt-screen; the §2.2 "one vocabulary" guarantee covered by a test that the
emulator parses exactly what `lib/vt`'s emitter produces.

**Docs** — terminal docs page updated in the same commit (§13).

The screen semantics both `lib/fbcon` and the terminal emulator use to apply
the `Op` stream (pending wrap, erase, scroll region, alt-screen,
save/restore) are pinned by one shared script, `lib/vt`'s
`conformance::check`, run against a `ScreenModel` each screen implements
over its own state in its own tests — so a change to one screen's semantics
that the other does not match fails a test rather than shipping a silent
divergence.

### Stage C3 — `lib/termcap`: the compiled-in capability database

**Status: done** (see `PLAN.md`, "CURSES Stage C3").

**Deliverables**
- New `no_std` crate `lib/termcap`:
  - `enum TermType { Xterm, XtermColor, Xterm16Color, Xterm256Color,
    Alacritty, XtermKitty, Dumb, Vt100, Vt220 }` (closed, §2.4).
  - A capability record per terminal: colour depth, cursor addressing, erase
    ops, alt-screen, key-input sequences (function/arrow/editing keys),
    mouse-reporting modes, bracketed paste, title-setting — all expressed in
    terms of `lib/vt` (no second vocabulary, §2.2).
  - `from_term(&str) -> TermType` parsing untrusted `TERM`, **fail-closed** to
    `Dumb`/`Vt100` on unknown/missing (§2.9, §5.4) — never a file read.
- Register in workspace `Cargo.toml`, `AGENTS.md` §3, `PLAN.md`; stability
  tier in `README.md`.

**Tests** — one capability test per `TermType`; explicit "unknown/empty `TERM`
falls back safely" test; a test that no record emits a sequence absent from
`lib/vt`.

**Docs** — `docs/src/lib/termcap.md` + rustdoc; SUMMARY.md entry.

### Stage C4 — `lib/curses`: the TUI/screen-model library (core)

**Status: done** (see `PLAN.md`, "CURSES Stage C4").

**Deliverables**
- New `no_std` + `alloc` crate `lib/curses`, I/O-injected, building on
  `lib/vt` + `lib/termcap`:
  - Screen/window model: a client draw buffer (windows/pads, cursor, current
    attribute) distinct from the emulator's server `Grid` — the legitimate
    §2.2 carve-out (different roles), not duplication.
  - **Minimal-diff renderer**: diff the desired screen against the last
    flushed screen and emit the smallest `lib/vt` sequence set the active
    `TermType` supports (e.g. degrade truecolour → 256 → 16 → mono by
    capability).
  - Input: decode key/mouse sequences from the tty via `lib/vt`'s parser +
    `lib/termcap`'s key tables into typed key/mouse events.
  - Full curses surface area: windows, sub-windows, pads, refresh/wnoutrefresh/
    doupdate, attribute/colour-pair API, box/border/line-drawing, scrolling
    regions, soft-label/keypad input, cursor visibility, `resize` handling.
  - No `unwrap`/`expect`/`panic!`; no fd-3 writes (`stdinfo` reserved, §20).

**Tests** — host-testable screen model + minimal-diff output golden tests;
capability-downgrade tests (truecolour app on a 16-colour `TERM`); input
decode tests per terminal; fuzz target for the input decoder (§19.5/§19.6).

**Docs** — `docs/src/lib/curses.md` (incl. a stability tier) + rustdoc;
SUMMARY.md entry.

### Stage C5 — Curses completeness + a demo/port

**Status: done** (see `PLAN.md`, "CURSES Stage C5"). The consumer is the
`top` process-overview viewer (`userland/apps/top`). Panels-equivalent
stacking is deferred until a consumer needs it (§2.3); overlays compose
through ordered `wnoutrefresh`.

**Deliverables**
- Complete the curses surface to "full" (the API a ported curses program
  expects): wide/UTF-8 cell handling, colour-pair allocation, `getch`/timeout/
  non-blocking input, panels-equivalent stacking if needed by C6 consumers.
- A first in-tree consumer that proves the API (e.g. a TUI front-end for an
  existing userland tool, or porting one), dynamically linking `lib/curses`
  as the OS-provided Terminal/TUI library (§16.4; an ordinary cargo path
  dependency in-tree).
- **Resize event** (TAIRiX has no `SIGWINCH`): `Tty::size` is the optional
  seam a channel reports its geometry through (`StreamTty` from the kernel's
  `TerminalSize`); `Screen` polls it at the start of `getch`/`read_events`
  (ahead of any input decoded in the same call) and of `doupdate`, and on a
  genuine change resizes itself — invalidating the physical diff base so the
  next `doupdate` repaints every cell — and queues one coalesced
  `Event::Resize`, the curses-native analogue of ncurses' `KEY_RESIZE`. An
  application blocked indefinitely in `getch` with no timeout learns of a
  resize only on its next keypress.

**Tests** — the consumer's behaviour tests; coverage ≥ 75% (userland) and the
§7 lib bar for the three new `lib/*` crates.

**Docs** — a porting guide page under `docs/src/userland/` (how to build a
curses app against `lib/curses`, capability/fail-closed notes).

### Stage C6 — Remote terminals (serial / SSH to Linux hosts)

**Deliverables**
- The text-mode and GUI terminals drive *remote Linux systems*: a tty/pty
  transport seam over serial and over an SSH channel (SSH crypto via
  `lib/crypto`, never hand-rolled, §2.12/§16.4; parsing of untrusted remote
  bytes runs under the §19.5 minimum-capability parser sandbox posture).
- Local side advertises the honest `TERM` to the remote host; the remote's
  output flows through the same `lib/vt` consumer used locally — one
  vocabulary end-to-end (§2.2).
- Capability-gated: the terminal acquires only the network/serial capability
  its session was granted; no ambient authority (§4, §5.4).

**Tests** — transport seam host tests with an injected channel; a recorded
real-`xterm-256color` session replayed through the consumer; SSH handshake
tests against `lib/crypto` test vectors; fuzz the remote-byte decoder.

**Docs** — `docs/src/userland/` remote-terminal page; update terminal docs.

## 4. Definition of done (per stage and overall)

Per `AGENTS.md` §7, over the **whole project** (never `-p`), and quote the
output:
1. `cargo fmt --all` (verify `cargo fmt --all --check`).
2. `cargo xtask ci` (clippy `-D warnings`, deps-check, cfg-check, test matrix,
   docs-check, `cargo deny`, the `--quick` fuzz/proptest gates, model-check,
   spec-review, abi-check).
3. `cargo xtask fuzz --secs 5`.
4. Anything else `.github/workflows/ci.yml` runs (e.g. `tools/ci/soak.sh`). On a
   developer machine (that's us) `tools/ci/soak.sh` runs for a **maximum of 20
   seconds** (`tools/ci/soak.sh both --secs 20`); the unbounded 24 h soak is for
   the CI/soak host only, never a developer machine.

Any failure found — new or pre-existing — is fixed or reverted before the
stage is done (§2.5, §7). Update `PLAN.md` and this file's stage statuses as
stages advance, and refresh this plan's status for the next chunk.

## 5. Charter cross-references

§2.2 (one vocabulary), §2.4 (frozen interfaces), §2.9 (no panic / fail
closed), §2.12 (roll your own; crypto is the only exception), §3 / §6 (lib
layout + registration), §7 (whole-project gate), §13 (docs in same commit),
§15 (AI-agent rules), §16.1 (no `/etc`,`/usr`,`/proc`), §16.4 (curated shared
libs; curses is the dynamically-linked Terminal/TUI class), §17.3/§17.4
(headless one-way edge), §19.5/§19.6 (parser sandbox + fuzzing), §20 (fd-3
reserved).
