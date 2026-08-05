# tairix-controls

Stability tier: **experimental**.

Shared **Reactive Alloy** GUI control behaviour for the TAIRiX desktop
(`lib/controls` — see `plans/GUI-CONTROLS-DESIGN.md`).

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

The **typed control-state vocabulary** (`state`) is the spec §5 model as composed
Rust: `ControlKind`/`ControlRole`, `ControlState` built from small typed fields
(`FocusState`/`PointerState`/`SelectionState`/`ValidationState`/`AuthorityState`/
`ActivityState`/`PressureState`/`RecoveryState`), the derived `ControlDisposition`
that keeps an authority denial distinct from a plain disabled control (spec §13),
the `PlateSeating` a control is drawn with (below), and the window-furniture
states (`WindowControlKind`/`WindowActivationState`/`WindowSizeState`/
`WindowFurnitureState`). Illegal states are unrepresentable and `ProgressValue`
clamps out-of-range input.

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
`TextAction`; read-only, disabled, and denied render distinctly. A
`TextField` additionally has a **masked (secret) mode** for credential entry —
`TextField::secret(max_len)` — described below; a `SearchField` has none,
since a query is not a credential.

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

## Plate seating: a panel or a bar

`PlateSeating` says where a control is seated, which decides whether it wears
chrome of its own. It is a property of the *surface behind the control*, never
of what the control is or what it is doing:

- `Panel` (the default) — the control always wears its Alloy Plate and Signal
  Rim, reading as a plate raised above the window or panel behind it.
- `Bar` — the control wears **no** rim in any state, and no plate at all while
  it has nothing of its own to state, so a run of icons reads as one continuous
  bar instead of a row of boxed buttons.

One state model, one renderer, and one resolved set of colours serve both
seatings; the whole consequence is a single shared rule
(`paint::FrameColors::face`), so no family can grow its own idea of a flat
control. Nothing is discarded, only moved off the edge: a hover raises the
shared pointer wash (`surface_hover`) and a press compresses it — the only
pointer feedback a rimless control has, which is why `tairix-theme` guarantees
the wash a visible step from the bar's fill — keyboard focus keeps the resting
fill and still draws its ring (a bare frame is by construction never a focused
one), and a denied/failed/pending/disabled control states itself on its glyph
tint and shape-coded bead rather than a coloured edge. Focus Field membership is
the one signal a bar-seated control cannot make, since membership is drawn only
as a rim lift; the icon strip has no such groups.

`IconButton` carries the choice (`IconButton::seated`) because it is the only
family that appears on both a window toolbar and the desktop's icon strip.
`shell::TaskbarItem` and `shell::TraySignal` exist only on the bar and are
bar-seated by construction; every other family is panel-seated.

## Owner-supplied icon artwork

