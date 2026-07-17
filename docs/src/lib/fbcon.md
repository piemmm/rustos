# `tairix-fbcon` — shared framebuffer text-console engine

`tairix-fbcon` (`lib/fbcon`) is the one architecture-neutral framebuffer text
console in the tree. Every architecture port renders its display console
through this crate, so the ANSI/VT/xterm-256color terminal emulation is defined
exactly once instead of being re-derived per target.

## What it does

It turns a byte stream into on-screen text by feeding it to the single shared
`tairix_vt::Parser`, applying each parsed `tairix_vt::Op` to a retained cell
grid, and repainting the dirtied cells once per write onto a borrowed 32-bit
scan-out surface (`&mut [u32]`), rendering glyphs with the
shared `tairix_font` Inconsolata EX + M PLUS 1 Code + D2Coding + Noto Sans
Hebrew coverage atlas: 15×28 cells with 16-level anti-aliased coverage,
Japanese hiragana/katakana/kanji, all precomposed Hangul syllables, and
Hebrew/Yiddish alongside the primary face's Latin/Greek/Cyrillic repertoire,
with a U+FFFD fallback for anything the merged family does not map. Double-width
glyphs occupy a lead plus a continuation cell (the same
`tairix_vt::char_width` layout the curses window writer produces), and their
bitmap paints across both cells as one repaint unit. It is a full terminal:

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

## Design

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
- `DirtyBand` / `merge_bands` — the `(start_y, end_y)` band a render touched, so
  a freestanding consumer can clean exactly those scanlines to coherency.

## Consumers

An architecture port (for example the aarch64 `video.rs` boot console) supplies
only the board-specific surface — MMIO base, geometry, and pixel format
discovered at runtime — plus the two cell grids (leaked from the kernel heap,
sized to the discovered geometry), and calls `TextConsole::write_bytes`. The
terminal emulation itself is never duplicated in a port.
