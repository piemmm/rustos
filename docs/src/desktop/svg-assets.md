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

`tairix_svg::decode(bytes)` returns an `SvgImage`: a square design grid
(`design()`), an ordered stack of filled layers (`layers()`, bottom layer
first — each an `SvgLayer { paint, rule, contours }`), and an optional pointer
hotspot (`hotspot()`). A layer is several contours under one fill rule rather
than a single ring, because a path with a hole and any stroke outline at all
are both many rings filled as one. That is exactly the shape `lib/cursor`'s
`VectorCursor` and `lib/icon`'s `VectorIcon` hold, so the conversion is a
direct field map and the asset still rasterises through `lib/raster`'s single
scan converter — there is no second rasterisation path (`AGENTS.md` §2.2).
The cursor and icon libraries expose the wrappers
`tairix_cursor::decode_svg` and `tairix_icon::decode_svg`.

Every asset is fitted to the **same** square design grid whatever its own
`viewBox` says, honouring `preserveAspectRatio`, so a drawing that is not
square is letter-boxed into the square slot rather than stretched, and a
consumer never rescales between assets. An app-bundle icon master is still
required to be authored square (`SvgImage::source_extent()` is what the image
build checks): letter-boxing is right for artwork in general, but an icon
with bars down two sides is not an icon.

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

- `DesktopSession::load_cursors` reads the asset named by the active theme's
  `CursorSet` for each cursor kind from
  `/System/Graphics/Cursors/<asset-id>.svg` and returns a `CursorTheme`.
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

- the window manager registers a `CursorTheme::from_assets` result through the
  existing `CursorRegistry`, so the `CursorController` picks it up unchanged;
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

On-disk assets are untrusted (`AGENTS.md` §19.5), so the decoder runs inside a
minimum-capability parser sandbox and is **total**: `decode` never panics for
any byte string, returns a precise `SvgError` for anything it cannot draw,
and the caller fails closed to its built-in fallback artwork
(a built-in cursor or `builtin_icon` glyph) rather than crashing the
compositor (`AGENTS.md` §2.9). The decode path has a `cargo xtask fuzz`
harness (§19.6).

## What an author may draw

The drawable part of SVG 1.1, in full — a designer's own file is shipped as
authored rather than traced into a simpler form:

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
- **Style**: presentation attributes, the `style` attribute, and inheritance
  down the tree, including `currentColor`.
- **Colour**: every hex form, `rgb()`/`rgba()`/`hsl()`/`hsla()` in both
  spellings, and the CSS named colours.
- **Gradients**: linear and radial, with units, spread, `gradientTransform`,
  and `href` inheritance between definitions.
- **Hotspot**: `data-hotspot-x` / `data-hotspot-y` on the `<svg>` element for
  cursor assets.

What it does **not** draw, because an artwork decoder is not a browser: text,
embedded images, filters, masks, clipping paths, patterns, animation, and CSS
stylesheets. An element it cannot draw is skipped rather than refusing the
document, so one unsupported decoration does not lose a whole asset; the open
question about that choice is recorded in `plans/ICONS.md`. There is still
exactly one rasterisation path (`AGENTS.md` §2.2), and pre-rasterised bitmap
assets may exist as a cache or fallback but are never the only path. The
staged design is `plans/SVG.md`.
