# tairix-raster

The single shared **software rasterisation** primitives for the TAIRiX
desktop (`AGENTS.md` §6, §17.4 — `PLAN.md` Stage 7).

Both the compositing window manager (`userland/gui/wm`) and the taskbar
(`userland/gui/taskbar`) draw pixels, but neither may depend on the other
(`AGENTS.md` §17.4). The shared rasteriser therefore lives in `lib/*` (§6),
exactly as `lib/geometry` owns the shared coordinate types and `lib/theme`
owns the shared design tokens.

This crate owns:

- `Color` / `Pixel` — a straight-alpha authored colour and its
  **premultiplied-alpha** pixel, with the `div255` rounded divide, the
  Porter–Duff `over` operator, `scale_alpha`, and `premultiply` /
  `unpremultiply`. The compositor works exclusively in premultiplied alpha,
  which keeps per-region opacity and rounded-corner coverage correct
  (`AGENTS.md` §10).
- `Surface` — a dense row-major premultiplied pixel buffer with bounds-checked
  `get`/`set`, the `row_span_mut` write seam, `fill`, and clipped
  `fill_rect`/`fill_round_rect`. It is the rendered content of a window for the
  compositor and the painted body of the taskbar.
- `Surface::with_clip` — the scoped clip window every write is confined to, so a
  view bounds what it paints to the area it owns even when it hands the surface
  to code that does not know it is clipped (see below).
