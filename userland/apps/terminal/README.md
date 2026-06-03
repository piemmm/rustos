# `rustos-terminal` — terminal emulator

Stage 7 deliverable (`AGENTS.md` §10, `PLAN.md` Stage 7). The default
graphical terminal: it hosts the system shell and shows its output on a
character-cell screen rendered through the shared desktop theme. Installed as
a `.app` bundle under `/Apps` (`AGENTS.md` §16.5).

## What this crate is

A screen **model** (`Grid` + `Parser`, tied together by `Terminal`) plus a
**renderer** (`render`), both driven by an injected `ShellSource`. Like the
file browser it is a graphical app, so it consumes the same `lib/*` building
blocks the taskbar does — `lib/geometry`, `lib/theme`, `lib/raster`,
`lib/font` — and never depends on the window manager (`AGENTS.md` §17.4).

The emulator is a **consumer of the shared `lib/vt` ANSI/VT/xterm vocabulary**
(`plans/CURSES.md` C2): its `Parser` is a thin adapter over `lib/vt`'s
streaming parser and its cells store `lib/vt`'s `Cell`/`Attributes`, so there
is exactly one escape-sequence definition in the tree — never a second,
divergent parser in this app (`AGENTS.md` §2.2).

## Screen model (`Grid`)

`Grid` is a fixed `cols`×`rows` rectangle of `lib/vt` `Cell`s (a glyph plus its
folded `Attributes`) with a cursor and a current rendition pen. It exposes the
cursor-relative operations a terminal needs — write a glyph with the pen
(wrapping and scrolling at the edges), the C0 moves, absolute/relative cursor
positioning, the ANSI erase operations, the scroll region and explicit
scrolling, the alternate screen (which saves and restores the main screen),
cursor visibility, the saved cursor, the window title, and clear. Every
operation is total and saturating, so an out-of-range coordinate clamps and a
full region scrolls rather than growing: a hostile or buggy byte stream can
never index out of bounds or panic (`AGENTS.md` §2.9).

## Control parser (`Parser`)

`Parser` is a thin adapter over `lib/vt`'s streaming parser: it lets `lib/vt`
turn shell output bytes into the shared `Op` vocabulary and applies each `Op`
to the `Grid`. The emulator is therefore xterm-class — printable text and
Unicode, the C0 controls, SGR rendition with the 16/256/truecolour colour
models, cursor movement and absolute positioning, the erase operations, the
scroll region (`DECSTBM`) and explicit scrolling, the alternate screen
(`?1049`), cursor visibility (`?25`), the saved cursor (`ESC 7`/`ESC 8`), and
the OSC window title. Because `lib/vt`'s parser is total, an unrecognised,
oversized, or malformed sequence is consumed without disturbing the screen, so
a stream the terminal does not understand degrades to dropped control rather
than a corrupted display or a panic (`AGENTS.md` §2.9). Holding the
escape-sequence state in the parser keeps the screen model free of parsing
concerns (`AGENTS.md` §2.3).

### The `TERM` it advertises

Every capability `xterm-256color` implies is really parsed, so the emulator
honestly advertises that name through the `TERM` constant (`AGENTS.md` §2.2 —
no lying about capabilities). The compiled-in capability database that maps a
`TERM` to its full record is the next `plans/CURSES.md` stage (`lib/termcap`).

## Terminal glue (`Terminal`)

`Terminal` owns the `Grid`, the `Parser`, and the `ShellSource`:

- `pump` reads the bytes the shell has produced and applies them to the
  screen, returning how many were applied.
- `send` / `send_str` forward the user's keystrokes to the shell.

The terminal never echoes input to the screen itself: echo, line editing, and
job control are the shell's responsibility, exactly as on a real tty. A
failing seam call surfaces the boundary `Errno` and leaves the screen
unchanged (`AGENTS.md` §5.4).

## Rendering (`render`)

`render(terminal, theme, viewport)` paints the grid into a `lib/raster`
`Surface` sized to the viewport, using the active theme's palette and the
shared `lib/font` monospace face. Each cell is drawn with its own rendition:
its `lib/vt` `Attributes` select the foreground and background, resolved one
way (`AGENTS.md` §2.2) — a `Default` colour takes the theme's `on_surface` /
`surface` roles, the 16 basic colours and the 256-colour palette map through
the standard ANSI tables, truecolour is used directly, `reverse` swaps the
pair, and `bold` brightens a basic colour. The visible cursor cell is
highlighted with the accent role. The surface is the compositor's to place and
round — there is no rounding here. Every length saturates and every blit
clips, so a viewport smaller than the grid paints what fits rather than
panicking (`AGENTS.md` §2.9).

## Seam

`ShellSource::read() -> Result<Vec<u8>, Errno>` and
`ShellSource::write(&[u8]) -> Result<(), Errno>` are the one thing the
terminal needs from outside. On a running system the seam is a
capability-checked pseudo-terminal channel to the shell process, so the
process-spawn and job-control authority lives behind the seam, not in this
app; tests wire an in-memory queue, so the screen model and the renderer are
exhaustively testable without a kernel (`AGENTS.md` §7). The binary that ships
as the terminal wires the real channel (deferred until the userland
process/IPC client lands).

## Layering & safety

`no_std` (with `alloc`); depends only on `rustos-abi` and the shared `lib/*`
desktop libraries, so this app never links a kernel, driver, or window-manager
crate (`AGENTS.md` §17.4). No `unsafe`, no `unwrap`/`expect`/`panic!` in
production paths (`AGENTS.md` §2.9).

## Test surface

`cargo test -p rustos-terminal` (34 unit tests): grid sizing fail-closed;
text fill + cursor advance; right-edge wrap; last-row scroll on CRLF and
line-feed-only down-move; carriage-return overwrite; backspace; tab stops;
CSI cursor positioning (1-based and home default), relative moves defaulting
to one, erase-in-line and erase-in-display; dropping unrecognised escapes and
high bytes; SGR folding (bold/colour, reset, 256-index and truecolour); the
scroll region confining scrolling and the bottom-margin line feed; the
alternate screen saving and restoring the main screen; cursor visibility and
the hidden cursor not painting; the OSC window title; the saved cursor
round-tripping position and pen; the §2.2 emitter↔consumer "one vocabulary"
identity; `pump` applying output, the empty read, and read-error propagation;
`send` forwarding without echo, the seam capturing bytes verbatim, and
write-error propagation; and the renderer (viewport sizing, cursor highlight,
and a degenerate zero-width viewport).
