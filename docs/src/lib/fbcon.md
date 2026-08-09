# `tairix-fbcon` — shared framebuffer text-console engine

`tairix-fbcon` (`lib/fbcon`) is the one architecture-neutral framebuffer text
console in the tree. Every architecture port renders its display console
through this crate, so the ANSI/VT/xterm-256color terminal emulation is defined
exactly once instead of being re-derived per target.

## What it does

It turns a byte stream into on-screen text by feeding it to the single shared
`tairix_vt::Parser`, applying each parsed `tairix_vt::Op` to a retained cell
grid, and repainting the dirtied cells once per write onto a borrowed 32-bit
scan-out surface (`&mut [u32]`), rendering glyphs with the shared `tairix_font`
compiled-in **console atlas**: every face of the console family — Latin,
Greek, Cyrillic, box drawing, arrows, punctuation and currency from
Inconsolata EX, plus the Japanese, Korean and Hebrew companions — in 8×16
cells with 16-level anti-aliased coverage, and a U+FFFD fallback for anything
outside them. Those glyphs are grid-fitted: the atlas generator snaps each
stroke onto whole pixels and every letter's baseline, x-height and cap height
onto shared rows, so a stem draws as one solid column rather than two grey
ones. The console runs in the kernel and cannot call the font service, so its
repertoire is whatever is compiled in; the companions are therefore part of the
atlas rather than left to `fontd`, and a `man` page renders in any shipped
script on the boot and headless console alike.
A wide (double-width) scalar occupies a lead plus a continuation
cell (the same `tairix_vt::char_width` layout the curses window writer
produces), and the one glyph is drawn across both. It is a full terminal:

- SGR colour: the 16 base colours, the 256-colour palette (colour cube + grey
  ramp), and 24-bit truecolour.
- Bold (brightens the base colours) and reverse video.
- Cursor motion (absolute and relative), tab, backspace, carriage return.
- Erase-in-line and erase-in-display.
- DEC scroll regions and explicit scroll up/down.
- **Scroll-up-at-bottom**: reaching the bottom of the screen scrolls the
  screen up like a real terminal rather than wrapping ring-style.
- **Alternate screen** (`CSI ? 1049 h` / `l`): a full-screen program such as
  `top` or an editor switches to a cleared alternate screen on entry and, on
  exit, the primary screen it covered is restored exactly — the xterm-family
  contract every terminal honours.

## The grid follows the panel

The cell is 8×16 pixels — what PC text consoles have used since VGA — so the
grid is the conventional `width / 8` × `height / 16`: 80×30 on a 640×480 mode,
128×48 on 1024×768, 160×64 on 1280×1024.

A denser panel does not simply hold more, smaller characters. `for_display`
magnifies the cell by the largest whole factor that still leaves a
conventional `tairix_vt::CONVENTIONAL_COLUMNS` × `CONVENTIONAL_ROWS` (80×25)
screen, so text stays legible as pixels get smaller: 1920×1080 doubles the
cell to a 120×33 grid, and a 4K panel takes the ×4 cap rather than shrinking
to an unreadable 240 columns. The grid is therefore derived from the display
the machine actually reports, never from a hand-picked pixel threshold. A
surface too small for even one conventional screen holds what it can at ×1: a
small panel is not a reason to refuse a console.

80×25 has one definition, in `lib/vt`, shared with the graphical terminal's
opening size, so the console and the terminal emulator cannot disagree about
what a normal screen is.

## Design

- **Pending wrap, not eager wrap.** Filling the last column does not wrap the
  cursor: it rests on that column with the wrap *owed*, paid by the next
  printable glyph and cancelled by anything that moves or erases first, so
  the recorded column is always a real cell — the erase operations and the
  cursor overlay never need to special-case an out-of-range column. This
  engine and the desktop terminal emulator's `Grid` are the two consumers of
  the shared `tairix_vt::Op` stream; each implements
  `tairix_vt::conformance::ScreenModel` over its own `COLS`×`ROWS` grid in its
  tests and runs the shared `conformance::check` script, so a change to one
  screen's semantics fails the other's test too.
- **Retained cell grid, one repaint per write.** The engine keeps the visible
  screen as a grid of `tairix_vt::Cell` (one glyph + its rendition per
  position). Every operation mutates only the active grid, and the cell rect a
  write dirtied is repainted from the grid **once** at the end of the write.
  Deferring the pixels to that single repaint bounds a write's render cost: a
  burst that scrolls the screen many times moves only the small,
  cache-resident grid per line and touches the framebuffer once — never one
  whole-framebuffer copy per scrolled line, which made a large console write
  monopolise the CPU for seconds on real hardware (starving the serial drain
  and every other task on a non-preemptible kernel span). Runs of blank cells
  are span-filled rather than glyph-blitted, and the framebuffer is never
  read, only written. The grid is also how leaving the alternate screen
  restores the primary one (a full-rect repaint). There are two grids
  (primary and alternate); the primary is left untouched while the alternate
  is shown.
