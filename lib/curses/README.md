# `rustos-curses`

The first-party curses / TUI screen-model library for RustOS's text stack, and
the fourth stage (C4) of the `plans/CURSES.md` build plan. Applications draw
into windows; this crate makes the terminal match, emitting the smallest
escape-sequence set the terminal supports.

It builds on the two earlier stages and never invents a second escape-sequence
table (`AGENTS.md` §2.2): output and input both go through `lib/vt`'s one
vocabulary, and terminal capabilities come from `lib/termcap`.

## What it provides

- **`Window`** — the application drawing surface: a cell `Buffer` with a cursor
  and current `Attributes`, plus `add_str`/`move_add_str`, `draw_box`/`draw_border`,
  horizontal/vertical lines, a scrolling region (`scrollok`-style), and `resize`.
  Pads are simply windows larger than the screen, shown through a viewport.
- **Minimal-diff renderer** (`render`) — diffs the desired screen against the
  last-flushed screen and emits one cursor move per change-run, one SGR
  transition per attribute change, and one `Print` per glyph. Colour is degraded
  by the terminal's `rustos_termcap::ColorDepth` (truecolour → 256 → 16 → mono),
  so a sequence the terminal could not honour is never emitted. A terminal
  without cursor addressing (`dumb`) takes a safe full-rewrite path.
- **`ColorPairs`** — the curses colour-pair table (pair `0` is the reserved
  default), plus the standalone `downgrade` colour reducer.
- **`Input` / `Event`** — decodes terminal bytes (through `lib/vt`'s one parser)
  into typed events: characters, arrow / function / editing keys, mouse reports,
  and bracketed-paste runs.
- **`Screen<T: Tty>`** — the I/O-injected driver tying it together
  (`wnoutrefresh`/`pnoutrefresh`/`doupdate`/`refresh`, mouse + bracketed-paste
  enabling, `resize`, `read_events`). The `Tty` seam mirrors the terminal app's
  `ShellSource`, so the whole pipeline is host-testable without a kernel.

## One vocabulary, fail closed

Every byte emitted or parsed is a `rustos_vt::Op`. The crate is `no_std` +
`alloc` and is **statically linked** by applications (`AGENTS.md` §16.4 — the
curated `/System/Libraries/` shared-library classes do not include a TUI
library). It contains no `unwrap` / `expect` / `panic!`: an out-of-range draw is
a `CursesError`, an unknown input sequence yields no event, and an unrenderable
colour is degraded (`AGENTS.md` §2.9). Nothing here writes to fd 3 (`stdinfo`,
§20).

## Layering

`lib/curses` depends on `rustos-vt` and `rustos-termcap` (and `lib/*`) only —
never on `kernel/*`, `drivers/*`, or `userland/*` (`AGENTS.md` §17.4) — and is
text-only infrastructure outside `userland/gui/*`, so a headless image links it
freely (§17.3).

## Stability

**experimental.** The surface is the C4 core; C5 completes it (wide/UTF-8 cell
handling, `getch`/timeout/non-blocking input, panels-equivalent stacking) and a
first in-tree consumer, so the API may still change until C5 pins it.
