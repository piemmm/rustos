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
  `get`/`set`, `fill`, and clipped `fill_rect`. It is the rendered content of
  a window for the compositor and the painted body of the taskbar.
- `Surface::fill_polygon` — the single supersampled, anti-aliased
  filled-polygon scan converter. Vector artwork (pointer cursors in
  `lib/cursor`, status icons in `lib/icon`) is authored on a design grid and
  drawn through this one path, so the desktop has exactly one polygon
  rasteriser rather than a copy per asset kind (`AGENTS.md` §2.2 / §10).
- `Surface::blit` — composite one surface over another through the `over`
  path, clipping a negative origin or an over-large source, so a
  transparent-background sprite (a rasterised cursor or icon) lays onto the
  destination without a rectangular halo.
- `RasterCache` — the single scale/theme-keyed rasterisation cache. Desktop
  assets are authored as resolution-independent vector forms and rasterised
  into a `Surface` only when they can change; the SVG-first rule (`AGENTS.md`
  §10) requires each asset to be converted **once** at the active scale and
  re-rendered only on a scale or theme change, never on the hot compositing
  path. `RasterCache` is keyed by an asset identity within an *epoch* (a scale
  paired with a theme identity): a changed epoch discards every entry, a
  stable epoch reuses, and a failed render is not remembered (fail closed,
  §2.9). The window manager caches pointer cursors and the taskbar caches
  notification glyphs through this one mechanism rather than each growing its
  own (`AGENTS.md` §2.2 / §6).

There is exactly one definition of the colour algebra here, so it is never
duplicated into a sibling crate (`AGENTS.md` §2.2). A theme `Rgba` token meets
that algebra at a single edge — `From<Rgba> for Color` — which is why this
crate depends on `lib/theme`: the conversion is owned in one place rather than
re-implemented by each consumer.

This crate does **not** round corners: that is the window manager's single
anti-aliased rounded-corner path (`AGENTS.md` §2.2). The taskbar renders a
rectangular `Surface` and the window manager rounds it at composition time,
exactly as it rounds windows.

## Why it lives in `lib/`

Sibling userland GUI crates may not depend on one another (`AGENTS.md` §17.4),
so the rasteriser they both use belongs in `lib/*`. It depends only on
`lib/theme` (for the `From<Rgba>` edge) and is depended on by the GUI crates,
never the reverse — `Layer::Lib` in the §17.4 layering.

## Stability tier

`experimental` — the Stage 7 desktop rasterisation seam, consumed by
`userland/gui/wm` and `userland/gui/taskbar`. It is `no_std` (with `alloc`).
No `unsafe`, and no `unwrap`/`expect`/`panic!` in production paths
(`AGENTS.md` §2.9).
