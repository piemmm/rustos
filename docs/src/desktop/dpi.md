# Variable DPI and UI scale

TAIRiX treats display density as a first-class, **settable** desktop property
(`AGENTS.md` §10). The same image must be comfortable on a low-DPI monitor and
on a high-DPI panel, and the user is free to pick the density that suits them.
The mechanism is one shared scale factor, `Scale`, in `lib/geometry`.

## Logical versus physical pixels

Every desktop length — theme corner radii and border thicknesses, font sizes,
taskbar extents, window chrome — is authored in *logical* pixels at a fixed
reference density, `tairix_geometry::REFERENCE_DPI` (96 DPI). A logical pixel
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
read it from the compositor, and an application is *told* its window's density
over the window channel (below) — so changing a monitor's DPI is transparent to
them and is never duplicated as a second source of truth (`AGENTS.md` §2.2).

## How an application learns the density

Picking the density is the desktop's job, not the application's: an app never
sets a scale. But it must *know* the one in force, or every length it draws is
a guess, and the compositor that owns the value lives in another process an app
cannot — and must not — reach into (`AGENTS.md` §17.3).

The window channel carries it. `tairix_abi::desktop::DesktopInfo` is the seat's
desktop as one record — the screen extent in physical pixels, the UI scale as a
percentage of the reference density, and the active appearance — and it reaches
an application two ways:

- `WindowRequest::QueryDesktop`, wrapped as `WindowClient::desktop`, is a
  read-only request an app issues **before** it creates its first window, so
  its opening frame is already the right size at the right density. It carries
  no capability: the reply describes the caller's own screen and theme, names
  no other principal's data, and grants no authority, so gating it would only
  force every application to guess (`AGENTS.md` §5.2 — the capability set stays
  small, and a descriptive fact is not a security boundary).
- `WindowEvent::DesktopChanged` is pushed to every live window whenever the
  session changes any of it, so a running app follows a density or appearance
  switch instead of sitting there at the state it opened with.

`tairix_window::Desktop` is the app-side holder: it resolves the reported
percentage into a `Scale` (refusing, never clamping, a percentage outside the
range `Scale` admits and keeping the last good value), reports the screen as a
`Rect`, and `Desktop::fit_window` caps a wanted window size to the screen so a
window can never open larger than the display it must appear on. Feeding it
every delivered event keeps it current, so no app repeats the bookkeeping.

The desktop drives a runtime switch through `DesktopShell::set_scale`, which
sets the output scale on the compositor and re-presents the taskbar at the new
density; the session then announces the change so every open window re-lays
itself out too.

The scale is *deliverable* but not yet *settable* by a user: nothing in the
tree sets an output scale other than 100%, so the plumbing is honest and
exercised at every layer while the settings surface that would change it is
still to come (`plans/DISPLAY.md`).

## The taskbar consumes the scale transparently

`TaskbarConfig` holds the bar's extents and thickness in logical pixels and
its screen dimensions in physical pixels (the real framebuffer).
`TaskbarConfig::scaled` converts the logical fields at a given `Scale`, and
`BarLayout::compute` applies it — scaling the extents and the theme corner
radius — while leaving the physical screen untouched. The `Taskbar` model
stores **no** scale of its own: `Taskbar::layout`, `hit_test`, and
`library_layout` take the density as a parameter, and the presenter supplies
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

`cargo test -p tairix-geometry` covers `Scale`: the 1:1 identity, logical→
physical scaling at several percentages, saturation rather than wrapping, the
fail-closed out-of-range rejection, and the DPI round-trip through the
reference density. `cargo test -p tairix-taskbar` covers the threading: 200%
doubling every extent and the corner radius, `Scale::ONE` reproducing the
unscaled layout exactly, and supplying a higher scale relaying the bar at the
new density. `cargo test -p tairix-wm` covers the compositor owning the output
scale (settable, idempotent, marking the screen dirty, and `window_scale` per
window) and the cursor controller re-rendering on a scale change. `cargo test
-p tairix-desktop-session` covers `DesktopShell::set_scale` driving the
compositor and re-laying the bar transparently, the session's own surfaces
(the lock, the confirmation prompt, the trusted picker) laying out differently
at a different density, and `desktop_info` reporting the compositor's real
screen, scale and appearance. `cargo test -p tairix-abi` covers the desktop
record's wire form, including its fail-closed refusal of a zero extent, a zero
scale, an unknown appearance, or a dirty reserved byte; `cargo test -p
tairix-window` covers the whole path — an app learning its desktop before it
owns a window, `fit_window` capping a window to the screen, a change reaching
every live window, adopting one exactly once, and refusing (rather than
clamping) a density outside the range `Scale` admits while the last good one
stands.
