# Traditional desktop taskbar

The taskbar (`userland/gui/taskbar`, crate `tairix-taskbar`) is the
GNOME/Windows-style bar pinned to a configured screen edge (`AGENTS.md` §10,
`PLAN.md` Stage 7, `plans/NEW-TASKBAR.md`). This page describes the **layout,
model, and rendering**: the geometry of every region, pointer hit-testing for
input routing, the program-library popup / pin-strip / task-list /
notification-area / Switchboard-tray state machines, the bar's right-click
**context menu**,
and painting those regions — including the clock label and task-title
**text** — into a themed pixel surface, plus **routing** pointer, scroll,
and key events into taskbar actions and drawing **notification-icon
artwork** (scalable, themeable vector glyphs) and the desktop's **icon
artwork** (the shipped class masters and each application's own bundle
icon, read and decoded by the session and blitted here).

## Where it sits

As a `userland/gui/*` crate the taskbar depends only on `lib/*`: the shared
[`tairix-geometry`](../lib/overview.md) coordinate types, the shared
`tairix-raster` rasteriser (the premultiplied-alpha `Color`/`Surface` the
window manager also paints with), the shared `tairix-font` text rasteriser
(the built-in bitmap face and glyph blitter, `AGENTS.md` §16.4), the shared
`tairix-input` event vocabulary (the same `PointerButton`/`InputEvent`/`Key`
the window manager routes), the shared Reactive Alloy control vocabulary
(`tairix-controls`, `plans/GUI-CONTROLS-DESIGN.md` — the leading launcher
buttons and the popup's panel, search field, list rows, and scrollbar; the
[widget gallery](widgets.md) is its worked reference), the
program-library catalog engine ([`tairix-proglib`](../lib/proglib.md) — the
typed `Catalog` the popup lists), and the shared
[`tairix-theme`](theming.md) definition. It never depends on the window
manager or on any sibling userland crate, and nothing depends on it in turn
(`AGENTS.md` §17.4, §17.3) — the desktop is an optional, one-way-dependent
frontend, so a headless image simply omits it.

The taskbar holds no authority and performs no I/O: pressing a launcher or
choosing a library entry only *reports* a typed `TaskbarResponse`, and the
[session glue](session.md) — which reads the catalog stores and holds the
spawn capability — resolves and performs the action.

## Layout

A `TaskbarConfig` fixes the screen `Edge`, the bar `thickness`, and the
per-region extents. The edge fixes the bar's `Orientation`: a top/bottom bar
runs horizontally (its long *main axis* is `x`), a left/right bar runs
vertically (main axis `y`). Regions are laid out along the main axis and the
code is otherwise orientation-agnostic.

From the leading end to the trailing end:

- **Library button** — the first of the two permanent leading launchers: the
  nine-tile-glyph invoker that opens the program-library popup.
- **Files button** — the second permanent launcher: a folder-glyph button that
  opens the file manager. The two leading buttons are fixed — never reordered,
  never removable (`plans/NEW-TASKBAR.md` T4) — and they are quiet peers: on an
  icon strip no single icon is the primary action of the surface, so neither
  carries a role fill.
- **Pin strip** — one compact square slot per user-pinned application
  shortcut, in the user's stored order, between the launchers and the task
  list (`plans/NEW-TASKBAR.md` T6). Zero-length (but still positioned) when
  nothing is pinned; `BarLayout::pin_drop_index` maps a drop point in the
  strip-plus-task band to the pin-list insertion index for the
  drag-to-taskbar gesture (see *Dragging a library entry to pin it*).
- **Task list** — the flexible middle region, between the pin strip
  and the notification area, holding one fixed-width slot per running task.
- **Notification area** — status/notification icons, packed immediately
  before the clock.
- **Clock** — immediately before the Switchboard capsule. Its display text is
  held by a `Clock` model whose label the caller sets (formatting a `Time64`
  value into a string is an upstream concern, `AGENTS.md` §21); the bar stores
  only the text to draw.
