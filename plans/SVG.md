# SVG — the first-party vector-asset decoder (`lib/svg`)

Binding under `AGENTS.md`. SVG is the canonical source format for the
desktop's chrome and for icon artwork that is authored as vectors, so this
crate is on the path of every cursor, status glyph, window decoration, and
icon the compositor draws. It is one of the curated image-decoding libraries
and is rolled in house: an asset format must not widen the trusted computing
base with an external parser.

Read first: `plans/ICONS.md` (the asset tiers and the decode cache every
surface draws through), `docs/src/desktop/svg-assets.md`, `plans/DISPLAY.md`
for where the rasterised result goes, `plans/VIEW.md` for the viewer that
opens a drawing as a document.

## Ledger

| Item | What it is | State |
|---|---|---|
| S1 | XML scanner: nesting, self-closing tags, CDATA/PI/doctype, entities, namespace prefixes, depth and element bounds | done |
| S2 | Number, length, percentage, opacity and arc-flag grammar | done |
| S3 | Colour grammar: hex 3/4/6/8, `rgb()`/`rgba()`/`hsl()`/`hsla()`, named colours, `currentColor` | done |
| S4 | The `d` grammar, curve and elliptical-arc flattening to a tolerance | done |
| S5 | Basic shapes, including `rect`'s rounded-corner rules | done |
| S6 | Stroke outline: segment quads, joins, caps, dashes | done |
| S7 | The `transform` grammar, `viewBox`, `preserveAspectRatio`, viewport fitting | done |
| S8 | Presentation-property cascade: attribute, `style` declaration, inheritance | done |
| S9 | Gradients: definitions, `href` inheritance, units, spread, per-use resolution | done |
| S10 | The tree walk: `<g>`, `<defs>`, `<use>`, `<switch>`, nested `<svg>` | done |
| S11 | Shared artwork tree (`lib/raster::artwork`) and its one renderer | done |
| S12 | Group opacity, composited in isolation | done |
| S13 | `clip-path` / `<clipPath>`: `clip-rule`, `clipPathUnits`, nesting | done |
| S14 | `mask` / `<mask>`: `maskUnits`, `maskContentUnits`, `mask-type`, the mask region | done |
| S15 | CSS stylesheets: `<style>`, the selector subset, specificity, `!important` | done |
| S16 | `<symbol>` / `<use>` viewport, and `overflow` clipping of a nested viewport | done |
| S17 | `<switch>` conditional processing: `systemLanguage`, empty conditions, drawable children only | done |
| S18 | `paint-order` | done |
| S19 | `<pattern>` as a paint server, including the `overflow: visible` fold | done |
| S20 | `<marker>`: `marker-start` / `-mid` / `-end`, and the element-visit bound their instancing needs | done |
| S21 | `vector-effect="non-scaling-stroke"`: the outline built in the host space, and what stands in for that space here | done |

