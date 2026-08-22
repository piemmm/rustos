# Pointer cursors

TAIRiX pointer cursors are **vectorised, colourful, scalable, and
replaceable** — richer than a one-bit fill mask (`AGENTS.md` §6 / §10,
`PLAN.md` Stage 7). They live in the shared `lib/cursor` crate
(`tairix-cursor`) so the window manager and the default apps use them without
depending on one another (`AGENTS.md` §17.4). The crate is `no_std`,
`#![forbid(unsafe_code)]`, and owns no colour arithmetic of its own.

## The vector representation

A cursor is a `VectorCursor`: an ordered stack of filled `Shape`s over a
square design grid, plus a hotspot. A shape is what it is painted with, which
points it encloses (its fill rule), and the contours that bound them — lists
of `Vertex`es. Because the artwork is geometry rather than a fixed bitmap:

- **Scaling** is exact: `VectorCursor::rasterise(scale_percent)` renders one
  pixel per design unit at `100`, doubles at `200`, halves at `50`. The
  `footprint` method reports the square pixel side without rendering.
- **Anti-aliasing** is exact: each output pixel takes the true fraction of its
  own area the shape covers, so an edge lands where the geometry puts it
  instead of on the nearest of a handful of sample points.
- **Colour** is real: every layer carries its own colour and alpha and is
  composited through `lib/raster`'s single premultiplied-alpha path, so the
  cursor library duplicates no colour arithmetic (`AGENTS.md` §2.2).
- **Shapes meet cleanly**: the stack is composed through
  `Surface::layered`, which paints a multi-shape cursor larger and averages it
  down, so a light body over a dark outline shows no pale seam where the two
  anti-aliased edges meet.

Both the scan conversion and the blend live in one place — `lib/raster`'s
`Surface::fill_polygon`. The cursor library maps each `Shape` onto its design
grid and hands the polygon to that shared path rather than carrying its own
scan converter, so the desktop has exactly one polygon rasteriser, shared with
the icon library (`AGENTS.md` §2.2 / §10).

Rasterising yields a `CursorImage`: a `lib/raster` `Surface` (transparent
outside the artwork) plus the hotspot in that image's pixel coordinates.
Degenerate cursors and scales fail closed with `None` rather than panicking
(`AGENTS.md` §2.9).

## Cursor sets

A `CursorTheme` binds one `VectorCursor` to each `tairix_theme::CursorKind`
(`Arrow`, `Text`, `Pointer`, `Move`, `Busy`, and the four resize double arrows
`ResizeHorizontal`, `ResizeVertical`, `ResizeDiagonalRising`,
`ResizeDiagonalFalling`). `tairix_theme::CURSOR_KINDS` is that closed
vocabulary as a table, so a loader, a cache, or a test iterates every kind
without restating the list. The fields are fixed and `CursorTheme::from_cursors`
asks for the artwork *by kind* rather than by argument position, so a set can
neither omit a cursor nor mis-order two (`AGENTS.md` §2.11). The built-in set
(`CursorTheme::builtin`) draws each cursor as a light body over a darker
outline so it stays legible on any background, and the busy cursor is a
genuine two-tone disc.

The four resize cursors are one arrow at four angles — a barbed head at either
end joined by a shaft, centred on the design grid with the hotspot at its
middle. Each is unchanged by a half turn about that hotspot, because a resize
edge can be dragged either way and a one-headed arrow would say otherwise; the
vertical arrow is the horizontal one transposed and the two diagonals are
mirror images, so a window's two corners get opposite slopes. The unit tests
assert all three relations on the rasterised coverage rather than trusting the
authored coordinate tables.

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
form shown here. `tairix_cursor::decode_svg(bytes)` (built on
`tairix_svg::decode` and `VectorCursor::from_svg`) performs that conversion,
preserving the asset's `data-hotspot-x`/`data-hotspot-y` hotspot; a malformed
or undecodable asset fails closed, so the caller keeps the built-in cursor
rather than crashing (`AGENTS.md` §2.9). See
[SVG asset decoding](./svg-assets.md). The built-in set remains the
always-present fallback.

## Placing one on screen

A `CursorImage` is artwork; where it goes is a `PlacedCursor`. It stores the
image's top-left corner as the pointer position minus the hotspot, so the
hotspot lands exactly on the pointer, and it answers the two questions a
screen has about a drawn cursor: `bounds()` — the rectangle it covers, for
damage — and how to get its pixels, sampled a row at a time (`local_row` /
`sample_row` / `sample_local`) by whatever is blending it over what lies
behind. Sampling is the only way in: a screen that painted the cursor into
what is behind it would have to rebuild those pixels before it could move.

This lives in `lib/cursor` rather than in the window manager because it has
two consumers that may not depend on one another (`AGENTS.md` §17.3 / §2.2):
the compositor, and the graphical login screen
(`userland/session/greeter`), which is a `userland/session/*` crate and so is
forbidden a `userland/gui/*` edge.

