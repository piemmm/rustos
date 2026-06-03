# SVG asset decoding

WM/desktop graphical assets are **SVG-first** (`AGENTS.md` §10): every cursor,
icon, notification glyph, and piece of window-chrome artwork is authored once
as SVG so a single source stays crisp at any DPI / UI scale. The decoder that
turns those assets into the desktop's fast-draw vector form lives in the
shared `lib/svg` crate (`rustos-svg`), one of the curated §16.4
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

`rustos_svg::decode(bytes)` returns an `SvgImage`: a square design grid
(`design()`), an ordered stack of filled polygon layers (`layers()`, bottom
layer first — each an `SvgLayer { fill, polygon }`), and an optional pointer
hotspot (`hotspot()`). That is exactly the shape `lib/cursor`'s `VectorCursor`
and `lib/icon`'s `VectorIcon` already hold, so the conversion is a direct
field map and the asset still rasterises through `lib/raster`'s single
supersampled polygon path — there is no second rasterisation path (`AGENTS.md`
§2.2). The cursor and icon libraries expose the wrappers
`rustos_cursor::decode_svg` and `rustos_icon::decode_svg`.

## Caching the rasterised form

Rasterising the vector form is the expensive step, so it happens only when its
result can change. `lib/raster`'s `RasterCache` is the one shared mechanism
that enforces "convert once, re-render only on a scale or theme change"
(`AGENTS.md` §10). It is keyed by an asset identity within an *epoch* — a scale
paired with a theme identity:

- the window manager's `CursorController` caches each on-screen pointer
  `CursorKind` against the `(scale, cursor-set)` epoch, so re-showing a kind
  reuses its image and only a scale change or a cursor-set swap re-rasterises;
- the taskbar's `TaskbarRenderer` caches each notification glyph against the
  `(tint, pixel-size)` epoch, so the bar repaints its cheap regions every frame
  but rasterises a glyph only once per theme and scale.

A changed epoch discards every cached entry; a render that fails closed (a
degenerate asset or scale, §2.9) is not remembered, so the asset is retried
rather than a failure being cached. Both consumers share this single cache
rather than each growing its own (`AGENTS.md` §2.2 / §6).

## Untrusted input

On-disk assets are untrusted (`AGENTS.md` §19.5), so the decoder runs inside a
minimum-capability parser sandbox and is **total**: `decode` never panics for
any byte string, returns a precise `SvgError` for anything outside the
supported subset, and the caller fails closed to its built-in fallback artwork
(a built-in cursor or `builtin_icon` glyph) rather than crashing the
compositor (`AGENTS.md` §2.9). The decode path has a `cargo xtask fuzz`
harness (§19.6).

## Supported subset

The decoder handles the flat, straight-line subset that maps cleanly onto
stacked filled polygons:

- **Document**: one `<svg>` root with a square `viewBox="0 0 D D"` (or equal
  `width`/`height`). A non-square or non-zero-origin box is rejected.
- **Shapes**: `<polygon>`, `<polyline>`, `<rect>`, and `<path>` whose `d` uses
  only the straight-line commands `M`/`L`/`H`/`V`/`Z` (absolute and relative).
- **Fills**: hex (`#rgb`, `#rrggbb`, and their `#rgba`/`#rrggbbaa` alpha
  forms), a small named-colour set, or `none`, optionally scaled by
  `fill-opacity`.
- **Hotspot**: `data-hotspot-x` / `data-hotspot-y` on the `<svg>` element for
  cursor assets.
- **Coordinates** and the design grid are integers.

Curves, arcs, gradients, transforms, and a second sub-path are out of subset
and fail closed: richer artwork is built by **stacking filled layers**, never
a second rasterisation path (`AGENTS.md` §2.2). Pre-rasterised bitmap assets
may exist as a cache or fallback but are never the only path.
