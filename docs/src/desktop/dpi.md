# Variable DPI and UI scale

RustOS treats display density as a first-class, **settable** desktop property
(`AGENTS.md` §10). The same image must be comfortable on a low-DPI monitor and
on a high-DPI panel, and the user is free to pick the density that suits them.
The mechanism is one shared scale factor, `Scale`, in `lib/geometry`.

## Logical versus physical pixels

Every desktop length — theme corner radii and border thicknesses, font sizes,
taskbar extents, window chrome — is authored in *logical* pixels at a fixed
reference density, `rustos_geometry::REFERENCE_DPI` (96 DPI). A logical pixel
is one physical pixel only at that density. On a denser panel a logical pixel
maps to several physical pixels, so the UI keeps its physical size instead of
shrinking.

The conversion is `Scale`, the ratio of physical to logical pixels expressed
as a percentage:

- `Scale::ONE` is 1:1 (100%, the reference density).
- `Scale::from_percent(percent)` builds a scale from a percentage; it returns
  `None` outside `Scale::MIN_PERCENT..=Scale::MAX_PERCENT`, so an out-of-range
  scale is rejected at construction rather than producing a degenerate desktop
  (`AGENTS.md` §5.4 / §2.9).
- `Scale::from_dpi(dpi)` turns a target density into a scale relative to
  `REFERENCE_DPI` — picking 192 DPI yields a 200% scale — and `Scale::dpi`
  reports the density back for a settings UI.
- `Scale::scale_length(logical)` is the **single** logical→physical
  conversion. It widens through `u64` and saturates at `u32::MAX`, so an
  extreme length clamps rather than wrapping (`AGENTS.md` §2.9).

## One conversion, no duplication

There is exactly one place that turns a logical length into physical pixels:
`Scale::scale_length`. The window manager, the taskbar, the cursors, and the
apps all call it, so the scaling arithmetic is never re-implemented per
consumer (`AGENTS.md` §2.2). `Scale` lives in `lib/geometry` because scaling a
length is geometry, and that crate sits at the bottom of the §17.4 layering
where every GUI consumer can reach it.

## The taskbar consumes the scale

`TaskbarConfig` holds the bar's extents and thickness in logical pixels and
its screen dimensions in physical pixels (the real framebuffer).
`TaskbarConfig::scaled` converts the logical fields at a given `Scale`, and
`BarLayout::compute` applies it — scaling the extents and the theme corner
radius — while leaving the physical screen untouched. A `Taskbar` carries a
settable scale: `Taskbar::set_scale` relays the bar at a new density at
runtime without rebuilding its model, exactly as a theme switch does. At 200%
on a large screen every extent and the corner radius simply double; at
`Scale::ONE` the layout is identical to the unscaled one.

## Crisp cursors at any density

Pointer cursors are vector artwork (`lib/cursor`), not fixed bitmaps:
`VectorCursor::rasterise` renders the design grid at the active scale with
anti-aliasing, so the pointer is sharp at any DPI. Bitmap assets are never the
only path.

## Tests

`cargo test -p rustos-geometry` covers `Scale`: the 1:1 identity, logical→
physical scaling at several percentages, saturation rather than wrapping, the
fail-closed out-of-range rejection, and the DPI round-trip through the
reference density. `cargo test -p rustos-taskbar` covers the threading: 200%
doubling every extent and the corner radius, `Scale::ONE` reproducing the
unscaled layout exactly, and `Taskbar::set_scale` relaying the bar at the new
density.
