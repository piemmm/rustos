# Traditional desktop taskbar

The taskbar (`userland/gui/taskbar`, crate `rustos-taskbar`) is the
GNOME/Windows-style bar pinned to a configured screen edge (`AGENTS.md` §10,
`PLAN.md` Stage 7). This page describes the **layout, model, and rendering**:
the geometry of every region, pointer hit-testing for input routing, the
start-menu / task-list / notification-area state machines, and painting those
regions — including the clock label and task-title **text** — into a themed
pixel surface, the **start-menu popup** geometry and rendering, plus
**routing** pointer presses into taskbar actions (including selecting an entry
in the open menu). Notification-icon artwork and the live window-manager IPC
build on this model in later increments.

## Where it sits

As a `userland/gui/*` crate the taskbar depends only on `lib/*`: the shared
[`rustos-geometry`](../lib/overview.md) coordinate types, the shared
`rustos-raster` rasteriser (the premultiplied-alpha `Color`/`Surface` the
window manager also paints with), the shared `rustos-font` text rasteriser
(the built-in bitmap face and glyph blitter, `AGENTS.md` §16.4), the shared
`rustos-input` pointer-event vocabulary (the same `PointerButton`/`InputEvent`
the window manager routes), and the shared [`rustos-theme`](theming.md)
definition. It never depends on the window manager or on any sibling userland
crate, and nothing depends on it in turn (`AGENTS.md` §17.4, §17.3) — the
desktop is an optional, one-way-dependent frontend, so a headless image simply
omits it.

## Layout

A `TaskbarConfig` fixes the screen `Edge`, the bar `thickness`, and the
per-region extents. The edge fixes the bar's `Orientation`: a top/bottom bar
runs horizontally (its long *main axis* is `x`), a left/right bar runs
vertically (main axis `y`). Regions are laid out along the main axis and the
code is otherwise orientation-agnostic.

From the leading end to the trailing end:

- **Start button** — a fixed-width button at the leading end that opens the
  start menu.
- **Task list** — the flexible middle region, between the start button and
  the notification area, holding one fixed-width slot per running task.
- **Notification area** — status/notification icons, packed immediately
  before the clock.
- **Clock** — anchored to the trailing end. Its display text is held by a
  `Clock` model whose label the caller sets (formatting a `Time64` value into
  a string is an upstream concern, `AGENTS.md` §21); the bar stores only the
  text to draw.

`BarLayout::compute` turns the config plus the current task and icon counts
into the screen `Rect` of every region. All arithmetic saturates, so a
pathological screen size or extent fails closed *inside* the bar rather than
wrapping (`AGENTS.md` §2.9); a task or icon slot that does not fit its region
is `Rect::EMPTY` and therefore never hit. `BarLayout::hit_test` maps a
pointer to the `Hit` element under it (start button, a task index, a
notification index, or the clock), which is what input routing dispatches
(see *Input routing*).

## Rounded edges

The taskbar supports rounded corners, but it does **not** draw them itself.
`BarLayout::corner_radius` carries the radius taken from the active theme's
`taskbar_corner_radius` metric; the window manager applies that radius through
its single anti-aliased rounded-corner path, exactly as it rounds windows.
There is no second rounded-corner implementation (`AGENTS.md` §2.2).

## Rendering

`render` paints the taskbar into a `rustos-raster` `Surface` sized to the bar,
filling each region with a colour role from the active theme's `Palette`:

- the bar background is the **raised surface** colour;
- the start button is the **accent**;
- each task slot is the **accent** when it is the focused, non-minimised task,
  the **raised surface** colour (so it recedes into the bar) when minimised,
  and the plain **surface** colour otherwise — which the palette guarantees
  reads as distinct from the raised background;
- each notification icon slot is the **muted** foreground colour.

On top of those fills, `render` draws **text** with the shared `rustos-font`
`BitmapFont` (the built-in 5×7 monospace face): the clock label is centred in
the clock region, and each task slot shows its window title aligned to the
leading edge. Each label takes the foreground role that matches its background
— `on_accent` over a focused (accent) slot, the **muted** foreground over a
minimised slot, and `on_surface` otherwise (and for the clock over the raised
bar) — so text stays legible after a theme switch. A label is truncated to the
characters that fit its region, so text never spills into a neighbouring slot
(`AGENTS.md` §2.9), and glyphs are composited through `rustos-raster`'s one
premultiplied-alpha `over` path — no blitter or colour algebra is duplicated
here (`AGENTS.md` §2.2).

The surface is rectangular: the taskbar paints no corners. The window manager
presents it and applies `BarLayout::corner_radius` through its single
anti-aliased rounded-corner path, exactly as it rounds windows (`AGENTS.md`
§2.2). The colour algebra is not duplicated here either — `rustos-raster`
owns the one premultiplied-alpha implementation and the
`From<Rgba> for Color` edge. Region rectangles are screen-space; each is
translated into the bar's local surface space, the translation saturates, and
`fill_rect` clips, so a degenerate layout paints nothing rather than
panicking (`AGENTS.md` §2.9). Switching themes simply re-renders with the new
palette.

