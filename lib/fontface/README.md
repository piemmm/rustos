# tairix-fontface

Shared TrueType glyph-outline engine: the one parser + anti-aliased
non-zero-winding rasteriser that turns a committed TrueType face into 4-bit
coverage bitmaps **at any requested pixel size**, plus the earliest-wins
merged-family codepoint resolution (`FontFamily`) that both the atlas
generator and the runtime font build on.

## Variable fonts

The engine instances OpenType **variable** fonts. `Face::parse_instance`
resolves a set of `AxisSetting`s (a chosen weight, width, or optical size)
into a point in the face's design space, and every glyph is instanced against
it:

- `fvar` — the declared variation axes, exposed via `Face::axes()`;
  `Face::is_variable()` reports whether a face varies.
- `avar` — version-1 segment maps applied to the normalised axis coordinate.
- `gvar` — the full tuple variation store for `glyf` outlines: shared and
  embedded peak tuples, intermediate regions, shared and private point
  numbers, packed x/y deltas, and **IUP** (Interpolation of Untouched Points)
  for the points a tuple does not touch. Composite-glyph component offsets and
  the four phantom points participate.
- `HVAR` — advance-width variation through an `ItemVariationStore` and a
  `DeltaSetIndexMap` (both formats, and the implicit no-map case). When a
  variable face carries no `HVAR`, the advance delta is derived from the
  glyph's varied phantom points instead.

Instancing at a face's defaults, and any static (non-variable) face, applies no
variation and rasterises byte-identically to an unvaried face — the generated
console atlas depends on that.

## Monospace and proportional

- `Face::rasterise_glyph` fills a fixed `CellGeometry` cell for a monospace
  character grid.
- `Face::rasterise_proportional` returns a `GlyphRaster` tight to the glyph's
  own ink in x, with the integer left-bearing column, for laying proportional
  text out by per-glyph `Face::advance` (and `Face::line_gap`).

Two consumers share this engine, so the rasteriser is written once
(`AGENTS.md` §2.2):

- `cargo xtask font-atlas` rasterises every mapped scalar once, at the native
  `ATLAS_EM_PX`, to emit the generated `lib/font` console atlas.
- the font service (`fontd`) rasterises a glyph on demand at the desktop's
  requested cell height and weight, so UI text is drawn from the outlines at
  its true size — crisp whether tiny or very large — rather than resampled
  from a fixed bitmap.

`no_std` + `alloc`, no `unsafe`. Fails closed: any malformed or unsupported
table — including a hostile variation store — yields a `FontError` rather than
a wrong glyph, an out-of-bounds read, or a panic, and every count taken from
the file is bounded by the glyph's own point count. Float rounding uses the
crate's own bounded helpers, so it needs no `std` libm.

## Stability

`experimental` — the API is settled around the in-tree consumers (the atlas
generator and `fontd`) but may change as installed-font support grows.
