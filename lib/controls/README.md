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

The **boolean-selector family** (`selector`) is the second drawn family:
`Toggle`, `Checkbox`, and `Radio`. Each is a labelled boolean control that
reads by *shape* as well as colour (a toggle's thumb slides to the active side
with an accent contact, a checkbox draws a filled square when checked and a
horizontal bar when mixed, a radio draws a centre bead when selected), so its
state is legible without hue. They share the button family's plate, colour,
bead, and interaction helpers (the private `paint` module — the one place the
§13 rim/bead recipe and the plate rounding live), resolve every visible
property from the active theme and scale, draw the shared overlay signals
(Pressure Rail, pending Heat Seam, Authority Mark bead) on top of the glyph,
and emit a typed `SelectorAction` — a denied selector keeps its value and
shows the lock bead rather than looking disabled.

The **value-control family** (`value`) is `Slider` and `Progress`: measured
controls whose value is a validated permille. A `Slider` drags/steps and commits
through its owner (`SliderAction`) with an optional cap marker and resource-tinted
track; a `Progress` is a read-only instrument trace (known %, working/
indeterminate segment that freezes under reduced motion, complete/failed). The
**text-entry family** (`text`) is `TextField` and `SearchField` over a pure
caret/selection `TextEditor` with clipped horizontal scroll, emitting a typed
`TextAction`; read-only, disabled, and denied render distinctly.

The **command surfaces** are the menu, toolbar, tab strip, and combo box:

- `menu` — `MenuItem` rows and the elevated `Menu` plate (icon column, shortcut
  or disabled-row reason, submenu chevron, destructive danger rail, §13 Signal
  Bead; current-row highlight distinct from a keyboard focus ring). The `Menu`
  owns Up/Down/Home/End/Right/Enter/Space/Escape and pointer hover/click, sizes
  itself (`preferred_width`/`preferred_height`), and emits a typed `MenuAction`.
- `toolbar` — `Toolbar` composes `IconButton`/`SplitButton` tools in `u16`
  groups (raised strip, group dividers, active-tool accent seam), routing
  pointer/keyboard input to the tools it owns and emitting a typed
  `ToolbarAction`.
- `tabs` — `Tab`/`Tabs`, an equal-width strip whose selected tab carries a
  strong lower seam, loading a Heat Seam, and modified/error a shape-coded bead;
  it emits a typed `TabsAction`.
- `combo` — `ComboBox` composes the text-field focus model and the `Menu` model
  (its popup *is* a `Menu`), opening/selecting/closing by pointer and keyboard
  and emitting a typed `ComboAction`.

The shared chevron and focus-ring/cell-outline primitives live once in the
private `paint` module (`ChevronDir`/`paint_chevron`, `draw_outline`), so no
family carries its own triangle or outline recipe.

## Where it sits

`#![no_std]`. The `scroll` and `state` modules are pure logic with no
dependencies; the drawn controls (`button`, `selector`, `value`, `text`, `menu`,
`toolbar`, `tabs`, `combo`) depend only on other `lib/*`
crates — `tairix-geometry`, `tairix-theme`, `tairix-raster`, `tairix-font`,
`tairix-icon`, and `tairix-input` — never on `kernel/*`, `drivers/*`, or
`userland/*`, so the crate stays a shared building block the desktop consumers
depend on and never the reverse. The owning viewport maps the computed
one-dimensional `ThumbSpan` onto a `tairix_geometry::Rect` for its orientation
at the edge.

The remaining drawn families are also complete: the value controls
(`value` — `Slider`/`Progress`), the text entries (`text` — `TextField`/
`SearchField`), the collection controls (`collection` — `ListRow`/`TableRow`/
`TableCell`/`Card`/`Panel`), the scrollbar renderer (`scrollbar` — the one
orientation-parameterized `ScrollBar` over the `scroll` engine), the
window-manager furniture (`window` — `WindowFrame`/`TitleBar`/`WindowControl`/
`ResizeGrabber`/`ScrollCorner`), the shell surfaces (`shell` —
`Notification`/`TaskbarItem`/`TraySignal`), and the decision surfaces
(`decision` — `Dialog`/`Tooltip`/`HelpTip`).

## Switchboard reference composition

The `switchboard` module assembles **Switchboard** (design spec §17) purely
from the shared controls above — the window furniture, a `Tabs` strip,
`ListRow`/`Card`/`Panel`/`Button` content, and one vertical `ScrollBar` over the
`scroll` engine — with no application-painted chrome and no second copy of any
control's behaviour. `Switchboard::new` turns a typed `SwitchboardModel`
(`TaskSummary`/`JobSummary`/`RecoveryItem`/`ResourceSummary`/`ServiceSummary`/
`SystemAction`) into controls; every interaction returns a typed
`SwitchboardAction` for the hosting service to authorise, and the frame hit map
(`furniture_at`) keeps the client viewport strictly separate from the
furniture. A denied action fails closed and renders distinctly from a disabled
one. It is the proof that no TAIRiX surface needs custom chrome.

## Staged work

The Reactive Alloy control set is complete (see
`.junie/gui-controls-work.md`). The remaining work is the window-manager /
taskbar / app consumers adopting these shared controls.
