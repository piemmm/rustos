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

The **typed control-state vocabulary** (`state`) is the §5 model as composed
Rust: `ControlKind`/`ControlRole`, `ControlState` built from small typed fields
(`FocusState`/`PointerState`/`SelectionState`/`ValidationState`/`AuthorityState`/
`ActivityState`/`PressureState`/`RecoveryState`), the derived `ControlDisposition`
that keeps an authority denial distinct from a plain disabled control (spec §13),
and the window-furniture states (`WindowControlKind`/`WindowActivationState`/
`WindowSizeState`/`WindowFurnitureState`). Illegal states are unrepresentable and
`ProgressValue` clamps out-of-range input.

The **button family** (`button`) is the first drawn control family: `Button`,
`IconButton`, and `SplitButton`. Each resolves every colour, metric, and corner
radius from the active `tairix_theme::Theme` and `tairix_geometry::Scale`; rounds
its plate through the shared `tairix_raster::Surface::fill_round_rect` (the one
rounded-rect definition, also used by the window-manager compositor — never a
second rounding path); draws labels through `tairix_font` and icons/marks through
`tairix_icon`; and consumes the shared `tairix_input` pointer/keyboard vocabulary.
Every §11 state is composed from the typed model, dark/light and high-contrast are
theme data, and a control emits only a typed action — the owning service enforces
authority.

## Where it sits

`#![no_std]`. The `scroll` and `state` modules are pure logic with no
dependencies; the drawn controls (`button`) depend only on other `lib/*`
crates — `tairix-geometry`, `tairix-theme`, `tairix-raster`, `tairix-font`,
`tairix-icon`, and `tairix-input` — never on `kernel/*`, `drivers/*`, or
`userland/*`, so the crate stays a shared building block the desktop consumers
depend on and never the reverse. The owning viewport maps the computed
one-dimensional `ThumbSpan` onto a `tairix_geometry::Rect` for its orientation
at the edge.

## Staged work

The remaining Reactive Alloy control families (toggle/checkbox/radio,
slider/progress, fields, menus, tabs, collections, dialogs, notifications, and
the window-manager furniture) and the window-manager / taskbar consumers of the
drawn controls are staged in `.junie/gui-controls-work.md`.
