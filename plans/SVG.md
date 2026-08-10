# SVG — the first-party vector-asset decoder (`lib/svg`)

Status: **done** for the drawable core; the deliberate non-goals and the one
open question are recorded at the end.

Binding under `AGENTS.md`. SVG is the canonical source format for the
desktop's chrome and for icon artwork that is authored as vectors, so this
crate is on the path of every cursor, status glyph, window decoration, and
icon the compositor draws. It is one of the curated image-decoding libraries
and is rolled in house: an asset format must not widen the trusted computing
base with an external parser.

Read first: `plans/ICONS.md` (the asset tiers and the decode cache every
surface draws through), `docs/src/desktop/svg-assets.md`, `plans/DISPLAY.md` for where
the rasterised result goes.

---

## What it produces

`decode(bytes) -> Result<SvgImage, SvgError>`, converting an asset **once**
into the fast-draw form the compositor blits — never re-parsing SVG on the hot
path.

An `SvgImage` is a square design grid (`DESIGN_GRID`, 2048 units a side) plus
an ordered stack of `SvgLayer`s, bottom first. A layer is a `Paint`, a
`FillRule`, and a list of **contours** in design-grid coordinates.

Three decisions shape everything else:

- **One geometry currency.** Curves, arcs, basic shapes, and stroke outlines
  all become flattened `SubPath`s in user space as early as possible
  (`lib/svg/src/geom.rs`), so there is exactly one place a curve stops being a
  curve and every later stage sees one kind of geometry.
- **A layer is multi-contour.** A path with a hole, a multi-sub-path shape,
  and *any* stroke (which is the union of one piece per segment, cap, and
  join) cannot be one ring. Contours are filled together under one rule, so
  the pieces merge or cancel as the rule says instead of being composited over
  each other — which would double-blend a translucent stroke.
- **One design grid for every asset.** Whatever a document's own `viewBox`
  says, it is fitted to the square grid with `preserveAspectRatio` honoured,
  so non-square artwork is letter-boxed into the square slot rather than
  stretched or refused, and curve flattening has a single known accuracy
  target (0.4 design units).

## Module map

| Module | What it owns |
|---|---|
| `xml` | The element tree: nesting, self-closing tags, CDATA/PI/doctype, entity decoding, namespace-prefix resolution, depth and element bounds |
| `number` | SVG's number grammar: separator-free runs, arc flags, CSS absolute units, percentages, opacity |
| `color` | CSS colour syntax: hex (3/4/6/8), `rgb()`/`rgba()`/`hsl()`/`hsla()` in both spellings, the named-colour table, `currentColor`, `none` |
| `geom` | `SubPath`, `StrokeStyle`, caps/joins, the object bounding box |
| `pathdata` | The whole `d` grammar and curve/arc flattening to a tolerance |
| `shape` | The basic shapes, including `rect`'s rounded-corner rules |
| `stroke` | Stroke outline: segment quads, joins, caps, dashes |
| `transform` | The `transform` grammar, `viewBox`, `preserveAspectRatio`, viewport fitting |
| `style` | The presentation-property cascade: attribute, `style` declaration, inheritance |
| `paint` | Gradients: definitions, `href` inheritance, units, spread, per-use resolution |
| `document` | The tree walk that turns all of the above into layers |

`Affine`, `FillRule`, and `Paint` live in `lib/raster`, not here: the
rasteriser and the decoder both need them, so they have one definition. The
`no_std` float maths (`sqrt`, `sin`, `atan2`, rounding) lives in
`lib/util::mathf`, shared with the glyph rasteriser in `lib/fontface`.

## Untrusted input

Every asset is hostile until proven otherwise. `decode` is total for any byte
string: no panic, no unbounded loop, no unbounded allocation, and no NaN or
infinity reaching the geometry. The fixed bounds — element count, nesting
depth, layer count, total vertices, segments per curve, dash-pattern length,
gradient stops, `use` and `href` chain depth — are **security bounds, not
capacities**: they do not scale with the machine and must not be raised to
make an asset fit.

A document that is malformed, or whose numbers, colours, or transforms are
outside the grammar, is refused **whole** with a precise `SvgError`; the
caller falls back to the tier below (`plans/ICONS.md`). Nothing is
half-applied.

## Deliberate non-goals

Not deferred work — these are outside what an artwork decoder is for, and
adding one would be a new plan of its own:

- Text (`<text>`, `<tspan>`, fonts, text layout). Glyph rendering is
  `lib/fontface`'s job, and artwork ships its lettering as outlines.
- Embedded raster images (`<image>`), which would nest one decoder in
  another.
- Filters, masks, clipping paths, and patterns.
- Animation (SMIL), scripting, external references of any kind, and CSS
  stylesheets (`<style>` blocks and selectors); the `style` *attribute* is
  supported.

## Open question

`AGENTS.md` fails closed by default, but an element this decoder cannot draw
is currently **skipped** rather than refusing the document — so an asset
carrying, say, a clipping path renders unclipped instead of falling back to
the tier below. Skipping is what lets one unsupported decoration not lose a
whole asset, and it is the behaviour the desktop has today. Whether the
drawable-element case should instead fail the document closed is recorded as
an open item in `plans/ICONS.md`; it is a deliberate decision to make, not an
oversight.
