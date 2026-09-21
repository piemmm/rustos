# SVG asset decoding

WM/desktop graphical assets are **SVG-first** (`AGENTS.md` §10): every cursor,
icon, notification glyph, and piece of window-chrome artwork is authored once
as SVG so a single source stays crisp at any DPI / UI scale. The decoder that
turns those assets into the desktop's fast-draw vector form lives in the
shared `lib/svg` crate (`tairix-svg`), one of the curated §16.4
image-decoding shared libraries. Like the rest of the desktop's parsers it is
a **first-party** implementation, not an external dependency (`AGENTS.md`
§2.12).

## Where SVG sits in the pipeline

SVG is never parsed or drawn on the hot compositing path. An asset is decoded
**once**, at load time, into an in-memory vector form the compositor blits,
and that form is cached and re-rendered only on a scale or theme change:

```
/System/Graphics/*.svg  ──decode──▶  SvgImage  ──▶  VectorCursor / VectorIcon
                                                       │
                                                rasterise(scale)
                                                       ▼
                                              lib/raster Surface  ──blit──▶  compositor
```

`tairix_svg::decode(bytes, viewport)` returns an `SvgImage`: a design grid
(`design()`), the artwork drawn on it (`nodes()`, bottom first), and an
optional pointer hotspot (`hotspot()`). The artwork is `lib/raster`'s shared
`artwork` tree: filled `Layer { paint, rule, contours }` nodes, and a `Group`
wherever a clip, a mask, or a group opacity composites a subtree as a unit. A
layer is several contours under one fill rule rather than a single ring,
because a path with a hole and any stroke outline at all are both many rings
filled as one. That is exactly the shape `lib/cursor`'s `VectorCursor` and
`lib/icon`'s `VectorIcon` hold, so the conversion is a direct field map and
the asset still rasterises through `lib/raster`'s single scan converter
(`Surface::draw_artwork`) — there is no second rasterisation path
(`AGENTS.md` §2.2). The cursor and icon libraries expose the wrappers
`tairix_cursor::decode_svg` and `tairix_icon::decode_svg`.

`viewport` chooses the shape the drawing is fitted to, and is the only thing
it chooses: a document is well formed or it is not, and that cannot depend on
who is asking.

`Viewport::Square` is the desktop's asset form. Every asset is fitted to the
**same** square design grid whatever its own `viewBox` says, honouring
`preserveAspectRatio`, so a drawing that is not square is letter-boxed into
the square slot rather than stretched, and a consumer never rescales between
assets. An app-bundle icon master is still required to be authored square
(`SvgImage::source_extent()` is what the image build checks): letter-boxing is
right for artwork in general, but an icon with bars down two sides is not an
icon.

`Viewport::Natural` is what a *viewer* of a picture asks for, where there is
no slot to fill and the drawing's own shape is the answer. The drawing is
normalised across the whole grid — filling both axes — and `source_extent()`
carries the proportions it was authored in, so a consumer that rasterises into
a surface of that shape gets the picture undistorted, with the grid's full
precision on both axes and no letter-box bands to find and crop. That works
because `Surface::draw_artwork` stretches the grid across the surface it is
given: normalising in the decoder and un-normalising in the surface's own
shape is one uniform scale, so the scan converter needs no non-square grid of
its own.

Its production consumer is the parser sandbox's view service, which opens an
SVG document as the second backing behind the same open/page/render/band
protocol a raster document uses (`plans/VIEW.md`). A viewer zoomed in states
the extent the whole drawing is scaled to and the one rectangle of it the
window shows, and `Surface::layered_window` draws the artwork straight
into that rectangle: `source_extent()` rounded to pixels is what "actual
size" means for a picture that has none of its own, every zoom level is drawn
at full precision rather than resampled from one, and a magnification larger
than memory costs the window rather than the magnification. A drawing whose
own box is past `tairix_raster::MAX_DRAWING_EXTENT` is refused with a stated
reason rather than reported at a clamped size.

That bound is on a *coordinate*, so it is not by itself a bound on the buffer:
a window with both sides inside it still multiplies out to 2^40 pixels, and
`Vec::try_reserve_exact` was measured granting the whole 4 TiB on an
overcommitting host — leaving the fill to touch pages until the process is
killed, an outcome no `Option` can report. `tairix_raster::MAX_SURFACE_PIXELS`
bounds the total a single surface may hold, so such a request is a refusal
rather than a host-policy lottery. It sits at twice the pixels of the largest
display target, so every legitimate full-screen buffer passes; like the extent
it is a containment bound on an absurd size and not a memory budget
(`AGENTS.md` §24.4) — a machine short of RAM still refuses a far smaller
surface through the allocator.