- `Surface::fill_contours` — the anti-aliased fill for real vector artwork:
  any number of implicitly-closed contours, resolved under a `FillRule`
  (`NonZero`, SVG's initial value, or `EvenOdd`) and painted with a `Paint` —
  a flat colour or a gradient. Nesting is what makes a hole, so a glyph or an
  SVG path fills in one call rather than as a stack of layers, and a gradient
  is sampled once per pixel at that pixel's centre mapped back into the
  contours' own coordinates. A pixel's alpha is the **exact fraction of its
  area** the shape covers, not the fraction of a sample grid that landed
  inside it: each row accumulates every edge's signed vertical extent and
  trapezoid area into per-pixel cells and one left-to-right sweep turns them
  into coverage. That is both exact — a sample count can only quantise the
  answer and the edge's position, which is what leaves small artwork soft and
  lopsided — and cheaper, since a fill costs the edges plus the pixels once
  rather than a sorted pass per sample row: 175 µs for a 4096-edge contour
  over a 128×128 surface, against 0.3 ms sorting four sample rows per pixel
  row and 409 ms probing every edge for every sub-sample.
- `Surface::layered` — paint a stack of filled shapes and resolve the seams
  between them. Anti-aliasing and compositing do not commute: where one
  layer's soft edge meets the next one's, two partial alphas blend as if they
  overlapped, leaving a shape's outline short of opaque and a pale line
  between two abutting parts of a glyph. A multi-layer stack is therefore
  painted several times larger and averaged back down, tapering to no
  enlargement at all once the result is fine enough for the seam not to show.
- `Surface::fill_polygon` — the single anti-aliased filled-polygon scan
  converter. Vector artwork (pointer cursors in
  `lib/cursor`, status icons in `lib/icon`) is authored on a design grid and
  drawn through this one path, so the desktop has exactly one polygon
  rasteriser rather than a copy per asset kind (`AGENTS.md` §2.2 / §10). Only
  the polygon's bounding box, clipped to the surface, is scanned, so a small
  shape on a large canvas costs its own area rather than the whole surface.
  It is `fill_contours` with one ring, the even-odd rule and a flat colour:
  both — and `fill_polygon_subpixel` below — are the one converter in `scan`,
  never a second copy.
- `Surface::fill_polygon_subpixel` — that same scan converter over vertices
  already in *device* sub-pixel units (`SUBPIXEL` per pixel) instead of a
  design grid stretched across the surface. This is how chrome that must stay
  sharp at a small pixel size is drawn: a caller grid-fits its shape to whole
  pixels and every axis-aligned edge then lands exactly on a pixel boundary,
  producing no anti-aliased fringe at all, while a diagonal keeps sub-pixel
  placement and stays smooth. The shape is *placed*, not stretched, so a glyph
  needs no square scratch surface and blit to position it.
- `FillRule` / `Paint` — what a contour fill resolves and paints with. A
  `Paint` is a flat `Color` or a `Gradient`: linear or radial (with a focal
  point), a stop list, a `SpreadMethod` (`Pad`/`Reflect`/`Repeat`), and the
  `Affine` mapping a shape's coordinates into *canonical* gradient space,
  where a linear ramp runs along x from 0 to 1 and a radial one is the unit
  circle at the origin. Every ellipse, rotation and units convention a document
  can express is then one matrix rather than a case in the sampler. Sampling is
  total: no stops paints nothing, one stop paints it everywhere, a focal point
  outside the circle is pulled just inside it as SVG requires, and an extreme
  or degenerate transform resolves to an end colour rather than a `NaN`.
- `Affine` — SVG's `matrix(a b c d e f)`, the transform vector artwork is
  placed by and a gradient carries: `translate`, `scale`, `rotate_degrees`
  (and about a centre), `skew_x_degrees`/`skew_y_degrees`, `then` composition
  (the receiver applies first), `apply`, an `invert` that answers `None` for a
  transform that collapses area or would invert to infinities, and `max_scale`
  — the exact larger singular value, which is what a curve flattener divides
  its tolerance by and what decides whether a stroke stays uniform. Its
  trigonometry comes from `tairix_util::mathf`, so no external libm enters the
  trusted computing base and this crate rotates identically to the glyph
  rasteriser and the SVG decoder.
- `Surface::stroke_polyline` — the one stroked-line path the desktop shares: a
  window-furniture diagonal and a history graph's trace are the same primitive
  at different scales. Each segment is offset along *its own* perpendicular, so
  every segment keeps its full width whatever its slope, and consecutive
  segments overlap at the vertex they share — which is what joins them, with no
  seam and no darkened joint, since compositing an opaque source twice yields
  the same pixel. That perpendicular is scaled by the segment's length, which
  is `u64::isqrt` over a widened sum of squares: bounded by construction, and
  exact for a segment far longer than any screen. It was once a hand-rolled
  Newton iteration that stopped when two successive estimates agreed — for a
  squared length one below a perfect square the estimates cycle and never
  agree, so an unlucky graph reading spun its process forever.
- `box_blur` / `Surface::frost_region` / `BlurScratch` — the single separable
  box blur and the one frosted glass built on it. The blur is a horizontal
  pass then a vertical one carrying running sums, so the cost is the region's
  area whatever the radius. Every channel including alpha is averaged, which
  on premultiplied data is the convex combination compositing would give, so
  the `colour <= alpha` invariant survives and no halo appears at a
  translucent edge; samples past an edge replicate it, which keeps the
  divisor constant and leaves a uniform field exactly unchanged.
  `frost_region` is the effect itself: it blurs one rectangle of a surface and
  mixes the result back weighted by a caller-supplied per-pixel coverage, so a
  rounded shape fades from frosted to untouched across its own arc rather than
  showing a square edge. Coverage is asked at coordinates relative to the
  rectangle's *own* top-left, so a rectangle the surface edge or the active clip
  window cuts short still reads its whole shape while the frost touches only
  what the bounds and the clip admit. A zero radius, an empty or wholly
  off-surface rectangle, and a scratch that cannot be grown each leave the
  surface exactly as it was. Nothing is written until both passes are done,
  which is what lets the horizontal one read the surface's own rows rather than
  a copy of them. `BlurScratch` holds the blurred pixels and the pass-to-pass
  intermediate across calls — grown on demand, reused, and handed back by
  `release` — so the per-frame caller (the compositor frosting a window's
  backdrop) allocates nothing once it is warm. The effect was the window
  manager's alone until the graphical login screen needed it behind a selected
  account tile, and neither the login screen nor any other `lib/*` consumer may
  depend on the window manager.

  `frost_region_around` frosts the same rectangle **except** a kept inner
  block, and writes exactly the pixels the whole-rectangle frost would write
  around it — proved by a differential sweep over random blocks, radii and
  coverages. The rectangle still decides the answer: samples replicate at *its*
  edges and coverage is read at its own coordinates, so a border is never a
  smaller frost of a smaller rectangle, which would spread a clipped
  neighbourhood and seam against the pixels it was kept beside. The border's
  four bands are all blurred before any is mixed back, because a band's
  neighbourhood reaches into the bands next to it and what it must read there
  is the *unfrosted* surface. Its caller is the compositor: a frosted window
  that has moved keeps every retained pixel neither the blur's replication nor
  its own corners can reach, and pays for the border alone instead of a whole
  blur per pointer sample.

  Each pass costs a load, a running-sum update, a multiply and a store per
  sample. The window is the same size for every output — replicated edges keep
  the divisor at `2·radius + 1` — so the divisor is resolved **once per pass**
  into a fixed-point reciprocal instead of dividing four times per pixel per
  pass, which was the dominant cost of a frosted window. The reciprocal is
  *exactly* the divide, not an approximation: the `Reciprocal` rustdoc carries
  the proof and `blur_tests` checks its condition for every window size the
  blur uses, plus that a size above the cutoff genuinely breaks it. The output slot
  and the two samples the sliding window trades are each monotone along the
  line, so all three are walked as strided iterators and the furthest offset
  any can reach is bounds-checked once per line rather than per sample. Blurred
  output is byte-identical to a naive `O(area·radius)` average, which
  `blur_tests` asserts over a spread of shapes and radii including the
  single-row, single-column and radius-wider-than-the-region cases.
- `blend_span` — composite a run of source pixels over a run of destination
  pixels, each source scaled by a factor on its way in and both roundings
  taken at the row's ordered dither. This is the crate's **one span
  composite**, and every blended run in the desktop goes through it: a blit,
  and the window manager laying a translucent window's row over the picture
  beneath it, are the same walk, so a blended pixel is the same arithmetic
  wherever it comes from. The two slices are paired by position and the
  shorter ends the walk; the dither is read at each pixel's own *surface*
  column, so a run split anywhere writes exactly what the whole run wrote and
  a moving segment boundary can never leave a seam (`color_tests`).
- `Surface::blit` — composite one surface over another through `blend_span`,
  clipping a negative origin or an over-large source, so a
  transparent-background sprite (a rasterised cursor or icon) lays onto the
  destination without a rectangular halo.
- `Surface::overwrite` — the same walk, but each source pixel **replaces**
  the pixel it lands on. A snapshot is a copy, not a composite: the window
  manager retains the backdrop beneath a translucent or blurred window with
  this, and at a screenful a frame the difference between a row copy and
  reading and blending every pixel is worth having. Sharing one geometry walk
  with `blit` is what keeps the two clipping identically.
- `Surface::blit_desaturated` — the same walk with each source pixel pulled
  toward its own luminance first (`Pixel::desaturate`: BT.601 luma, `255`
  identity, `0` pure grey). One definition of saturation reduction, applied on
  the way in, so a caller that draws the same sprite hot and greyed — a window
  title bar's identity icon, focused and not — keeps one cached copy of it.
- `Surface::blit_faded` — the same walk with each source pixel weakened to a
  strength as it lands, so an opaque source mixes the destination toward it in
  exactly that proportion. One picture dissolving into another is this: the
  desktop paints the arriving wallpaper and lays the ground that was on screen
  over it at the inverse strength. Weakening on the way in leaves the source
  untouched, so neither end of a crossfade is copied to be faded, and the two
  ends (`0` and `255`) cost nothing.
- `Surface::fill_vertical_gradient` — a top-to-bottom colour ramp, one span
  fill per row. Interpolation is in *straight* alpha, so a ramp that fades out
  keeps its hue instead of being dragged toward black. The ramp is evaluated in
  the rectangle's own coordinates, so a clipped gradient shows the band the
  whole rectangle would have had rather than a re-scaled one. This is the
  crate's one *wash*, and washes round differently from every other paint — see
  below.
- `Surface::mask_to_round_rect` — confine content already painted on a surface
  to a rounded shape: everything outside is cleared and corner pixels are
  scaled by the shared coverage. This is what a *fill* cannot do — a rounded
  fill over the corners would leave an opaque frame, not a transparent one —
  so a control that draws its own square plate can be reshaped into a stadium
  after the fact (`lib/greeter`'s secret pill).
- `round_rect_coverage` — the single anti-aliased rounded-rectangle coverage
  definition, supersampled with a fast interior path so its own cost is the
  corner area, not the whole rectangle. The window manager's corner mask,
  `Surface::fill_round_rect`'s Reactive Alloy control plates, and the mask
  above all round through this one function, so they never drift apart
  (`AGENTS.md` §2.2).
- `round_rect_radius` — the radius that coverage actually rounds by: the
  requested one clamped to half the shorter side. A caller reasoning about
  *where* a shape's corners are — which rows carry an arc at all, as the
  compositor asks per window row — reads the clamp rather than restating it.
- `resample` / `resample_rows` — the single image resampler the whole desktop
  scales through: the icon pipeline fitting a bundle's artwork into a slot and
  the wallpaper pipeline placing a photograph onto a screen are the same
  arithmetic, so there is one implementation rather than one per consumer
  (`AGENTS.md` §2.2). Separable, integer, and filtered in **premultiplied**
  space so a transparent neighbour can never bleed its colour into an opaque
  pixel. See "The resampler" below.
- `Surface`'s `tairix_reclaim::CachedBytes` impl — the one measurement of a
  surface's retained heap size (its pixel buffer) and the one wipe that
  clears it to fully transparent black before release. The window manager's
  and the taskbar's rasterisation caches are `tairix_reclaim::ReclaimCache`
  instances (`plans/SMARTRAM.md` section 6.4) built from the shared desktop
  cache policy (`tairix_reclaim::desktop`); a `Surface` plugs into that
  bounded, pressure-governed cache through this one impl rather than each GUI
  crate measuring or wiping it separately (`AGENTS.md` §2.2). Rasterisation
  caching itself — budgeting, generation invalidation, pressure-forced
  shrinking, zero-on-release — lives entirely in `lib/reclaim`; this crate
  only supplies the rendered value the cache holds.

There is exactly one definition of the colour algebra here, so it is never
duplicated into a sibling crate (`AGENTS.md` §2.2). A theme `Rgba` token meets
that algebra at a single edge — `From<Rgba> for Color` — which is why this
crate depends on `lib/theme`: the conversion is owned in one place rather than
re-implemented by each consumer.

Every fill and `blit` writes a row at a time: the destination row's starting
index is computed once, then the rest of the row is read or written through
plain slice iteration rather than a per-pixel bounds check and index
recomputation. This keeps the cost of a fill or a blit proportional to the
shape it draws — a clipped rectangle, a polygon's bounding box, or the
overlap between source and destination — never the whole canvas.

`fill_round_rect` is the union of full-coverage spans and the four
`radius`×`radius` corner squares, because only a corner pixel can be partially
covered. An interior row, and the middle span of a corner row, take the same
whole-span path `fill_rect` uses; only a corner pixel evaluates
`round_rect_coverage`. A rounded panel therefore costs a rectangle fill plus
its corners — at a 12-pixel radius that is 576 corner pixels however large the
panel is — rather than a coverage evaluation per pixel.

A fully opaque source keeps none of the destination, so `Pixel::over` returns
it unchanged and a full-coverage opaque span is a single slice fill rather
than a per-pixel blend. A translucent source still takes the general
Porter–Duff path — as a *run* through `blend_span`, so the layer decision,
the coordinate conversion, and the bounds checks around the arithmetic are
paid once per run rather than once per pixel. Measurement is what settled
that shape: in the window manager's composite the arithmetic itself was
under half the cost of a blended pixel, and the dispatch around it the rest.

`row_span_mut` is the row-at-a-time write seam for a consumer that composites
through a mask of its own — `lib/font`'s glyph blitter scales a text colour by
a coverage bitmap through it — so such a consumer also pays one bounds check
and one index computation per row instead of per pixel. It returns the column
the span really starts at, so a caller pairing it with its own mask advances
that mask by whatever leading columns the clip withheld.

## Rounding, and why a translucent paint dithers

Every operator here rounds at a caller-chosen point: `div255_biased(value,
bias)` is the one divide, and `ROUND_NEAREST` (127) *is* nearest rounding,
because a quotient's fractional part is a multiple of 1/255 and can never fall
exactly half way. `div255`, `Pixel::over`, `Pixel::scale_alpha` and the mixer
are that same arithmetic at that same bias, so naming the rounding point added
no second definition and moved no existing pixel.

It matters because compositing into 8 bits *loses* levels: a source of alpha
`a` admits only `256 - a` of the 256 the destination held. Round every pixel
of a large translucent field the same way and a smoothly varying picture under
it resolves into flat plateaus with a hard step between them — banding. No
extra arithmetic precision fixes it; the levels to say it with are gone.

So a **translucent composite into a surface** rounds at a per-pixel bias from
the ordered (Bayer 8×8) `DitherRow`: a value between two levels lands on the
lower one in some pixels and the higher one in others, and the area mean
carries the fraction. A heavy wash over a 64-row ramp keeps 37 of its 64 tones
apart where a fixed rounding kept nine. That covers the gradient wash, a
translucent `fill_rect`/`fill_round_rect` plate and its anti-aliased arc, and
`frost_region`'s mix-back — one rule, no exceptions to remember. The
compositor (`userland/gui/wm`) reads the same `DitherRow` at each pixel's
screen position, so a window blended over the wallpaper rounds exactly as a
wash over it would.

Three properties make it safe to apply everywhere:

- The bias is a pure function of the pixel's **surface** coordinates, so frames
  are reproducible, two spans that meet cannot seam, and a rectangle
  recomposited on its own lands where the whole-surface pass would have put it.
- The tile's mean bias is exactly `ROUND_NEAREST`, so a dithered paint neither
  lightens nor darkens what it covers, and no pixel is ever more than one level
  from the undithered answer.
- A wash of the colour already underneath it is exactly the identity at every
  bias, so a flat backdrop gains no noise from a paint it cannot see.

An *opaque* source keeps none of the destination, so it stays a slice fill with
no dither work at all — which is also why the compositor's opaque-run copy path
is untouched.

## The resampler

`resample(src, region, w, h)` scales a rectangle of a straight-alpha RGBA8
image to a new size; `resample_rows` produces any contiguous run of
destination rows of that same result, so a caller that cannot hold (or cannot
transport) a whole destination at once builds it a band at a time. Bands are
computed from the source and the filter plan alone, never from a previous
band, so assembling them yields byte-for-byte what one call would have
produced.

Resampling is reconstruction followed by prefiltering, and which of the two
dominates is decided by the ratio between the extents — so the kernel is
chosen per axis by the direction that axis is going:

- **Reducing** — a destination sample is the exact area integral of the
  source over the footprint it covers, with *fractional* weights on the two
  partly-covered end samples. Aliasing-free at every ratio. The fractional
  ends are what matter: a filter averaging whole samples takes one source
  sample for some destination pixels and two for the next at a ratio of 1.4,
  and that alternation is visible as hard-edged, blocky texture across a
  photograph.
- **Enlarging (or 1:1)** — the destination samples the Catmull-Rom cubic
  through the source samples. Holding each source sample across the
  destination pixels it lands on — a sample-and-hold — reproduces the source
  grid as visible blocks, exactly the artefact a wallpaper drawn larger than
  its decoded source must not show. The cubic is interpolating, so at 1:1 its
  weights collapse to a single unit tap and the resample is an exact copy:
  no needless blur on the ratio callers hit most.

A plan whose two axes are both that identity is recognised as the copy it
is and answered by copying the region's rows. That is not a second filter:
it is the observation that premultiplying each channel by its alpha and
dividing it back out reproduces the byte it started from. The case is the
common one on a desktop, because a decode is asked for the size the
composition wants — a full-screen wallpaper decoded at screen size costs
1.2 ms this way against 24 ms filtered on the development host.

Weights are fixed-point and **normalised to sum to exactly one** per
destination sample, which keeps a flat region exactly flat — a rounding
residual spread across a large image would show as banding — and makes two
calls that should agree unable to drift the way floating point would.

Colour is filtered premultiplied and divided back out at the end, the only
correct way to filter an image with an alpha channel: a fully transparent
source pixel contributes its transparency but not its (meaningless) colour,
so artwork with transparent padding cannot drag that padding's colour into
its visible edge. The division rounds to nearest and happens *before* any
clipping, so the cubic's overshoot at an edge lands on the alpha where it
belongs rather than shifting the colour.

Cost is proportional to the source plus the destination, not to the ratio:
each axis is a small run of taps per destination sample, and the horizontal
pass is shared between the destination rows that read it. A plan holds each
tap's *resolved* source sample beside its weight — the edge clamp and the
index arithmetic are done once, when the plan is built, not per pixel — and
drops the tap columns that carry no weight for any destination sample, so a
plan sized for its worst sample does not charge every other sample for it.
A destination row's first contributing tap writes the accumulator and the
rest add to it, so a row never pays to clear a buffer it is about to
overwrite. Scratch memory is a fixed handful of destination-width rows
however extreme the ratio, so
reducing a 4K photograph to a thumbnail costs no more working memory than
reducing it to a screen. Every entry point is total — degenerate geometry, a
region outside its image, a mis-sized output buffer, and a band past the
destination are typed refusals, never a panic or a partial write.

## The clip window

`Surface::with_clip(x, y, w, h, paint)` confines every write `paint` makes to
that rectangle and restores the enclosing window when it returns. This is how a
view bounds what it draws to the area it owns: the file manager's icon grid
paints its tiles inside its own item area, so nothing a tile draws can mark the
chrome above it or the scrollbar gutter beside it — the container states the
bound once instead of every drawing routine trimming its own geometry to an
edge. A clipped rounded rectangle keeps the corner arcs of the whole shape, and
a clipped glyph keeps its own metrics, because only the *writes* are withheld —
never the geometry, so a shape that straddles the edge is drawn correctly rather
than distorted.

Two properties make it safe to hand a clipped surface to code that does not know
it is clipped:

- **A nested window can only narrow.** `with_clip` intersects with the window
  already in force, so a control handed a clipped surface cannot paint its way
  back out to the area its host withheld.
- **There is one enforcement point.** Every write — `set`, the fills, the polygon
  scan converter, `blit`, and an external mask blitter — reaches pixels through
  `row_span_mut`, which tests the window and the surface bounds together. No
  primitive can honour the clip while another forgets it.

The clip also *saves* work rather than costing it: the admitted rows and columns
are resolved once per call, outside the row loop, so a sprite or fill mostly
outside a narrow window costs only the sliver that survives it.

## Measuring it: `cargo xtask bench`

These primitives are per-pixel loops, so their cost is measured, never
guessed (`plans/FIX-DESKTOP-SPEEDUP.md` A.2):

```
cargo xtask bench                       # every family, the default budget
cargo xtask bench --filter blur         # one family
cargo xtask bench --iters 64 --rounds 9 # a longer, steadier budget
```

It reports **ns per pixel** and **ns per frame** for `Surface::blit` (opaque
and translucent sources), `fill_round_rect`, `box_blur` and `frost_region`
over several radii, `resample`, the scan-out channel encode, and the window
manager's whole-frame composite. The per-pixel figure divides by the pixels
the case genuinely touches — for `resample`, the *destination* pixels; for a
composited frame, the damage actually recomposited, which is why a 64×24
change behind a blurred window reports the several hundred thousand pixels
that change really costs.

The measurement is `lib/cpuops`'s existing bounded, median-of-rounds
`BenchHarness` with a host nanosecond clock injected through its
`CycleCounter` seam — there is no second timing loop. Quote numbers from a
`--release` build; a dev-profile figure is not evidence.

Wall-clock timings are load-dependent, so this is **not** a pass/fail gate and
no test asserts an elapsed time. CI may run it only as a smoke check that
every family still produces a number. The regression gates are the
deterministic work counters instead.

## Why it lives in `lib/`

Sibling userland GUI crates may not depend on one another (`AGENTS.md` §17.4),
so the rasteriser they both use belongs in `lib/*`. It depends only on
`lib/theme` (for the `From<Rgba>` edge), `lib/reclaim` (for the `CachedBytes`
trait `Surface` implements) and `lib/util` (for the bounded `no_std`
trigonometry and square root `Affine` and a radial gradient need), and is
depended on by the GUI crates, never the reverse — `Layer::Lib` in the §17.4
layering.

## Stability tier

`experimental` — the Stage 7 desktop rasterisation seam, consumed by the GUI
crates (`userland/gui/wm`, `userland/gui/taskbar`) and by the `lib/*` crates
that draw through it (text, cursors, icons, controls, the login screen). It is
`no_std` (with `alloc`).
No `unsafe`, and no `unwrap`/`expect`/`panic!` in production paths
(`AGENTS.md` §2.9).
