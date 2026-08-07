# `tairix-fbcon` — shared framebuffer text-console engine

Stability tier: **experimental**.

`tairix-fbcon` is the one architecture-neutral framebuffer text console in the
tree. It turns a byte stream into on-screen text by feeding it to the single
shared `tairix_vt::Parser`, applying each parsed `tairix_vt::Op` to a retained
cell grid, and repainting the dirtied cells once per write onto a borrowed
32-bit scan-out surface (`&mut [u32]`), rendering glyphs with the shared
`tairix_font` Inconsolata EX + M PLUS 1 Code + D2Coding + Noto Sans Hebrew
coverage atlas (15×28 cells, 16-level anti-aliasing, all precomposed Hangul
syllables, the merged family's wider Unicode repertoire with a U+FFFD fallback,
and two-cell double-width glyphs).
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

One surface has one presenter, so a console sharing its scan-out with a
graphical session is `hide()`den while that session holds the seat and `show()`n
when it ends — see "Sharing the surface" below.

## Design

- **Retained cell grid, one repaint per write.** The engine keeps the visible
  screen as two borrowed `tairix_vt::Cell` grids (primary + alternate). Every
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
  depends on `tairix_vt` and `tairix_font` with `default-features = false`.
- **Host-testable.** Every operation is pure CPU pixel arithmetic over a
  borrowed slice, so the whole engine is unit-tested on the host.
- **Fail closed.** Firmware-supplied geometry is validated at construction
  (`Geometry::for_display`), and a glyph the atlas cannot draw renders `?`
  rather than being dropped.

## Sharing the surface

A framebuffer text console and a compositing display client want the same
scan-out memory, and a surface can only have one presenter. `TextConsole`
therefore carries a visibility state that the kernel's seat lease drives
(`kernel/core/src/seat.rs`): the console owns the surface while the seat is
unowned, and gives it up while a session holds it.

- `hide()` — give the surface up. Writes keep being parsed into the retained
  grid and touch **no** pixel, so output produced under a desktop is neither
  drawn over the composited frame nor lost.
- `show(pixels)` — take the surface back and repaint the whole screen from the
  retained grid: the surface fill blanks the margins outside the cell grid, the
  full-grid flush paints every cell, so no pixel of the previous presenter can
  survive. What arrived while hidden is on screen too, which is why a user who
  leaves a graphical session returns to their shell exactly as they left it
  with the session's diagnostics printed beneath.

Both directions are idempotent, so the kernel panic path can reclaim the
surface without knowing who held it.

## Surface format

The engine assumes a 32-bit little-endian surface whose bytes are `B, G, R,
X/A` per pixel (the memory order shared by `Bgra8888` and `XRGB8888`). A colour
is packed opaque as `0xFF00_0000 | (r << 16) | (g << 8) | b`.

The dirty-band return value (`(start_y, end_y)`, exclusive end) lets a
freestanding consumer clean exactly the touched scanlines to the point of
coherency after a write.
