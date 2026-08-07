# `tairix-terminal` — terminal emulator

Stage 7 deliverable (`AGENTS.md` §10, `PLAN.md` Stage 7). The default
graphical terminal: it hosts the system shell and shows its output on a
character-cell screen rendered through the shared desktop theme. Installed as
a `.app` bundle under `/Apps` (`AGENTS.md` §16.5).

## What this crate is

A screen **model** (`Grid` + `Parser`, tied together by `Terminal`) driven by
an injected `ShellSource`, a **renderer** (`render`), and everything that
makes it a first-class desktop program: the user's `Profile`, the colour
`Scheme`s a screen is painted with, the `Effects` pipeline a frame passes
through, the window `layout` rules, the right-click `menu`, and the
`settings` sheet. Like the file browser it is a graphical app, so it consumes
the same `lib/*` building blocks the taskbar does — `lib/geometry`,
`lib/theme`, `lib/raster`, `lib/font`, `lib/controls`, `lib/input` — and never
depends on the window manager (`AGENTS.md` §17.4). The staged design is
`plans/GUI-TERMINAL.md`.

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
never index out of bounds or panic (`AGENTS.md` §2.9). Its semantics — the
owed wrap at the right edge above all — are pinned by the shared
`tairix_vt::conformance` script the framebuffer boot console runs too, so the
two screens a program can be drawn on cannot disagree about where its output
lands.

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

## Window geometry (`layout`)

A terminal's natural size is a character count, not a pixel count, so the
window is whatever the conventional 80×25 screen (`COLS`×`ROWS`) measures in
the face actually being drawn with. `grid_size` and `grid_dims` convert
between a grid and pixels through the face's own advance and line height;
`chrome_insets` reads the window furniture from the one shared
`WindowFrame::insets` definition the compositor decorates with, so the app and
the window manager cannot disagree about a decorated window's extent.

When a display cannot hold that grid, `fit_font_size` steps the *text size*
down — never the grid — until it fits, stopping at `MIN_FONT_SIZE_PX`: a
terminal that quietly dropped to 60 columns would break every program that
lays itself out for 80. There is deliberately no compile-time window size:
the face's metrics come from the font service at runtime and can differ from
the compiled-in atlas fallback, so only a runtime measurement is honest.

## Colour schemes (`scheme`)

One place a terminal colour comes from. A `ColorScheme` is the sixteen ANSI
slots plus background, foreground, cursor, and cursor text; `Painted` resolves
the scheme in force once per repaint (never once per cell) and answers each
cell's pair. `Scheme::System` follows the desktop theme and is the default;
`Midnight`, `Phosphor`, `Amber`, `Ember`, `Contrast`, and `Paper` carry fixed
palettes; `Custom` is the user's own. Adding a scheme is a variant plus a
palette, and the compiler then forces every consumer to state what it means.

## The profile (`profile`)

Everything a user can change — the scheme, their custom colours, the text
size, and every effect strength — is one `Profile`, stored per user at
`~/Settings/Terminal/terminal.conf` in the same `key value` / `#` comment
grammar every line-oriented TAIRiX configuration store shares. Closed key
registry (`ProfileKey`), whole-document fail-closed parse, `render`/`parse`
exact inverses, every key always emitted. An absent document means the
documented defaults; an unusable one also means the defaults and says why on
`stderr`. Colours are bare `rrggbb`, never `#rrggbb`, because the grammar's
comment marker would cut the line at the `#`.

## Screen effects (`effects`)

A typed, ordered `Pass` list rather than code inlined into the renderer, so a
display that can composite hardware layers can programme its own engine from
the same description with the software passes staying the conformance oracle.
Translucency is not a pass: the default background is filled at the profile's
alpha, so the compositor's own premultiplied blend does the work and a glyph
stays opaque over it. Backdrop blur is the compositor's, since only it can see
behind a window. Scan lines, fuzz, phosphor persistence, and wobble run over
the finished frame in integer arithmetic, with the phosphor trail reusing one
grown-once buffer rather than allocating per frame. Every animated pass is a
pure function of a monotonically increasing `Phase`, which the `Run` binary
advances on a one-shot frame deadline — never a poll loop, and no wake at all
when the effects are off.

## Menu and settings (`menu`, `settings`, `swatch`)

A secondary press opens a context menu of six typed `Command`s, built from one
ordered list and read back through the same list so a reordering cannot re-map
a row; every advertised shortcut is really honoured by `Command::accelerator`.
The popup is the shared `lib/controls` `Menu` placed by that control's own
`anchored_rect` rule. `Settings` is an in-window modal sheet composed from the
shared Reactive Alloy controls plus the app-local `SwatchGrid` colour wells;
every edit clamps through `Profile::clamp`, so the sheet can never produce an
invalid profile.

## Rendering (`render`)

