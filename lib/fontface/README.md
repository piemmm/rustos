# tairix-fontface

Shared glyph-coverage engine: the one parser + anti-aliased non-zero-winding
rasteriser that turns a committed TrueType face into 4-bit coverage bitmaps
**at any requested pixel size**, plus the earliest-wins merged-family
codepoint resolution (`FontFamily`) that both the atlas generator and the
runtime font build on, and the pixel-exact `lineart` geometry for the
characters that exist to tile rather than to be read.

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

## Grid fitting

A face puts its stems and its x-height where the design wants them, which at a
text-console cell size is almost never a whole pixel: 1.2 pixels of stem
straddling two columns fills both at about 60%, and a screen of that is grey
haze rather than type. A face carrying TrueType hinting bytecode grid-fits
itself; the committed faces do not (`Inconsolata-EX` ships a stub `prep` and no
`fpgm`/`cvt`), so `gridfit` synthesises the fitting from the outline, the way
FreeType's autofitter does for the same reason.

The fit is a monotone piecewise-linear warp of one coordinate. Near-axis
segments cluster into edges (a stroke's two sides run opposite ways, so they
never merge into one), edges with ink between them pair into strokes, each
stroke snaps to a whole number of pixels — never fewer than one, so a hairline
darkens rather than disappears — and the rest of the outline interpolates. An
edge keeps the runs it covers rather than their span, because a side is often
several: an `m`'s top is the left stem's flat and two arch crowns, a `g`'s is
its bowl's crown and its ear, so the ink test has to be asked where both sides
really have material and not in the valley between them. Diagonals never
register as an edge, so they stay straight rather than becoming staircases,
and monotonicity means a fitted contour cannot fold over itself.

Rows also snap to the face's own alignment zones — baseline, x-height, cap
height, ascender, descender, read once at parse from the face's `x`, `o`, `H`,
`O`, `b`, `p`, `g` — so a line of text agrees on those rows instead of each
letter rounding alone. A round letter's overshoot is flattened onto its zone
only while it is worth less than a pixel, which is exactly when drawing it
would cost a whole row.

Columns are snapped only for a fixed cell, where the cell owns the advance and
moving a stem costs no spacing; the cell path also scales columns so the
face's uniform advance lands on a whole number of cells rather than rounding
to them — one for the face the grid was derived from, two for a full-width
fallback face lending glyphs to a half-width grid. Proportional text is fitted
along rows alone, because ink snapped sideways would drift out from under the
unfitted advance that laid the run out.

Measured on the console face at its 8×16 cell, the share of ink at full
coverage rises from 9% unfitted to 34%, against about 27% for FreeType's
autofitter on the same face and size.

## Line art

Box Drawing (U+2500–U+257F) and Block Elements (U+2580–U+259F) do not come
from an outline at all. They exist to tile — a border has to join its
neighbours into one unbroken rule and a filled block has to abut the next with
no seam — which a rasterised hairline manages only where it happens to land on
pixel boundaries. `lineart::coverage` draws them as whole pixels computed from
the cell instead, so they stay crisp at any cell size. A double rule is derived
as the outline of the region its arms sweep, so all twenty-nine junctions agree
without a per-glyph table.

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

A monospace family takes the cell path either way — the atlas by
construction, `fontd` because the family declares itself fixed-pitch — and
both substitute `lineart` for the two tiling ranges, so a border on the
framebuffer console and one in a terminal window are the same picture. Only a
proportional family is served tight to its ink.

`no_std` + `alloc`, no `unsafe`. Fails closed: any malformed or unsupported
table — including a hostile variation store — yields a `FontError` rather than
a wrong glyph, an out-of-bounds read, or a panic, and every count taken from
the file is bounded by the glyph's own point count. Float rounding uses the
crate's own bounded helpers, so it needs no `std` libm.

## Stability

`experimental` — the API is settled around the in-tree consumers (the atlas
generator and `fontd`) but may change as installed-font support grows.
