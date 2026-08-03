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

## Owner-supplied icon artwork

A `shell::TaskbarItem`, a `collection::Card` tile, a `collection::ListRow`, and
a `button::IconButton` each draw an icon whose artwork their owner may already
hold rasterised (a desktop icon cache, an app bundle's own icon). They share one
seam: `icon_side(bounds, scale, theme, font)` reports the exact pixel side the
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

Every control and composition in this crate compares equal exactly when the
two values **would draw the same pixels**. That is a deliberate contract, not
an accident of `#[derive(PartialEq)]`, and a consumer may rely on it: a host
that holds the composition it last drew can compare the one it is about to
draw against it and skip the render and the window present entirely when
they match. A comparison costs microseconds where a full composition render
plus a window repaint costs milliseconds, so this is the difference between
a desktop that stays responsive under a moving pointer and one that does not.

Controls also carry state that exists only for hit testing — the last raw
pointer coordinate, the press latch that remembers where a button-down
landed, a scrollbar's drag anchor. None of it reaches `render`, so counting
it as a difference would defeat the contract: a pointer sample crossing no
control would look like a change. Such a field is wrapped in
`RenderInvariant<T>` (see `src/paint.rs`), a transparent newtype whose own
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
`TableCell`/`Card`/`Panel`), the scrollbar renderer (`scrollbar` — the one
orientation-parameterized `ScrollBar` over the `scroll` engine), the
window-manager furniture (`window` — `WindowFrame`/`TitleBar`/`WindowControl`/
`ResizeGrabber`/`ScrollCorner`), the shell surfaces (`shell` —
`Notification`/`TaskbarItem`/`TraySignal`), and the decision surfaces
(`decision` — `Dialog`/`Tooltip`/`HelpTip`).

## Switchboard reference composition

The `switchboard` module assembles **Switchboard** (design spec §17) purely
from the shared controls above — the window furniture, a header band of `Meter`
instruments, a `Tabs` strip, `ListRow`/`Card`/`Panel`/`Button` content, and one
vertical `ScrollBar` over the `scroll` engine — with no application-painted
chrome and no second copy of any control's behaviour. `Switchboard::new` turns
a typed `SwitchboardModel`
(`TaskSummary`/`JobSummary`/`PressureCause`/`ActivitySummary`/`RecoveryItem`/
`ResourceSummary`/`ServiceSummary`/`SystemAction`) into controls, and
`select_section` opens the panel on whichever section the host's caller asked
for rather than steering it with synthetic input. Pressure cards carry a
per-action `ActionVerdict` (ready / disabled-by-state / denied-by-authority —
one mapping onto `ControlState`, shared with every other action button);
activity rows compose a flat header+member list with a `Menu`-based Group
popup on task rows and a `TextField`-based inline rename whose committed text
the host reads back through `submitted_activity_name`; and a horizontal
action focus (Left/Right, then Enter) makes every row button
keyboard-reachable in every section. A host sampling live state publishes
each new reading with `set_model`, which runs that same one derivation over
the new model while keeping the section, scroll offsets, and keyboard focus
the user chose — a scrolled list is never snatched back to the top by the
next sample — and drops the row selection, hover, any open popup, and any
half-finished press that named a row the refresh may have replaced (an
in-flight rename survives only while an activity with the same stable id
remains). The always-visible resource band draws one meter per
`ResourceSummary` — the same fact the Overview resource cards show — and takes
no input, so a press over it reaches nothing beneath it. Every interaction
returns a typed `SwitchboardAction` for the hosting service to authorise, and
the frame hit map (`furniture_at`) keeps the client viewport strictly separate
from the furniture. A denied action fails closed and renders distinctly from a
disabled one. It is the proof that no TAIRiX surface needs custom chrome.

## Staged work

The Reactive Alloy control set is complete (see
`.junie/gui-controls-work.md`). The remaining work is the window-manager /
taskbar / app consumers adopting these shared controls.
