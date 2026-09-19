# tairix-svg

Shared SVG image-decoding library for the TAIRiX desktop (`lib/svg`,
`AGENTS.md` §6 / §16.4 — `PLAN.md` Stage 7).

SVG is the canonical, scalable **source** format for every WM/desktop
graphical asset — cursors, icons, notification glyphs, window-chrome
artwork (`AGENTS.md` §10). This crate is the first-party decoder for that
SVG-first pipeline. It is one of the curated §16.4 image-decoding shared
libraries and, like the rest of the desktop's parsers, it is **rolled in
house** rather than pulled from an external crate (`AGENTS.md` §2.12), so the
trusted computing base does not grow for an asset format.

## What it produces

`decode(bytes) -> Result<SvgImage, SvgError>` turns an SVG byte string into an
`SvgImage`:

- a **square design grid** (`design()`, always `DESIGN_GRID` units a side —
  every asset is fitted to it, honouring `preserveAspectRatio`, so a consumer
  never rescales between assets),
- the **artwork** drawn on it (`nodes()`, bottom first): `tairix_raster`
  `Layer`s, and a `Group` wherever a clip, a mask, or a group opacity
  composites a subtree as a unit, plus
- an optional pointer **hotspot** (`hotspot()`) for cursor assets, and the
  authored design box (`source_extent()`) for a caller that has something to
  say about the *shape* an asset was drawn in.

A layer is several contours under one fill rule rather than a single ring,
because a path with a hole and any stroke outline at all are both many rings
filled as one. That is exactly the vector form `lib/cursor`'s `VectorCursor`
and `lib/icon`'s `VectorIcon` rasterise through `lib/raster`'s single scan
converter (`Surface::draw_artwork`), so the pipeline converts an asset
**once** into this fast-draw form and never re-parses SVG on the hot
compositing path (`AGENTS.md` §10, §2.2). `tairix_cursor::decode_svg` and
`tairix_icon::decode_svg` wrap this decoder for their respective vector forms.

Group opacity, clipping, and masking are one mechanism, not three: each asks
for a subtree to be composited as a unit and then weakened by a per-pixel
factor, so a clip is a mask whose content is the clip's shapes filled opaque
white. A group is emitted only where it changes the picture, so a flat asset
decodes to a flat list.

A `<pattern>` fill is artwork of its own rather than a colour: the layer
carries the tile's nodes and the map from the drawing into tile space, and
`lib/raster` renders one period at the density the fill reads it back at. A
tile is a buffer in flight exactly as a group is, so both are charged against
one nesting bound and a cycle of patterns painting one another ends at it.
Content an author let spill past its tile (`overflow: visible`) is folded
back: a pattern is periodic, so the replicas that reach one period are finite
and drawing each of them into that period is exact rather than approximate.
The fill's opacity weakens the assembled tile, because SVG weakens the fill
operation as a whole and overlapping replicas must not each pay it.

## Untrusted input

On-disk assets under `/System/Graphics` are untrusted (`AGENTS.md` §19.5).
`decode` is **total**: it never panics for any byte string, returns a precise
`SvgError` for anything it cannot draw, and a caller fails
closed to its built-in fallback artwork rather than crashing the compositor
(`AGENTS.md` §2.9). The decoder has a `cargo xtask fuzz` harness
(`tests/fuzz_svg.rs`, §19.6).

## What it understands

The drawable part of SVG 1.1, in full:

- the document tree — `<g>`, `<defs>`, `<symbol>`, `<use>`, `<switch>`, and
  nested `<svg>` viewports;
- every basic shape — `<path>`, `<rect>` (with rounded corners), `<circle>`,
  `<ellipse>`, `<line>`, `<polyline>`, `<polygon>`;
- the whole path grammar, including cubic and quadratic curves and elliptical
  arcs, flattened to a bounded error rather than a fixed segment count;
- the whole `transform` grammar, and `viewBox` with `preserveAspectRatio`;
- strokes — width, caps, joins, miter limit, and dashes;
- the property cascade — presentation attributes, the document's own
  `<style>` sheets (type, class, id, universal and compound selectors, the
  descendant and child combinators, specificity and `!important`), the
  `style` attribute, and inheritance;
- CSS colour syntax — every hex form, `rgb()`/`rgba()`/`hsl()`/`hsla()` in
  both spellings, the named-colour table, and `currentColor`;
- linear and radial gradients, with units, spread, and `href` inheritance;
- `<pattern>` as a paint server — `patternUnits`, `patternContentUnits`,
  `patternTransform`, its own `viewBox`, `href` inheritance of both
  attributes and content, and `overflow` — whose tile is rendered at the
  resolution the drawing is being rasterised at, so a patterned fill stays as
  sharp as the rest of the artwork;
- `clip-path` and `<clipPath>` (`clip-rule`, `clipPathUnits`, nesting),
  `mask` and `<mask>` (`maskUnits`, `maskContentUnits`, `mask-type`, the mask
  region), and group opacity, each composited in isolation;
- `<marker>` and `marker-start`/`-mid`/`-end` (plus the `marker` shorthand) —
  placed at the vertices the author wrote, turned by the path's true tangent
  there rather than by a flattened chord, with `refX`/`refY`, `markerUnits`,
  `orient` (including `auto-start-reverse`), the marker's own `viewBox` and
  its `overflow` clip;
- `paint-order` as a full permutation of fill, stroke, and markers, and a
  `<switch>`'s conditional-processing attributes.

It is a renderer for artwork, not a browser. Text, embedded images, filters,
and animation are **not drawn**; an element it cannot draw is skipped rather
than refusing the document, so one unsupported decoration does not lose a
whole asset. The staged design, what is left, and the open question about
that choice are in `plans/SVG.md`.

## Layout

- `document` — `SvgImage`, the tree walk, and the top-level `decode` entry
  point (with the decode resource limits, `AGENTS.md` §2.9).
- `xml` — the element tree: nesting, entities, namespaces, character data,
  depth bounds.
- `css` — the document's own `<style>` sheets: the selector subset,
  specificity, `!important`, and the one declaration splitter the `style`
  attribute shares.
- `number` — SVG's number, length, and coordinate-list grammar.
- `geom` — `SubPath`, `StrokeStyle`, the object bounding box, and the
  marker-vertex currency: the one geometry every stage hands on.
- `pathdata` — the `d` grammar and curve/arc flattening.
- `shape` — the basic shapes.
- `marker` — `<marker>` placement: the reference point, the viewport and its
  units, `orient`, and the one matrix per instance.
- `stroke` — stroke outline: segment quads, joins, caps, dashes.
- `transform` — the `transform` grammar and viewport fitting.
- `style` — the property cascade.
- `paint` — gradients, pattern placement, and what a `url(#id)` reference
  comes to.
- `color` — CSS colour syntax → a `lib/raster` `Color`.
- `error` — the closed `SvgError` rejection set.

## Where it sits

Like `lib/geometry`, `lib/theme`, `lib/raster`, `lib/font`, `lib/cursor`, and
`lib/icon`, this crate lives in `lib/*` so the cursor and icon libraries
consume it without depending on the window manager (`AGENTS.md` §17.4). It is
`no_std`, `#![forbid(unsafe_code)]`, and owns no colour arithmetic,
rasterisation, or float maths of its own: `Color`, `Affine`, `FillRule`,
`Paint`, and the artwork tree itself come from `lib/raster`, and the bounded
`no_std` maths from
`lib/util`'s `mathf` (shared with the glyph rasteriser, so no external libm
enters the trusted computing base).

## Stability

Tier: `experimental`.
