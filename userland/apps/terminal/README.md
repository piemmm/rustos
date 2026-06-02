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

## Screen model (`Grid`)

`Grid` is a fixed `cols`×`rows` rectangle of cells with a cursor. It exposes
the cursor-relative operations a terminal needs — write a glyph (wrapping and
scrolling at the edges), the C0 moves (backspace, tab, line feed, carriage
return), absolute/relative cursor positioning, the ANSI erase operations, and
clear. Every operation is total and saturating, so an out-of-range coordinate
clamps and a full screen scrolls rather than growing: a hostile or buggy byte
stream can never index out of bounds or panic (`AGENTS.md` §2.9).

## Control parser (`Parser`)

`Parser` is the streaming interpreter from shell output bytes to `Grid`
operations. It recognises printable ASCII, the C0 controls, and a subset of
ANSI CSI escape sequences (cursor movement `A`/`B`/`C`/`D`, positioning
`H`/`f`, erase-in-line `K`, erase-in-display `J`). Anything else — a byte
`>= 0x80`, an unrecognised escape, or an unsupported CSI final byte — is
consumed without disturbing the screen, so a stream the terminal does not
fully understand degrades to dropped control rather than a corrupted display
(`AGENTS.md` §2.9). Holding the escape-sequence state in the parser keeps the
screen model free of parsing concerns (`AGENTS.md` §2.3).

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
shared `lib/font` monospace face. Each grid cell maps to one glyph cell and
the cursor cell is highlighted with the accent role. The surface is the
compositor's to place and round — there is no rounding and no colour algebra
here (`AGENTS.md` §2.2). Every length saturates and every blit clips, so a
viewport smaller than the grid paints what fits rather than panicking
(`AGENTS.md` §2.9).

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

`cargo test -p rustos-terminal` (23 unit tests): grid sizing fail-closed;
text fill + cursor advance; right-edge wrap; last-row scroll on CRLF and
line-feed-only down-move; carriage-return overwrite; backspace; tab stops;
CSI cursor positioning (1-based and home default), relative moves defaulting
to one, erase-in-line and erase-in-display; dropping unrecognised escapes and
high bytes; `pump` applying output, the empty read, and read-error
propagation; `send` forwarding without echo, the seam capturing bytes
verbatim, and write-error propagation; and the renderer (viewport sizing,
cursor highlight, and a degenerate zero-width viewport).
