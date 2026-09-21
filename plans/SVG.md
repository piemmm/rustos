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
| S22 | Glyph outlines out of `lib/fontface`: a public contour API, so text reaches this crate as geometry and never as pixels | done |
| S23 | The font seam: resolving `font-family`/`font-weight`/`font-style`/`font-stretch` to a face at decode time, injected and capability-scoped | done |
| S24 | `<text>` and `<tspan>`: the `x`/`y`/`dx`/`dy`/`rotate` lists, `text-anchor`, white-space and `xml:space`, `letter-spacing`/`word-spacing`, `textLength`/`lengthAdjust` | done |
| S25 | The text property cascade beyond the `font-*` family: the baselines (`dominant-baseline`, `alignment-baseline`, `baseline-shift`), `text-decoration` | planned |
| S26 | `<textPath>`: glyphs laid along a path, `startOffset`, `method`, `spacing`, `side` | planned |
| S27 | Bidirectional text and shaping: UAX#9, `direction`/`unicode-bidi`, OpenType `GSUB`/`GPOS`, `writing-mode` and vertical text | planned |
| S28 | The SVG 1.1 text leftovers: `<tref>`, and SVG fonts (`<font>`, `<glyph>`, `<altGlyph>`) | planned |
| S29 | `<image>`: `href` and `data:` URIs, its own `preserveAspectRatio`, decoded through `lib/image` inside the parser sandbox, with a nested-document recursion bound | planned |
| S30 | The filter region and graph: `<filter>`, `filterUnits`, `primitiveUnits`, the region rectangle, `color-interpolation-filters`, the artwork-tree filter node and the renderer's evaluator | planned |
| S31 | Source and plumbing primitives: `feFlood`, `feOffset`, `feMerge`/`feMergeNode`, `feTile`, `feImage`, and the named inputs | planned |
| S32 | Colour primitives: `feColorMatrix`, `feComponentTransfer` with its four transfer functions, `feBlend`, `feComposite` including `arithmetic` | planned |
| S33 | Spatial primitives: `feGaussianBlur` over `lib/raster`'s box blur, `feMorphology`, `feConvolveMatrix`, `feDisplacementMap`, `feDropShadow` | planned |
| S34 | Lighting and noise: `feDiffuseLighting` and `feSpecularLighting` with all three light sources, and `feTurbulence`'s exactly-specified generator | planned |
| S35 | The time model: what an animated `SvgImage` is, and how decoding once survives a picture that varies with time | planned |
| S36 | `<animate>`, `<set>`, `<animateTransform>`: `from`/`to`/`by`/`values`, `calcMode`, `keyTimes`, `keySplines`, `additive`, `accumulate` | planned |
| S37 | `<animateMotion>` and `<mpath>`, including `rotate="auto"` along the motion path | planned |
| S38 | The timing graph: `begin`/`end` lists, offsets, syncbase and repeat timing, `restart`, `fill`, `repeatCount`/`repeatDur`, `min`/`max` | planned |
| S39 | The CSS surface currently dropped: attribute selectors, pseudo-classes, `@media`, `@supports`, `@font-face`, custom properties with `var()`, and `calc()` | planned |
| S40 | CSS presentation of geometry: `transform` as a property with `transform-origin`/`transform-box`, `mix-blend-mode` and `isolation`, and the shorthand function forms of `filter`/`clip-path`/`mask` | planned |
| S41 | The remaining `vector-effect` values: `non-scaling-size`, `non-rotation`, `fixed-position` | planned |
| S42 | Structural leftovers: `<view>`, `<cursor>`, `<a>` link regions with `pointer-events`, and `<title>`/`<desc>`/`<metadata>` as retained metadata | planned |
| S43 | Colour management: `color-interpolation` (linearRGB gradients and compositing), `<color-profile>`/ICC, and the `shape-rendering`/`text-rendering`/`image-rendering` hints | planned |
| S44 | External references, capability-gated: `@import`, an `href` into another document, external fonts and images | planned |
| S45 | `<foreignObject>`: undrawable, so a `<switch>` takes its fallback sibling | planned |
| S46 | The scripted-document model: where a script runs, what a live document is, and which consumers may enable one at all | planned |
| S47 | The ECMAScript engine: parse, interpret, collect, and the bounded execution budget that makes it abortable | planned |
| S48 | The SVG DOM binding: the document/element/attribute/style interfaces, and the mutation path back into the artwork tree | planned |
| S49 | Events and timers: `load`/pointer/keyboard events with the hit testing they need, `setTimeout`/`setInterval`/`requestAnimationFrame` | planned |

