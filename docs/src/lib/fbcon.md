# `rustos-fbcon` — shared framebuffer text-console engine

`rustos-fbcon` (`lib/fbcon`) is the one architecture-neutral framebuffer text
console in the tree. Every architecture port renders its display console
through this crate, so the ANSI/VT/xterm-256color terminal emulation is defined
exactly once instead of being re-derived per target.

## What it does

It turns a byte stream into on-screen text by feeding it to the single shared
`rustos_vt::Parser` and applying each parsed `rustos_vt::Op` straight onto a
borrowed 32-bit scan-out surface (`&mut [u32]`), rendering glyphs with the
shared `rustos_font` 5×7 atlas. It is a full terminal:

- SGR colour: the 16 base colours, the 256-colour palette (colour cube + grey
  ramp), and 24-bit truecolour.
- Bold (brightens the base colours) and reverse video.
- Cursor motion (absolute and relative), tab, backspace, carriage return.
- Erase-in-line and erase-in-display.
- DEC scroll regions and explicit scroll up/down.
- **Scroll-up-at-bottom**: reaching the bottom of the screen scrolls the pixels
  up like a real terminal rather than wrapping ring-style.

## Design

- **No retained cell grid.** The pixels *are* the state, so a write paints (or
  scrolls) the surface immediately; the engine holds no per-cell buffer and a
  boot console keeps no scrollback.
- **Allocator-free.** The engine is `no_std` and never allocates, so a
  freestanding boot console with no global allocator links it directly. It
  depends on `rustos_vt` and `rustos_font` with `default-features = false`.
- **Host-testable.** Every operation is pure CPU pixel arithmetic over a
  borrowed slice, so the whole engine is unit-tested on the host.
- **Fail closed.** Firmware-supplied geometry is validated at construction
  (`Geometry::for_display`); a glyph the atlas cannot draw renders `?` rather
  than being dropped.

## API

- `Geometry` — validated scan-out extents plus the glyph scale the policy
  chose; `for_display(width, height, pitch_bytes)` fails closed on an unusable
  surface.
- `TextConsole` — owns the parser and the screen; `write_bytes(pixels, bytes)`
  interprets an ANSI/VT stream and returns the touched pixel-row band; `clear`
  paints the background and homes the cursor.
- `DirtyBand` / `merge_bands` — the `(start_y, end_y)` band a render touched, so
  a freestanding consumer can clean exactly those scanlines to coherency.

## Consumers

An architecture port (for example the aarch64 `video.rs` boot console) supplies
only the board-specific surface — MMIO base, geometry, and pixel format
discovered at runtime — and calls `TextConsole::write_bytes`. The terminal
emulation itself is never duplicated in a port.
