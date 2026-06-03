# rustos-svg

Shared SVG image-decoding library for the RustOS desktop (`lib/svg`,
`AGENTS.md` §6 / §16.4 — `PLAN.md` Stage 7).

SVG is the canonical, scalable **source** format for every WM/desktop
graphical asset — cursors, icons, notification glyphs, window-chrome
artwork (`AGENTS.md` §10). This crate is the first-party decoder for that
SVG-first pipeline. It is one of the curated §16.4 image-decoding shared
libraries and, like the rest of the desktop's parsers, it is **rolled in
house** rather than pulled from an external crate (`AGENTS.md` §2.12), so the
trusted computing base does not grow for an asset format.

## What it produces

`decode(bytes) -> Result<SvgImage, SvgError>` turns an SVG byte string into an
`SvgImage`:

- a **square design grid** (`design()`), and
- an ordered stack of filled polygon **layers** (`layers()`, bottom layer
  first), each an `SvgLayer { fill, polygon }`, plus
- an optional pointer **hotspot** (`hotspot()`) for cursor assets.

That is exactly the vector form `lib/cursor`'s `VectorCursor` and `lib/icon`'s
`VectorIcon` already rasterise through `lib/raster`'s single supersampled
polygon path, so the pipeline converts an asset **once** into this fast-draw
form and never re-parses SVG on the hot compositing path (`AGENTS.md` §10,
§2.2). `rustos_cursor::decode_svg` and `rustos_icon::decode_svg` wrap this
decoder for their respective vector forms.

## Untrusted input

On-disk assets under `/System/Graphics` are untrusted (`AGENTS.md` §19.5).
`decode` is **total**: it never panics for any byte string, returns a precise
`SvgError` for anything outside the supported subset, and a caller fails
closed to its built-in fallback artwork rather than crashing the compositor
(`AGENTS.md` §2.9). The decoder has a `cargo xtask fuzz` harness
(`tests/fuzz_svg.rs`, §19.6).

## Supported subset

A flat `<svg>` document with a square `viewBox="0 0 D D"` (or equal
`width`/`height`), whose shapes are `<polygon>`, `<polyline>`, `<rect>`, or
`<path>` restricted to the straight-line commands `M`/`L`/`H`/`V`/`Z`
(absolute and relative). Fills are hex (`#rgb`/`#rrggbb` and their alpha
forms), a small set of named colours, or `none`, optionally scaled by
`fill-opacity`. Coordinates and the design grid are **integers**. Curves,
arcs, gradients, transforms, and a second sub-path are out of subset and fail
closed — richer artwork is built by stacking filled layers, never a second
rasterisation path (`AGENTS.md` §2.2).

## Layout

- `document` — `SvgImage`, `SvgLayer`, and the top-level `decode` entry point
  (with decode resource limits, `AGENTS.md` §2.9).
- `xml` — the fail-closed start-tag scanner.
- `path` — `points` / `rect` / `path d` geometry → a single polygon ring.
- `color` — the `fill` / `fill-opacity` colour subset → a `lib/raster` `Color`.
- `error` — the closed `SvgError` rejection set.

## Where it sits

Like `lib/geometry`, `lib/theme`, `lib/raster`, `lib/font`, `lib/cursor`, and
`lib/icon`, this crate lives in `lib/*` so the cursor and icon libraries
consume it without depending on the window manager (`AGENTS.md` §17.4). It is
`no_std`, `#![forbid(unsafe_code)]`, depends only on `lib/raster` for the
`Color` type, and owns no colour arithmetic or rasterisation of its own.

## Stability

Tier: `experimental`.