- **Borrowed grid storage — allocator-free.** The engine is `no_std` and never
  allocates: the two grids are passed in as `&mut [Cell]` buffers. A
  freestanding boot console with no global allocator supplies a `static`; an
  allocator-having caller leaks a heap buffer sized to the discovered geometry
  (`Geometry::cell_count`). It depends on `tairix_vt` and `tairix_font` with
  `default-features = false`.
- **Host-testable.** Every operation is pure CPU pixel arithmetic over a
  borrowed slice, so the whole engine is unit-tested on the host.
- **Fail closed.** Firmware-supplied geometry is validated at construction
  (`Geometry::for_display`); a glyph the atlas cannot draw renders `?` rather
  than being dropped.
- **One surface, one presenter.** A console sharing its scan-out with a
  graphical session is hidden while that session holds the seat. The retained
  grid is what makes this lossless: a hidden console still parses everything
  written to it and touches no pixel, and taking the surface back repaints the
  whole screen from the grid — so output produced under a desktop is neither
  drawn over the composited frame nor discarded. See [Seat
  ownership](../desktop/seat.md) for the lease that drives it.

## API

- `Geometry` — validated scan-out extents plus the glyph scale the policy
  chose; `for_display(width, height, pitch_bytes)` fails closed on an unusable
  surface.
- `TextConsole::new(geometry, main, alt)` — owns the parser and the screen,
  borrowing the two `&mut [Cell]` grids (each at least `geometry.cell_count()`
  long); `write_bytes(pixels, bytes)` interprets an ANSI/VT stream and returns
  the touched pixel-row band; `clear` paints the background and homes the
  cursor. The cursor is drawn in software as a reverse-video block over its
  cell — lifted before each batch of operations and repainted after, honouring
  DECTCEM show/hide (`CSI ? 25 h` / `l`) — so a shell prompt or editor shows a
  live insertion point on the framebuffer console.
- `Cell` — the character cell (`tairix_vt::Cell`), re-exported so a caller can
  size and blank the grid buffers.
- `purge(pixels)` — the session boundary: discard everything a finished
  session left on the console. Stronger than `clear`, which blanks only the
  grid being shown: a purge blanks **both** grids, so text a program left on
  the screen it was not using cannot be revealed by whoever comes next, drops
  a partly received escape sequence so the next session's first bytes cannot
  complete a prefix the last one held, and returns every other piece of screen
  state (cursor, pen, scroll region, saved cursors) to its initial value — a
  purged console is indistinguishable from one nobody has used. A console that
  does not own the surface purges its retained state and paints nothing, so
  the purge is what the next `show` reveals, and a blanked surface stays
  blanked rather than being taken back to show what the purge left. The kernel
  drives it from `terminal_purge`.
- `Surface` / `surface()` / `hide()` / `show(pixels)` / `blank(pixels)` — the
  three dispositions of a shared scan-out. `hide` gives it up to another
  presenter; `show` takes it back, filling the surface (blanking the margins
  outside the cell grid, which a cell flush never covers) and then flushing
  every cell and the cursor, so no pixel of the previous presenter can
  survive. Each is idempotent, which is what lets the kernel panic path
  reclaim the screen without knowing who held it.
  `blank` is for a hand-over between two graphical presenters, where the seat
  has no presenter at all for a moment: it clears every pixel and leaves the
  surface cleared, so neither the outgoing frame nor a replay of the retained
  text screen appears in the gap. It quietens the console rather than
  silencing it — the retained grid is untouched, and the next write from a
  *program* (`write_output_bytes`) takes the surface back whole, so a
  hand-over whose successor never arrives still shows the reason. A **kernel
  diagnostic** (`write_bytes`) does not: on a shippable image the diagnostic
  sink renders onto this very framebuffer, so one routine record logged
  between two graphical sessions would otherwise replay the whole boot log
  into the gap. The record still reaches the retained grid, and its log; it
  simply has no claim on a screen the seat has promised to an incoming
  presenter, where a text login or a stated failure plainly does.
- `DirtyBand` / `merge_bands` — the `(start_y, end_y)` band a render touched, so
  a freestanding consumer can clean exactly those scanlines to coherency.

## Consumers

An architecture port (for example the aarch64 `video.rs` boot console) supplies
only the board-specific surface — MMIO base, geometry, and pixel format
discovered at runtime — plus the two cell grids (leaked from the kernel heap,
sized to the discovered geometry), and calls `TextConsole::write_bytes`. The
terminal emulation itself is never duplicated in a port.
