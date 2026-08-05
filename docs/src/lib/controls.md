# `tairix-controls` — the shared Reactive Alloy control behaviour

Reactive Alloy is TAIRiX's GUI control design language
(`plans/GUI-CONTROLS-DESIGN.md`), and `lib/controls` is the single home for
its behaviour. A control is typed Rust state resolved against the shared
design tokens (`lib/theme`) and drawn through the shared rasteriser
(`lib/raster`); no application carries a second copy of a control's
behaviour. The crate lives in `lib/*` because its consumers — the compositing
window manager, the taskbar, and the graphical apps — may not depend on one
another.

## The families

| Module | Controls |
|---|---|
| `button` | `Button`, `IconButton`, `SplitButton` |
| `selector` | `Toggle`, `Checkbox`, `Radio` |
| `value` | `Slider`, `Progress` |
| `meter`, `chart` | `Meter`, `Chart` |
| `metric` | `MetricTile`, `StatusPill` |
| `record` | `FactList`, `Timeline` |
| `text` | `TextField`, `SearchField` |
| `menu`, `toolbar`, `tabs`, `combo` | `Menu`/`MenuItem`, `Toolbar`, `Tab`/`Tabs`, `ComboBox` |
| `nav`, `rail` | `Breadcrumb`, `ActionRail` |
| `collection` | `ListRow`, `TableRow`, `TableHeader`, `Card`, `Panel` |
| `scroll`, `scrollbar` | the geometry engine and the one `ScrollBar` over it |
| `window` | `WindowFrame`, `TitleBar`, `WindowControl`, `ResizeGrabber` |
| `shell` | `Notification`, `TaskbarItem`, `TraySignal` |
| `decision` | `Dialog`, `Tooltip`, `HelpTip` |

Every one of them resolves its colours, metrics, and corner radii from the
active `Theme` and `Scale` rather than a hard-coded pixel or hue, composes its
appearance from the typed `state` vocabulary, and emits a typed action for the
owning service to authorise. Two control values compare equal exactly when
they would draw the same pixels, so a host can skip a repaint by comparing
what it is about to draw against what it drew last.

### Reporting a reading, and standing beside a list

The families a monitoring surface is built from report state without acting on
it, or frame the content that does:

- `MetricTile` is one at-a-glance report of a resource: a quiet label, a large
  reading with a quieter unit, an optional detail line, and an optional
  `MetricInstrument` beneath it — nothing, a `Track` proportional to the
  current level (the same `MeterValue` a `Meter` reads, tinted by the tile's
  resource kind), or a `Trend` `Chart` of its recent history, never two
  instruments for one number. `MetricLayout` picks the anatomy: `Stacked` puts
  the label above the reading for a tile with a column of its own, `Inline`
  puts the label leading and the reading trailing so a narrow stack of
  readings can be scanned down. A tile takes no input and reports nothing.
- `StatusPill` is the compact capsule that names a state in a word, toned by
  its signal role, for a place a full tile would not fit.
- `FactList` is a column of key/value readouts with the values right-aligned
  on one another: the value keeps its room and the label truncates first, so a
  narrow detail pane loses a word of description rather than a digit.
- `Timeline` is a vertical spine spanning only its first to its last mark,
  with shape-coded `EventMark`s and a stamp column sized to the widest stamp,
  so a reader can tell one kind of event from another without colour.
- `Breadcrumb` is the location trail: its trailing crumb is where the reader
  is and is deliberately not activatable, and a trail too long for its bounds
  elides oldest-first through one activatable ellipsis, so the current
  location is never the crumb that gets dropped.
- `ActionRail` is the vertical counterpart of `Toolbar`: a column of `Button`
  commands anchored beside content, so plate, role, disabled, and denied
  rendering are not restated per surface. It lights the Edge Wake described
  below down its own leading edge while the content beside it is scrolled.
- `TableHeader` gives the row family sortable column titles over the same
  column-width model `TableRow` lays its cells out with, and reports the sort
  its owner commits rather than reordering anything itself.
- `TabsOrientation` gives the existing strip a vertical orientation, so a
  sidebar of pages is the one selection control rather than a second one.

## Plate seating: a panel or a bar

Where a control sits decides whether it wears chrome of its own.
`state::PlateSeating` is that one fact, and it is a property of the *surface
behind the control* — never of what the control is or what it is doing:

- `Panel` (the default) — the control always wears its Alloy Plate and Signal
  Rim, so it reads as a plate raised above the window or panel behind it.
- `Bar` — the control wears **no** rim in any state, and no plate at all while
  it has nothing of its own to state. A run of icons therefore reads as one
  continuous bar rather than a row of boxed buttons.

