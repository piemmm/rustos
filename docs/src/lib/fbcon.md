# `rustos-fbcon` — shared framebuffer text-console engine

`rustos-fbcon` (`lib/fbcon`) is the one architecture-neutral framebuffer text
console in the tree. Every architecture port renders its display console
through this crate, so the ANSI/VT/xterm-256color terminal emulation is defined
exactly once instead of being re-derived per target.

## What it does

It turns a byte stream into on-screen text by feeding it to the single shared
`rustos_vt::Parser` and applying each parsed `rustos_vt::Op` straight onto a
borrowed 32-bit scan-out surface (`&mut [u32]`), rendering glyphs with the
shared `rustos_font` Inconsolata EX coverage atlas: 15×28 cells with 16-level
anti-aliased coverage, the face's full Unicode repertoire with a U+FFFD
fallback for anything it does not map, and double-width glyphs occupying a
lead plus a continuation cell (the same `rustos_vt::char_width` layout the
curses window writer produces). It is a full terminal:

- SGR colour: the 16 base colours, the 256-colour palette (colour cube + grey
  ramp), and 24-bit truecolour.
- Bold (brightens the base colours) and reverse video.
- Cursor motion (absolute and relative), tab, backspace, carriage return.
- Erase-in-line and erase-in-display.
- DEC scroll regions and explicit scroll up/down.
- **Scroll-up-at-bottom**: reaching the bottom of the screen scrolls both the
  grid and the pixels up like a real terminal rather than wrapping ring-style.
- **Alternate screen** (`CSI ? 1049 h` / `l`): a full-screen program such as
  `top` or an editor switches to a cleared alternate screen on entry and, on
  exit, the primary screen it covered is restored exactly — the xterm-family
  contract every terminal honours.

## Design

- **Retained cell grid.** The engine keeps the visible screen as a grid of
  `rustos_vt::Cell` (one glyph + its rendition per position). Each write updates
  the active grid *and* paints the surface immediately, so the display stays
  live without a separate flush; the grid exists so a screen can be repainted
  from its cells — which is how leaving the alternate screen restores the
  primary one. There are two grids (primary and alternate); the primary is left
  untouched while the alternate is shown.
- **Borrowed grid storage — allocator-free.** The engine is `no_std` and never
  allocates: the two grids are passed in as `&mut [Cell]` buffers. A
  freestanding boot console with no global allocator supplies a `static`; an
  allocator-having caller leaks a heap buffer sized to the discovered geometry
  (`Geometry::cell_count`). It depends on `rustos_vt` and `rustos_font` with
  `default-features = false`.
- **Host-testable.** Every operation is pure CPU pixel arithmetic over a
  borrowed slice, so the whole engine is unit-tested on the host.
- **Fail closed.** Firmware-supplied geometry is validated at construction
  (`Geometry::for_display`); a glyph the atlas cannot draw renders `?` rather
  than being dropped.

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
- `Cell` — the character cell (`rustos_vt::Cell`), re-exported so a caller can
  size and blank the grid buffers.
- `DirtyBand` / `merge_bands` — the `(start_y, end_y)` band a render touched, so
  a freestanding consumer can clean exactly those scanlines to coherency.

## Consumers

An architecture port (for example the aarch64 `video.rs` boot console) supplies
only the board-specific surface — MMIO base, geometry, and pixel format
discovered at runtime — plus the two cell grids (leaked from the kernel heap,
sized to the discovered geometry), and calls `TextConsole::write_bytes`. The
terminal emulation itself is never duplicated in a port.
