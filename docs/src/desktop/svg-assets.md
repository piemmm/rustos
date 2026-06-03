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
