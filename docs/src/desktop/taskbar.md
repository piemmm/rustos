# Traditional desktop taskbar

The taskbar (`userland/gui/taskbar`, crate `rustos-taskbar`) is the
GNOME/Windows-style bar pinned to a configured screen edge (`AGENTS.md` §10,
`PLAN.md` Stage 7). This page describes the **layout-and-model** increment:
the geometry of every region, pointer hit-testing for input routing, and the
start-menu / task-list / notification-area state machines. Pixel rendering
and the live window-manager IPC build on this model in later increments.

## Where it sits

As a `userland/gui/*` crate the taskbar depends only on `lib/*`: the shared
[`rustos-geometry`](../lib/overview.md) coordinate types and the shared
[`rustos-theme`](theming.md) definition. It never depends on the window
manager or on any sibling userland crate, and nothing depends on it in turn
(`AGENTS.md` §17.4, §17.3) — the desktop is an optional, one-way-dependent
frontend, so a headless image simply omits it.

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
- **Clock** — anchored to the trailing end.

`BarLayout::compute` turns the config plus the current task and icon counts
into the screen `Rect` of every region. All arithmetic saturates, so a
pathological screen size or extent fails closed *inside* the bar rather than
wrapping (`AGENTS.md` §2.9); a task or icon slot that does not fit its region
is `Rect::EMPTY` and therefore never hit. `BarLayout::hit_test` maps a
pointer to the `Hit` element under it (start button, a task index, a
notification index, or the clock), which is what input routing will dispatch.

## Rounded edges

The taskbar supports rounded corners, but it does **not** draw them itself.
`BarLayout::corner_radius` carries the radius taken from the active theme's
`taskbar_corner_radius` metric; the window manager applies that radius through
its single anti-aliased rounded-corner path, exactly as it rounds windows.
There is no second rounded-corner implementation (`AGENTS.md` §2.2).

## Start menu

The start menu is **not** an application launcher at this stage. It is
populated only with the session controls — log out, lock, shut down, restart
— each carried by a `MenuEntry` with a stable `MenuEntryId` and a `MenuAction`.
`StartMenu::activate` returns the entry's action and closes the menu; an
unknown id changes nothing and returns `None` (fail closed, `AGENTS.md` §5.4 /
§2.9). Launcher entries are a later increment: they arrive as a new
`MenuAction` variant, so the list/activate interface does not change when they
land (`AGENTS.md` §2.4 — extend, do not creep).

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

## Theming

The taskbar reads its corner radius from the active theme and adopts a new one
with `Taskbar::apply_theme`; the rest of its state is untouched, so a runtime
dark/light switch needs no model relayout (`AGENTS.md` §10). Colours and fonts
are wired through the same theme in the rendering increment.

## Tests

The crate's headless unit tests cover edge/orientation, the start-menu
session-control population and fail-closed activation, the task-list
focus/minimise rule, notification add/remove deduplication, the region layout
and hit-testing for a bottom bar, vertical-bar layout, all four edges,
overflow clipping, degenerate (tiny-screen) fail-closed behaviour, and the
theme-driven corner radius.