Every item is `done` and none is half-built: the decoder draws the whole of
the subset this plan set out to draw. What it does not draw is the
[deliberate non-goals](#deliberate-non-goals), and nothing else.

---

## What it produces

`decode(bytes, viewport) -> Result<SvgImage, SvgError>`, converting an asset
**once** into the fast-draw form the compositor blits — never re-parsing SVG
on the hot path.

An `SvgImage` is a design grid (`DESIGN_GRID`, 2048 units a side) plus an
ordered artwork tree, bottom first: `tairix_raster::artwork::Node`s, each
either a `Layer` (a `Paint`, a `FillRule`, and a list of **contours** in
design-grid coordinates) or a `Group` (a subtree composited as a unit through
an opacity and an optional mask). A `Paint` is a colour, a gradient, or a
`Pattern` — a tile of artwork and the map into tile space.

Four decisions shape everything else:

- **One geometry currency.** Curves, arcs, basic shapes, and stroke outlines
  all become flattened `SubPath`s in user space as early as possible
  (`lib/svg/src/geom.rs`), so there is exactly one place a curve stops being a
  curve and every later stage sees one kind of geometry.
- **A layer is multi-contour.** A path with a hole, a multi-sub-path shape,
  and *any* stroke (which is the union of one piece per segment, cap, and
  join) cannot be one ring. Contours are filled together under one rule, so
  the pieces merge or cancel as the rule says instead of being composited over
  each other — which would double-blend a translucent stroke.
- **The artwork form lives in `lib/raster`, and so does its renderer.** The
  decoder builds it and the cursor, icon, and viewer paths draw it; a second
  definition or a second walk of it in each consumer is what the charter
  forbids. `Affine`, `FillRule`, and `Paint` were already there for the same
  reason.
- **One design grid for every asset, and a `Viewport` that chooses its
  shape.** Whatever a document's own `viewBox` says, it is fitted to the same
  grid, so a consumer never rescales between assets and curve flattening has a
  single known accuracy target (0.4 design units, resolved against the
  placement each shape is actually drawn under rather than the document's own
  root map — a subtree inside `scale(10)` reaches the grid ten times larger,
  and the root's tolerance would facet it by ten times the error). `Viewport::Square` fits it
  under `preserveAspectRatio`, so non-square artwork is letter-boxed into the
  square slot rather than stretched or refused — the desktop's asset form.
  `Viewport::Natural` normalises the drawing across both axes for a consumer
  that will rasterise into the drawing's own shape, which is the viewer
  showing a picture (`plans/VIEW.md`): `Surface::draw_artwork` stretches the
  grid across the surface it is given, so normalising here and un-normalising
  there is one uniform scale and the scan converter needs no non-square grid.
  `source_extent()` carries the authored proportions the consumer sizes that
  surface from.

  A viewport chooses a shape and nothing else: a document is well formed or it
  is not, whoever asks. The one consequence it does carry is that filling both
  axes flattens curves to the larger scale's tolerance, so a drawing close to
  the total-vertex bound can pass it under `Natural`. That bound is a
  containment bound and is not relaxed to suit a shape.

## Module map

| Module | What it owns |
|---|---|
| `xml` | The element tree: nesting, self-closing tags, CDATA/PI/doctype, entity decoding, namespace-prefix resolution, character data, depth and element bounds |
| `number` | SVG's number grammar: separator-free runs, arc flags, CSS absolute units, percentages, opacity |
| `color` | CSS colour syntax: hex (3/4/6/8), `rgb()`/`rgba()`/`hsl()`/`hsla()` in both spellings, the named-colour table, `currentColor`, `none` |
| `css` | The document's own `<style>` sheets: the selector subset, specificity, `!important`, and the declarations one element matches |
| `geom` | `SubPath`, `StrokeStyle`, caps/joins, the object bounding box, the marker-vertex currency (`Vertex`, `Vertices`), and carrying either into another coordinate space |
| `pathdata` | The whole `d` grammar and curve/arc flattening to a tolerance |
| `shape` | The basic shapes, including `rect`'s rounded-corner rules |
| `marker` | `<marker>` placement: `refX`/`refY`, the viewport and its units, `orient`, and the one matrix per instance |
| `stroke` | Stroke outline: segment quads, joins, caps, dashes |
| `transform` | The `transform` grammar, `viewBox`, `preserveAspectRatio`, viewport fitting |
| `style` | The presentation-property cascade: attribute, stylesheet, `style` declaration, inheritance |
| `paint` | Paint servers: gradient definitions and pattern placement, `href` inheritance, units, spread, per-use resolution, and the three answers a `url(#id)` can come to |
| `document` | The tree walk that turns all of the above into the artwork tree |

The `no_std` float maths (`sqrt`, `sin`, `atan2`, rounding) lives in
`lib/util::mathf`, shared with the glyph rasteriser in `lib/fontface`.

## Compositing: one mechanism for three features

Group opacity, clipping, and masking are not three problems. Each one asks
for a subtree to be drawn *as a unit* and then composited through a
per-pixel factor, so the crate has exactly one answer:

```
Group { opacity, mask: Option<Mask>, children }
Mask  { kind: Alpha | Luminance, content: Vec<Node> }
```

- **Group opacity** is a group with no mask. Compositing the subtree and then
  weakening it is not the same picture as weakening each shape and
  compositing — two overlapping opaque shapes at 50% show the lower one
  through the upper only in the second — so the subtree really is rendered
  into its own buffer.
- **A clip** is a mask whose content is the clip's shapes filled opaque
  white, read as `Alpha`. Several shapes in one `<clipPath>` union exactly
  because opaque over opaque is opaque, and a `clip-path` *on* a `<clipPath>`
  is a group inside the mask's own content — so nesting needs no second rule.
- **A `<mask>`** is the same group with `Luminance`, which is why the mask's
  own region rectangle, and a clip or a nested mask inside it, all fall out
  of the model rather than being special cases.

A pattern's tile is the same kind of level: a buffer in flight holding a
drawing of its own, so it is charged against the same nesting bound as a
group. That is what makes a cycle of patterns painting one another terminate,
and what keeps "whatever the decoder admits, the renderer draws" true without
a second bound to keep in step.

The decoder emits a group **only where one changes the picture**: full
opacity, no clip, and no mask means the children are spliced into the parent
list and nothing is allocated. A shape that produces a single layer folds its
own `opacity` into that layer's alpha, because one layer composited at a
group opacity is the same pixels as that layer painted at the product — the
isolation buffer is allocated only where two layers of the same element (a
fill and its stroke) actually overlap.

Rendering is `Surface::draw_artwork`, and it fails closed: a group whose
isolation buffer cannot be allocated, a pattern whose tile cannot be
rendered, or a tree deeper than the renderer's own bound draws **nothing**
and reports it, so the caller falls back to the tier below rather than
showing a half-composited picture.

## What a reference inherits

A `<clipPath>`, a `<mask>`, and a gradient are reached by *reference*, and
what they inherit is their own place in the document — not the style of
whatever element pointed at them. That is what makes one definition mean the
same thing wherever it is used: a mask whose content states no fill takes the
fill its own ancestors give it, a clip takes its `clip-rule` the same way, and
a `currentColor` stop stands for the `color` the gradient sits in. The chain
from the root down to a definition is found once per definition a document
actually references, and memoised.

A `<pattern>`'s tile content is the same: it takes the fill, colour and
`overflow` its own ancestry gives it, and only its *geometry* comes from the
shape being filled. Where `href` inherits the content from another pattern,
the content inherits from where *that* one sits.

A `<use>` is the exception, and deliberately: SVG defines its content as
inheriting from the `<use>` itself, which is what lets one symbol be tinted
per user.

## The cascade

Four sources set a property, in this order — later wins:

1. presentation attributes (`fill="red"`),
2. normal declarations from the document's `<style>` sheets, ordered by
   selector specificity then source order,
3. normal declarations in the element's own `style` attribute,
4. `!important` declarations, stylesheet then `style` attribute.

The selector subset is type (`rect`), class (`.cls`), id (`#id`), universal
(`*`), any compound of those (`rect.a.b`), a selector list (`a, b`), and the
descendant and child combinators. Specificity is CSS's `(id, class, type)`
triple.

A rule this decoder cannot parse — an at-rule, an attribute selector, a
pseudo-class — is **dropped**, exactly as an unknown *property* already is,
because that is what the declaration means: it does not apply. Refusing the
document instead would lose an asset over a `@media print` block it would
never have drawn. A declaration whose property *is* understood but whose
value is malformed is still an error, so a bad colour fails closed.

`<style>` is the only element whose character data is read. A stylesheet with
a `type` that is neither absent nor `text/css` is ignored.

## Untrusted input

Every asset is hostile until proven otherwise. `decode` is total for any byte
string: no panic, no unbounded loop, no unbounded allocation, and no NaN or
infinity reaching the geometry. The fixed bounds — element count, **element
visits**, nesting depth, layer count, total vertices, group depth, tile
extent, tile fold, stylesheet rules and declarations, segments per curve,
dash-pattern length, gradient stops, `use` and `href` chain depth — are
**security bounds, not capacities**: they do not scale with the machine and
must not be raised to make an asset fit.

**Element visits bound decode *work*, where the others bound output.** A
`<use>`, a `<clipPath>`, a pattern tile, and a marker are each drawn once per
*reference*, so a document can make the walk visit far more elements than it
holds — and content that resolves to no paint charges no layer and no vertex,
so none of the output bounds notices. Measured on the unbounded form, a 188 KB
document of four thousand `<use>`s over a four-thousand-element subtree that
drew nothing took 1.2 s to decode, and 3.2 s with a stylesheet to match
against; `<clipPath>` and `<pattern>` fan-outs were the same shape. Charging
one visit wherever the walk reaches an element caps that at a few milliseconds
and refuses the rest, and it is what makes a marker — which multiplies hardest
of all, one `<path>` element placing an instance at every vertex of its `d` —
bounded by the work it asks for rather than by the elements it holds. It is
also what ends a marker that places itself.

Isolation buffers are what a clip, a mask, a group opacity and a pattern
tile cost, so the nesting bound is also a memory bound: it caps how many
surfaces one asset can have live at once, and the renderer refuses rather
than allocating past it. A tile's own extent is bounded separately, because
a tile is sized from the drawing's resolution rather than from the tile's
nesting, and so is its fold, which bounds how many times that one buffer is
drawn into rather than how many buffers are live.

A document that is malformed, or whose numbers, colours, or transforms are
outside the grammar, is refused **whole** with a precise `SvgError`; the
caller falls back to the tier below (`plans/ICONS.md`). Nothing is
half-applied.

## Patterns

A pattern is a tile of artwork repeated across a shape, so it is a paint
whose colour at a point is *pixels*. `tairix_raster::Paint::Pattern` carries
the tile as **artwork**, not as an image, and the renderer draws one repeat
per fill into a buffer sized from the density that fill reads it back at.

That is the whole of the design decision. A tile baked to pixels at decode
time would fix a resolution the decoder does not know: an `SvgImage` is
resolution-independent and is rasterised per (asset, pixel side), so a tile
rendered at some grid-derived size would alias when the asset is drawn small
and blur when it is drawn large — and it would be the one aliased thing in a
pipeline whose whole point is exact area coverage at the target size.
Rendering the tile at draw time costs one small buffer per patterned fill and
keeps the picture correct at every size.

- **Tile space is the unit square**, exactly as canonical gradient space is
  the x axis or the unit circle: `Pattern::to_tile` maps the filled
  geometry's coordinates into it, so `patternUnits`, `patternTransform` and a
  bounding-box placement are one matrix rather than cases in the sampler. The
  tile's own content is drawn on the shared design grid, of which the whole
  grid is one tile, so a repeat is rendered by the same `draw_artwork` walk
  as any other drawing.
- **The tile buffer is the clip.** A surface writes nothing outside itself,
  so confining the content to the tile needs no mask. That is the
  `overflow: hidden` a pattern is drawn under.
- **`overflow: visible` is folded, not approximated.** Content that escapes
  its tile makes neighbouring replicas overlap, which one sampled tile cannot
  express *as drawn* — but a pattern is periodic, so the infinitely many
  replicas restricted to one period sum to the finitely many whose content
  reaches into that period, folded back by whole periods. The tile is
  therefore rendered by drawing the content once per replica in a bounded
  window, each translated a whole period, into a buffer whose stated origin
  puts the tile's own replica where it belongs. The result is still one
  periodic tile: the wrap sampler and the per-pixel cost are unchanged and
  only the tile's own render pays, once per fill and off the frame path.
  `overflow: hidden` is the same mechanism with a zero-sized window, not a
  second path.
  - **The window is measured by the decoder**, from the content extent it
    already walks, and carried in `TileFold`. That is what keeps "whatever
    the decoder admits, the renderer draws" true: the bound is enforced where
    the document is refused, and `overflow` is a decoder concept the artwork
    form otherwise does not carry. The renderer checks the bound again before
    allocating, so a `Pattern` assembled by hand fails closed rather than
    being trusted.
  - **The overhangs cross over.** Content reaching past the tile's *right*
    edge is what the replica to its *left* spills back in, so a right
    overhang is counted as a replica *before* the tile.
  - **Replicas draw in raster order** — rows top to bottom, each row left to
    right — which is what a renderer blitting whole tiles across the plane
    produces. Order is observable once replicas overlap, and folding
    preserves it: the window is a contiguous box enumerated in that same
    order, so two replicas meeting at a point meet in the order the unfolded
    plane would have drawn them.
  - **`MAX_TILE_FOLD` bounds the window**, at one whole period each side —
    nine replica draws against one. A fixed containment bound, not a
    capacity: content reaching further overlaps its neighbour's neighbour, so
    the repeat has stopped being a repeat and the picture is artwork, more
    cheaply authored as artwork. A spill past it is `SvgError::TooComplex`,
    like every other budget overrun; refusing an absurd spill is not
    refusing `overflow: visible`.
- **Sampling is bilinear with a wrap.** The tile grid and the device grid
  share a density by construction but not a phase, so reading the nearest
  texel would shift a tiled feature by up to half a pixel, differently in
  each repeat. A sample reaching past an edge reads the opposite edge, so a
  tile whose content meets itself still does.
- **`MAX_TILE_EXTENT` bounds the render, not the repeat.** A tile's period is
  geometry; its resolution is not, so clamping the render costs sharpness and
  never distorts the picture. A hostile document therefore cannot ask for a
  tile the size of the drawing.
- **A tile costs a nesting level.** It is a buffer in flight like a group, so
  both are charged against `MAX_GROUP_DEPTH`, which is what ends a cycle of
  patterns painting one another.
- **The fill's opacity weakens the assembled tile**, carried on the paint
  rather than as a group around the content. SVG weakens the fill operation
  as a whole, so a group inside the tile would be paid once per *replica* and
  overlaps would come out too strong. There is no second spelling of group
  opacity here, because there is no single subtree to wrap: the thing being
  weakened is the tile the renderer assembles, not a node. Scaling the
  finished buffer's premultiplied channels is exactly compositing that one
  layer at the opacity, and it costs no isolation buffer — so the decoder
  charges no nesting level for it either, and both sides moved together.

## Markers

A marker is a drawing placed at a shape's vertices and turned to follow the
path through each. Two things about it cut across the rest of the crate.

- **The vertices are the ones the author wrote, and the direction is the
  curve's true tangent — both of which flattening destroys.** One geometry
  currency means a curve stops being a curve in exactly one place, so there is
  no second parse and no second flattening pass to recover them from: the
  parser fills an opt-in `Vertices` sink *while* the command structure is
  still live. A shape that references no marker passes nothing and pays a
  branch per command, which is what keeps the common shapes out of it.
- **The tangent is exact, never the first flattened chord.** The chord is free
  and already there, but it is an artefact of the tolerance, which is resolved
  against the placement a shape is drawn under — so a marker oriented by it
  would swing as the asset was rasterised larger. A cubic's is its
  control-point direction, a quadratic's likewise, an arc's comes from the
  same centre parameterisation the flattener sweeps; a control point
  coincident with its endpoint falls through to the next point along, which is
  the limit of the curve's own direction there. They differ visibly: a cubic
  whose first control point is a thousandth of a unit from its start leaves
  along the x axis, where its first chord already points almost along y.

The rest follows from mechanisms the crate already has.

- **Only the four shapes with authored vertices take markers** — `path`,
  `line`, `polyline`, `polygon`. A `<rect>`, `<circle>`, or `<ellipse>` has
  none of its own: its outline is *this decoder's* flattening, so its
  "vertices" would be tolerance artefacts and a marker would slide along the
  outline as the asset was drawn larger. SVG 2 places markers on those shapes
  too, against an *equivalent path* of arcs this crate has no notion of,
  having flattened at parse time.
- **A closed sub-path is one closed curve, so its two ends read the same
  turn**: in along the closing segment, out along the sub-path's first. That
  is one rule in the vertex builder, and it covers both what SVG says of the
  initial vertex and of the closepath vertex — and a segment following a
  closepath without a `moveto` overwrites the outgoing half, because that is
  where the pen actually goes next. `marker-start` and `marker-end` belong to
  the *path*, not to each sub-path, so a closed shape carries both on the one
  point it returns to. An exact reversal has no bisector at all; approached
  from either side the answer tends to one of the two perpendiculars, so a
  perpendicular is what it takes rather than whatever the arithmetic would
  otherwise fall out with.
- **One placement matrix per instance.** `refX`/`refY`, `markerWidth`/
  `markerHeight`, `markerUnits`, `orient`, and the marker's own `viewBox`/
  `preserveAspectRatio` collapse into a single `Affine`, exactly as
  `patternUnits`/`patternTransform`/bbox collapse into `Pattern::to_tile`, so
  the walk that draws an instance takes no cases. The rotation is built
  straight from the unit direction vector rather than from an angle, which is
  exact and costs no trigonometry. The `<marker>` element is read once per
  shape; only the matrix is per vertex.
- **A marker takes its own place in the document**, like every other
  referenced definition — SVG states outright that properties do not inherit
  from the element referencing a marker into its contents. SVG 2's
  `context-fill`/`context-stroke` are the sanctioned way across that boundary
  and are not in SVG 1.1, so a marker here is never tinted by its user.
- **`overflow` is the `<symbol>` mechanism**, unchanged: a viewport clip whose
  "cuts nothing off, so costs nothing" fast path keeps a marker whose content
  fits inside its viewport free of an isolation buffer, per instance.
- **`paint-order` is a permutation of all three**: whichever slots the value
  names come first in the order written, and whichever are left follow in the
  initial order, with an invalid value dropped as CSS drops it rather than
  resetting the property.
- **An element's opacity composites its markers with the rest of it.** A
  marker is a subtree that may overlap the shape and the next instance of
  itself, so there is no single layer to fold the opacity into.

## Non-scaling strokes

`vector-effect="non-scaling-stroke"` spends the element's transform on the
path and not on the pen. It is the one thing here whose outline cannot be
built where every other outline is: `stroke` is written against
pre-transform geometry and the result is mapped onto the grid afterwards, so
this inverts that order and needs both spaces at once.

- **The host space is the document's own root user space.** SVG calculates
  such a stroke in the *host* coordinate space, which the specification
  equates to the screen's. A decoded asset has no screen — an `SvgImage` is
  resolution-independent and rasterised per (asset, pixel side) — so a width
  fixed in device pixels would bake in a resolution the decoder does not
  know, which is the same objection that keeps a pattern's tile from being
  baked to pixels. The root user space stands in for it, and is the right
  stand-in twice over: it is the space the document's own lengths are
  written in, and its map to the device is a *uniform* scale under both
  viewports, so a round pen stays round. `Square` letter-boxes the drawing
  by one scale; `Natural` stretches the grid to the drawing's shape and
  un-stretches it again at rasterisation, so that anisotropy cancels. The
  design grid is therefore **not** the host space — outlining there would
  draw an elliptical pen under `Natural`.
- **It is the root, not the nearest viewport.** A nested `<svg>` and a
  `<use>`/`<symbol>` slot each establish a viewport on the way down, and
  both scales are part of the chain the effect cancels. That is what the
  specification's "screen" means, and it is the only case where the two
  readings draw differently.
- **Anisotropy stops being a question rather than being answered.** An
  ordinary pen under `scale(3 1)` is an ellipse, and a single-number scale
  cannot describe it — so the obvious design asks whether a transform is
  uniform before trusting one. It never has to: the anisotropic part of the
  chain is exactly what the effect cancels, so the pen is round by
  construction. No uniformity test is needed, and `Affine` needs no
  `min_scale` to answer one.
- **The width, the dashes and the offset move together, because the stroker
  has no opinion about which space it is in.** It reads all three as lengths
  in whatever coordinates it is handed, so carrying the geometry across
  carries them too, and the miter limit is a ratio and is space-free. Only
  the flattening tolerance is restated, being the one length resolved
  against the placement rather than authored; converting the dash lengths
  as well would have produced the obvious bug, a pattern that went on
  scaling while the width did not.
- **A marker measured in stroke widths follows the stroke across.** SVG
  sizes one by the stroke width *after* the transforms that affect the
  width, and says outright that a non-scaling stroke therefore makes its
  markers non-scaling. So a `strokeWidth` marker is placed in the host space
  — at the vertex as that space sees it, turned by the direction the path
  runs there — while a `userSpaceOnUse` marker names its own space, depends
  on no stroke width, and goes on scaling.
- **Nothing new is bounded, because nothing here multiplies.** The same
  elements are visited and the same instances placed; one outline is built
  per stroked shape as before, and the additional cost is one matrix
  compose and a transient copy of geometry the vertex budget has already
  paid for. The host tolerance cannot buy more output either: the stroker
  still floors it and still caps the segments of an arc.

## Deliberate non-goals

Not deferred work — these are outside what an artwork decoder is for, and
adding one would be a new plan of its own:

- Text (`<text>`, `<tspan>`, fonts, text layout). Glyph rendering is
  `lib/fontface`'s job, and artwork ships its lettering as outlines.
- Embedded raster images (`<image>`), which would nest one decoder in
  another.
- Filters.
- Animation (SMIL), scripting, and external references of any kind. A
  stylesheet is read only from the document's own `<style>` elements; an
  `@import` is not fetched.
- The `vector-effect` values beside `non-scaling-stroke` —
  `non-scaling-size`, `non-rotation`, `fixed-position`. SVG 2 records them
  as at risk of being dropped for want of implementations, and
  `non-scaling-size` in particular suppresses scaling of the whole user
  coordinate system, of which a non-scaling stroke is one consequence:
  drawing half of it would be a wrong picture where drawing none of it is a
  missing decoration. They parse as the valid CSS they are and ask for
  nothing.

## Open question

`AGENTS.md` fails closed by default, but an element this decoder cannot draw
is currently **skipped** rather than refusing the document — so an asset
carrying an undrawable decoration renders without it instead of falling back
to the tier below. Skipping is what lets one
unsupported decoration not lose a whole asset, and it is the behaviour the
desktop has today. Whether the drawable-element case should instead fail the
document closed is recorded as an open item in `plans/ICONS.md`; it is a
deliberate decision to make, not an oversight. The set at stake has stopped
shrinking, there being nothing left to shrink it by: clipping, masking, group
opacity, patterns (spilling tiles included), markers and non-scaling strokes
are all honoured, so the only cases that still render a *wrong* picture are
the deliberate non-goals. That makes it a materially different question from
the one it started as. It is no longer worth waiting for the decoder to catch
up; it is a decision about text, embedded images, filters and animation,
which are not going to be drawn.
Patterns also drew the distinction that answers part of the question: a reference naming a server
the document does not define takes its fallback colour, while a server that
is defined and paints nothing is `none` and takes none — so an *empty*
pattern or gradient no longer renders as a fallback colour it was never
given.
