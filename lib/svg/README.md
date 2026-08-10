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
- an ordered stack of filled **layers** (`layers()`, bottom layer first), each
  an `SvgLayer { paint, rule, contours }`, plus
- an optional pointer **hotspot** (`hotspot()`) for cursor assets, and the
  authored design box (`source_extent()`) for a caller that has something to
  say about the *shape* an asset was drawn in.

A layer is several contours under one fill rule rather than a single ring,
because a path with a hole and any stroke outline at all are both many rings
filled as one. That is exactly the vector form `lib/cursor`'s `VectorCursor`
and `lib/icon`'s `VectorIcon` rasterise through `lib/raster`'s single scan
converter, so the pipeline converts an asset **once** into this fast-draw form
and never re-parses SVG on the hot compositing path (`AGENTS.md` §10, §2.2).
`tairix_cursor::decode_svg` and `tairix_icon::decode_svg` wrap this decoder
for their respective vector forms.

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
- the presentation-property cascade, including the `style` attribute and
  inheritance;
- CSS colour syntax — every hex form, `rgb()`/`rgba()`/`hsl()`/`hsla()` in
  both spellings, the named-colour table, and `currentColor`;
- linear and radial gradients, with units, spread, and `href` inheritance.

It is a renderer for artwork, not a browser. Text, embedded images, filters,
masks, clipping paths, patterns, animation, and CSS stylesheets are **not
drawn**; an element it cannot draw is skipped rather than refusing the
document, so one unsupported decoration does not lose a whole asset. The
staged design and the open question about that choice are in `plans/SVG.md`.

## Layout

- `document` — `SvgImage`, `SvgLayer`, the tree walk, and the top-level
  `decode` entry point (with the decode resource limits, `AGENTS.md` §2.9).
- `xml` — the element tree: nesting, entities, namespaces, depth bounds.
- `number` — SVG's number, length, and coordinate-list grammar.
- `geom` — `SubPath`, `StrokeStyle`, and the object bounding box: the one
  geometry every stage hands on.
- `pathdata` — the `d` grammar and curve/arc flattening.
- `shape` — the basic shapes.
- `stroke` — stroke outline: segment quads, joins, caps, dashes.
- `transform` — the `transform` grammar and viewport fitting.
- `style` — the presentation-property cascade.
- `paint` — gradients and paint-server resolution.
- `color` — CSS colour syntax → a `lib/raster` `Color`.
- `error` — the closed `SvgError` rejection set.

## Where it sits

Like `lib/geometry`, `lib/theme`, `lib/raster`, `lib/font`, `lib/cursor`, and
`lib/icon`, this crate lives in `lib/*` so the cursor and icon libraries
consume it without depending on the window manager (`AGENTS.md` §17.4). It is
`no_std`, `#![forbid(unsafe_code)]`, and owns no colour arithmetic,
rasterisation, or float maths of its own: `Color`, `Affine`, `FillRule`, and
`Paint` come from `lib/raster`, and the bounded `no_std` maths from
`lib/util`'s `mathf` (shared with the glyph rasteriser, so no external libm
enters the trusted computing base).

## Stability

Tier: `experimental`.