Filling both axes does mean a curve is flattened to the tolerance of the
larger scale, so a drawing already close to the total-vertex bound can pass it
under `Natural` and be admitted under `Square`. The bound is a containment
bound and is not relaxed to suit a shape (`AGENTS.md` §24.4); the refusal is
the bound doing its job on the geometry actually produced.

## Compositing: one mechanism for three features

Group opacity, clipping, and masking each ask for a subtree to be drawn *as a
unit* and then composited through a per-pixel factor, so they are one
mechanism rather than three. A clip is a mask whose content is the clip's
shapes filled opaque white and read as alpha; a `<mask>` is the same group
read as luminance; a group opacity is that group with no mask at all. The
mask's own region rectangle, a clip on a `<clipPath>`, and a mask on a
`<mask>` all fall out of the model rather than being special cases.

Isolation is not cosmetic. Weakening each shape and compositing is a
different picture from compositing and then weakening — two overlapping
opaque shapes at half opacity show the lower one through the upper only in
the first — so the subtree really is rendered into its own buffer. The
decoder therefore emits a group **only where one changes the picture**: a
plain `<g>` costs nothing, a shape that produces one layer folds its own
opacity into that layer's alpha, and a viewport or mask region that cuts
nothing off its content adds no group at all. A group whose buffer cannot be
allocated draws nothing and says so, so the caller falls back to the tier
below rather than showing half a composite (`AGENTS.md` §2.9).

A pattern's tile is the same kind of level: a buffer in flight, holding a
drawing of its own, so it is charged against the same nesting bound and
refused past it rather than descending into a cycle of patterns painting one
another. A fill's opacity weakens the *assembled* tile rather than each of
its layers, which is what a group does for a subtree — the same rule, reached
without a second buffer, because scaling a premultiplied tile is exactly
compositing that one layer at it.

## Loading a whole asset set

A cursor or icon *set* is one SVG asset per kind. Reading the bytes from
`/System/Graphics` needs a filesystem capability and is the userland desktop's
job, so the `no_std` libraries take the bytes through an injected seam — the
same pattern the default apps use for their VFS/shell channels — rather than
opening any path of their own (`AGENTS.md` §17.4 / §19.5):

- `lib/cursor`'s `load` module: a `CursorAssetSource` yields the SVG bytes for
  each `CursorKind`, and `CursorTheme::from_assets(source)` builds a complete
  set. The result is a `CursorTheme` registered through the existing
  `CursorRegistry`, so the compositor is unchanged.
- `lib/icon`'s `load` module: an `IconAssetSource` yields the SVG bytes for
  each `IconKind`, and `IconSet::from_assets(source)` builds the set;
  `IconSet::icon(kind, tint)` returns the loaded asset (keeping its own
  authored colours) or, for any kind it lacks, the tinted `builtin_icon` glyph.

Both loaders are **total and fail-closed per kind** (`AGENTS.md` §2.9): a kind
whose asset is missing, malformed, or undecodable keeps its built-in artwork
rather than leaving the set without a glyph for that kind. An empty source
therefore yields the built-in set, and a partly-broken set mixes loaded assets
with built-in fallbacks — a corrupt `/System/Graphics` can never blank the
pointer or a status icon. `CURSOR_KINDS` / `ICON_KINDS` are the closed kind
lists a loader iterates.

## Reading the bytes from `/System/Graphics`

The seam above takes asset *bytes*; reading them off disk needs a filesystem
capability, so it is the desktop session's job, not the `no_std` libraries'
(`AGENTS.md` §17.4 / §19.5). `userland/gui/session`'s `assets` module supplies
the userland side: a `SessionFileReader` (the session's one file-reading seam
— VFS-backed on a running system, an in-memory table in tests) reads one
asset per kind and the module assembles the set:

- `DesktopSession::load_cursors` reads, for one named cursor **set**, the
  asset the active theme's `CursorSet` gives each kind, from
  `/System/Graphics/Cursors/<set>/<asset-id>.svg`, and returns a
  `CursorTheme`. The session walks the store once at bring-up
  (`tairix_cursor::catalog_sets`) and loads every set it offers, so
  *activating* one later reads nothing — see [Pointer
  cursors](./cursors.md).
- `DesktopSession::load_icons` reads the asset named by each `IconKind`'s
  `asset_id()` (the inverse of `IconKind::for_asset`) from
  `/System/Graphics/Icons/<asset-id>.svg` and returns an `IconSet`.