One state model, one renderer, and one resolved set of colours serve both. The
whole consequence is a single shared rule (`paint::FrameColors::face`), so no
family can grow its own idea of a flat control: a bar-seated control's rim
collapses onto its plate, and the quiet *resting* frame — the one frame in which
a control carries no role colour, no disposition, no pointer and no keyboard —
drops the plate entirely.

Nothing about the control's feedback is discarded, only moved off the edge:

- A **hover** raises the plate as the shared pointer wash (`surface_hover`), and
  a **press** compresses it (`surface_pressed`). For a rimless control the wash
  is the *only* pointer feedback there is, which is why `lib/theme` owes it a
  visible step away from the bar's own fill and asserts that separation on both
  appearances.
- **Keyboard focus** keeps the resting fill and takes the ordinary focus ring,
  so focus never reads as hover — and a bare frame is by construction never a
  focused one, so the ring can never be dropped along with the plate.
- A **disposition** (denied, failed-closed, pending, disabled) states itself on
  the glyph tint and its shape-coded Signal Bead rather than a coloured edge, so
  it stays legible without colour vision.
- Presence, activity, and pressure use the marks the control already owns: the
  `TaskbarItem` presence mark, the Heat Seam, the Pressure Rail, the bead.
- Focus Field membership (below) is the one signal a bar-seated control cannot
  make, because membership is drawn only as a lift of the rim. A Focus Field
  groups a row with its own actions inside a panel, and the icon strip has no
  such groups.

`IconButton` is the only family that carries the choice (`IconButton::seated`),
because it is the only one that appears on both kinds of surface — a window
toolbar and the desktop's icon strip. `shell::TaskbarItem` and
`shell::TraySignal` exist only on the bar and are bar-seated by construction;
everything else is panel-seated.

## Owner-supplied icon artwork

Four controls draw an icon whose artwork their owner may already hold
rasterised: a `shell::TaskbarItem`, a `collection::IconTile`, a
`collection::ListRow`, and a `button::IconButton`. Each offers the same pair —
one query and one parameter:

- `icon_side(bounds, scale, theme, …) -> u32` reports the exact pixel side
  the control's icon slot will be drawn into, and `0` when the geometry leaves
  room for none. An owner asks its cache for artwork at precisely that size
  rather than guessing one and rescaling at draw time.
- `render(…, artwork: Option<&Surface>)` blits that artwork centred in the
  slot when it is supplied and rasterises the control's built-in vector glyph
  when it is not, so a missing, refused, or undecodable asset always degrades
  to a meaningful icon instead of a blank slot (`AGENTS.md` §10).

The rule lives once, in the crate's shared paint recipe
(`paint::paint_icon_slot`), so the four controls cannot drift apart
(`AGENTS.md` §2.2). Artwork whose surface does not match the slot is centred
on it rather than pinned to a corner, so a size mismatch reads as an even
margin instead of a lopsided drawing; a control that reserves no icon slot
ignores the parameter entirely. A control never decodes an image — artwork
reaches it already decoded and rasterised through the desktop's sandboxed
asset path (`AGENTS.md` §19.5), so a malformed file can only fail to produce
artwork, never reach a drawing path.

## The icon-view tile

`collection::IconTile` is one item of an icon view — a picture with its name
beneath it — and it is what the file manager's grid and the desktop's icon field
are both made of, so the two cannot drift into lookalikes.

A resting tile draws **only** its picture and its label: no plate, no rim, no
rail. That is the point of the control. An icon view is a field of many items,
and a plate per item would put a box around every icon; whatever lies behind the
tile — a window's surface, or the desktop wallpaper — shows through instead. A
`Card` is the opposite case and keeps its plate: a card's plate bounds the one
group of state and actions it owns.

Only state paints anything behind the picture, and each state uses the mark the
language already owns for it: the shared pointer wash for hover and press, the
selection accent for a selected tile (whose label and glyph invert onto it, so
selection differs from a hover in contrast rather than only in hue), the shared
focus ring for the keyboard, and the shape-coded Signal Bead for a denied or
unhealthy item. Nothing a tile draws escapes its bounds, so a view may lay tiles
edge to edge — and bound the whole grid's paint to the area it owns — without a
tile bleeding onto its neighbour.

A tile renders state and never dispatches. The view owns the grid geometry and
hit-tests pointer input against that same geometry, so a tile carries no pointer
position or press latch of its own, unlike a `ListRow` the user clicks directly.

## Grouped focus and anchored edges

Two of the design language's reactive state patterns describe a *relationship
between controls* rather than the state of any one of them, so both are
resolved in the crate's shared paint recipe and inherited by every family
instead of being drawn per surface.

### The Focus Field

`FocusState` carries two independent facts: whether a control holds the
keyboard, and whether it belongs to a group whose **Focus Field** is
highlighted. A row of related controls — a list row and the action buttons
that act on it — is one such group: the member the keyboard is actually on
takes the focus ring, and every other member states its membership by
lifting its rim part-way toward the active rim.

