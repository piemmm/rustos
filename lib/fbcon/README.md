# `rustos-fbcon` — shared framebuffer text-console engine

Stability tier: **experimental**.

`rustos-fbcon` is the one architecture-neutral framebuffer text console in the
tree. It turns a byte stream into on-screen text by feeding it to the single
shared `rustos_vt::Parser` and applying each parsed `rustos_vt::Op` straight
onto a borrowed 32-bit scan-out surface (`&mut [u32]`), rendering glyphs with
the shared `rustos_font` 5×7 atlas. It is a full ANSI/VT/xterm-256color
terminal: SGR colour (16 / 256 / truecolour), bold, reverse video, cursor
motion, erase, DEC scroll regions, and — reaching the bottom of the screen —
scrolling the pixels up like a real terminal rather than wrapping ring-style.

Every architecture port (`kernel/arch/<target>`) drives its display console
through this crate, so the terminal emulation lives in exactly one place; a
port supplies only the board-specific surface (MMIO base, geometry, pixel
format) discovered at runtime and calls `TextConsole::write_bytes`.

## Design

- **No retained cell grid.** The pixels *are* the state, so a write paints (or
  scrolls) the surface immediately and the engine holds no per-cell buffer.
- **Allocator-free.** The engine is `no_std` and never allocates, so a
  freestanding boot console with no global allocator links it directly. It
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
