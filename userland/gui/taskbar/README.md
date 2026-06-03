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
- **Variable DPI** — the bar's extents, thickness, and corner radius are
  *logical* pixels (the screen dimensions are physical). `BarLayout::compute`
  takes a `rustos-geometry` `Scale` and converts the logical lengths to
  physical pixels through the one shared `Scale::scale_length` (`AGENTS.md`
  §10, §2.2). A `Taskbar` carries a settable `scale()`/`set_scale()`, so a
  runtime DPI change relays the bar at the new density without rebuilding its
  model — exactly as a theme switch does.
- **Hit-testing** — `BarLayout::hit_test` maps a pointer to the `Hit` element
  under it (start button, a task, a notification icon, or the clock) for
  input routing. A region slot that does not fit is `Rect::EMPTY` and is never
  hit.
- **Input routing** — `TaskbarInput` consumes the shared `rustos-input`
  `InputEvent` stream (the same one the window manager routes) and turns a
  primary press into a `TaskbarResponse`: toggling the start menu, applying
  the task activate/minimise rule, or reporting a notification-icon or clock
  press. While the start menu is open it is modal: a press inside the popup
  selects the entry under the pointer (`MenuEntrySelected`), a press on the
  start button toggles the menu shut, and a press anywhere else dismisses it
  (`StartMenuDismissed`) without acting on what it landed on — one click does
  one thing (`AGENTS.md` §2.1). Anything else, or a press that misses every
  region, is `Ignored` (fail closed, `AGENTS.md` §2.9).
- **Start menu** — `StartMenu` is seeded with the session controls (log out,
  lock, shut down, restart) on the fixed ids `1..=4`; `add_launcher` appends
  **application launcher** entries and `add_appearance_toggle` a **light/dark
  toggle** after them. All kinds are ordinary `MenuEntry` values distinguished
  by their `MenuAction` (`Session(SessionControl)`, `Launch(LauncherId)`, or
  `ToggleAppearance`), so each was added without changing the list/activate
  interface (`AGENTS.md` §2.4). The taskbar cannot spawn processes or own a
  theme: activating a launcher reports its `LauncherId` and activating the
  toggle reports `ToggleAppearance`, which the session glue resolves —
  launching an application bundle (`AGENTS.md` §16.5) or calling
  `ThemeRegistry::toggle_appearance` and relaying the new theme (`AGENTS.md`
  §10). `MenuLayout::compute` is its
  popup geometry: the panel opens *outward* from the start button on the
  bar's edge (above a bottom bar, below a top bar, to the inner side of a
  left/right bar) with one scale-aware row per entry, and `MenuLayout::hit_test`
  maps a pointer to the entry under it. Like the bar the popup is a
  rectangular surface the window manager places and rounds through its single
  rounded-corner path (`MenuLayout::corner_radius`, §2.2).
- **Task list** — `TaskList` tracks one entry per top-level window with the
  familiar click-to-activate / minimise-restore rule.
- **Notification area** — `NotificationArea`, an ordered set of status icons.
- **Clock** — `Clock` holds the display label the caller sets (formatting a
  `Time64` value is an upstream concern, `AGENTS.md` §21).
- **Rendering** — `TaskbarRenderer::render` paints the bar into a
  `rustos-raster` `Surface`: each region is filled with its theme colour role,
  then the notification-icon glyphs, clock label, and task titles are drawn on
  top with the `rustos-font` built-in bitmap face, each in the foreground role
  matching its background and truncated to fit its region. The surface is
  rectangular — the window manager rounds it (see below) — and the colour/blit
  algebra is reused from `lib/*`, never duplicated (`AGENTS.md` §2.2). The
  renderer is stateful so it can hold a `rustos-raster` `RasterCache` of the
  rasterised notification glyphs across frames: a glyph is converted once per
  tint and size and re-rendered only on a theme or scale change (the SVG-first
  rule, `AGENTS.md` §10), sharing the one cache the window manager uses for
  cursors (`AGENTS.md` §2.2). The `Taskbar` model stays pure data.
  `render_menu` (a `&self` method, no cache — the popup is text only) paints
  the open start-menu popup the same way, returning `None` when closed.
- **Notification icon set** — the renderer draws each notification glyph from
  a `rustos-icon` `IconSet`: the built-in glyph set until `set_icons` installs
  one decoded from the on-disk `/System/Graphics` SVG assets
  (`IconSet::from_assets`). A loaded asset keeps its authored colours; a kind
  the assets omit keeps its tinted built-in glyph, so a corrupt asset set can
  never blank a status icon (`AGENTS.md` §10/§2.9). Installing a set bumps the
  glyph cache's generation, so the next frame re-rasterises from the new set
  (`AGENTS.md` §2.2). Reading the asset bytes needs a filesystem capability and
  is the desktop's job, so the set is built outside this crate and handed in.

## Rounded edges

The taskbar does not draw its own rounded corners. `BarLayout::corner_radius`
carries the radius from the active theme's `taskbar_corner_radius`; the window
manager applies it through its single anti-aliased rounded-corner path, the
same one it uses for windows. There is no second implementation
(`AGENTS.md` §2.2).

## Dependencies and layering

The crate depends only on `lib/*`: `rustos-geometry` (the shared `Point` /
`Rect` vocabulary), `rustos-input` (the shared `PointerButton`/`InputEvent`
vocabulary the window manager also routes), `rustos-raster` (the
premultiplied-alpha `Color`/`Surface` the window manager also paints with),
`rustos-font` (the shared text rasteriser, `AGENTS.md` §16.4), and
`rustos-theme` (the single shared theme definition). It does **not** depend on
the window manager or any sibling userland crate, and nothing depends on it in
turn (`AGENTS.md` §17.4, §17.3): the desktop is an optional, one-way-dependent
frontend, so a headless image omits it cleanly.

It is `no_std` (with `alloc`). No `unsafe`, and no `unwrap`/`expect`/`panic!`
in production paths (`AGENTS.md` §2.9).

## Still to come (Stage 7)

The live window-manager IPC wiring and selecting a font face from the theme's
`FontSpec` roles once installed fonts exist.
