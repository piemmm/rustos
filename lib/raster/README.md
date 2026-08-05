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
- `Surface::fill_polygon` — the single supersampled, anti-aliased
  filled-polygon scan converter. Vector artwork (pointer cursors in
  `lib/cursor`, status icons in `lib/icon`) is authored on a design grid and
  drawn through this one path, so the desktop has exactly one polygon
  rasteriser rather than a copy per asset kind (`AGENTS.md` §2.2 / §10). Only
  the polygon's bounding box, clipped to the surface, is scanned, so a small
  shape on a large canvas costs its own area rather than the whole surface.
- `Surface::fill_polygon_subpixel` — that same scan converter over vertices
  already in *device* sub-pixel units (`SUBPIXEL` per pixel) instead of a
  design grid stretched across the surface. This is how chrome that must stay
  sharp at a small pixel size is drawn: a caller grid-fits its shape to whole
  pixels and every axis-aligned edge then lands exactly on a pixel boundary,
  producing no anti-aliased fringe at all, while a diagonal keeps sub-pixel
  placement and stays smooth. The shape is *placed*, not stretched, so a glyph
  needs no square scratch surface and blit to position it.
- `Surface::stroke_polyline` — the one stroked-line path the desktop shares: a
  window-furniture diagonal and a history graph's trace are the same primitive
  at different scales. Each segment is offset along *its own* perpendicular, so
  every segment keeps its full width whatever its slope, and consecutive
  segments overlap at the vertex they share — which is what joins them, with no
  seam and no darkened joint, since compositing an opaque source twice yields
  the same pixel.
- `Surface::blit` — composite one surface over another through the `over`
  path, clipping a negative origin or an over-large source, so a
  transparent-background sprite (a rasterised cursor or icon) lays onto the
  destination without a rectangular halo.
- `round_rect_coverage` — the single anti-aliased rounded-rectangle coverage
  definition, supersampled with a fast interior path so its own cost is the
  corner area, not the whole rectangle. The window manager's corner mask and
  `Surface::fill_round_rect`'s Reactive Alloy control plates both round
  through this one function, so the two never drift apart (`AGENTS.md` §2.2).
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
Porter–Duff path.

`row_span_mut` is the row-at-a-time write seam for a consumer that composites
through a mask of its own — `lib/font`'s glyph blitter scales a text colour by
a coverage bitmap through it — so such a consumer also pays one bounds check
and one index computation per row instead of per pixel. It returns the column
the span really starts at, so a caller pairing it with its own mask advances
that mask by whatever leading columns the clip withheld.

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
pass is shared between the destination rows that read it. Scratch memory is a
fixed handful of destination-width rows however extreme the ratio, so
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

## Why it lives in `lib/`

Sibling userland GUI crates may not depend on one another (`AGENTS.md` §17.4),
so the rasteriser they both use belongs in `lib/*`. It depends only on
`lib/theme` (for the `From<Rgba>` edge) and `lib/reclaim` (for the
`CachedBytes` trait `Surface` implements) and is depended on by the GUI
crates, never the reverse — `Layer::Lib` in the §17.4 layering.

## Stability tier

`experimental` — the Stage 7 desktop rasterisation seam, consumed by
`userland/gui/wm` and `userland/gui/taskbar`. It is `no_std` (with `alloc`).
No `unsafe`, and no `unwrap`/`expect`/`panic!` in production paths
(`AGENTS.md` §2.9).
