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

## The density belongs to the output, not the desktop

Display density is a property of a **monitor**, not of the desktop as a whole:
two monitors attached at once may run at different DPIs. The compositor owns
the framebuffer for the output it scans out to (`AGENTS.md` §10), so it also
owns that output's `Scale`. It is the single source of truth:

- `Compositor::scale` reads the output's density and `Compositor::set_scale`
  changes it, marking the whole screen dirty so every window re-rasterises at
  the new density on the next composite. Setting the scale already in effect
  returns `false` so the caller can skip the refresh.
- `Compositor::window_scale(id)` reports the density of the output a window is
  on. With one output it is `Compositor::scale`; a multi-monitor compositor
  returns the scale of whichever output the window currently sits on.

Nothing else stores a copy of the scale. The taskbar and the cursor controller
read it from the compositor, and apps read *their* window's density through
`window_scale` — so changing a monitor's DPI is transparent to them and is
never duplicated as a second source of truth (`AGENTS.md` §2.2).

## Apps stay out of it

Picking the density is the desktop's job, not the application's. An app never
sets a scale; at most it *reads* `Compositor::window_scale` for its own window
when it must size something in physical pixels (an accessibility or
pixel-exact concern). The desktop drives a runtime switch through
`DesktopShell::set_scale`, which sets the output scale on the compositor and
re-presents the taskbar at the new density.

## The taskbar consumes the scale transparently

`TaskbarConfig` holds the bar's extents and thickness in logical pixels and
its screen dimensions in physical pixels (the real framebuffer).
`TaskbarConfig::scaled` converts the logical fields at a given `Scale`, and
`BarLayout::compute` applies it — scaling the extents and the theme corner
radius — while leaving the physical screen untouched. The `Taskbar` model
stores **no** scale of its own: `Taskbar::layout`, `hit_test`, and
`menu_layout` take the density as a parameter, and the presenter supplies
`Compositor::scale` at present time. A runtime DPI change is therefore just a
re-present at the new density — no taskbar state to update and no restart. At
200% on a large screen every extent and the corner radius simply double; at
`Scale::ONE` the layout is identical to the unscaled one.

## Crisp cursors at any density

Pointer cursors are vector artwork (`lib/cursor`), not fixed bitmaps:
`VectorCursor::rasterise` renders the design grid at the active scale with
anti-aliasing, so the pointer is sharp at any DPI. Bitmap assets are never the
only path. The `CursorController` does not store a scale either — it reads
`Compositor::scale` when it rasterises, and `CursorController::refresh`
re-renders the pointer when the kind, the cursor set, **or** the output scale
changes. So a DPI switch is `Compositor::set_scale` followed by one `refresh`.

## Tests

`cargo test -p rustos-geometry` covers `Scale`: the 1:1 identity, logical→
physical scaling at several percentages, saturation rather than wrapping, the
fail-closed out-of-range rejection, and the DPI round-trip through the
reference density. `cargo test -p rustos-taskbar` covers the threading: 200%
doubling every extent and the corner radius, `Scale::ONE` reproducing the
unscaled layout exactly, and supplying a higher scale relaying the bar at the
new density. `cargo test -p rustos-wm` covers the compositor owning the output
scale (settable, idempotent, marking the screen dirty, and `window_scale` per
window) and the cursor controller re-rendering on a scale change. `cargo test
-p rustos-desktop-session` covers `DesktopShell::set_scale` driving the
compositor and re-laying the bar transparently.
