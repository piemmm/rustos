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
artwork** (scalable, themeable vector glyphs) and per-application **pin
artwork** (rasterised by the session, blitted here).

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

- **Library button** — the first of the two permanent leading launchers:
  the accent-filled invoker that opens the program-library popup.
- **Files button** — the second permanent launcher: a quiet folder-glyph
  button that opens the file manager. The two leading buttons are fixed —
  never reordered, never removable (`plans/NEW-TASKBAR.md` T4).
- **Pin strip** — one compact square slot per user-pinned application
  shortcut, in the user's stored order, between the launchers and the task
  list (`plans/NEW-TASKBAR.md` T6). Zero-length (but still positioned) when
  nothing is pinned; `BarLayout::pin_drop_index` maps a drop point in the
  strip-plus-task band to the pin-list insertion index for the
  drag-to-taskbar gesture (T7).
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
  outrank it on a degenerate screen. Hover (or a pinning primary press)
  expands the capsule's instrument readout, presented like the other
  popovers (`Taskbar::tray_readout_layout`).

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
radius the painter draws can never come from two different themes. It also
carries a **repaint latch**: pixel-only state changes (a launcher hover, a
popup scroll or edit, a theme switch) set it, and the embedder drains it with
`Taskbar::take_repaint` to re-present exactly when something changed — one
present per visual change, no per-frame busy repainting (`AGENTS.md` §2.16).

## Rendering

`TaskbarRenderer::render` paints the taskbar into a `tairix-raster` `Surface`
sized to the bar using the taskbar's own theme, filling each region with a
colour role from the `Palette`:

- the bar background is the **raised surface** colour;
- the **Library button** is the shared `tairix-controls` `IconButton` in the
  `Primary` role — the accent-filled plate carrying the nine-tile library
  glyph — pressed in while its popup is open, hover-lit under the pointer;
- the **Files button** is a quiet (`Neutral`) `IconButton` carrying the
  folder glyph;
- each **pin slot** and each **task slot** is one shared `tairix-controls`
  `TaskbarItem` — the bar's application buttons have exactly one visual
  recipe (`AGENTS.md` §2.2). A pin uses the icon-only presentation (a
  centred icon sized off the plate); a task shows its icon beside the
  truncated window title. The item's `TaskVisibility` paints the state: the
  **active** window's item shows the lower accent seam, a **minimised** one
  recesses its plate and shows the muted tick, a running one rests on its
  plate, and a **closed** pin (its application not running) rests without a
  plate at all — only the icon sits on the bar — until hovered;
- a pin's per-application **artwork** (its bundle icon, rasterised by the
  session through the sandboxed icon pipeline — see
  [the session](session.md)) is blitted through the control in place of the
  built-in glyph, and a running task whose window matches a pin **borrows**
  that same artwork, so one application shows one icon everywhere;
- each notification icon slot draws a **scalable vector glyph** (see
  *Notification icons* below), tinted in the **muted** foreground colour;
- the **Switchboard capsule** is the shared `tairix-controls` `TraySignal`
  drawn in its slot — the mixer-glyph plate with its live badge, seam, rail,
  and beads — and `TaskbarRenderer::render_tray_readout` paints the expanded
  instrument readout as its own popover surface, rounded by the window
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
glyphs are not, so it holds a `tairix-raster` `RasterCache` of rasterised
glyphs across frames. The renderer is the right home for that state: the
`Taskbar` model stays pure data. `render_library` (the popup painter) needs
no cache of its own, so it stays a `&self` method.

## Notification icons

The notification area holds an ordered list of status icons, each with a
stable `IconId` and a theme **asset id**. When the bar renders, every
notification slot resolves its asset id to a `tairix-icon` `IconKind`
(`IconKind::for_asset`, falling back to a generic glyph for an unknown id,
`AGENTS.md` §2.9), builds the matching scalable `VectorIcon` in the **muted**
foreground colour, rasterises it to the slot size at the active scale, and
composites it onto the bar through `tairix-raster`'s `Surface::blit`. The
glyph is artwork, not a flood fill, so the raised bar background shows through
around it. The icons rasterise through the *same* supersampled polygon path
(`Surface::fill_polygon`) the cursors use — there is no second scan converter
(`AGENTS.md` §2.2) — and a slot too small to hold a glyph paints nothing
rather than panicking (`AGENTS.md` §2.9).