S1–S24 are `done` and none is half-built. S25–S44 are the rest of SVG, which
this crate must draw and does not yet: the remaining text surface, embedded
images, filters,
animation, the CSS surface the cascade still drops, and the reference
resolution that reaches outside the document. S45–S49 follow from the
decisions recorded under [Decisions taken](#decisions-taken).

Until an item lands its elements are **silently skipped** by the walk, which
is the behaviour the [Open question](#open-question) is about — and while the
list below is non-empty, that question has a live answer rather than a
theoretical one.

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

## Text

Latin, Greek and Cyrillic text draws (S23, S24). What remains is the
property surface beyond the `font-*` family (S25), `<textPath>` (S26), and
the reordering and shaping the other scripts need (S27–S28).

### A glyph is geometry, never pixels

SVG text is filled, stroked, gradient-painted, clipped, masked and
transformed exactly as a `<path>` is, and an `SvgImage` is
resolution-independent. So a glyph arrives as contours in font units and
joins the one geometry currency: `lib/svg` flattens the quadratics through
its own `flatten_quadratic` at the tolerance the *placement* resolves, the
same single step every other curve takes, so a glyph subdivides exactly as a
`<path>` of the same shape. A rasterised cell would fix a resolution at
decode time and put text on a second rasterisation path.

`lib/fontface`'s `Face::glyph_outline` (S22) is where that geometry comes
from, and `plans/FONT-SERVICE.md` §3.3's open question is closed: it reaches
a consumer as a **contour reply kind on `FONT_ENDPOINT`**
(`FontRequest::Outlines`), never as face bytes — handing over a face would
put an untrusted TrueType parser back into every consumer, which is the
defect the font service exists to remove.

- **No pixel height on the request.** A drawing has no resolution, so the
  field must be zero and a frame carrying one is refused. Coordinates are
  26.6 fixed-point font units, which makes NaN and infinity
  *unrepresentable* rather than merely checked for.
- **The resolved face's em and its synthesis travel per record**, not per
  batch, because a per-scalar fallback crosses faces: a family's primary and
  its Chinese companion need share neither an em nor a `wght` axis. The
  batch header carries the *requested family's primary* face geometry, which
  is what a run's baseline and line box are measured in.
- **What the face cannot furnish is stated, not substituted.** A face with
  no `wght` axis reports an em-relative bold stroke; one that cannot lean
  reports an oblique shear. The caller completes it with the crate's own
  stroker and one shear folded into the glyph transform, so there is no
  second thickening or slanting implementation. Width is never synthesised:
  stretching letterforms is a distortion, not a width.
- **The byte budget did not move.** A worst-case outline reply is 163,876
  bytes against the coverage reply's 524,312, so no receive buffer grew and
  no existing bound changed. A reply answers a prefix of the run under the
  one shared fill rule both batch kinds obey.

### The seam is injected, and the caller decides what text can do

`decode(bytes, viewport, provider)`. `lib/svg` gains no `lib/abi` edge and
no authority: the provider is a pure trait modelled on the help engine's
read seam. `NoFonts` furnishes nothing, so a compositor asset path decodes
drawings without text rather than acquiring the authority to draw them;
`tairix_font::ServiceFonts` is the one adapter to the endpoint.

The `font-family` *list* is walked by the decoder, since that is CSS
semantics; the **generic** ladder lives at the service, which is the thing
that knows the store, and a family declares which generic it answers for in
its own `FontFamily` manifest. A family nothing can furnish **fails the
document closed** rather than drawing a picture with its lettering missing:
that is the one place this crate refuses instead of skipping, and it is why
the [Open question](#open-question) below is now about decorations alone.

### The sandbox supplies glyphs without gaining a capability

`view.app` decodes inside the §19.5 parser sandbox, which holds two pipe
ends and nothing else, so it cannot call the font endpoint — and is not
given the ability to. A **two-phase exchange** over the pipes it already has
supplies the glyphs: the worker decodes once against placeholder geometry,
recording every face and scalar the table it holds cannot answer; the host,
a GUI process that does hold a font client, fetches exactly that and sends
it back; the worker decodes again. It terminates in two rounds by
construction, because glyph geometry cannot change which elements the walk
visits or which scalars the text holds. A document with no text records
nothing and costs one round.

### Layout

A run of one style emits **one `Layer`** with `FillRule::NonZero` and all
its glyphs' contours together — the TrueType rule S22 settled, so a counter
is a contour wound against the one enclosing it. White space is collapsed
across the whole `<text>` before any positioning, because
`<text>a <tspan> b</tspan></text>` is `a b` and no per-element pass can see
that space. An element's `x`/`y`/`dx`/`dy`/`rotate` lists address the
characters its *descendants* contributed as well as its own, a descendant's
own value winning; an absolute position begins the chunk `text-anchor`
shifts.

`textLength` spreads its difference across the gaps, or scales the glyphs
too under `spacingAndGlyphs`. A `font-size` is computed before every other
length in the same cascade, which is what lets `stroke-width: 0.1em` mean
the size the element ends up set in.

### Bounds

Glyphs per document, `<tspan>` nesting, the resolved length of one `<text>`,
the runs a document may emit, and — because a provider may be a live service
across an IPC boundary — the *requests* one document may make of it, so a
hostile document cannot turn one decode into thousands of round trips. The
outline points a glyph contributes are charged against the same total-vertex
budget a `<path>` spends. All fixed containment bounds.

### What the running machine attests

Host tests cover every layer — the wire form and its refusals, the service's
resolution and synthesis, the layout against the SVG 1.1 positioning rules,
the two-round exchange, and the decoder's own `<text>` drawing — and the
build-time icon verification drives the real service. The **pipe between
them** is what only a running machine can show, so one QEMU vertical
(`tests/integration/svgtext_qemu_aarch64`) does: a command app opens
drawings through the viewer's own `open_view`/`render_page` sequence inside
the parser sandbox, against a `fontd` its own first glyph request activated.

Its measurement is what keeps it honest. Two drawings that differ in exactly
one character must ink *differently*, and in the direction their characters
do — nothing but real, character-dependent outlines does that, where a
witness marker alone would pass on a decode that drew nothing. The wide
drawing is then opened a third time through a sandbox given no font seam and
must be **refused**, which leaves the service as the only place the outlines
could have come from. Each expectation fails with its own name, so a broken
run says which one it missed.

Nothing in the script waits for the font service, deliberately. `fontd` is
registered on-demand, so the fixture's first glyph request is what activates
it, and the service manager holds that call until the endpoint is answerable
(`plans/NEW-SERVICEMANAGER.md` SVC-5). A script that gated on a readiness
line instead would be proving its own ordering rather than the system's, so
the enrolment pins that it does not. The vertical still boots with a
framebuffer because the fixture renders a picture, not because the service
needs one; its script never leaves the shell.

### What is left

S25 is the rest of the text property cascade: the baselines
(`dominant-baseline`, `alignment-baseline`, `baseline-shift`) and
`text-decoration`. S26 is `<textPath>`. S27 is bidi and shaping — UAX#9
reordering and OpenType `GSUB`/`GPOS`, neither of which `lib/fontface` has —
so Arabic, Hebrew, and the Indic and CJK scripts are *skipped* like any
other element this decoder cannot yet draw rather than half-drawn.

## Embedded images

- **The decoder exists; the seam does not.** `lib/image` already reads PNG,
  JPEG, GIF, BMP, ICO, TIFF and WebP, with sequence support and its own
  `DecodeLimits`. S29 adds no format work — it adds `<image>`, the `data:` URI
  grammar, the element's own `preserveAspectRatio` fit, and a raster carrier in
  the artwork tree, which today holds only colour, gradient and pattern paints.
- **Nesting a decoder in a decoder is the risk, so it is bounded and
  sandboxed.** The decode runs in the minimum-capability parser sandbox, under
  fixed input-byte and output-pixel bounds, and a malformed image fails closed
  to drawing nothing rather than taking the asset with it. An `<image>` naming
  an SVG re-enters this decoder, so it charges the same recursion and
  element-visit budgets the rest of the walk does.

## Filters

- **A filter is pixels, so it is a new kind of artwork node.** Group, mask and
  pattern already give the renderer a subtree drawn into its own buffer; a
  filter is that buffer plus a primitive graph evaluated over it. The graph
  belongs in `lib/raster` beside the renderer that runs it, not in the decoder,
  which builds and bounds it (S30).
- **The filter region is the memory bound.** It is stated in user or
  bounding-box units and can be made enormous, so it is clamped like a
  pattern tile's extent — a fixed containment bound, refused rather than
  allocated past.
- **`color-interpolation-filters` defaults to linearRGB**, which is the trap in
  this area: a filter graph evaluated in sRGB gives visibly wrong results for
  blur and lighting. The conversion is part of S30, not an afterthought in each
  primitive.
- **Two primitives are exactly specified and must be bit-faithful.**
  `feTurbulence`'s Perlin generator is given as reference code in the
  specification, and `feGaussianBlur` is defined as three successive box blurs
  at a stated width — which `lib/raster`'s `box_blur` already provides, so that
  one is composition rather than new arithmetic.

## Animation

- **This is the one item that changes what an `SvgImage` is**, and the decision
  belongs in S35 before S36–S38 are written. Today the type is a static artwork
  tree decoded once and blitted many times, which is the crate's central
  performance claim. A document that changes with time cannot be that, and the
  two honest shapes are: decode at a stated time, so the consumer asks for the
  picture at *t* and caching stays per (asset, time); or an animated image
  carrying the timeline, which the consumer steps. The first keeps the existing
  type and the existing cache, and costs a decode per distinct time; the second
  decodes once but makes every consumer time-aware. Neither is free, and
  choosing without measuring the compositor cost would be a guess.
- **The timing graph is the substance, not the interpolation.** `begin`/`end`
  are lists of offsets, syncbase references to other animations, event
  triggers and repeat triggers, and they form a dependency graph that can be
  cyclic — so it is bounded and must terminate, like every other reference
  chain here.
- **Bounds.** Timeline length, animation count, and resolved repeat count, all
  fixed.

## Decisions taken

Both were raised rather than settled inside this plan, and both are now
decided. What each obliges is recorded here because it shapes items above.

- **`<foreignObject>` is undrawable, and says so (S45).** Its content is
  another language — in practice HTML — so drawing it means an HTML parser,
  the CSS box model and a layout engine inside an asset decoder. Instead it
  is treated as an element this decoder cannot draw, which is exactly what
  makes a `<switch>` choose the sibling fallback beside it. That is how
  documents in the wild already degrade, it needs no new mechanism, and it is
  a *defined* answer rather than a silent skip.

- **Scripting is in, and containment is the whole design (S46–S49).** The
  decision is to support it; the obligation that comes with it is that a
  script must not be able to reach anything the picture does not need.
  - **A scripted document is a different product from an asset.** Everything
    else this crate decodes is converted once and blitted many times. A
    script makes the document *live*: it mutates the DOM, it runs on a timer,
    it responds to input, and it has no decode that is ever finished. S46
    settles that boundary before S47 is written, and it depends on the time
    model (S35), because a live document is the animated case with a second
    source of change.
  - **Scripting is opt-in per consumer, and the desktop's asset paths never
    opt in.** A cursor, a status glyph, window furniture and an icon are
    decoded from files the user did not choose to run, on the compositor's
    path; there is no picture worth an execution engine there. The consumer
    that legitimately enables it is a document *viewer* opening a file the
    user asked for (`plans/VIEW.md`). The capability to run a script is
    therefore granted by the caller, defaults to absent, and is refused
    rather than assumed.
  - **It runs in its own process, not the caller's.** The engine lives behind
    the minimum-capability parser sandbox: a dedicated address space holding
    one IPC endpoint and nothing else — no filesystem, no network, no spawn,
    no capability delegation inward. A script that crashes, hangs or is
    killed costs its own sandbox and returns an error to the caller, exactly
    as a malformed parse does.
  - **Execution is budgeted and abortable.** An instruction budget, a
    wall-clock budget and a heap ceiling, all fixed containment bounds. There
    is no "run until it finishes": a script over budget is stopped and the
    document keeps the last picture it had. This is the one place in the
    crate where the work is genuinely unbounded by the input's size, so the
    budget is the only thing standing in for every other bound here.
  - **Whether the engine is first-party is a sub-decision still to take.**
    Rolling an ECMAScript implementation in house is a larger trusted
    computing base than the whole of the rest of `lib/svg`, and doing it
    badly is worse than a vetted dependency — the reasoning the charter
    already applies to cryptography. It is called out here so it is chosen
    deliberately when S47 starts, not defaulted into.

## Open question

`AGENTS.md` fails closed by default, but an element this decoder cannot draw
is currently **skipped** rather than refusing the document — so an asset
carrying one renders without it instead of falling back to the tier below.
Skipping is what lets one unsupported decoration not lose a whole asset, and
it is the behaviour the desktop has today. Whether the drawable-element case
should instead fail the document closed is recorded as an open item in
`plans/ICONS.md`; it is a deliberate decision to make, not an oversight.

Text answered half of it. Lettering nothing can furnish refuses the document
(S23), because absent lettering is missing *content* rather than a missing
decoration — which is the distinction the question was really about, now
encoded in one place rather than chosen globally. What remains outstanding
is the decoration half: an unsupported filter, an embedded image, or an
animation still skips, and whether any of those should refuse instead should
be taken against the set that remains rather than the set that happens to be
unimplemented today.

Patterns drew the distinction that answers part of the question: a reference
naming a server the document does not define takes its fallback colour, while
a server that is defined and paints nothing is `none` and takes none — so an
*empty* pattern or gradient no longer renders as a fallback colour it was
never given.