A `shell::TaskbarItem`, a `collection::IconTile`, a `collection::ListRow`, and
a `button::IconButton` each draw an icon whose artwork their owner may already
hold rasterised (a desktop icon cache, an app bundle's own icon). They share one
seam: `icon_side(bounds, scale, theme, …)` reports the exact pixel side the
icon slot will be drawn into (`0` when the geometry leaves room for none), so the
owner can ask its cache for artwork at precisely that size; `render(…,
artwork: Option<&Surface>)` then blits that artwork centred in the slot, or
rasterises the control's built-in vector glyph when none is supplied. A
missing, refused, or undecodable asset therefore degrades to a meaningful icon
rather than a blank slot.

The "blit the artwork, else draw the built-in glyph" rule lives **once**, in
the private `paint` module (`paint_icon_slot`), so the four controls cannot
drift apart. Artwork that does not match the slot is centred on it rather than
pinned to a corner, and a control with no icon slot ignores the parameter. No
control decodes an image: artwork arrives already decoded and rasterised
through the desktop's sandboxed asset path, so a malformed file can only fail
to produce artwork.

## Masked text entry

`TextField::secret(max_len)` turns a field into a password/passphrase/PIN
entry, and `TextField::is_secret` reports it. Everything else about the field
is unchanged: the plate, rim, focus ring, validation rim, Authority Mark,
read-only and disabled rendering, high contrast, and reduced motion all behave
exactly as they do for a plain field, and every editing key, the pointer caret
placement, and drag-selection work identically. There is deliberately no way
to reveal the buffer through the control.

**It draws beads, not a repeated glyph.** A masked field paints one filled
round bead per `char` at a fixed advance — derived from the theme's selector
extent and the active `Scale`, never a hard-coded pixel size — through the same
shared circle primitive the Signal Bead uses. Beads rather than a repeated
character because the drawn run's width then depends only on the buffer's
*length* and never on which characters it holds, so the rendering cannot leak
anything about the secret through its width, and because no particular glyph
has to exist in the font. The caret sits between bead cells, the selection
highlight covers whole cells, and the pointer hit test divides the pointer
offset by the cell advance and resolves the resulting cell to a `char`
boundary, so a click can never land mid-scalar. An empty field still shows its
placeholder: a placeholder is not a secret.

**The buffer is reserved once, up front.** Secret mode is inseparable from its
bound, because the bound is what lets the editor reserve the worst case UTF-8
needs for `max_len` characters the moment the mode is set. A `String` that
grows moves its contents to a new allocation and releases the old block with
the characters typed so far still in it — a copy of the credential no later
erase can reach. Reserving up front means the buffer can never grow while it
fills, so there is only ever one copy to erase.

**Discarded bytes are erased.** Every path that drops buffer content —
replacing the text, overwriting a selection, clearing, truncating to the
bound, and the editor's `Drop` — overwrites the bytes it discards first. The
erase is the workspace's shared `tairix_util::secret::wipe`, not a plain
`fill(0)`: nothing reads those bytes back, so an ordinary store is dead by the
language's own rules and a release build may delete it outright, leaving the
plaintext in the released block. The erase applies in plain mode too — it is
cheap, harmless, and one editor is better than two. A `TextField`'s `Debug`
output redacts a secret buffer, printing its character count in place of its
content.

## Equality is render equivalence

Every control in this crate compares equal exactly when the two values
**would draw the same pixels**. That is a deliberate contract, not an accident
of `#[derive(PartialEq)]`, and a consumer may rely on it: a host that holds
the surface it last drew can compare the one it is about to draw against it
and skip the render and the window present entirely when they match. A
comparison costs microseconds where a full render plus a window repaint costs
milliseconds, so this is the difference between a desktop that stays
responsive under a moving pointer and one that does not.

Controls also carry state that exists only for hit testing — the last raw
pointer coordinate, the press latch that remembers where a button-down
landed, a scrollbar's drag anchor. None of it reaches `render`, so counting
it as a difference would defeat the contract: a pointer sample crossing no
control would look like a change. Such a field is wrapped in
`RenderInvariant<T>` (see `src/state.rs`), a transparent newtype whose own
`PartialEq` always compares equal. The exception therefore lives in the
*type of the excluded field*, so each struct keeps its `derive` and a field
added later is covered automatically — where a hand-written `PartialEq` per
struct could silently forget one.

Visible state stays in equality: a hover highlight, a pressed appearance, a
focus ring, a selection, and a scrollbar's `dragging`/`held` (which it draws)
all make two values unequal, because they make them look different. Every
exclusion carries a drift guard in the tests: two values differing *only* in
that field are rendered and their pixels compared byte for byte, so the
exclusion is proved rather than asserted.

## Where it sits

`#![no_std]`. The `scroll` and `state` modules are pure logic with no
dependencies; the drawn controls (`button`, `selector`, `value`, `text`, `menu`,
`toolbar`, `tabs`, `combo`) depend only on other `lib/*`
crates — `tairix-geometry`, `tairix-theme`, `tairix-raster`, `tairix-font`,
`tairix-icon`, `tairix-input`, and `tairix-util` (the shared secret erase a
masked text field discards its buffer through) — never on `kernel/*`,
`drivers/*`, or `userland/*`, so the crate stays a shared building block the
desktop consumers depend on and never the reverse. The owning viewport maps the
computed one-dimensional `ThumbSpan` onto a `tairix_geometry::Rect` for its
orientation at the edge.

The remaining drawn families are also complete: the value controls
(`value` — `Slider`/`Progress`), the text entries (`text` — `TextField`/
`SearchField`), the collection controls (`collection` — `ListRow`/`TableRow`/
`TableCell`/`TableHeader`/`IconTile`/`Card`/`Panel`), the scrollbar renderer
(`scrollbar` — the one orientation-parameterized `ScrollBar` over the `scroll`
engine), the window-manager furniture (`window` — `WindowFrame`/`TitleBar`/
`WindowControl`/`ResizeGrabber`/`ScrollCorner`), the shell surfaces (`shell` —
`Notification`/`TaskbarItem`/`TraySignal`), and the decision surfaces
(`decision` — `Dialog`/`Tooltip`/`HelpTip`).

An application's *screen* is not here. This crate holds only behaviour any
surface may reuse, so a composition that arranges these controls into one
particular window — the Switchboard screen (design spec §17), which lives in
`userland/gui/switchboard/src/view/` — belongs to the application that owns
it. The shared heavier-contrast test fixture is reachable from such a crate
through the `test-support` feature (`tairix_controls::testkit`), so an
application's own render tests exercise the same two contrast axes as the
controls without a second copy of the fixture.

## Reading a system out, and standing beside a list

A monitoring surface reports state without acting on it, and these families
are what it reports through — each one generic, so no application draws its
own readout:

- `metric` — `MetricTile`, one at-a-glance report of a resource: a quiet
  label, a large reading with a quieter unit, an optional detail line, and an
  optional `MetricInstrument` beneath it (`None`, a `Track` proportional to the
  current level over the same `MeterValue` a `Meter` reads, or a `Trend`
  `Chart` of its recent history — never two instruments for one number).
  `MetricLayout` picks the anatomy: `Stacked` puts the label above the reading
  for a tile with a column of its own, `Inline` puts the label leading and the
  reading trailing so a narrow stack of readings can be scanned down. A tile
  takes no input and reports nothing back. `StatusPill` is the compact capsule
  that names a state in a word, toned by its signal role, where a full tile
  would not fit.
- `record` — `FactList`, a column of key/value readouts with the values
  right-aligned on one another, where the value keeps its room and the label
  truncates first, so a narrow detail pane loses a word of description rather
  than a digit; and `Timeline`, a spine spanning only its first to its last
  mark, with shape-coded `EventMark`s and a stamp column sized to the widest
  stamp, so one kind of event reads differently from another without colour.
- `nav` — `Breadcrumb`, the location trail whose trailing crumb is where the
  reader is and is deliberately not activatable, eliding oldest-first through
  one activatable ellipsis so the current location is never the crumb dropped.
- `rail` — `ActionRail`, the vertical counterpart of `Toolbar`: a column of
  `Button` commands anchored beside content, so plate, role, disabled, and
  denied rendering are not restated per surface. A rail lights the Edge Wake
  down its own leading edge (`with_edge_wake`) while the content beside it is
  scrolled away from its start, so a still frame shows that the list moved
  under an anchored column rather than the column moving with the list.
- `collection::TableHeader` — sortable column titles over the same
  column-width model `TableRow` lays its cells out with, reporting the sort
  its owner commits rather than reordering anything itself.
- `tabs::TabsOrientation` — a vertical orientation of the existing strip, so a
  sidebar of pages is the one selection control rather than a second one.

## Staged work

The Reactive Alloy control set is complete. The remaining work is the
window-manager / taskbar / app consumers adopting these shared controls.