## In the compositor

The window manager owns the active `CursorRegistry`. It resolves a
`CursorKind` to a `VectorCursor`, rasterises it at the display scale once, and
composites the resulting `PlacedCursor` as the top-most overlay so the hotspot
tracks the pointer. Moving the pointer marks the cursor's old and new
rectangles dirty, so only those pixels are recomposited (the same damage model
the window stack uses), and hiding the cursor restores the pixels beneath it.

## On the login screen

The greeter has no compositor and no cursor sets on disk yet, so it takes the
built-in `Arrow`, rasterises it once at start-up for the active
`tairix_geometry::Scale`, and samples the `PlacedCursor` over the painted
surface as each frame is composed — the pointer is therefore always on top of
everything the authentication surface drew, and the surface itself never
holds it. That is what lets the login screen keep its rendered surface
between frames and rebuild it only when its own content changes, so a moving
mouse re-composes a cursor-sized patch of pixels that already exist instead
of repainting the screen. Motion damages the union of the cursor's old and
new rectangles clipped to the screen, so a mouse move never costs a
whole-screen present and never leaves a cursor painted where it no longer is.
An arrow that will not rasterise costs the *drawing* only: the pointer still
moves and still hit-tests, the event is logged, and the screen stays usable
(`AGENTS.md` §2.9).

## Choosing the shape from interaction state

Which `CursorKind` to show is decided from what the user is doing, not hard
coded per window action. The window manager's `select` module
(`userland/gui/wm`) holds that policy:

- `desired_cursor(at, router, compositor)` is a pure function of state. `at`
  is the pointer position, which the desktop's input seat owns: a router holds
  a position only for as long as it holds the pointer, and the shape has to be
  right wherever the pointer is — including over the desktop's own bar, which
  the window manager's router never holds. An
  in-flight grab outranks everything: a window move-grab yields `Move`, and a
  resize-grab keeps the double arrow of the edge it is dragging for the whole
  gesture — the pointer routinely runs past that edge, and re-deriving the
  shape from where it now is would flicker mid-drag. Otherwise a point on a
  decorated window's resize edge yields the double arrow of the axis that edge
  moves along (the two sides share the horizontal arrow, the two corners take
  opposite diagonals), so a grabbable edge announces itself before it is
  pressed. Otherwise the pointer takes the **cursor hint** of the top-most
  window under it; over the desktop background it is the plain `Arrow`.
  The resize zone is the frame's own hit map, so it reaches into the client's
  outermost pixels exactly as far as a press on them does — the pointer never
  changes shape somewhere a press would not start a resize, and an undecorated
  window has no resize edges to point at.
- Each window carries a `cursor_hint` (default `Arrow`) that its owner sets
  through `Compositor::set_window_cursor` — a text view advertises `Text`, a
  control `Pointer`, a working view `Busy`. Changing a hint is window state,
  not pixels, so it marks no damage; the displayed pointer updates the next
  time the policy runs.
- `CursorController` ties the policy to the artwork. It owns the active
  `CursorRegistry` and remembers the kind on screen and the density it was
  rasterised at, but it does **not** own the scale: the desktop density belongs
  to the output, so the controller reads it from `Compositor::scale` when it
  installs a cursor (`AGENTS.md` §10 / §2.2). `refresh(at, router, compositor)`
  runs the policy and
  re-renders only when the chosen kind, the active cursor set, **or** the output
  scale changed, installing the result in place — at `at`, the seat's pointer
  position, which is also where the hotspot is placed. A runtime cursor-set
  swap is `set_registry(registry, at, compositor)`, which needs no router at
  all; a DPI change is `Compositor::set_scale` followed by one
  `refresh`. Rasterisation can fail for a degenerate cursor or scale; the
  controller then fails closed, leaving the current pointer untouched rather
  than blanking it (`AGENTS.md` §2.9).
- The controller rasterises each kind at most once per scale and cursor set: a
  `tairix_reclaim::ReclaimCache` keyed by `CursorKind` within a
  `(scale, cursor-set)` epoch (`CursorEpoch`) keeps the converted
  `CursorImage`, so toggling back to a previously-shown kind reuses its image
  and only a scale change or a set swap re-rasterises (the SVG-first "convert
  once, re-render only on a scale or theme change" rule, `AGENTS.md` §10). The
  cache is built by `cursor_cache` from the shared
  `tairix_reclaim::desktop::disposable_ui_cache` policy: owned by the seat,
  bounded by a budget derived from the real framebuffer byte size, dropped
  under memory pressure, and wiped on release rather than left to linger in
  reusable heap. It is the same policy the taskbar uses for its notification
  glyphs — one mechanism, not one per asset kind (`AGENTS.md` §2.2). See
  [SVG asset decoding](./svg-assets.md).
