# rustos-font

The single shared **text rasterisation** primitives for RustOS (`AGENTS.md`
§6, §16.4, §17.4 — `PLAN.md` Stage 7).

Font rendering is one of the curated OS shared-library classes (`AGENTS.md`
§16.4). Like `lib/geometry` (coordinate types), `lib/theme` (design tokens),
and `lib/raster` (the pixel surface), text rendering lives in `lib/*` so the
framebuffer console (`lib/fbcon`), the taskbar (`userland/gui/taskbar`), and
the default apps can draw text without depending on the window manager and
without duplicating a blitter (`AGENTS.md` §17.4, §2.2).

## The face

The system face is **Inconsolata** (SIL Open Font License 1.1). The TrueType
source and its licence are committed under `assets/`
(`Inconsolata-Regular.ttf`, `OFL.txt`); nothing parses TrueType at runtime.
`cargo xtask font-atlas --write` rasterises every codepoint the face maps into
the generated atlas (`src/atlas.rs` + `src/atlas_coverage.bin`), and
`cargo xtask font-atlas` (run by `ci`) fails closed if the committed atlas
drifts from a fresh generation — the same generated-view discipline as the C
ABI headers. The generator is first-party and deterministic: a minimal
TrueType reader and an anti-aliasing scanline rasteriser in
`tools/xtask/src/commands/font_atlas.rs`.

## What this crate owns

- `atlas` — the generated data: 12×26-pixel glyph cells (a 24 px em; the
  face is strictly monospace at half an em), 4-bit coverage packed two pixels
  per byte, a sorted codepoint→cell range table, and the U+FFFD fallback
  index. Pure `const`/`static` data with no dependencies.
- `glyph` — Unicode lookup over the atlas: binary search of the range table,
  the packed-nibble accessor, and the U+FFFD fallback for any scalar the face
  does not map (visibly wrong rather than silently dropped, `AGENTS.md` §2.9).
  Coverage spans the face's repertoire: Latin and its extensions, box drawing
  and block elements, arrows, punctuation, currency — 882 codepoints.
- `font::BitmapFont` — the face's metrics (cell size, pen advance, line
  height) plus the glyph blitter. `draw_text` composites each covered pixel
  onto a `lib/raster` `Surface` through that crate's single
  premultiplied-alpha `Pixel::over` path, scaling the text colour once into a
  16-entry coverage table — anti-aliased edges and translucent text both
  blend correctly with no colour arithmetic duplicated here (`AGENTS.md`
  §2.2). `text_width` and `truncate_to_width` give the shared layout
  arithmetic.

The cell model is **one scalar per cell** — the deliberate simplification
`lib/vt` and `lib/curses` document. A zero-advance combining mark renders in
its own cell, and the double-width (`rustos_vt::char_width`) cell layout is
the consumers' concern: `lib/fbcon`, the terminal emulator, and `lib/curses`
all write a wide glyph as a lead cell plus a continuation cell.

The `atlas` and `glyph` modules are allocator-free, so a consumer that brings
its own blitter (`lib/fbcon`, which blends coverage into device-coherent
memory itself) depends with `default-features = false`; the
`lib/raster`-backed blitter rides the default-on `render` cargo feature — one
font definition either way (§2.2).

There is no installed-font machinery yet: a `rustos-theme` font role selects a
font by family name under `/System/Fonts`, but no faces are installed, so
everything draws with the built-in `BitmapFont::inconsolata` face. When
installed faces arrive they extend this crate; consumers keep calling
`draw_text`.

## Why it lives in `lib/`

Sibling userland GUI crates may not depend on one another (`AGENTS.md`
§17.4), and the kernel's boot console must not depend on userland, so the
text rasteriser they share belongs in `lib/*`. It depends only on
`lib/raster` (behind `render`) and is depended on by `lib/fbcon` and the GUI
crates, never the reverse — `Layer::Lib` in the §17.4 layering.

## Stability tier

`experimental` — consumed by `lib/fbcon` (every arch port's display console),
`userland/gui/taskbar`, and the default apps. It is `no_std`, contains no
`unsafe`, and follows the shared workspace lints.
