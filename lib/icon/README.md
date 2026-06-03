# rustos-icon

Shared desktop-icon library for the RustOS desktop (`lib/icon`, `AGENTS.md`
§6 / §10 — `PLAN.md` Stage 7).

The status/notification icons here are **scalable vector artwork, not fixed
bitmaps**: each `VectorIcon` is a small ordered stack of filled polygon layers
over a resolution-independent design grid, so the same glyph is

- **vectorised** — authored once as geometry, not a fixed-resolution bitmap;
- **scalable** — rasterised crisply at any pixel size (`rasterise(side)`) with
  supersampled anti-aliasing;
- **themeable** — every glyph is a monochrome silhouette tinted by a single
  colour the caller supplies from the active theme, so re-theming is data, not
  new code (`AGENTS.md` §10);
- **single-path** — every layer blends through `lib/raster`'s one supersampled
  `Surface::fill_polygon` path; the icon library owns no scan converter of its
  own (`AGENTS.md` §2.2), exactly like `lib/cursor`.

## Layout

- `vector` — `IconLayer`, `VectorIcon`: the vector representation and
  `rasterise(side) -> Surface` / `draw_onto(&mut Surface)`.
- `glyph` — `IconKind` (the closed glyph set: network, volume, battery, bell,
  and a generic fallback), `IconKind::for_asset` (theme asset id → kind,
  falling back to `Generic`, `AGENTS.md` §2.9), and `builtin_icon`.
- `svg` — `VectorIcon::from_svg` and `decode_svg(bytes)`: build an icon from a
  decoded `lib/svg` `SvgImage` (the SVG-first asset rule, `AGENTS.md` §10). A
  malformed or out-of-subset asset fails closed, so the caller substitutes a
  `builtin_icon` glyph rather than crashing (`AGENTS.md` §2.9).
- `load` — `IconAssetSource` and `IconSet::from_assets(source)`: build a whole
  icon *set* from on-disk SVG assets (one per `IconKind`, served through the
  injected seam so the `/System/Graphics` read and its capability stay in
  userland, `AGENTS.md` §17.4 / §19.5). `IconSet::icon(kind, tint)` is total:
  a kind that loaded an authored SVG asset keeps its own colours, and a kind
  whose asset is missing, malformed, or out of subset falls back to the
  `builtin_icon` glyph tinted with `tint` (`AGENTS.md` §2.9). `ICON_KINDS` is
  the closed kind list a loader iterates.

## Where it sits

Like `lib/geometry`, `lib/theme`, `lib/raster`, `lib/font`, and `lib/cursor`,
this crate lives in `lib/*` so the taskbar consumes it without the taskbar and
the window manager depending on one another (`AGENTS.md` §17.4). It is
`no_std`, `#![forbid(unsafe_code)]`, and owns no colour arithmetic of its own.

The taskbar resolves a notification icon's asset id to an `IconKind`, builds a
`VectorIcon` in the theme's foreground colour, rasterises it to the
notification slot's pixel size, and composites it onto the bar.

## Stability

Tier: `experimental`.
