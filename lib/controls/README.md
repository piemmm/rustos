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

**The theme chooses the face, not the caller.** No control accepts a typeface.
A control names the job its text does (a `tairix_theme::TextRole`) and the
active theme answers with the family, size, and weight, converted to physical
pixels through the one shared DPI scale. An application cannot substitute a
face of its own, so a menu drawn inside a monospace terminal is still the
desktop's own menu. An application's *own* content — a document, a terminal
grid — is unaffected; the rule binds the shared controls.

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
track, reporting its live value while the interaction continues and a distinct
*settled* value when it ends — durable work belongs on the settle alone, because
a drag reports one value per pointer sample; a `Progress` is a read-only instrument trace (known %, working/
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
  It also carries `plate_rect` — the one rule that places a plate against a
  `PlatePlacement` — and `ChainModel`, the one *model* every menu the desktop
  renders is built as: a titled, parent-indexed list of `ChainRow`s, which a
  desktop surface builds in process and an application's wire declaration
  decodes into (`ChainModel::from_app_menu`). The model lives here rather than
  with the chain that renders it because its clients are not all in the process
  that owns the chain (`plans/NEW-MENUS.md` §1.6).
- `toolbar` — `Toolbar` composes `IconButton`/`SplitButton` tools in `u16`
  groups (raised strip, group dividers, active-tool accent seam), routing
  pointer/keyboard input to the tools it owns and emitting a typed
  `ToolbarAction`.
- `tabs` — `Tab`/`Tabs`, an equal-width strip whose selected tab carries a
  strong lower seam, loading a Heat Seam, and modified/error a shape-coded bead;
  it emits a typed `TabsAction`. Where the pointer rests and where the keyboard
  cursor is are two separate records — both lift a tab, only the keyboard's is
  ringed — so a host re-stating its keyboard focus, which a monitoring host does
  on every refresh, cannot erase a resting pointer's highlight.
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

A focused control shows **exactly one accent line, and it is the ring**, a
border inside the plate. The perimeter keeps its quiet resting rim — under the
pointer too, where a hover would otherwise lift it — because a ring with a
second accent edge around it reads as a doubled border. Focus is told from
hover by *where* the line sits, not by its colour, and the pointer still states
itself in the plate wash. A rim carrying a role or a disposition (a destructive
edge, a pending check) is that control's own statement and keeps it.

## Surface ground: opaque or floating chrome

The **ground** is seating's counterpart: whether the backgrounds drawn with a
theme cover what is behind them or let it through. `tairix_theme::SurfaceGround`
rides on the theme a surface is drawn with (`Theme::floating`, reported by
`Theme::ground`) rather than on each control, so everything drawn on one surface
agrees and none can be forgotten and left an opaque patch. `Opaque` (the
default) draws the palette's own colours; `Floating` — desktop chrome over a
backdrop the compositor blurs — keeps each background's colour role and takes
the palette's chrome alpha for its layer, so a floating surface preserves the
relationships the theme authored (a `Menu` still grounds in `surface_raised`, a
`Panel` in `surface`).

`ground_fill(theme, fill, layer)` is that one rule, and `ChromeLayer` the only
choice at a call site: `Ground` for the surface and anything reading as *part*
of it (a list row, a menu row, a scrollbar channel), so a resting row is exactly
its ground rather than a patch on it; `Plate`, a step more solid, for a control
raised on it (a button, a text field, a card), furniture on the glass rather
than a hole cut in it. Only backgrounds pass through it — a semantic mark (a
role fill, a menu's highlighted command, a pressure rail, a bead, a focus ring,
a control's own Signal Rim) stays solid, because it must read against whatever
wallpaper is behind it; a *surface's* rim is its edge rather than a mark on it,
so it takes the surface's own weight and reads as the same glass a step lighter
(a step darker on a light theme). And a background is **laid down**, never
composited: composited over
the pass beneath it a translucent fill would come back more opaque than the
theme authored and frost nothing, while an opaque colour covers either way —
the same byte wherever the shape fully covers a pixel, one rounding rather than
two on a corner arc. One translucent layer per surface follows: a floating
`Panel` draws no header band and states its header with the rail and title it
already has.

`paint_surface_plate` (rim ring, then the ground inside it, reporting the
interior) and `plate_border` (the desktop's one rim thickness) are public: the
taskbar is such a surface without being a control here, and it must not invent
a second edge recipe.

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

## A selected icon tile: the accent over a frosted backdrop

`collection::IconTile` wears no plate of its own, so a selection has to be
drawn behind the picture. What it blurs is the **backdrop**: the pixels the
tile covers — a window's surface, the desktop wallpaper — are frosted by the
scaled `selection_backdrop_blur` through `tairix_raster`'s one shared region
frost, the same call the compositor frosts a window's backdrop with, and the
theme's `selection_fill` — the accent at three tenths opacity — is then laid over them
with a **crisp** edge, rounded like every other control plate. Frost and fill
are confined to that one rounded shape, so nothing lands outside the tile's
bounds and no square edge shows around the rounded fill. Softening the *fill*
instead is what left the mark a smear with no shape of its own. Only a selected
tile pays for it.

The radius is deliberately short, and the tests bracket it from both sides
rather than pin it. A box blur of radius `r` averages `2r + 1` samples, so a
radius approaching the tile's own size averages its whole backdrop to a single
colour: the mark then reads as a smudge with an accent cast and the wallpaper
behind it is simply gone. One test renders over a one-pixel pattern and
requires the fine grain to collapse; its pair renders over a broad one and
requires the shapes to survive — measured across the *middle* of the tile,
because the frost stops at the tile's edge and replicates the pixel there, so
the outermost columns keep their own colour at any radius and would answer for
a backdrop that had been averaged away.

`with_selection_fade` draws the mark at a given strength, `0` to `u8::MAX`, so
an owner can cross-fade a selection between items over the theme's
`MotionInteraction::SelectionChange` duration. It scales the frost and the fill
together, so a backdrop never snaps into focus ahead of the colour leaving it.
The item being left is already unselected while its mark decays, and the item
arrived at is already selected while its mark grows, so the strength is the
owner's to state rather than the composed state's to infer. A host that does
not animate sets nothing and the mark follows the selection. Under a heavier
`Contrast` the mark does not fade at all — see below.

The fill is translucent, so the wallpaper or window surface still reads through
it and the name keeps the theme's ordinary foreground: near-white on-accent ink
over a light theme's pale-orange result would wash out, while the theme's own
foreground separates either way up. Under a heavier `Contrast` the tile fills
the crisp opaque accent panel and inverts its ink to `on_accent`, the instant
the item is selected rather than fading in: a translucent wash would trade away
the very contrast that policy exists to add, and a half-arrived plate under
inverted ink would too. Hover and press keep their own crisp washes in the
shared plate colours — never the accent — so a pointer can never imitate a
selection.

A **selected** tile draws neither the pointer wash nor the focus ring, whatever
strength its mark is currently at. Both are suppressed by the selection itself
rather than by the strength, because an outline that appeared for as long as a
mark took to arrive read as a border flickering on and off under the pointer.
The ring is what distinguishes a *focused* tile from a hovered one, so an
unselected tile still takes it.

The name wraps rather than being cut: as many whole lines as the band under the
picture holds, each centred, the last elided with the shared ellipsis when the
name runs past them; a band too short for one whole line draws nothing rather
than clipping a glyph. `IconTile::label_lines` reports that budget from the same
geometry the render lays out to, so an owner sizing its tiles asks the tile
instead of re-deriving its label layout — the pair to `icon_side` above.

`with_label_shadow` draws that name — the eliding ellipsis included — through
`lib/font`'s one shadowed draw, for a tile whose ground is a picture rather
than a colour the theme knows: a resting tile paints no plate, so the login
chooser's account names sit straight on the wallpaper. A tile that sets none
draws exactly the pixels it always did.

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
pointer coordinate, the press latch that remembers where a button-down landed
(a card's body press among them), a scrollbar's drag anchor. None of it reaches
`render`, so counting it as a difference would defeat the contract: a pointer
sample crossing no control would look like a change. Such a field is wrapped in
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

## Reporting what changed

Equality answers *whether* a surface changed; it cannot say *where*. Every
input call therefore takes a damage sink (`&mut tairix_geometry::Region`, from
`damage::sink()`) and pushes the rectangles it repainted into it, so a host
renders and presents those instead of the window. A pointer crossing a
control-rich window costs the control left and the control entered, not the
surface.

Two guarded writes are the whole rule, so nothing invents a third — and they are
public, because a host reports its own drawn changes through the same two:
`damage::set` writes one drawn field and reports the bounds it is drawn in when
the value actually changed, and `damage::move_mark` reports the two children a
mark moves between — the menu row a highlight leaves and the one it arrives on,
the hovered tab, the focused crumb, the focused header column, the sorted column,
a host's own keyboard focus — never the strip, popup, or window around them. The
mark is compared whole, so the same child marked differently (a sort caret
turning over) is still a changed child. A `RenderInvariant` field reports nothing,
exactly as it compares equal, which is why a motion sample inside one control is
free.

A control reports every drawn change it makes itself. Two kinds of change are
the *host's* to report, because only the host knows where it put the controls:

- **A value it commits back into a control.** A control never mutates its own
  committed value: it reports an action and the owner writes the value in
  (`Toggle::set_on`, `Slider::set_value`, `Radio::set_selected`, …). The owner
  holds that control's rectangle at exactly that moment, so it reports it. The
  value is drawn inside it, so nothing narrower is available and nothing wider
  is needed.
- **A mark of its own that moves between two controls.** Keyboard focus is the
  one every host has: each control's ring is a function of the host's own focus
  field, so `damage::move_mark` over that field reports the control the ring
  left and the control it arrives on. A focus that lands on the host's own
  chrome maps to `None`, and the chrome reports its own pixels.

The exception is a mark a *container* draws on one of its own children, whose
two rectangles only that container can name: `Breadcrumb::set_focus`,
`TableHeader::set_focus`, `TableHeader::set_sort`, `Tabs::set_current`,
`Tabs::set_selected` and `Menu::set_current` therefore take the layout the host
already renders and hit-tests with, and report.

A host that is *composing or rebuilding* a control has no layout to resolve a
child against and nothing to report against either, because it presents that
surface whole. It says so, rather than passing a rectangle it does not have:
`adopt_focus`, `adopt_sort`, `adopt_current` and `adopt_selected` adopt the mark
without reporting, and each shares the one admission rule with its reporting
sibling so a rebuild cannot admit a mark the interactive path would refuse.
Passing a fabricated rectangle, scale, or theme to the reporting form is the
alternative, and it is forbidden — a made-up theme is one read away from being
silently wrong.

Over-covering is safe, under-covering is not: a rectangle reported that did not
change costs one redundant repaint, while a change left unreported leaves a
stale pixel on screen. Where the two pull against each other — a disabled
control tracking a hover it does not draw — the report stands.

## Where it sits

`#![no_std]`. The `scroll` and `state` modules are pure logic with no
dependencies; the drawn controls (`button`, `selector`, `value`, `text`, `menu`,
`toolbar`, `tabs`, `combo`) depend only on other `lib/*`
crates — `tairix-geometry`, `tairix-theme`, `tairix-raster`, `tairix-font`,
`tairix-icon`, `tairix-input`, `tairix-util` (the shared secret erase a masked
text field discards its buffer through) and `tairix-abi` (the menu row id an
outcome names, and the bounded wire menu `ChainModel` decodes) — never on
`kernel/*`,
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
  current level over the shared `MeterValue` (`state`), or a `Trend`
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
- `collection::Card` — the grouped state-and-actions surface a master list of
  causes, faults, or jobs is made of, and the one control that reports *two*
  interactions: `CardAction::FooterActivated` for a completed click on one of
  its footer `Button`s, and `CardAction::Pressed` for a completed click on the
  card's own body, clear of every footer button. The footer sees each event
  first, so a click can never report both, and a master/detail screen selects
  the pressed card rather than needing a click target of its own. A press gives
  the card no wash: choosing a card is the owner marking it *selected*, so the
  body pointer position and press latch are hit-test state excluded from
  equality (above), and a card that is disabled or denied by authority reports
  nothing at all — the body press runs through the same fail-closed latch every
  clickable control shares. `footer_rects` is the one definition of where the
  footer buttons are, so a composer embedding a card reads the same rectangles
  the card hit-tests and paints.
- `collection::TableHeader` — sortable column titles over the same
  column-width model `TableRow` lays its cells out with, reporting the sort
  its owner commits rather than reordering anything itself. Header and row
  reserve the *same* fixed leading rail gutter and the *same* fixed trailing
  Signal Bead band, both sized from the surface alone, so a row that becomes
  denied or gains a recovery mark keeps every column exactly where the header
  names it — a bead paints inside a band that is always reserved.
- `collection::TableRow::cell_rects` — where a row's cells are laid out, in
  the coordinate space of the bounds it is asked about, derived from the very
  span `render` draws with. A composer placing its own content inside a column
  (a sparkline beside a number) reads the layout instead of re-deriving it, so
  the answer cannot drift from the paint.
- `collection::TableCell` — an optional leading `IconKind` naming what the
  cell's value *is*, taken the way `MetricTile` takes one, drawn on a fixed
  slot ahead of the text at every alignment and out of the text's own budget.
  A column too narrow to seat it omits it rather than overlapping the text,
  and it never moves a column boundary.
- `tabs::TabsOrientation` — a vertical orientation of the existing strip, so a
  sidebar of pages is the one selection control rather than a second one.

## Staged work

The Reactive Alloy control set is complete. The remaining work is the
window-manager / taskbar / app consumers adopting these shared controls.
