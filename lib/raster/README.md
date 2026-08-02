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
  `get`/`set`, whole-row `row_mut`, `fill`, and clipped
  `fill_rect`/`fill_round_rect`. It is the rendered content of a window for the
  compositor and the painted body of the taskbar.
- `Surface::fill_polygon` — the single supersampled, anti-aliased
  filled-polygon scan converter. Vector artwork (pointer cursors in
  `lib/cursor`, status icons in `lib/icon`) is authored on a design grid and
  drawn through this one path, so the desktop has exactly one polygon
  rasteriser rather than a copy per asset kind (`AGENTS.md` §2.2 / §10). Only
  the polygon's bounding box, clipped to the surface, is scanned, so a small
  shape on a large canvas costs its own area rather than the whole surface.
- `Surface::blit` — composite one surface over another through the `over`
  path, clipping a negative origin or an over-large source, so a
  transparent-background sprite (a rasterised cursor or icon) lays onto the
  destination without a rectangular halo.
- `round_rect_coverage` — the single anti-aliased rounded-rectangle coverage
  definition, supersampled with a fast interior path so its own cost is the
  corner area, not the whole rectangle. The window manager's corner mask and
  `Surface::fill_round_rect`'s Reactive Alloy control plates both round
  through this one function, so the two never drift apart (`AGENTS.md` §2.2).
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

`row_mut` is the row-at-a-time write seam for a consumer that composites
through a mask of its own — `lib/font`'s glyph blitter scales a text colour by
a coverage bitmap through it — so such a consumer also pays one bounds check
and one index computation per row instead of per pixel.

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
