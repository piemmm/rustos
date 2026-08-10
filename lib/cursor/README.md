# tairix-cursor

Shared pointer-cursor library for the TAIRiX desktop (`lib/cursor`, `AGENTS.md`
§6 / §10 — `PLAN.md` Stage 7).

Cursors here are **richer than a one-bit fill mask**: each is a small ordered
stack of filled, coloured polygons over a resolution-independent design grid,
so the same definition is

- **vectorised** — authored once as geometry, not a fixed bitmap;
- **scalable** — rasterised crisply at any size (`rasterise(scale_percent)`),
  each pixel taking the exact area the artwork covers of it and the shapes
  meeting without a pale seam;
- **colourful** — every layer carries a straight-alpha colour and blends
  through `lib/raster`'s single premultiplied-alpha path (`AGENTS.md` §2.2);
- **replaceable** — a whole cursor set is plain data, swapped at runtime.

## Layout

- `vector` — `Vertex`, `Shape`, `VectorCursor`: the vector representation.
- `raster` — `VectorCursor::rasterise` → `CursorImage` (a `lib/raster`
  `Surface` plus the hotspot in pixel coordinates).
- `placed` — `PlacedCursor`: a `CursorImage` put somewhere. It stores the
  image's top-left corner as the pointer minus the hotspot, reports its
  `bounds()` for damage, and samples per row (`local_row` / `sample_row` /
  `sample_local`) for a screen blending it over whatever is behind it. Every
  screen that shows a pointer places it through this, so "the hotspot lands
  on the pointer" has one definition (`AGENTS.md` §2.2).
- `theme` — `CursorTheme`: one `VectorCursor` per `tairix_theme::CursorKind`,
  plus the built-in default set (light body over dark outline, two-tone busy
  disc).
- `registry` — `CursorRegistry`: the available cursor sets and the active one,
  with fail-closed `register` / `set_active` (`AGENTS.md` §5.4 / §2.9).
- `svg` — `VectorCursor::from_svg` and `decode_svg(bytes)`: build a cursor
  (hotspot included) from a decoded `lib/svg` `SvgImage` (the SVG-first asset
  rule, `AGENTS.md` §10). A malformed or undecodable asset fails closed, so
  the caller keeps the built-in cursor rather than crashing (`AGENTS.md` §2.9).
- `load` — `CursorAssetSource` and `CursorTheme::from_assets(source)`: build a
  whole cursor *set* from on-disk SVG assets (one per `CursorKind`, served
  through the injected seam so the `/System/Graphics` read and its capability
  stay in userland, `AGENTS.md` §17.4 / §19.5). Total and fail-closed per
  kind: a kind whose asset is missing, malformed, or undecodable keeps its
  built-in cursor, so an empty source yields the built-in set and a partial
  set mixes loaded cursors with built-in fallbacks (`AGENTS.md` §2.9). The
  result is a `CursorTheme` registered through `CursorRegistry`, so the
  compositor is unchanged. `CURSOR_KINDS` is the closed kind list a loader
  iterates.

## Where it sits

Like `lib/geometry`, `lib/theme`, `lib/raster`, and `lib/font`, this crate
lives in `lib/*` so the window manager and the default apps consume it without
depending on one another (`AGENTS.md` §17.4). It is `no_std`,
`#![forbid(unsafe_code)]`, and owns no colour arithmetic of its own.

The window manager resolves a `CursorKind` to a `VectorCursor`, rasterises it
at the display scale, and composites the resulting `PlacedCursor` over the
desktop so the hotspot tracks the pointer. The graphical login screen
(`userland/session/greeter`) draws its pointer through the same `PlacedCursor`
without depending on the window manager — which is exactly why the placement
lives here and not in the compositor (`AGENTS.md` §17.3). Both sample it over
what is behind rather than painting it in, so neither has to rebuild what the
pointer passed over.

## Stability

Tier: `experimental`.