Rasterising a glyph is the expensive step, so the `TaskbarRenderer` does it
only once per tint and size: its `RasterCache` is keyed by `IconKind` within a
`(tint, pixel-size, set-generation)` epoch, so repeated frames reuse the
cached glyph and only a theme change (new tint), a scale change (new size), or
an installed icon set (new generation) re-rasterises — the SVG-first "convert
once, re-render only on a scale or theme change" rule (`AGENTS.md` §10),
sharing the one cache the window manager uses for cursors (`AGENTS.md` §2.2).
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
entry count; an entry row is indented beneath its folder with the app-bundle
glyph; the hovered row raises its fill, and the keyboard cursor row shows the
shared selection rail and focus ring — the calm placeholder when nothing is
listed, and the scrollbar when the rows overflow. Like the bar it is a
rectangular surface the window manager places and rounds with
`LibraryLayout::corner_radius`, and it returns `None` while the popup is
closed (`AGENTS.md` §2.9).

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
per pin — a display label, the class glyph, optional rasterised artwork,
the program-library entry it references (when it is an `entry` pin), and
the running desktop window it currently matches, if any
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
- a primary press on the **Switchboard capsule** pins its readout open (a
  second press — or one anywhere else — releases the pin; the press that
  opens the overview window lands with that window,
  `plans/NEW-TASKBAR.md` T11), a press inside the open readout is claimed
  inert like the notification popover's chrome, **scrolling** over the
  capsule or its readout cycles the running tasks (wrapping both ways), and
  a **middle** press over the capsule switches to the previous task (the
  task list's MRU-of-two) — each failing closed when there is nothing to
  cycle or return to.

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

- a primary press on a row activates it (a folder toggles, an entry reports
  `LibraryLaunch { entry }` and closes the popup);
- a primary press on the **Library button** toggles the popup shut
  (`LibraryDismissed`);
- a press **anywhere else** — any button — dismisses the popup without
  acting on what it landed on (`LibraryDismissed`), the standard click-away
  behaviour;
- scroll wheels the row viewport; keys drive the keyboard model above.

Popup-internal changes (a hover, a scroll, an edit, a fold) are reported as
`Ignored` with the repaint latch set, so the embedder re-presents without
mistaking them for actions.

## Theming

The taskbar owns the active theme (see *The owned theme*) and adopts a new
one with `Taskbar::apply_theme`; the rest of its state is untouched, so a
runtime dark/light switch needs no model relayout (`AGENTS.md` §10). The
region **colours**, the control plates, and the text **foreground** roles are
wired through that theme by the renderer. The interactive light/dark switch
lives in the Switchboard's System menu (`plans/NEW-TASKBAR.md` T13); until
that lands the session switches themes programmatically
(`DesktopSession::set_theme`).

## Tests

The crate's headless unit tests cover edge/orientation, the task-list
focus/minimise rule, notification add/remove deduplication, the region layout
and hit-testing for a bottom bar (both permanent launchers included),
vertical-bar layout, all four edges, overflow clipping, degenerate
(tiny-screen) fail-closed clipping of the launcher buttons, DPI scaling of
layout and hit-testing, and the theme-driven corner radius and repaint latch.
The pin tests cover the strip's placement between the launchers and the
task list (and its reflow as pins come and go), pin hit-testing on every
edge, the drop-index mapping (leading/trailing halves, the empty-strip
first drop, appends past the last slot, vertical bars), the live visibility
derivation (running / active / minimised / closed / stale match), pin
activation (launch vs the task click rule), the context menu's rows,
modality, keyboard path, click-away, entry pin/unpin verb switch, menu
geometry on every edge, and the rendered pin plates — artwork override,
built-in glyph fallback, a focused pin's accent seam, and a task borrowing
its pin's artwork.
The popup model tests cover taxonomy-ordered folders with name-sorted
entries, hidden empty folders, folder labels, the calm empty and no-match
placeholders, the deterministic reopen state, case-insensitive filtering with
Enter-launches-first-match, Escape's clear-then-dismiss, wrap-around cursor
movement, folder fold/expand from both pointer and keyboard, focus cycling
with type-to-filter, and cursor-follows-view scrolling. The input tests cover
both buttons' presses, click-away dismissal that activates nothing beneath,
secondary-press dismissal, wheel scrolling of an overflowing popup, and hover
repaint latching. The rendering tests probe painted pixels for the bar
regions, the accent Library plate and quiet Files glyph, focused / unfocused
/ minimised task fills, notification glyphs (including the unknown-asset
fallback and cache retint on theme switch), clock and truncated task-title
text, and the popup's panel, rows, hover/selection states, placeholder ink,
scrollbar, and dark / light / high-contrast rendering.
The Switchboard tray tests cover the slot's trailing-most placement on every
edge (and its survival order on degenerate screens), the summary→state derive
matrix (absent service, calm top-task preview, jobs, every pressure kind,
recovery, hung, and compositions), repaint latching, hover/pin readout
geometry on every edge, the readout's inert press claim, scroll cycling and
the middle-click previous-task switch with their fail-closed cases, and
pixel probes of the capsule, rail, seam, and badge tones across themes.
