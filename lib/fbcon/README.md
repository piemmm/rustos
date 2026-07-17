# `rustos-fbcon` — shared framebuffer text-console engine

Stability tier: **experimental**.

`rustos-fbcon` is the one architecture-neutral framebuffer text console in the
tree. It turns a byte stream into on-screen text by feeding it to the single
shared `rustos_vt::Parser`, applying each parsed `rustos_vt::Op` to a retained
cell grid, and repainting the dirtied cells once per write onto a borrowed
32-bit scan-out surface (`&mut [u32]`), rendering glyphs with
the shared `rustos_font` Inconsolata EX + M PLUS 1 Code + Noto Sans Hebrew
coverage atlas (15×28 cells, 16-level anti-aliasing, the merged family's
Unicode repertoire with a U+FFFD fallback and two-cell double-width glyphs).
It is a full
ANSI/VT/xterm-256color terminal: SGR colour (16 / 256 / truecolour), bold,
reverse video, cursor motion, erase, DEC scroll regions, and — reaching the
bottom of the screen — scrolling up like a real terminal rather than wrapping
ring-style.

Every architecture port (`kernel/arch/<target>`) drives its display console
through this crate, so the terminal emulation lives in exactly one place; a
port supplies only the board-specific surface (MMIO base, geometry, pixel
format) discovered at runtime. Verbatim streams use `TextConsole::write_bytes`;
program output uses `TextConsole::write_output_bytes`, which applies terminal
`LF` → `CR LF` processing while retaining the same one-repaint batch.

## Design

- **Retained cell grid, one repaint per write.** The engine keeps the visible
  screen as two borrowed `rustos_vt::Cell` grids (primary + alternate). Every
  operation mutates only the active grid, and the cell rect a write dirtied is
  repainted from the grid **once** at the end of the write. Deferring the
  pixels to that single repaint bounds a write's render cost: a burst that
  scrolls the screen many times moves only the small, cache-resident grid per
  line and touches the framebuffer once — never one whole-framebuffer copy per
  scrolled line. Program-output newline processing feeds the same retained-grid
  batch without splitting at each line feed. Runs of blank cells are
  span-filled rather than glyph-blitted, and the framebuffer is never read,
  only written.
- **Allocator-free.** The engine is `no_std` and never allocates (the grids
  are borrowed `&mut [Cell]`), so a freestanding boot console with no global
  allocator links it directly. It
  depends on `rustos_vt` and `rustos_font` with `default-features = false`.
- **Host-testable.** Every operation is pure CPU pixel arithmetic over a
  borrowed slice, so the whole engine is unit-tested on the host.
- **Fail closed.** Firmware-supplied geometry is validated at construction
  (`Geometry::for_display`), and a glyph the atlas cannot draw renders `?`
  rather than being dropped.

## Surface format

The engine assumes a 32-bit little-endian surface whose bytes are `B, G, R,
X/A` per pixel (the memory order shared by `Bgra8888` and `XRGB8888`). A colour
is packed opaque as `0xFF00_0000 | (r << 16) | (g << 8) | b`.

The dirty-band return value (`(start_y, end_y)`, exclusive end) lets a
freestanding consumer clean exactly the touched scanlines to the point of
coherency after a write.
