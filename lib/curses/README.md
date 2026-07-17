# `tairix-curses`

The first-party curses / TUI screen-model library for TAIRiX's text stack, and
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
  by the terminal's `tairix_termcap::ColorDepth` (truecolour → 256 → 16 → mono),
  so a sequence the terminal could not honour is never emitted. A terminal
  without cursor addressing (`dumb`) takes a safe full-rewrite path.
- **`ColorPairs`** — the curses colour-pair table (pair `0` is the reserved
  default), with explicit `init_pair` and auto-`alloc_pair`, plus the standalone
  `downgrade` colour reducer.
- **Character width** (`char_width`/`is_wide`/`str_width`/`truncate_to_width`,
  re-exported from `lib/vt`'s `width` module — the one definition every cell
  grid shares) — knows a CJK/fullwidth/emoji glyph occupies two columns.
  `Window::add_char` writes a double-width glyph as a lead cell plus a
  `CONTINUATION` cell and the renderer prints it once and steps the cursor two
  columns, so wide text never shifts the rest of the row.
- **`Input` / `Event`** — decodes terminal bytes (through `lib/vt`'s one parser)
  into typed events: characters, arrow / function / editing keys, mouse reports,
  and bracketed-paste runs.
- **`Screen<T: Tty>`** — the I/O-injected driver tying it together
  (`wnoutrefresh`/`pnoutrefresh`/`doupdate`/`refresh`, mouse + bracketed-paste
  enabling, `resize`, `alloc_pair`). Input is read with `getch` (one event) or
  `read_events` (batched), and `set_input_mode` selects blocking, non-blocking
  (`nodelay`), or timeout waiting. The `Tty` seam mirrors the terminal app's
  `ShellSource`, so the whole pipeline is host-testable without a kernel.
- **`StreamTty`** (feature `program`, freestanding targets only) — the one
  production `Tty` over a program's inherited standard streams (fd 0/1)
  through `tairix-rt`: blocking and timed reads park in the kernel, a closed
  stream is a loud `CursesError::Io` (never a silent empty read a session
  could spin on), and an elapsed timed read is the caller's tick. Every
  full-screen tool's `Run` binary links this one definition instead of
  carrying its own copy (`AGENTS.md` §2.2).

## One vocabulary, fail closed

Every byte emitted or parsed is a `tairix_vt::Op`. The crate is `no_std` +
`alloc` and is **part of the OS**: it is the curated `/System/Libraries/`
Terminal/TUI shared-library class, so applications — OS-bundled and third-party
alike — **dynamically link** it rather than compiling it in (`AGENTS.md`
§16.4). It contains no `unwrap` / `expect` / `panic!`: an out-of-range draw is
a `CursesError`, an unknown input sequence yields no event, and an unrenderable
colour is degraded (`AGENTS.md` §2.9). Nothing here writes to fd 3 (`stdinfo`,
§20).

## Layering

`lib/curses` depends on `tairix-vt` and `tairix-termcap` (and, behind the
`program` feature on freestanding targets, `tairix-rt` + `tairix-abi` for
`StreamTty`) — all `lib/*`, never `kernel/*`, `drivers/*`, or `userland/*`
(`AGENTS.md` §17.4) — and is
text-only infrastructure outside `userland/gui/*`, so a headless image links it
freely (§17.3).

## Stability

**experimental.** C5 completed the surface — wide/UTF-8 cell handling,
colour-pair allocation, and `getch`/timeout/non-blocking input — and added the
first in-tree consumer (`userland/apps/top`). Panels-equivalent stacking is
deferred until a consumer needs it (§2.3); overlays compose today through
ordered `wnoutrefresh` calls. The API may still change while the crate is
experimental.