The reader's only contract is "give me the bytes at this path, or an `Errno`".
A read error is treated exactly like a missing asset: that kind falls back to
its built-in artwork, so neither loader can fail. The bytes never reach the
compositing path raw — they are decoded into the cached vector form below.

## Feeding a loaded set into the desktop at runtime

A built-in set always exists, so the desktop is usable before any asset loads;
a loaded set is swapped in at runtime without rebuilding the consumer:

- the window manager registers every loaded `CursorTheme::from_assets` result
  through the existing `CursorRegistry` under its own set name, so the
  `CursorController` picks them up unchanged and activating one is
  `set_active_set`;
- the taskbar's `TaskbarRenderer::set_icons` installs an
  `IconSet::from_assets` result (the built-in `IconSet` is in use until then).
  Installing a set bumps an internal generation that is part of the glyph
  cache's epoch, so the next frame discards the previously rasterised glyphs
  and re-rasterises from the new set (`AGENTS.md` §2.2). A loaded glyph keeps
  its authored colours; a kind the assets omit keeps its tinted built-in
  glyph.

## Caching the rasterised form

Rasterising the vector form is the expensive step, so it happens only when its
result can change. `tairix_reclaim::ReclaimCache`, built by each consumer from
the shared `tairix_reclaim::desktop::disposable_ui_cache` policy
(`plans/SMARTRAM.md` SMART5), is the one shared mechanism that enforces
"convert once, re-render only on a scale or theme change" (`AGENTS.md` §10).
It is keyed by an asset identity within an *epoch* — a scale paired with a
theme identity:

- the window manager's `CursorController` caches each on-screen pointer
  `CursorKind` against the `(scale, cursor-set)` epoch, so re-showing a kind
  reuses its image and only a scale change or a cursor-set swap re-rasterises;
- the taskbar's `TaskbarRenderer` caches each notification glyph against the
  `(tint, pixel-size, set-generation)` epoch, so the bar repaints its cheap
  regions every frame but rasterises a glyph only once per theme, scale, and
  installed icon set;
- the window manager's `Compositor` caches each decorated window's rendered
  furniture strips against the `(scale, theme-generation)` epoch, so a frame
  is painted once and re-used until that window itself changes. It is the
  one consumer whose ceiling is a whole screenful rather than the small
  fraction a cursor or a glyph is allowed
  (`tairix_reclaim::desktop::screenful_ui_cache`), because no more
  furniture than fills the screen can be visible at once and everything
  above that belongs to a minimised or stacked-under window.

All three caches are owned by the seat they belong to, bounded by a budget
derived from the real framebuffer byte size rather than a hand-picked
constant, and shrink or drop under memory pressure exactly like the kernel's
own reclaimable caches (see [the reclaimable-memory
model](../architecture/memory.md)). A changed epoch discards every
cached entry; a render that fails closed (a degenerate asset or scale, §2.9)
is not remembered, so the asset is retried rather than a failure being
cached. Logout or seat revocation tears them all down, wiping every
retained entry — a cached glyph or a rendered title bar is user-visible data,
not disposable bytes. Every consumer builds from the same policy rather than
each growing its own cache (`AGENTS.md` §2.2 / §6).

