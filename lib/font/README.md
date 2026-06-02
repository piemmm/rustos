# rustos-font

The single shared **text rasterisation** primitives for the RustOS desktop
(`AGENTS.md` §6, §16.4, §17.4 — `PLAN.md` Stage 7).

Font rendering is one of the curated OS shared-library classes (`AGENTS.md`
§16.4). Like `lib/geometry` (coordinate types), `lib/theme` (design tokens),
and `lib/raster` (the pixel surface), text rendering lives in `lib/*` so the
taskbar (`userland/gui/taskbar`) and the default apps can draw text without
depending on the window manager and without duplicating a blitter
(`AGENTS.md` §17.4, §2.2).

This crate owns:

- `glyphs` — a built-in **5×7 monospace bitmap atlas** covering printable
  ASCII (space through `~`). Each glyph is written as binary row literals so
  the data is self-documenting: the `1` bits trace the letter on the page
  (`AGENTS.md` §2.11).
- `font::BitmapFont` — a face (atlas + metrics: cell size, pen advance, line
  height) plus the glyph blitter. `draw_text` composites each lit glyph pixel
  onto a `lib/raster` `Surface` through that crate's single premultiplied-alpha
  `Pixel::over` path, so text blends correctly over what is already painted —
  no colour arithmetic is duplicated here (`AGENTS.md` §2.2). A character
  outside the atlas renders a visible fallback box rather than being silently
  dropped, and off-screen pixels clip rather than panic (`AGENTS.md` §2.9).
  `text_width` gives the tight one-line bounding width for layout.

There is no installed-font machinery yet: a `rustos-theme` font role selects a
font by family name under `/System/Fonts`, but no faces are installed, so the
desktop draws with the built-in `BitmapFont::mono5x7` face. When scalable faces
arrive they extend this crate; consumers keep calling `draw_text`.

## Why it lives in `lib/`

Sibling userland GUI crates may not depend on one another (`AGENTS.md`
§17.4), so the text rasteriser they share belongs in `lib/*`. It depends only
on `lib/raster` (the pixel surface and colour algebra it composites through)
and is depended on by the GUI crates, never the reverse — `Layer::Lib` in the
§17.4 layering.

## Stability tier

`experimental` — the Stage 7 desktop text-rendering seam, consumed by
`userland/gui/taskbar` (and the default apps next). It is `no_std`, contains
no `unsafe` (`#![forbid(unsafe_code)]`), and no `unwrap`/`expect`/`panic!` in
production paths (`AGENTS.md` §2.9).
