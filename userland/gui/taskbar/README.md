# rustos-taskbar

The RustOS traditional desktop **taskbar** (`AGENTS.md` §10, `PLAN.md`
Stage 7): a GNOME/Windows-style bar pinned to a configured screen edge.

This crate is the Stage 7 **layout, model, and rendering** increment. It
owns:

- **Layout** — `TaskbarConfig` (edge, thickness, per-region extents) and
  `BarLayout::compute`, which lays the bar out along its main axis: a start
  button at the leading end, the running-task list in the middle, the
  notification-icon area packed before the clock, and the clock anchored to
  the trailing end. All arithmetic saturates, so a degenerate screen size
  fails closed inside the bar (`AGENTS.md` §2.9).
- **Hit-testing** — `BarLayout::hit_test` maps a pointer to the `Hit` element
  under it (start button, a task, a notification icon, or the clock) for
  input routing. A region slot that does not fit is `Rect::EMPTY` and is never
  hit.
- **Start menu** — `StartMenu` holds only the session controls (log out,
  lock, shut down, restart) at this stage; it is shaped so launcher entries
  can be added later as a new `MenuAction` variant without changing the
  list/activate interface (`AGENTS.md` §2.4).
- **Task list** — `TaskList` tracks one entry per top-level window with the
  familiar click-to-activate / minimise-restore rule.
- **Notification area** — `NotificationArea`, an ordered set of status icons.
- **Clock** — `Clock` holds the display label the caller sets (formatting a
  `Time64` value is an upstream concern, `AGENTS.md` §21).
- **Rendering** — `render` paints the bar into a `rustos-raster` `Surface`:
  each region is filled with its theme colour role, then the clock label and
  task titles are drawn on top with the `rustos-font` built-in bitmap face,
  each in the foreground role matching its background and truncated to fit its
  region. The surface is rectangular — the window manager rounds it (see
  below) — and the colour/blit algebra is reused from `lib/*`, never
  duplicated (`AGENTS.md` §2.2).

## Rounded edges

The taskbar does not draw its own rounded corners. `BarLayout::corner_radius`
carries the radius from the active theme's `taskbar_corner_radius`; the window
manager applies it through its single anti-aliased rounded-corner path, the
same one it uses for windows. There is no second implementation
(`AGENTS.md` §2.2).

## Dependencies and layering

The crate depends only on `lib/*`: `rustos-geometry` (the shared `Point` /
`Rect` vocabulary), `rustos-raster` (the premultiplied-alpha `Color`/`Surface`
the window manager also paints with), `rustos-font` (the shared text
rasteriser, `AGENTS.md` §16.4), and `rustos-theme` (the single shared theme
definition). It does **not** depend on the window manager or any sibling
userland crate, and nothing depends on it in turn
(`AGENTS.md` §17.4, §17.3): the desktop is an optional, one-way-dependent
frontend, so a headless image omits it cleanly.

It is `no_std` (with `alloc`). No `unsafe`, and no `unwrap`/`expect`/`panic!`
in production paths (`AGENTS.md` §2.9).

## Still to come (Stage 7)

The live window-manager IPC wiring, notification-icon artwork, selecting a
font face from the theme's `FontSpec` roles once installed fonts exist, and
launcher entries in the start menu.
