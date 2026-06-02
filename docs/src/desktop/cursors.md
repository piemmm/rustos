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

## In the compositor

The window manager owns the active `CursorRegistry`. It resolves a
`CursorKind` to a `VectorCursor`, rasterises it at the display scale once, and
composites the resulting `CursorImage` as the top-most overlay so the hotspot
tracks the pointer. Moving the pointer marks the cursor's old and new
rectangles dirty, so only those pixels are recomposited (the same damage model
the window stack uses), and hiding the cursor restores the pixels beneath it.