The lift is partial by design, so a member never looks like the focused
control; a control that is *both* focused and a member simply takes the ring,
because the language draws one or the other and never both on the same
control. A filled plate is left alone: its rim is its plate colour by
construction, and tinting one without the other would put a foreign edge on a
coloured control. Under a high-contrast theme the lift goes all the way to the
active rim — contrast comes before glow, and a partial blend would wash out.

Membership is the *weakest* claim a rim can carry. A disabled, denied,
needs-capability, failed-closed, or pending control keeps the rim its
disposition gave it and draws identically whether or not its group is
highlighted: each of those is telling the user something they need far more
than which row a control belongs to, and a control that cannot be actioned
must never look livelier than a resting one that can. Only an ordinary
interactive control — including one merely awaiting confirmation, which is
still actionable and still takes its plain role emphasis — is lifted.

### The Edge Wake

An anchored control that content scrolls past does not move, which leaves a
still frame ambiguous: did the column stay put, or is it merely where the rows
left it? The **Edge Wake** answers that on the control's edge. An `ActionRail`
anchored beside a list lights its own leading edge (`ActionRail::with_edge_wake`)
for exactly as long as the content beside it is displaced from its start.

It is a state, not an animation. There is nothing to fade, so a reduced-motion
theme needs no second path and a screenshot carries the same information as a
live surface. The seam is drawn at the shared seam breadth in the active rim
colour, doubled under heavy contrast like every other edge in the theme. A
section whose items are cards has no wake: a card draws its own footer actions
inside itself, so no anchored column stands beside the list.

## Masked text entry

`TextField::secret(max_len)` puts a field into masked mode for credential
entry — a password, a passphrase, a PIN — and `TextField::is_secret` reports
it. A `SearchField` has no such mode: a query is not a credential. Nothing
else about the field changes. The plate, rim, focus ring, validation rim,
Authority Mark, read-only and disabled rendering, high contrast, and reduced
motion behave exactly as for a plain field, and every editing key, the pointer
caret placement, and drag-selection work identically. The control offers no
way to reveal the buffer.

### One bead per character, not a repeated glyph

A masked field paints one filled round bead per `char`, at a fixed advance
derived from the theme's selector extent and the active `Scale`, through the
same shared circle primitive the Signal Bead uses. It draws beads rather than
a repeated masking character for two reasons:

- the drawn run's width then depends only on the buffer's *length*, never on
  which characters it holds, so the rendering cannot report anything about the
  secret through its width; and
- no particular masking glyph has to exist in the font.

The caret stands between bead cells and the selection highlight covers whole
cells, both through the same painting a plain field uses, so a masked field
measures exactly as tall as an unmasked one. The pointer hit test divides the
pointer offset by the fixed cell advance and resolves the resulting cell to a
`char` boundary — never a byte index derived from glyph widths — so a click can
never land mid-scalar. An empty field still shows its placeholder: a
placeholder is not a secret.

### The buffer is reserved once, up front

Masked mode is inseparable from its character bound, and the bound is the
reason. It lets the editor reserve the worst case UTF-8 needs for `max_len`
characters the moment the mode is set, so the buffer can never grow while it
fills. A `String` that grows copies its contents to a fresh allocation and
releases the old block with everything typed so far still written in it — a
copy of the credential that no later erase can reach, because nothing holds
its address any more. Reserving the whole capacity up front means there is
only ever one copy to erase.

### Discarded bytes are erased

Every path that drops buffer content — replacing the text, overwriting a
selection, clearing, truncating to the bound, and the editor's `Drop` —
overwrites the bytes it discards before releasing them. The erase is the
workspace's shared `tairix_util::secret::wipe` rather than a plain fill: on
the drop path the bytes are freed immediately afterwards and nothing reads
them back, so an ordinary store is dead by the language's own rules and a
release build is entitled to delete it outright, leaving the plaintext in the
released block. The shared wipe writes volatile and fences, so the erasure
survives optimisation.

The erase runs in plain mode too. It is cheap, it is harmless, and one editor
is better than two. A `TextField`'s `Debug` output redacts a masked buffer,
printing its character count in place of its content, so a diagnostic dump
cannot carry a password.

## Where it sits

`#![no_std]`, and `#![forbid(unsafe_code)]`. The crate depends only on other
`lib/*` crates — `tairix-geometry`, `tairix-theme`, `tairix-raster`,
`tairix-font`, `tairix-icon`, `tairix-input`, and `tairix-util` for the shared
secret erase — and never on `kernel/*`, `drivers/*`, or `userland/*`, so the
desktop depends on it and never the reverse.