`render(terminal, painted, viewport, font)` paints the grid into a `lib/raster`
`Surface` sized to the viewport, using the resolved colours and the shared
`lib/font` monospace face it is given. The face is the caller's because its
size follows both the profile and the desktop's UI density: the `Run` binary
resolves it from the two and re-resolves it (re-deriving the grid and resizing
the pty) when either changes, so the renderer always measures cells at the
size the window is actually drawn at.

Each cell is drawn with its own rendition:
its `lib/vt` `Attributes` select the foreground and background, resolved one
way (`AGENTS.md` §2.2) — a `Default` colour takes the scheme's own foreground
or background, the 16 basic colours and the low 16 palette entries take the
scheme's ANSI slots, the 6×6×6 cube and greyscale ramp above them are the
fixed xterm arithmetic no scheme reinterprets, truecolour is used directly,
`reverse` swaps the pair (opaquely, so a highlight cannot show the desktop
through), and `bold` brightens a basic colour. The visible cursor cell is the
scheme's cursor block. The surface is the compositor's to place and round —
there is no rounding here. Every length saturates and every blit clips, so a
viewport smaller than the grid paints what fits rather than panicking
(`AGENTS.md` §2.9).

## Seam

`ShellSource::read() -> Result<Vec<u8>, Errno>` and
`ShellSource::write(&[u8]) -> Result<(), Errno>` are the one thing the
terminal needs from outside. On a running system the seam is
`spawned::PipeShellSource`: two kernel pipes to a shell child the terminal
spawned under its own `CAP_PROC_SPAWN` (`plans/APPWIN.md` AW4), wired at
spawn through `spawned::shell_wires` — the child's stdin is the keystroke
pipe, and its stdout *and* stderr land on the one output pipe a terminal
renders. The process-spawn authority lives in the `Run` binary, behind the
seam, not in the screen model; tests wire an in-memory queue or injected
closures, so everything with behaviour is exhaustively testable without a
kernel (`AGENTS.md` §7).

## The `Run` bundle

`src/run.rs` is the `terminal.app` bundle's entry point: it reads the user's
profile, creates the pty, spawns the user's default shell
(`tairix_users::policy::DEFAULT_SHELL`) with `TERM` exported, creates and
grants the zero-copy window frame region, and **parks** on one wait-set with
three members — its window-event mailbox (`WaitSourceKind::Port`), the
shell-output pipe's read end (`WaitSourceKind::Stream`, the AW4 kernel
addition: ready on buffered bytes or end-of-stream), and the shell child
(`WaitSourceKind::Child`) — dispatching on the woken member's token, never a
poll loop. The park carries a one-shot frame deadline only while an animated
effect is in force. A key press is claimed by an open menu or settings sheet,
else by a terminal accelerator, else encoded through the one shared
`lib/keymap` rule; shell output is pumped into the grid and the repainted
frame presented. A settings change re-derives the colours and the face,
re-applies the backdrop blur, reshapes the grid and the pty, and rewrites the
profile document. The shell exiting, the user choosing *Close*, or a
`CloseRequested` from the desktop ends the session cleanly; every bring-up
refusal exits fail-loud with a reserved code and its reason on `stderr`.

The bundle's manifest requests `CAP_FS_ACCESS` for the profile document — an
ordinary read and write under the launching user's own identity, reaching
nothing they could not already reach — alongside `CAP_CONSOLE_WRITE`,
`CAP_PROC_SPAWN`, and `CAP_SHM`.

## Layering & safety

`no_std` (with `alloc`); depends only on `tairix-abi` and the shared `lib/*`
desktop libraries, so this app never links a kernel, driver, or window-manager
crate (`AGENTS.md` §17.4). No `unsafe`, no `unwrap`/`expect`/`panic!` in
production paths (`AGENTS.md` §2.9).

## Test surface

`cargo test -p tairix-terminal`: grid sizing fail-closed;
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
write-error propagation; the renderer (viewport sizing, cursor highlight,
and a degenerate zero-width viewport); and the spawned-shell wiring (the
attach block's wire layout surviving the kernel's own canonical parse, the
bounded one-chunk read, end-of-stream and error refusals, the short-write
resume loop, and the wedged-channel fail-closed refusal).

The desktop half is covered beside its own modules: the colour schemes (hex
round-trips, every scheme resolving, the whole `Attributes`→colour mapping
including the bright/cube/grey/reverse rules, and every built-in's text being
legible against its own ground); the profile document (round-trip exactness
and every documented refusal, with its line number); the layout rules (the
headline 80×25-plus-furniture-fits-640×480 property, the step-down search, and
the grid/pixel inverses); the effects (determinism, each pass's visible
result, the phosphor trail, and degenerate surfaces); the menu (accelerators,
row activation, dismissal, viewport clamping); and the settings sheet and
colour wells (rendering at a tiny and a small-screen viewport, every control
reaching its profile field, keyboard-only reachability, and clamping).
