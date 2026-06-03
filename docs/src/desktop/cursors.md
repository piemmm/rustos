# Pointer cursors

RustOS pointer cursors are **vectorised, colourful, scalable, and
replaceable** — richer than a one-bit fill mask (`AGENTS.md` §6 / §10,
`PLAN.md` Stage 7). They live in the shared `lib/cursor` crate
(`rustos-cursor`) so the window manager and the default apps use them without
depending on one another (`AGENTS.md` §17.4). The crate is `no_std`,
`#![forbid(unsafe_code)]`, and owns no colour arithmetic of its own.

## The vector representation

A cursor is a `VectorCursor`: an ordered stack of filled `Shape`s over a
square design grid, plus a hotspot. Each `Shape` is a single polygon ring (a
list of `Vertex`es) and a straight-alpha fill colour. Because the artwork is
geometry rather than a fixed bitmap:

- **Scaling** is exact: `VectorCursor::rasterise(scale_percent)` renders one
  pixel per design unit at `100`, doubles at `200`, halves at `50`. The
  `footprint` method reports the square pixel side without rendering.
- **Anti-aliasing** comes from supersampling — each output pixel is probed on a
  4×4 sub-pixel grid and the fraction inside a shape becomes its coverage.
- **Colour** is real: every layer carries its own colour and alpha and is
  composited through `lib/raster`'s single premultiplied-alpha path, so the
  cursor library duplicates no colour arithmetic (`AGENTS.md` §2.2).

Both the supersampling scan conversion and the blend live in one place —
`lib/raster`'s `Surface::fill_polygon`. The cursor library maps each `Shape`
onto its design grid and hands the polygon to that shared path rather than
carrying its own scan converter, so the desktop has exactly one polygon
rasteriser, shared with the icon library (`AGENTS.md` §2.2 / §10).

Rasterising yields a `CursorImage`: a `lib/raster` `Surface` (transparent
outside the artwork) plus the hotspot in that image's pixel coordinates.
Degenerate cursors and scales fail closed with `None` rather than panicking
(`AGENTS.md` §2.9).

## Cursor sets

A `CursorTheme` binds one `VectorCursor` to each `rustos_theme::CursorKind`
(`Arrow`, `Text`, `Pointer`, `Move`, `Busy`). The fields are fixed, so a
lookup can never miss (`AGENTS.md` §2.11). The built-in set
(`CursorTheme::builtin`) draws each cursor as a light body over a darker
outline so it stays legible on any background, and the busy cursor is a
genuine two-tone disc.

Because a `CursorTheme` is plain data, an entirely different look is just a
different theme. The `CursorRegistry` holds the available sets and the active
one, keyed by a `CursorSetId`. The built-in set is always present, so there is
always an active set to return; `register` and `set_active` fail closed on a
duplicate or unknown id rather than panicking (`AGENTS.md` §5.4 / §2.9).
Swapping the active set replaces the whole pointer look at runtime — no
window-manager change.

On-disk cursor sets follow the desktop's **SVG-first** asset rule
(`AGENTS.md` §10): a replaceable set under `/System/Graphics` is authored as
SVG and decoded — through the curated §16.4 image-decoding library (`lib/svg`)
in a §19.5 parser sandbox — into the in-memory `VectorCursor`/`CursorTheme`
form shown here. `rustos_cursor::decode_svg(bytes)` (built on
`rustos_svg::decode` and `VectorCursor::from_svg`) performs that conversion,
preserving the asset's `data-hotspot-x`/`data-hotspot-y` hotspot; a malformed
or out-of-subset asset fails closed, so the caller keeps the built-in cursor
rather than crashing (`AGENTS.md` §2.9). See
[SVG asset decoding](./svg-assets.md). The built-in set remains the
always-present fallback.

## In the compositor

The window manager owns the active `CursorRegistry`. It resolves a
`CursorKind` to a `VectorCursor`, rasterises it at the display scale once, and
composites the resulting `CursorImage` as the top-most overlay so the hotspot
tracks the pointer. Moving the pointer marks the cursor's old and new
rectangles dirty, so only those pixels are recomposited (the same damage model
the window stack uses), and hiding the cursor restores the pixels beneath it.

## Choosing the shape from interaction state

Which `CursorKind` to show is decided from what the user is doing, not hard
coded per window action. The window manager's `select` module
(`userland/gui/wm`) holds that policy:

- `desired_cursor(router, compositor)` is a pure function of state. An
  in-flight window move-grab outranks everything and yields `Move`; otherwise
  the pointer takes the **cursor hint** of the top-most window under it; over
  the desktop background it is the plain `Arrow`.
- Each window carries a `cursor_hint` (default `Arrow`) that its owner sets
  through `Compositor::set_window_cursor` — a text view advertises `Text`, a
  control `Pointer`, a working view `Busy`. Changing a hint is window state,
  not pixels, so it marks no damage; the displayed pointer updates the next
  time the policy runs.
- `CursorController` ties the policy to the artwork. It owns the active
  `CursorRegistry` and the desktop `Scale` and remembers the kind on screen.
  `refresh` runs the policy and, only when the chosen kind changes, rasterises
  the matching cursor at the current scale and installs it. A runtime cursor-set
  swap (`set_registry`) or DPI change (`set_scale`) re-renders the current kind
  in place. Rasterisation can fail for a degenerate cursor or scale; the
  controller then fails closed, leaving the current pointer untouched rather
  than blanking it (`AGENTS.md` §2.9).
- The controller rasterises each kind at most once per scale and cursor set: a
  `rustos-raster` `RasterCache` keyed by `CursorKind` within a
  `(scale, cursor-set)` epoch keeps the converted `CursorImage`, so toggling
  back to a previously-shown kind reuses its image and only a scale change or a
  set swap re-rasterises (the SVG-first "convert once, re-render only on a scale
  or theme change" rule, `AGENTS.md` §10). It is the same cache the taskbar uses
  for its notification glyphs — one mechanism, not one per asset kind
  (`AGENTS.md` §2.2). See [SVG asset decoding](./svg-assets.md).