## Start menu

The start menu is **not** an application launcher at this stage. It is
populated only with the session controls — log out, lock, shut down, restart
— each carried by a `MenuEntry` with a stable `MenuEntryId` and a `MenuAction`.
`StartMenu::activate` returns the entry's action and closes the menu; an
unknown id changes nothing and returns `None` (fail closed, `AGENTS.md` §5.4 /
§2.9). Launcher entries are a later increment: they arrive as a new
`MenuAction` variant, so the list/activate interface does not change when they
land (`AGENTS.md` §2.4 — extend, do not creep).

### Popup geometry and rendering

`MenuLayout::compute` is the start-menu popup's geometry. The popup opens
*outward* from the start button on the bar's edge — above a bottom bar, below
a top bar, and to the inner side of a left or right bar — with its leading
edge aligned to the start button. It carries the popup `panel` rectangle, the
`corner_radius` taken from the theme's `popup_corner_radius`, and one `Rect`
per entry stacked down the panel. The popup width, row height, and radius are
*logical* lengths converted to physical pixels through the one shared
`Scale::scale_length`, so the menu scales with the desktop DPI exactly as the
bar does (`AGENTS.md` §10, §2.2), and all arithmetic saturates so a
pathological screen or scale fails closed (`AGENTS.md` §2.9).
`MenuLayout::hit_test` maps a pointer to the entry index under it.

Like the bar, the popup is a *rectangular* surface the window manager places
and rounds: `render_menu` paints a raised-surface panel with each entry's
label drawn on top through the same `rustos-font` / `rustos-raster` path the
bar uses (no second blitter or rounded-corner path, `AGENTS.md` §2.2), and
returns `None` when the menu is closed. `Taskbar::menu_layout` computes the
geometry from the current state.

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

## Input routing

`TaskbarInput` is the taskbar's input router, the counterpart of the window
manager's `InputRouter`. It consumes the **same** shared `rustos-input`
`InputEvent` stream the compositor routes (`AGENTS.md` §17.4, §2.2), tracking
the pointer position from motion events and acting only on a primary-button
press. A press is hit-tested against the current `BarLayout` and dispatched to
the model, reported as a `TaskbarResponse`:

- a press on the **start button** toggles the start menu
  (`StartMenuToggled { open }`);
- a press on a **task slot** applies the click-to-activate / minimise rule and
  reports the `ActivateOutcome` (`TaskActivated { id, outcome }`);
- a press on a **notification icon** reports its `IconId`
  (`NotificationActivated`);
- a press on the **clock** reports `ClockPressed`.

A non-primary button, a release, or a press that misses every region changes
nothing and is reported as `Ignored` (fail closed, `AGENTS.md` §2.9).

While the start menu is **open** the router treats it as modal, so a click
lands on exactly one thing (`AGENTS.md` §2.1):

- a press inside the popup selects the entry under the pointer, performing its
  action and closing the menu (`MenuEntrySelected { id, action }`);
- a press on the **start button** keeps its toggle behaviour and closes the
  menu (`StartMenuToggled { open: false }`);
- a press **anywhere else** dismisses the menu without acting on what it
  landed on (`StartMenuDismissed`) — the standard click-away behaviour.

## Theming

The taskbar reads its corner radius from the active theme and adopts a new one
with `Taskbar::apply_theme`; the rest of its state is untouched, so a runtime
dark/light switch needs no model relayout (`AGENTS.md` §10). The region
**colours** and the text **foreground** roles are wired through the same theme
by `render` (see *Rendering*). The text is drawn with the built-in
`rustos-font` face today; selecting a face from the theme's `FontSpec` roles
joins this once installed font faces exist.

## Tests

The crate's headless unit tests cover edge/orientation, the start-menu
session-control population and fail-closed activation, the task-list
focus/minimise rule, notification add/remove deduplication, the region layout
and hit-testing for a bottom bar, vertical-bar layout, all four edges,
overflow clipping, degenerate (tiny-screen) fail-closed behaviour, and the
theme-driven corner radius. The rendering tests assert the painted surface
matches the bar dimensions and that the background, start button, focused /
unfocused / minimised task slots, and notification icons take the expected
theme colour, including that a dark↔light switch repaints the background. The
text tests assert that a set clock label paints `on_surface` glyphs inside the
clock region (and an empty label paints none), that a focused task's title is
drawn in `on_accent`, and that a title too long for its slot is truncated
rather than spilling into the next slot. The input-routing tests assert that a
primary press on the start button toggles the menu, on a task slot applies the
activate/minimise rule, and on a notification icon and the clock reports them,
and that a non-primary button, a release, a miss, and pointer motion all leave
the model unchanged. A further group covers the start-menu popup: that it
opens outward on each edge and scales with DPI, that its rows hit-test
correctly, that a press inside the open popup selects the entry and closes the
menu, that a press outside it dismisses the menu without activating the task
beneath, that the start button still toggles it shut, that `render_menu`
paints the panel and entry labels (and is `None` when closed), and that an
empty menu produces a fail-closed empty popup.