- **Switchboard capsule** — anchored to the very trailing end, immovable
  (`plans/NEW-TASKBAR.md` T9). The `SwitchboardTray` model derives the shared
  `tairix-controls` `TraySignal` capsule from the Switchboard service's
  tray-signal summary plus the session's unresponsive count — one pure
  derive, dominant state hung > pressure > jobs > recovery > calm, with the
  working seam / pressure rail / recovery posture composed orthogonally, and
  an absent service deriving the calm capsule (fail closed). Its slot is
  computed **first** among the trailing regions, so the clock, icons, pins,
  and tasks can never displace it — only the permanent leading launchers
  outrank it on a degenerate screen. Hover expands the capsule's instrument
  readout, presented like the other popovers
  (`Taskbar::tray_readout_layout`), and a press resolves as a tap or a hold
  (see *The Switchboard capsule's tap-or-hold gesture*).

`BarLayout::compute` turns the config plus the current pin, task, and icon
counts into the screen `Rect` of every region. All arithmetic saturates, so a
pathological screen size or extent fails closed *inside* the bar rather than
wrapping (`AGENTS.md` §2.9); a launcher, pin, task, or icon slot that does
not fit its region is `Rect::EMPTY` and therefore never hit, and the trailing
regions clip against the permanent leading launchers (never the reverse), so
a degenerate screen shrinks the clock and icons to nothing — and, last of
all, the Switchboard capsule — rather than overlaying them on a launcher.
`BarLayout::hit_test`
maps a pointer to the `Hit` element under it (the Library button, the Files
button, a pin index, a task index, a notification index, the clock, or the
Switchboard capsule), which
is what input routing dispatches (see *Input routing*).

## Rounded edges

The taskbar supports rounded corners, but it does **not** draw them itself.
`BarLayout::corner_radius` carries the radius taken from the active theme's
`taskbar_corner_radius` metric; the window manager applies that radius through
its single anti-aliased rounded-corner path, exactly as it rounds windows.
There is no second rounded-corner implementation (`AGENTS.md` §2.2).

## The owned theme

The bar owns a copy of the active `Theme` (`Taskbar::theme`), adopted at
construction and swapped by `Taskbar::apply_theme`. Layout, hit-testing, and
painting all read that one copy, so the radius a hit-test assumes and the
radius the painter draws can never come from two different themes.

## The per-surface repaint latch

Pixel-only state changes (a launcher hover, a popup scroll or edit, a theme
switch) set a **repaint latch**, and the embedder drains it with
`Taskbar::take_repaint` to re-present exactly when something changed — one
present per visual change, no per-frame busy repainting (`AGENTS.md` §2.16).

The latch is a `TaskbarRepaint`, and it names **each rendered surface
separately** rather than carrying one "something changed" bit:

| Flag            | Surface                                              |
| --------------- | ---------------------------------------------------- |
| `bar`           | the bar strip itself                                  |
| `library`       | the program-library popup                             |
| `menu`          | the bar's context menu                                |
| `notifications` | the notification popover                              |
| `readout`       | the Switchboard capsule's expanded instrument readout |

`TaskbarRepaint::NONE` and `::ALL` are the two extremes, one constant names
each single surface, `any()` asks whether anything is pending, and `|` / `|=`
compose latches, so a mutator touching two surfaces latches both in one
expression.

**Why per surface.** The five surfaces are wildly unequal in cost: measured on
the host in release, rendering the bar takes 1655 µs and the library popup
1001 µs, against the context menu's 104 µs. With a single boolean the embedder
could not tell which had changed, so it re-rendered and re-pushed all five
every time — a pointer drifting from one row to the next of a small open menu
cost about 2.8 ms of rendering plus a recomposite of five window rectangles,
when 104 µs and one small rectangle was the whole of the change. That is a
pointer moving over a menu, the most frequent interaction the desktop has, and
it is why the desktop felt laggy. Naming the surface lets the presenter repaint
the 104 µs one and leave the other four exactly as the compositor already has
them.

**The contract.** Every change that alters what a surface draws latches that
surface, and a change touching several latches all of them:

- a hover moving between bar buttons, pins, or task slots → `bar`;
- a highlight moving inside the open context menu → `menu`;
- opening or closing the popup → `library` **and** `bar` (the Library button
  reads as visually held open);
- raising or dismissing a notification → `notifications` **and** `bar` (the
  notification-area icon);
- a Switchboard tray summary change → `bar`, plus `readout` while the readout
  is expanded;
- a theme swap, or an edge or resize through `Taskbar::set_config` → `ALL`,
  since every surface draws from the palette and anchors off the bar's
  geometry.

Latch sites deliberately **err toward latching more, never less**: an extra
latch costs one redundant repaint, while a missing one leaves stale pixels on
screen, which is a correctness bug. Because the contract holds at every
mutator, the embedder may present *strictly* from the drained latch — and
present nothing at all when it is empty.

The contract holds even for a borrow the bar cannot see into. Each `&mut`
sub-model accessor latches the moment it hands the borrow out, whether or not
the caller goes on to change anything: `tasks_mut` and `clock_mut` latch
`bar`, and `library_mut` latches `library` **and** `bar`, since the same
borrow can open or close the popup and so redraw the Library button. Those
accessors carry genuine state changes — a task added, a catalog resolved, the
clock advanced — never a per-input-sample update, so the conservative latch
costs nothing on the hot path. The crate-internal routing seams the input
router borrows the popup and the menu through are the deliberate opposite:
they do not latch, because the router latches from the outcome the sub-model
reports, which is what keeps a pointer sample over the open popup from
repainting it.

That leaves exactly one thing the embedder still owns: the desktop `Scale`,
which the compositor passes per layout and render call rather than being bar
state, so a scale change is the caller's own and it knows it dirtied
everything.

## Rendering

`TaskbarRenderer::render` paints the taskbar into a `tairix-raster` `Surface`
sized to the bar using the taskbar's own theme, filling each region with a
colour role from the `Palette`. Its last argument is the caller's
`tairix_icon::IconArtwork` lookup — the session's decoded artwork, or
`NoArtwork` on a system that has none (see *Icon artwork*, below):

- the bar background is the **raised surface** colour;
- every icon on the bar is **bar-seated** (`PlateSeating::Bar`): it wears no
  perimeter in any state, and no plate at all while it has nothing of its own to
  state, so the strip reads as one bar rather than a row of boxed buttons. A
  hover raises the shared pointer wash (`surface_hover`, one clear step from the
  bar's own fill), a press compresses it (`surface_pressed`), and keyboard focus
  keeps the resting fill and draws its ring inside the slot. The rule itself
  lives in `tairix-controls` (`plans/GUI-CONTROLS-DESIGN.md` §10), not here — the
  bar chooses the seating and nothing else;
- the **Library button** is the shared `tairix-controls` `IconButton` in the
  quiet `Neutral` role, carrying the shipped `Library` artwork over its
  nine-tile glyph — compressed while its popup is open, washed under the
  pointer;
- the **Files button** is the same quiet (`Neutral`) `IconButton` carrying the
  shipped `Folder` artwork over its folder glyph;
- each **pin slot** and each **task slot** is one shared `tairix-controls`
  `TaskbarItem` — the bar's application buttons have exactly one visual
  recipe (`AGENTS.md` §2.2). A pin uses the icon-only presentation (a
  centred icon sized off the plate); a task shows its icon beside the
  truncated window title. The item's `TaskVisibility` paints the state through
  the **presence mark** on the lower edge: the **active** window's item takes
  the full-width accent seam, a merely **running** one a short centred muted
  mark (so presence and activation differ in length as well as in hue), a
  **minimised** one that same mark plus a recessed plate and the muted leading
  tick, and a **closed** pin (its application not running) no mark and no plate
  at all — only the icon sits on the bar — until hovered;
- a pin's and a running task's per-application **artwork** (the owning
  bundle's icon, read and rasterised by the session through the sandboxed
  icon pipeline — see [the session](session.md)) is blitted through the
  control in place of the built-in glyph. A task's comes from the bundle the
  kernel attested opened its window, so an open window is recognised by its
  own application whether or not that application is pinned;
- each notification icon slot draws a **scalable vector glyph** (see
  *Notification icons* below), tinted in the **muted** foreground colour;
- the **Switchboard capsule** is the shared `tairix-controls` `TraySignal`
  drawn in its slot — its shipped `switchboard` artwork (the built-in mixer
  glyph where the system ships none) with its live badge, seam, rail, and
  beads, bar-seated like every other icon on the strip, so it carries no
  outline of its own — and `TaskbarRenderer::render_tray_readout` paints the
  expanded instrument readout as its own popover surface, rounded by the window
  manager with `TrayReadoutLayout::corner_radius`.

On top of those plates, the renderer draws **text** with the shared `tairix-font`
`BitmapFont` (the built-in Inconsolata EX + M PLUS 1 Code + D2Coding + Noto Sans
Hebrew family): the clock label is centred in the clock region, and each task
item truncates its title to the characters that fit, so text never spills
into a neighbouring slot (`AGENTS.md` §2.9). Glyphs are composited through
`tairix-raster`'s one premultiplied-alpha `over` path — no blitter or colour
algebra is duplicated here (`AGENTS.md` §2.2).

The surface is rectangular: the taskbar paints no corners. The window manager
presents it and applies `BarLayout::corner_radius` through its single
anti-aliased rounded-corner path, exactly as it rounds windows (`AGENTS.md`
§2.2). Region rectangles are screen-space; each is translated into the bar's
local surface space, the translation saturates, and `fill_rect` clips, so a
degenerate layout paints nothing rather than panicking (`AGENTS.md` §2.9).
Switching themes simply re-renders with the new palette.

`TaskbarRenderer` is a small stateful object — the region fills, clock, and
task titles are cheap to repaint every frame, but the vector notification
glyphs are not, so it holds a `tairix_reclaim::ReclaimCache` of rasterised
glyphs across frames, built by `icon_cache` from the shared
`tairix_reclaim::desktop::disposable_ui_cache` policy: owned by the seat,
bounded by a budget derived from the real framebuffer byte size, dropped
under memory pressure, and wiped on release. The renderer is the right home
for that state: the `Taskbar` model stays pure data. `render_library` (the
popup painter) needs no cache of its own, so it stays a `&self` method.

## Icon artwork

Every application icon the bar draws resolves through **one** rule, written
once in `render` rather than restated per slot (`AGENTS.md` §2.2):

1. the artwork its owner already supplied for that specific application (a
   `PinView`'s bundle icon, a running task's own bundle icon, a library row's
   own icon), else
2. the artwork the `IconArtwork` lookup holds for the slot's `IconKind` — the
   shipped class master under `/System/Graphics/Icons`, else
3. the control's built-in vector glyph.

The bar reads no file and decodes no image: it *asks* the lookup the session
owns, at exactly the pixel side the slot will be drawn at (`pin_icon_side`,
`task_icon_side`, and the controls' own `icon_side`), so nothing is ever
rescaled at draw time. Because the third rung always exists, resolution is
**total**: a lookup that
answers nothing — `tairix_icon::NoArtwork`, a machine with no
`/System/Graphics`, or a cache that is giving memory back under pressure —
still renders every element, so a headless-graphics or freshly-installed
system stays fully usable rather than showing blank slots (`AGENTS.md` §10,
§2.9).

## Notification icons

The notification area holds an ordered list of status icons, each with a
stable `IconId` and a theme **asset id**. When the bar renders, every
notification slot resolves its asset id to a `tairix-icon` `IconKind`
(`IconKind::for_asset`, falling back to a generic glyph for an unknown id,
`AGENTS.md` §2.9), builds the matching scalable `VectorIcon` in the **muted**
foreground colour, rasterises it to the slot size at the active scale, and
composites it onto the bar through `tairix-raster`'s `Surface::blit`. The
glyph is artwork, not a flood fill, so the raised bar background shows through
around it. The icons rasterise through the *same* polygon path
(`Surface::fill_polygon`) the cursors use — there is no second scan converter
(`AGENTS.md` §2.2) — and a slot too small to hold a glyph paints nothing
rather than panicking (`AGENTS.md` §2.9).

Rasterising a glyph is the expensive step, so the `TaskbarRenderer` does it
only once per tint and size: its `ReclaimCache` is keyed by `IconKind` within a
`(tint, pixel-size, set-generation)` epoch (`IconEpoch`), so repeated frames
reuse the cached glyph and only a theme change (new tint), a scale change (new
size), or an installed icon set (new generation) re-rasterises — the
SVG-first "convert once, re-render only on a scale or theme change" rule
(`AGENTS.md` §10), sharing the same disposable-UI-cache policy the window
manager uses for cursors (`AGENTS.md` §2.2).
See [SVG asset decoding](svg-assets.md) for the caching layer and
[Desktop icons](icons.md) for the vector representation and the glyph set.

## The program-library popup

`LibraryPopup` is the folder-organised application launcher the Library
button opens (`plans/NEW-TASKBAR.md` T5). It is a pure model over the
**resolved** program-library `Catalog` the session hands it
(`LibraryPopup::set_catalog`) — the machine store merged with the user's
overlay through `tairix_proglib::merge` (see
[the program library](../lib/proglib.md)). The popup never touches the VFS;
choosing an entry only reports `LibraryLaunch { entry }` with the entry's
identifier, and the session resolves the bundle and launches it through the
ordinary signature-checked load gate.

The surface is composed from the shared Reactive Alloy vocabulary
(`plans/GUI-CONTROLS-DESIGN.md`): a `Panel` whose anchor notch points back at
the Library button, a `SearchField` filter, one shared `ListRow` per folder
or entry, and a `ScrollBar` when the rows overflow the viewport. Folders are
the closed ten-folder taxonomy in its canonical order; a folder with no
entries is never listed; entries sort by display name within their folder. An
empty library renders a calm "No programs are catalogued" placeholder — and a
filter matching nothing "No matching programs" — never an error
(`plans/NEW-TASKBAR.md`).

Opening is deterministic: the search comes up cleared, every folder expanded,
the cursor and scroll at the top, and the keyboard on the search field, so
the same catalog always presents the same way.

### Geometry

`Taskbar::library_layout` computes the popup's geometry (`LibraryLayout`):
the panel opens *outward* from the bar — above a bottom bar, below a top bar,
to the inner side of a left/right bar — aligned to the Library button and
clamped to the screen. Its height is sized to the rows it has, capped by the
space between the bar and the opposite screen edge; overflowing rows scroll.
The panel chrome overhead is *measured* by probing the shared `Panel`
geometry rather than re-deriving its arithmetic, so a metrics change can
never drift the layout from what the panel draws (`AGENTS.md` §2.2). Widths,
row heights, and the corner radius are *logical* lengths converted through
the one shared `Scale::scale_length`; all arithmetic saturates, and a screen
too small for even one row yields chrome with an empty viewport rather than a
panic (`AGENTS.md` §2.9).

### Keyboard model

While open the popup is modal and fully keyboard-driven
(`plans/GUI-CONTROLS-DESIGN.md` §9): `Tab` cycles the two focus fields
(search ↔ rows); `Up`/`Down` move the row cursor (wrapping), `Home`/`End`
jump, `PageUp`/`PageDown` move by a viewport, and the view follows the
cursor; `Enter` (or space) activates the cursor row — a folder toggles its
expansion, an entry launches; `Left` collapses a folder or climbs from an
entry to its folder, `Right` expands or steps into the first entry; typing
anywhere routes into the search field (type-to-filter, case-insensitive,
flat name-sorted results), `Enter` in the search launches the first match,
and `Escape` clears a non-empty search, then dismisses. Everything fails
closed: activating with no cursor, launching with no match, and stepping in
an empty list all change nothing (`AGENTS.md` §2.9).

### Rendering

`TaskbarRenderer::render_library` paints the open popup: the `Panel` chrome
(anchored back at the Library button), the search field, the visible rows —
a folder row carries the open/closed folder glyph, its label, and a trailing
entry count; an entry row is indented beneath its folder and draws **the
application's own icon** (below) over the app-bundle glyph; the hovered row
raises its fill, and the keyboard cursor row shows the shared selection rail
and focus ring — the calm placeholder when nothing is listed, and the
scrollbar when the rows overflow. Like the bar it is a rectangular surface
the window manager places and rounds with `LibraryLayout::corner_radius`, and
it returns `None` while the popup is closed (`AGENTS.md` §2.9).

### Each application's own icon

An entry row shows the icon its bundle ships, and the popup carries that
artwork the same way the pin strip does: the owner resolves it, the bar only
draws it (`AGENTS.md` §2.2, §17.4). Three methods express the split:

- `LibraryPopup::visible_icon_requests(layout, scale, theme)` reports one
  `LibraryIconRequest { row, side, entry }` per entry row **the viewport
  actually shows**, each carrying the exact pixel side that row's slot will
  draw at. A hundred-entry library therefore costs the session a read and a
  decode per *visible* row, never per catalogued application, and scrolling
  asks only for the rows that just came into view.
- `set_row_artwork(row, artwork)` files an answer; an out-of-range index is
  ignored rather than mis-filed.
- `row_artwork(row)` is what `render_library` blits.

Any rebuild of the row list (a new catalog, a changed filter, a folder folded
or expanded) clears the filed artwork, so a stale index can never draw one
application's icon on another's row. A row with no artwork draws the
app-bundle glyph, which is why a library still lists legibly on a system with
no artwork at all.

## Task list

`TaskList` holds one `TaskEntry` per top-level window. At most one task is
*focused*; a task is independently *minimised*. `TaskList::activate` applies
the familiar click rule and reports the `ActivateOutcome` so the caller can
drive the window manager:

- clicking the focused, non-minimised task **minimises** it and drops focus;
- clicking any other task (or a minimised one) **restores and focuses** it.

Adding a task with a duplicate id, or removing/activating an unknown id,
changes nothing and is reported as such — the window manager assigns unique
ids, so a clash signals a bug rather than a benign retry.

## Pinned shortcuts

`PinStrip` holds the session's *resolved view* of each pin
(`plans/NEW-TASKBAR.md` T6): pins are per-user configuration (the
[`tairix-taskpins`](../lib/taskpins.md) store, read and written by the
session under the user's own identity), and the bar receives one `PinView`
per pin — a display label, the class glyph, its optional rasterised bundle
artwork, the program-library entry it references (when it is an `entry`
pin), and the running desktop window it currently matches, if any
(`Taskbar::set_pins`). The strip derives each pin's live `TaskVisibility`
from the `TaskList` at paint time — Active when its matched window is
focused, Minimized when minimised, Running otherwise, and Closed when it
has no live window (including a stale match, fail closed) — so there is
never a second copy of window state to fall out of step.

A primary press on a pin with a live window follows the task list's own
click-to-activate / minimise rule (reported as `TaskActivated`); one with
no window reports `ActivatePin { index }` and the session launches the
pinned bundle. `Taskbar::pin_icon_side` exposes the exact pixel side a pin
icon paints at (through the same control geometry the renderer uses), so
the session rasterises artwork at exactly the drawn size.

## Dragging a library entry to pin it

A pin is created by dragging a program out of the **program-library popup**
and releasing it over the bar's pin band (`plans/NEW-TASKBAR.md` T7). The
popup is the drag source because every row it lists is a *catalogued entry*
by construction, so the gesture names an `EntryId` the store can record
directly — nothing is guessed from a path, and nothing the catalog cannot
vouch for can be pinned.

The gesture is the shared `lib/browse` `BundleDrag` threshold detector, the
same one the file manager uses to drag a bundle out of a window, so there is
one definition of "this press became a drag" on the desktop
(`AGENTS.md` §2.2):

- a primary press on an **entry row** arms the detector instead of launching
  at once (a folder header has nothing to drag and still toggles on the
  press);
- motion past the shared threshold reports `PinDragOffered { entry }` — once
  per gesture, never repeatedly as the pointer keeps moving;
- the release that ends the press reports `PinDragDropped`; where it landed
  is the session's to resolve, and the popup stays open;
- a press-and-release that never travelled is an ordinary click and
  launches, so the gesture costs the click nothing;
- `Escape` mid-drag reports `PinDragWithdrawn` and keeps the popup open, so
  backing out of a drag does not also cost the user the listing;
- a fresh press always supersedes an earlier arming, an offer whose row no
  longer names an entry is abandoned rather than guessed at, and dismissing
  the popup mid-drag withdraws the offer session-side, so a gesture never
  outlives the popup it started in.

The popup reports these as `PopupOutcome::{DragOffered, DragDropped,
DragWithdrawn}` and the router surfaces them as the corresponding
`TaskbarResponse`. The bar performs nothing: the session arms, withdraws, and
resolves the offer, re-checking the entry against the live catalog at the
drop and mapping the drop point through `BarLayout::pin_drop_index` (see
[the session's pin service](session.md#pin-service-and-window-channel-bridge)).
An entry already pinned is not duplicated.

## The context menu

`BarMenu` is the bar's one right-click surface, composed from the shared
`tairix-controls` `Menu`. A secondary press on a pin opens it over that pin
(*Open* / *Unpin*, with *Open* restoring a running window or launching
otherwise); a secondary press on a program-library **entry row** in the
open popup opens it over that entry (*Open* / *Pin to taskbar*, or *Unpin
from taskbar* when the entry is already pinned). The menu opens outward
from the bar edge, anchored at the slot or row it is about, clamped to the
screen (`Taskbar::menu_layout`), and is presented by the session as its own
small rounded window above the bar. Choosing a row reports a typed response
(`ActivatePin` / `Unpin { index }` / `PinEntry { entry }` /
`LibraryLaunch`); the menu itself performs nothing — the session edits the
store and re-resolves the strip (`AGENTS.md` §5.4).

## Input routing

`TaskbarInput` is the taskbar's input router, the counterpart of the window
manager's `InputRouter`. It consumes the **same** shared `tairix-input`
`InputEvent` stream the compositor routes (`AGENTS.md` §17.4, §2.2), tracking
the pointer position from motion events (which also drives the leading
buttons', pin slots', and task slots' hover feedback through the repaint
latch). With the popup and menu closed it acts only on a primary or
secondary press, hit-tested against the current
`BarLayout` and reported as a `TaskbarResponse`:

- a primary press on the **Library button** opens the program-library popup
  (`OpenLibrary`);
- a primary press on the **Files button** reports `OpenFiles` — the session
  opens the file manager, raising an already-open files window rather than
  launching a second copy;
- a primary press on a **pin slot** activates the pin (see *Pinned
  shortcuts*), and a **secondary** press on one opens its context menu;
- a primary press on a **task slot** applies the click-to-activate /
  minimise rule and reports the `ActivateOutcome`
  (`TaskActivated { id, outcome }`);
- a primary press on a **notification icon** reports its `IconId`
  (`NotificationActivated`);
- a primary press on the **clock** reports `ClockPressed`;
- a primary press on the **Switchboard capsule** resolves as a tap or a
  hold (see *The Switchboard capsule's tap-or-hold gesture*), a press
  inside the open readout drives its one safe action or is claimed inert
  like the notification popover's chrome, **scrolling** over the capsule or
  its readout cycles the running tasks (wrapping both ways), and a
  **middle** press over the capsule switches to the previous task (the task
  list's MRU-of-two) — each failing closed when there is nothing to cycle
  or return to.

Any other button, a release, a key, or a press that misses every region
changes nothing and is reported as `Ignored` (fail closed, `AGENTS.md` §2.9).

While the context menu is **open** it is the top modal layer: the whole
stream routes into it first — motion highlights rows, a primary press-and-
release chooses, arrows and `Enter` drive it from the keyboard, `Escape`
dismisses, and a press outside its plate dismisses **only the menu**,
leaving whatever is beneath for the next click (one click does one thing,
`AGENTS.md` §2.1).

While the popup is **open** the router treats it as modal and consumes the
whole event stream — presses, releases, scroll, and keys all route into the
popup, so a click lands on exactly one thing (`AGENTS.md` §2.1):

- a primary press on a **folder header** toggles it at once; a primary press
  on an **entry row** arms the drag gesture instead, and the release that
  ends the press launches the entry (`LibraryLaunch { entry }`, closing the
  popup) unless the pointer travelled far enough to become a drag (see
  *Dragging a library entry to pin it*);
- a primary press on the **Library button** toggles the popup shut
  (`LibraryDismissed`);
- a press **anywhere else** — any button — dismisses the popup without
  acting on what it landed on (`LibraryDismissed`), the standard click-away
  behaviour;
- scroll wheels the row viewport; keys drive the keyboard model above.

Popup-internal changes (a hover, a scroll, an edit, a fold) are reported as
`Ignored` with the repaint latch set, so the embedder re-presents without
mistaking them for actions.

## The Switchboard capsule's tap-or-hold gesture

The capsule is the desktop's way into the Switchboard overview window
(`plans/NEW-TASKBAR.md` T11), and one primary press resolves into exactly
one of two destinations, reported as
`OpenSwitchboard { section }`:

- **tap** — a press and quick release opens the running-task section
  (`CommandSection::Tasks`, the wire section the bar names directly), the
  panel's NOW column;
- **hold** — a press held past `LONG_PRESS_AFTER_NS` (half a second) opens
  the **Recovery** section instead, where a hung application's recovery
  actions live;
- the open readout's one safe action, **"Open Switchboard"**, reports the
  same response as a tap, so the pointer and the readout reach one
  destination through one route (`AGENTS.md` §2.2).

The session performs it: it asks the Switchboard service to open — or,
when the service has died, revive and open — its window at that section.
The press *is* the demand, so there is no respawn loop
(`plans/NEW-TASKBAR.md` T10).

The threshold is resolved against the monotonic time the caller passes to
`TaskbarInput::handle`, on whichever event the router next handles once it
has elapsed — ordinarily a motion sample taken while the press is still
held, or the release when none arrives sooner. Nothing polls and nothing
sleeps (`AGENTS.md` §2.23). One gesture reports exactly one response: a
hold that has already opened Recovery never also opens Tasks on release,
and a press dragged off the capsule before release opens nothing at all
(fail closed, `AGENTS.md` §5.4).

## The system quick-actions menu

A **secondary** press on the Switchboard capsule opens the desktop's system
menu through the bar's one modal menu surface — there is no second popup
(`plans/NEW-TASKBAR.md` T13). Its rows are one table, `system::ROWS`, from
which both the rendered menu and the row → command mapping are derived, so
the two can never disagree (`AGENTS.md` §2.2):

| Row | What the session does |
|---|---|
| About This System | opens the Switchboard's overview |
| System Monitor | opens the Switchboard's task list |
| Task Shell | launches the terminal bundle |
| Light / Dark Appearance | switches the desktop's theme |
| Lock Screen | secures the screen behind this user's password |
| Log Out | ends the session; the login supervisor re-prompts |
| Restart / Shut Down | confirmed, then relayed to the one holder of the power capability |

The bar holds none of this authority. Each row reports a typed response and
the session resolves it, and every row whose backing is missing renders
**non-actionable with its reason stated** rather than being hidden or
silently offered: an uninstalled terminal bundle, a Switchboard that has not
attested it can power the machine, or a session that has attested it cannot
prompt for a password. Every such attestation defaults to refusing, so a bar
that was never told offers nothing it cannot deliver (`AGENTS.md` §5.4).

Locking heads the last group because it is the one way out of the session
that *keeps* the session; everything below it ends work in progress. The row
is offered only where the session runs on a console whose login supervisor
can re-verify the signed-in user (`DesktopShell::set_lock_available`) — a
lock that could never be undone is a trap, not a security measure. What the
lock then guarantees is described in [the desktop
session](session.md#the-screen-lock).

## Theming

The taskbar owns the active theme (see *The owned theme*) and adopts a new
one with `Taskbar::apply_theme`; the rest of its state is untouched, so a
runtime dark/light switch needs no model relayout (`AGENTS.md` §10). The
region **colours**, the control plates, and the text **foreground** roles are
wired through that theme by the renderer. The interactive light/dark switch
is the appearance pair in the system quick-actions menu above; the session
also switches programmatically (`DesktopSession::set_theme`).

## Tests

The crate's headless unit tests cover edge/orientation, the task-list
focus/minimise rule, notification add/remove deduplication, the region layout
and hit-testing for a bottom bar (both permanent launchers included),
vertical-bar layout, all four edges, overflow clipping, degenerate
(tiny-screen) fail-closed clipping of the launcher buttons, DPI scaling of
layout and hit-testing, and the theme-driven corner radius. The repaint tests
pin the per-surface attribution itself: a hover that changes only a bar button
latches `bar` alone, a highlight moving inside the open menu latches `menu`
alone, a pointer move that changes no hover state latches nothing at all,
opening and closing the popup latches `library` and `bar`, raising and
dismissing a notification latches `notifications` and `bar`, typing in the
popup's filter latches `library` and not `menu`, a theme swap and a
`set_config` edge move each latch every surface, and draining clears the
latch. Further tests pin the borrow rule: each `&mut` sub-model accessor
latches its surfaces even when the caller changes nothing, reading through the
immutable accessors latches nothing, a repeated pointer sample over the open
popup latches nothing, and `NONE` / `ALL` / `any()` / `|` compose as
documented.
The pin tests cover the strip's placement between the launchers and the
task list (and its reflow as pins come and go), pin hit-testing on every
edge, the drop-index mapping (leading/trailing halves, the empty-strip
first drop, appends past the last slot, vertical bars), the live visibility
derivation (running / active / minimised / closed / stale match), pin
activation (launch vs the task click rule), the context menu's rows,
modality, keyboard path, click-away, entry pin/unpin verb switch, menu
geometry on every edge, and the rendered pin slots — artwork override,
built-in glyph fallback, and a focused pin's full-width accent seam.
The popup model tests cover taxonomy-ordered folders with name-sorted
entries, hidden empty folders, folder labels, the calm empty and no-match
placeholders, the deterministic reopen state, case-insensitive filtering with
Enter-launches-first-match, Escape's clear-then-dismiss, wrap-around cursor
movement, folder fold/expand from both pointer and keyboard, focus cycling
with type-to-filter, and cursor-follows-view scrolling. The input tests cover
both buttons' presses, click-away dismissal that activates nothing beneath,
secondary-press dismissal, wheel scrolling of an overflowing popup, and hover
repaint latching. The drag-to-pin tests cover the whole gesture: a press that
barely moves is still a click and launches, travel past the shared threshold
offers the entry exactly once however far the pointer goes on, the release is
a drop that launches neither the pressed row nor the row it ended over and
leaves the popup open, `Escape` withdraws and keeps the popup open (the
release that follows drops nothing and a second `Escape` dismisses as ever),
and a folder header arms nothing.
The rendering tests probe painted pixels for the bar regions; both launchers
resting bare on the bar (glyph ink on the bar's own fill, with no role fill,
no rim, and no reactive edge); the hover wash appearing under the pointer
in that slot alone and never as an edge; the Library button compressed while
its popup is open; focused / unfocused / minimised task fills; notification
glyphs (including the unknown-asset fallback and cache retint on theme
switch); clock and truncated task-title text; and the popup's panel, rows,
hover/selection states, placeholder ink, scrollbar, and dark / light /
high-contrast rendering.
The icon-artwork tests drive `render` with a recording lookup: the two
launcher buttons ask for the `Library` and `Folder` kinds at their drawn side
and blit what comes back, an application slot with no artwork of its own
falls back to its kind's artwork before the glyph, two unpinned running tasks
each draw their own application's picture and only their own (a task the
session could not attribute keeping the shared glyph, and artwork offered for
a window the bar does not list changing nothing), the popup asks only for
the entry rows the viewport shows (and, after a scroll, only for the rows
that just appeared), a rebuild drops stale row artwork, and a bar rendered
through `NoArtwork` still draws every element from its built-in glyphs — the
property that keeps a machine with no `/System/Graphics` fully usable.
The Switchboard tray tests cover the slot's trailing-most placement on every
edge (and its survival order on degenerate screens), the summary→state derive
matrix (absent service, calm top-task preview, jobs, every pressure kind,
recovery, hung, and compositions), repaint latching, hover readout geometry
on every edge and its survival across a live update, the tap-or-hold gesture
(tap → Tasks, hold → Recovery on the first sample past the threshold and on a
still-fingered release, never twice for one press, nothing at all once the
press drags off, and no gesture armed by a press elsewhere on the bar), the
readout's "Open Switchboard" action and its inert press claim away from it,
scroll cycling and the middle-click previous-task switch with their
fail-closed cases, and pixel probes of the capsule, rail, seam, and badge
tones across themes.

Those tests all run the model headless. What only a real machine can show
is that the bar is wired to the volume and to the services around it, and
`tests/integration/taskbar_pin_qemu_aarch64` proves that on the aarch64
`virt` board: it boots the graphical session, opens the program library,
raises an entry's context menu with a secondary press, chooses *Pin to
taskbar*, opens the Switchboard from its capsule, and clicks the pin the
gesture created. Its coordinates are not hand-copied — the host script
drives this very crate's `TaskbarInput` with the events the guest will
receive and reads the rectangles back out of `Taskbar::layout`,
`Taskbar::menu_layout`, and `Menu::row_rect` — and it passes only when the
pin store's directory is created on disk, the Switchboard panel is served
and painted, and the *pinned bundle* is loaded. Two scan-out readbacks
check the bar itself: the first pin slot is uniformly the bar's own plate
colour before anything is pinned, and carries the shortcut's glyph
afterwards with the panel beside it.
