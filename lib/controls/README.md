# tairix-controls

Stability tier: **experimental**.

Shared **Reactive Alloy** GUI control behaviour for the TAIRiX desktop
(`lib/controls`, `AGENTS.md` §6 / §17.4 — `plans/GUI-CONTROLS-DESIGN.md`).

Reactive Alloy is TAIRiX's GUI control design language: controls are typed
Rust state resolved against the shared theme (`lib/theme`) and drawn through
the shared raster/compositor path (`lib/raster`), with no per-application copy
of a control's behaviour. This crate is the shared home for that behaviour. It
lives in `lib/*` because its consumers — the compositing window manager
(`userland/gui/wm`), the taskbar (`userland/gui/taskbar`), and the default
graphical apps — may not depend on one another, exactly as `lib/geometry` owns
the shared coordinate types and `lib/theme` owns the shared design tokens.

## What lives here today

The first module is the **scroll geometry engine** (`scroll`): the single,
orientation-independent definition of scrollbar behaviour the design language
requires be shared by the window-manager root viewport and by nested
application content, over one range validation, thumb math, and input model —
never separate vertical, horizontal, window-manager, and application recipes.

- `ScrollRange` — a validated content/viewport/offset triple in the viewport's
  logical scroll unit. Always normalised: the offset never exceeds
  `max_offset`, and a viewport that covers its content (or a degenerate
  zero-size viewport) pins the offset to zero. Fields are private so the
  invariant cannot be violated.
- `ScrollModel` — a range plus line-step and page-step distances; the single
  source of truth for the offset, moved by `line_*`/`page_*`/`scroll_by`/
  `scroll_to`/`to_start`/`to_end` and re-clamped by `resize`.
- `ScrollGeometry` — turns a range plus a physical track length and the theme's
  minimum thumb length into a `ThumbSpan` (proportional length, mapped
  position), classifies a track coordinate (`hit` → `TrackHit`), and maps a
  thumb position or a pointer drag (with a preserved pointer-to-thumb anchor)
  back to a clamped offset.
- `ScrollOrientation` — vertical/horizontal, a pure layout parameter; the math
  is identical on both axes.

The engine is pure integer arithmetic with no rendering. It works in `u128`
internally so no `u64` extent overflows, and every division is guarded by a
non-zero denominator, so no path panics. Invalid, overflowing, or stale range
data fails closed to a non-draggable, zero-offset scrollbar rather than
producing out-of-bounds geometry.

## Where it sits

`#![no_std]`, no dependencies: the bottom of the layering. The owning viewport
maps the computed one-dimensional `ThumbSpan` onto a `tairix_geometry::Rect`
for its orientation at the edge.

## Staged work

The broader Reactive Alloy surface (typed control-state vocabulary, theme
tokens, the button/toggle/field/window-furniture controls, and the window
manager / taskbar consumers of the scroll engine) is staged in
`.junie/gui-controls-work.md`.
