# `rustos-curses`

The first-party curses / TUI screen-model library for RustOS's text stack, and
the fourth stage (C4) of the `plans/CURSES.md` build plan. A curses application
does not write escape sequences by hand — it draws into windows, and the
library makes the terminal match by emitting the smallest sequence set the
terminal supports.

It builds on the two earlier stages: output and input both flow through
`lib/vt`'s one vocabulary (`AGENTS.md` §2.2), and terminal capabilities come
from `lib/termcap`. There is no second escape-sequence table anywhere in this
crate.

Stability tier: **experimental** (the C4 core; C5 completes the surface and
pins the API).

## The model

- **`Window`** is the application's drawing surface — a cell `Buffer` with a
  cursor and the current `rustos_vt::Attributes`. It offers `add_str` /
  `move_add_str`, `draw_box` / `draw_border`, horizontal and vertical lines, a
  scrolling region (the `scrollok` behaviour), `set_colors`, and `resize`. A
  pad is simply a window larger than the screen, shown through a viewport with
  `pnoutrefresh`.
- **`Screen<T: Tty>`** is the I/O-injected driver. It keeps the assembled
  *virtual* screen and the last-flushed *physical* screen; `wnoutrefresh`
  composites a window onto the virtual screen and `doupdate` flushes the
  difference. The `Tty` byte channel is the one thing it needs from the outside
  world — the same seam shape as the terminal app's `ShellSource` — so the
  whole driver is host-testable over an in-memory channel without a kernel
  (`AGENTS.md` §7).
- **`Input` / `Event`** decode the terminal's bytes (through `lib/vt`'s one
  parser) into typed events: characters, the arrow / function / editing keys,
  mouse reports, and bracketed-paste runs delivered as a single `Event::Paste`.

## Minimal-diff rendering and colour downgrade

`render` walks the desired and last-flushed screens cell by cell and emits a
`rustos_vt::Op` only where they differ: one `CursorPosition` per run of
changes, one SGR transition per attribute change, and one `Print` per glyph.

Every colour first passes through `downgrade` for the terminal's
`rustos_termcap::ColorDepth`, so a truecolour application drawn on a 16-colour
`TERM` emits only colours that terminal renders — truecolour degrades to the
256-colour palette, then to the 16 ANSI colours, then to monochrome, by
capability. A terminal that cannot address the cursor (the `dumb` baseline)
takes a conservative full-rewrite path instead of absolute positioning, so even
the fallback degrades safely rather than emitting sequences it would not honour
(`AGENTS.md` §2.9).

`ColorPairs` is the curses colour-pair table; pair `0` is the reserved terminal
default and cannot be redefined.

## One vocabulary, fail closed

Every byte this crate emits or parses is a `rustos_vt::Op`. It is `no_std` +
`alloc` and is **statically linked** by applications (`AGENTS.md` §16.4 — the
curated `/System/Libraries/` shared-library classes do not include a TUI
library). It contains no `unwrap` / `expect` / `panic!`: an out-of-range draw
is a `CursesError`, an unknown input sequence yields no event, and an
unrenderable colour is degraded (`AGENTS.md` §2.9). Nothing here writes to fd 3
(`stdinfo`, §20).

## Layering and testing

`lib/curses` depends on `rustos-vt` and `rustos-termcap` (and `lib/*`) only —
never on `kernel/*`, `drivers/*`, or `userland/*` (`AGENTS.md` §17.4) — and is
text-only infrastructure outside `userland/gui/*`, so a headless image links it
freely (§17.3).

Tests (`AGENTS.md` §7) live next to the code (`src/tests.rs`): the window model
(wrapping, boxes, scrolling, resize), golden minimal-diff op sequences,
capability-downgrade checks (a truecolour cell on a 16-colour `TERM`),
per-terminal input decoding driven through `lib/vt`'s emitter, and the
`Screen` driver over an in-memory `Tty`. The input decoder also has a
`cargo xtask fuzz` harness (`fuzz_curses_input`) for the untrusted-byte path
(§19.5 / §19.6).