A window's **content** pixels are governed by the same `PressureGauge` and the
same `tairix_reclaim::shrink_target` ordering but are deliberately *not* a
fourth cache: they are an app's own frame rather than a rasterised asset the
desktop can rebuild, and evicting a visible window's pixels is a visual defect
rather than a slowdown. They are released by a pressure-driven policy that
looks at what the user can currently see, and asked back through the window
protocol's redraw handshake — see [Releasable window
content](./wm.md#releasable-window-content).

## Untrusted input

The decoder is **total** whoever calls it: `decode` never panics for any byte
string, returns a precise `SvgError` for anything it cannot draw, and the
caller fails closed to its built-in fallback artwork (a built-in cursor or
`builtin_icon` glyph) rather than crashing the compositor (`AGENTS.md` §2.9).
The decode path has a `cargo xtask fuzz` harness (§19.6).

Where the *bytes* come from decides whether it additionally runs in a
minimum-capability parser sandbox (`AGENTS.md` §19.5):

- **In a sandbox** when a principal could have chosen the file: a wallpaper
  or a viewed document, which `WallpaperChoice::Image` or a file picker can
  name at any absolute path — and **every icon**, because the one artwork
  resolver serves the shipped class masters and each bundle's *own* icon
  alike, and a third-party bundle's is not ours. A crash there is contained
  and the sandbox replaced.
- **In the session** only where the file can be nothing but first-party
  shipped artwork reached by a name the store itself supplied: the cursor
  sets under `/System/Graphics/Cursors`, on the read-only, system-signed
  volume that nothing but the installer and the updater may write
  (`AGENTS.md` §16.2). The byte and complexity bounds still apply, so a
  corrupt shipped asset costs its own artwork and nothing else.

## What an author may draw

The shape, paint and compositing surface of SVG 1.1 — a designer's own file
is shipped as authored rather than traced into a simpler form. What is not
on this list yet is staged in `plans/SVG.md`, not declined:

- **Document**: one `<svg>` root with a `viewBox` (or a `width`/`height`
  pair), and inside it `<g>`, `<defs>`, `<symbol>`, `<use>`, `<switch>`, and
  nested `<svg>` viewports.
- **Shapes**: `<path>`, `<rect>` (with `rx`/`ry` rounded corners),
  `<circle>`, `<ellipse>`, `<line>`, `<polyline>`, `<polygon>`.
- **Paths**: the whole `d` grammar, including cubic and quadratic curves and
  elliptical arcs. Curves are flattened to a bounded error, so a large arc is
  subdivided more finely than a small one.
- **Transforms**: the whole `transform` grammar, and `viewBox` with
  `preserveAspectRatio`.
- **Strokes**: `stroke`, `stroke-width`, caps, joins, miter limit, and
  dashes. A stroke becomes its own filled layer, painted over the fill in
  SVG's own order.
- **Style**: the property cascade — presentation attributes, the document's
  own `<style>` sheets, the `style` attribute, and inheritance down the tree,
  including `currentColor`. The selector subset is type, class, id, universal
  and any compound of those, in a selector list, with the descendant and
  child combinators; specificity is CSS's `(id, class, type)` triple and
  `!important` wins. A rule outside the subset — an attribute selector, a
  pseudo-class, an at-rule — is *dropped*, exactly as an unknown property
  already is, because that is what the construct means: refusing the document
  would lose an asset over a `@media print` block it would never have drawn.
- **Colour**: every hex form, `rgb()`/`rgba()`/`hsl()`/`hsla()` in both
  spellings, and the CSS named colours.
- **Gradients**: linear and radial, with units, spread, `gradientTransform`,
  and `href` inheritance between definitions.
- **Patterns**: `<pattern>` as a paint server, with `patternUnits`,
  `patternContentUnits`, `patternTransform`, its own `viewBox` and
  `preserveAspectRatio`, and `href` inheritance of both attributes and
  content, and `overflow`. The tile is drawn at the resolution the drawing is
  being rasterised at and repeated across the shape, so a patterned fill is
  as sharp at any size as the rest of the artwork. A tile is confined to
  itself, which is the `overflow: hidden` a pattern is drawn under; content
  an author asked to spill into the neighbouring repeats is drawn by folding
  those neighbours back into the one period. That is exact rather than
  approximate — a pattern is periodic, so only finitely many replicas reach
  one period — and it leaves the repeat periodic, so the sampler and the
  per-pixel cost are a confined tile's. Overlapping replicas draw in raster
  order, and a spill reaching more than a period past its tile is refused as
  a budget overrun.
- **Compositing**: `clip-path` and `<clipPath>` (`clip-rule`,
  `clipPathUnits`, nesting), `mask` and `<mask>` (`maskUnits`,
  `maskContentUnits`, `mask-type`, the mask region), and group opacity.
- **Markers**: `<marker>` with `marker-start`, `marker-mid`, `marker-end`
  and the `marker` shorthand, on the four shapes whose vertices the author
  writes (`<path>`, `<line>`, `<polyline>`, `<polygon>`). An instance is
  placed at each of those vertices and turned by the path's *true* tangent
  there — a curve's control-point direction, not its first flattened chord,
  so a marker does not swing as the asset is rasterised larger — bisecting
  the two segments at a corner. `refX`/`refY`, `markerWidth`/`markerHeight`,
  `markerUnits`, `orient` (`auto`, `auto-start-reverse`, or an angle), the
  marker's own `viewBox`/`preserveAspectRatio`, and its `overflow` clip all
  collapse into one placement per instance. A marker takes its style from its
  own place in the document rather than from the shape that placed it, and a
  reference to a marker the document does not define simply draws nothing.
- **Non-scaling strokes**: `vector-effect="non-scaling-stroke"`, which
  spends the element's transform on the path and not on the pen. SVG
  calculates such a stroke in the *host* coordinate space, which the
  specification equates to the screen's; a decoded asset has no screen, so
  the document's own root user space stands in for it — the space its
  lengths are written in, and the one whose map to the device is a single
  uniform scale however the drawing is fitted. So the width, the dash
  lengths, and a marker measured in stroke widths all stop scaling together,
  a pen stays round under an anisotropic transform instead of drawing as an
  ellipse, and a nested `<svg>` or `<symbol>` slot's own scale is cancelled
  along with every other. What the effect does *not* cancel is the scale the
  asset itself is rasterised at, which is not knowable when it is decoded.
- **Paint order**: `paint-order`, a permutation of a shape's fill, stroke,
  and markers — so a stroke may go under its fill, or the markers under
  both.
- **Hotspot**: `data-hotspot-x` / `data-hotspot-y` on the `<svg>` element for
  cursor assets.

A reference to a `<clipPath>` or `<mask>` the document does not define means
the element is **not rendered**, rather than rendered unclipped: an empty
picture is an honest refusal where a wrong one is not (`AGENTS.md` §5.4). A
reference to a *paint server* the document does not define takes the fallback
colour written beside it, which is what a fallback is for; a paint server
that is defined but paints nothing — a gradient with no stops, a pattern with
no tile — is `none`, and takes no fallback.

- **Text**: `<text>` and `<tspan>`, with the `x`/`y`/`dx`/`dy`/`rotate`
  lists, `text-anchor`, white-space collapsing under `xml:space`,
  `letter-spacing`/`word-spacing`, `textLength`/`lengthAdjust`, and the
  `font-family`/`-size`/`-weight`/`-style`/`-stretch` properties. See
  [A glyph arrives as geometry](#a-glyph-arrives-as-geometry).

What it does **not yet** draw — staged as numbered items in `plans/SVG.md`,
not declined: the remaining text surface (the baselines, `text-decoration`,
`<textPath>`, bidi and shaping), embedded images
(`<image>`), filters, SMIL animation, the CSS surface the cascade still drops,
external references, the three remaining `vector-effect` values, and
scripting — which runs only where a consumer asks for one, in its own
sandbox, under a fixed budget, and which the desktop's own chrome and icon
paths never enable. `<foreignObject>` is deliberately undrawable, so a
`<switch>` takes its fallback sibling. An element it cannot yet draw is
skipped rather than refusing
the document, so one unsupported decoration does not lose a whole asset.
Text is the exception, and deliberately: lettering nothing can furnish
**refuses the document**, because absent lettering is a wrong picture rather
than a missing decoration, and a caller then falls back to the tier below
(the open question about the general case is recorded in `plans/ICONS.md`).
There is still exactly
one rasterisation path (`AGENTS.md` §2.2), and pre-rasterised bitmap assets
may exist as a cache or fallback but are never the only path.

## A glyph arrives as geometry

SVG text is filled, stroked, gradient-painted, clipped, masked and
transformed exactly as a `<path>` is, and a decoded drawing has no
resolution. So a glyph reaches the decoder as **contours in font units**,
joins the one geometry currency, and is flattened by the single step every
other curve takes — at the tolerance the *placement* resolves, so a letter
subdivides exactly as a `<path>` of the same shape at the same size. A
rasterised cell would fix a size at decode time and put text on a second
rasterisation path.

The decoder holds no font data and no authority to fetch any. It resolves
text through an injected provider (`tairix_svg::font::FontProvider`), and
what a caller supplies decides what it can draw:

- **`NoFonts`** — the compositor's own asset paths. A drawing with text is
  refused, and the caller shows its built-in glyph instead.
- **`tairix_font::ServiceFonts`** — a process that already talks to the OS
  font service. The desktop chrome loads its icons and cursors through this.
- **The sandbox's two-phase exchange** — a document a user opened. The
  parser sandbox holds no capabilities, so it cannot call the font service;
  it decodes once against placeholder geometry to record exactly which
  faces and scalars the document needs, the host fetches them, and it
  decodes again. That terminates in two rounds by construction, because
  glyph geometry cannot change which scalars a document holds.

A run of one style becomes **one layer** whose contours fill together under
the non-zero rule — the TrueType convention, so a counter is a contour wound
against the one enclosing it rather than a shape in its own right. A weight
or a posture the face cannot furnish is reported rather than substituted:
the synthetic bold is the crate's own stroker at the reported width, and the
synthetic oblique is one shear folded into the glyph's transform, so there
is no second thickening or slanting implementation anywhere.

Text is bounded like everything else here, and the bounds are fixed
containment bounds: glyphs per document, `<tspan>` nesting, the resolved
length of one `<text>`, the runs a document may emit, and — because a
provider may be a live service across an IPC boundary — the *requests* one
document may make of it. The outline points a glyph contributes are charged
against the same total-vertex budget a `<path>` spends, so lettering and
paths cannot outspend one another.
