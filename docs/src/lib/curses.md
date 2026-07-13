# `rustos-curses`

The first-party curses / TUI screen-model library for RustOS's text stack,
built over stages C4–C5 of the `plans/CURSES.md` build plan. A curses application
does not write escape sequences by hand — it draws into windows, and the
library makes the terminal match by emitting the smallest sequence set the
terminal supports.

It builds on the two earlier stages: output and input both flow through
`lib/vt`'s one vocabulary (`AGENTS.md` §2.2), and terminal capabilities come
from `lib/termcap`. There is no second escape-sequence table anywhere in this
crate.

Stability tier: **experimental**. C5 completed the surface — wide/UTF-8 cell
handling, colour-pair allocation, and `getch`/timeout/non-blocking input — and
added the first in-tree consumer (`userland/apps/top`).

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
  (`AGENTS.md` §7). `enter_full_screen` / `leave_full_screen` take over and
  give back the display for a full-screen session (curses `initscr`/`endwin`,
  terminfo `smcup`/`rmcup`): the alternate screen where the terminal has one
  (the covered content is restored on leave) followed by an explicit
  home-and-erase — the switch alone is a no-op on a console a predecessor
  left on the alternate screen, so a cleared buffer is never assumed — an
  in-place erase where it can only erase, and a no-op on the dumb baseline.
  Either way the application never draws over stale text from the previous
  command, and the diff base is reset so the next `doupdate` repaints what
  the application drew.
- **`Input` / `Event`** decode the terminal's bytes (through `lib/vt`'s one
  parser) into typed events: characters, the arrow / function / editing keys,
  `Ctrl-` and `Alt-`chorded characters (the "meta sends escape" `ESC`-prefix
  form), mouse reports, and bracketed-paste runs delivered as a single
  `Event::Paste`. The driver reads them with `getch` (one event) or
  `read_events` (batched); `set_input_mode` selects `Blocking`, `NonBlocking`
  (`nodelay`), or `Timeout(..)` waiting. A blocking `getch` re-reads past a
  chunk that decodes to no event (an unmodelled sequence), so `None` from it
  means exactly one thing: the channel has closed.
- **`StreamTty`** (feature `program`, freestanding targets only) is the one
  production `Tty` over a program's inherited standard streams (fd 0/1)
  through `rustos-rt`. Blocking and timed reads park the task in the kernel; a
  closed stream surfaces as a loud `CursesError::Io` rather than a silent
  empty read a session could spin on, and an elapsed timed read is the
  caller's tick, kept distinct from failure. Every full-screen tool's `Run`
  binary (`top`, `vim`, `edit`, `fstree`, `login`) links this one definition
  instead of carrying a per-app copy (`AGENTS.md` §2.2). The host build and
  the host tests never compile it, so the seam stays kernel-free there.
- **Character width** (`char_width` / `is_wide` / `str_width` /
  `truncate_to_width`, re-exported from `lib/vt`'s `width` module — the one
  definition every cell grid shares) knows a CJK / fullwidth / emoji glyph
  occupies two columns. `Window::add_char` stores a double-width glyph as a
  lead cell plus a `CONTINUATION` cell; the renderer prints it once and steps
  the terminal cursor two columns, so wide text never shifts the rest of a row,
  and a glyph that would straddle the right edge wraps whole.

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
default and cannot be redefined. `init_pair` defines a specific id and
`alloc_pair` returns the id of the requested colours — reusing an identical
existing pair, or defining the next free id when the pair is new — so an
application can request the same colours on every redraw without tracking ids
itself and without ever filling the table. `Screen::colored_attributes(fg, bg)`
composes the two steps applications actually want: it checks the terminal's
colour depth, allocates (or reuses) the pair, and returns ready-to-apply
`Attributes` — or `None` on a terminal that cannot show either colour, so the
caller falls back to a monochrome rendition (reverse video, bold, plain)
instead of mis-colouring. `top` and `edit` colour through it.

## One vocabulary, fail closed

Every byte this crate emits or parses is a `rustos_vt::Op`. It is `no_std` +
`alloc` and is **part of the OS**: the curated `/System/Libraries/`
Terminal/TUI shared-library class, so applications — OS-bundled and third-party
alike — **dynamically link** it rather than compiling it in (`AGENTS.md`
§16.4). It contains no `unwrap` / `expect` / `panic!`: an out-of-range draw
is a `CursesError`, an unknown input sequence yields no event, and an
unrenderable colour is degraded (`AGENTS.md` §2.9). Nothing here writes to fd 3
(`stdinfo`, §20).

## The shared refresh-delay grammar

The full-screen viewers (`top`, `sysmon`) all accept GNU `top`'s
`-d, --delay secs.tenths` option, and its parsed value directly
parameterises the `Screen` input timeout, so the grammar lives here once
(`delay::parse_delay_tenths`): seconds with an optional fraction of which
only the first digit (tenths) is kept, failing closed on anything else,
with a parsed zero clamped up to `MIN_DELAY_TENTHS` — RustOS never
busy-loops, a deliberate divergence each tool's Help documents. Each tool
keeps its own usage banner and error enum and maps the parser's `None`
onto its usage error.

## Layering and testing

`lib/curses` depends on `rustos-vt` and `rustos-termcap` (and, behind the
`program` feature on freestanding targets, `rustos-rt` + `rustos-abi` for
`StreamTty`) — all `lib/*`, never `kernel/*`, `drivers/*`, or `userland/*`
(`AGENTS.md` §17.4) — and is
text-only infrastructure outside `userland/gui/*`, so a headless image links it
freely (§17.3).

Tests (`AGENTS.md` §7) live next to the code (`src/tests.rs`): the window model
(wrapping, boxes, scrolling, resize), golden minimal-diff op sequences,
capability-downgrade checks (a truecolour cell on a 16-colour `TERM`),
per-terminal input decoding driven through `lib/vt`'s emitter, and the
`Screen` driver over an in-memory `Tty`. The input decoder also has a
`cargo xtask fuzz` harness (`fuzz_curses_input`) for the untrusted-byte path
(§19.5 / §19.6).
