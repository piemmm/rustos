# tairix-taskbar

The TAIRiX traditional desktop **taskbar** (`AGENTS.md` §10, `PLAN.md`
Stage 7, `plans/NEW-TASKBAR.md`): a GNOME/Windows-style bar pinned to a
configured screen edge.

This crate is the Stage 7 **layout, model, and rendering** increment. It
owns:

- **Layout** — `TaskbarConfig` (edge, thickness, per-region extents) and
  `BarLayout::compute`, which lays the bar out along its main axis: the two
  permanent leading launcher buttons (**Library**, then **Files** — fixed
  order, never removable), the running-task list in the middle, the
  notification-icon area packed before the clock, and the clock anchored to
  the trailing end. All arithmetic saturates, so a degenerate screen size
  fails closed inside the bar (`AGENTS.md` §2.9).
- **Variable DPI** — the bar's extents, thickness, and corner radius are
  *logical* pixels (the screen dimensions are physical). `BarLayout::compute`
  takes a `tairix-geometry` `Scale` and converts the logical lengths to
  physical pixels through the one shared `Scale::scale_length` (`AGENTS.md`
  §10, §2.2). The `Taskbar` stores **no** scale: the desktop density belongs
  to the output and is owned by the compositor, so `layout`, `hit_test`, and
  `library_layout` take the `Scale` as a parameter and the presenter supplies
  `Compositor::scale` at present time. A runtime DPI change is therefore just
  a re-present at the new density — transparent to the bar, no model rebuild.
- **The owned theme and the repaint latch** — the bar owns a copy of the
  active `Theme` (`Taskbar::theme`, swapped by `apply_theme`), so layout,
  hit-testing, and painting read one definition. Pixel-only state changes (a
  launcher hover, a popup scroll or edit, a theme switch) set a repaint
  latch the embedder drains with `take_repaint`, so one present follows each
  visual change — never a per-frame busy repaint (`AGENTS.md` §2.16).
- **Hit-testing** — `BarLayout::hit_test` maps a pointer to the `Hit` element
  under it (the Library button, the Files button, a task, a notification
  icon, or the clock) for input routing. A region slot that does not fit is
  `Rect::EMPTY` and is never hit.
- **Input routing** — `TaskbarInput` consumes the shared `tairix-input`
  `InputEvent` stream (the same one the window manager routes) and turns a
  primary press into a typed `TaskbarResponse`: `OpenLibrary` (the Library
  button opened the popup), `OpenFiles`, the task activate/minimise rule
  (`TaskActivated`), or a notification-icon / clock press. While the
  program-library popup is open it is modal and consumes the whole stream —
  presses, releases, scroll, and keys all route into the popup; a press on
  the Library button toggles it shut and a press anywhere else dismisses it
  (`LibraryDismissed`) without acting on what it landed on — one click does
  one thing (`AGENTS.md` §2.1). Anything else, or a press that misses every
  region, is `Ignored` (fail closed, `AGENTS.md` §2.9).
- **The program-library popup** — `LibraryPopup` (`plans/NEW-TASKBAR.md`
  T5), a pure model over the **resolved** `tairix-proglib` `Catalog` the
  session hands it (`set_catalog`; the popup never touches the VFS),
  composed from the shared `lib/controls` vocabulary: a `Panel` anchored
  back at the Library button, a `SearchField` filter, one `ListRow` per
  folder or entry, and a `ScrollBar` on overflow. Folders follow the closed
  taxonomy order with name-sorted entries; empty folders are hidden; an
  empty library or filter shows a calm placeholder. Opening is
  deterministic (search cleared, folders expanded, cursor at top). The
  keyboard model is complete — `Tab` cycles search↔rows, arrows wrap,
  Home/End/PageUp/PageDown jump with the view following, Enter/space
  activates, Left/Right fold/climb/descend, typing filters
  case-insensitively, Enter in the search launches the first match, Escape
  clears then dismisses. Choosing an entry reports
  `LibraryLaunch { entry }`; the session resolves the bundle and launches
  it — the taskbar cannot spawn processes (`AGENTS.md` §16.5).
  `Taskbar::library_layout` computes the popup geometry (outward from the
  bar on every edge, clamped to the screen, chrome measured by probing the
  shared `Panel` rather than re-deriving it, §2.2).
- **Task list** — `TaskList` tracks one entry per top-level window with the
  familiar click-to-activate / minimise-restore rule. `set_focused` mirrors the
  window manager's keyboard focus into the highlight (and restores the focused
  task), so a window the user clicks directly stays in step with the bar; an
  unknown id is rejected without disturbing the highlight (`AGENTS.md` §2.9).
  The session glue (`tairix-desktop-session`'s `TaskBridge`) drives this from
  the window stack.
- **Notification area** — `NotificationArea`, an ordered set of status icons.
- **Clock** — `Clock` holds the display label the caller sets (formatting a
  `Time64` value is an upstream concern, `AGENTS.md` §21).
- **Rendering** — `TaskbarRenderer::render` paints the bar into a
  `tairix-raster` `Surface` using the taskbar's own theme: the two leading
  `IconButton`s (Library is the accent-filled `Primary` invoker carrying the
  `lib/icon` `Library` glyph, pressed-in while its popup is open; Files a
  quiet folder glyph) draw with their live hover state, each remaining
  region is filled with its theme colour role, then the notification-icon
  glyphs, clock label, and task titles are drawn on top with the
  `tairix-font` face, each in the foreground role matching its background
  and truncated to fit its region. The surface is rectangular — the window
  manager rounds it (see below) — and the colour/blit algebra is reused from
  `lib/*`, never duplicated (`AGENTS.md` §2.2). The renderer is stateful so
  it can hold a `tairix-raster` `RasterCache` of the rasterised notification
  glyphs across frames: a glyph is converted once per tint and size and
  re-rendered only on a theme or scale change (the SVG-first rule,
  `AGENTS.md` §10), sharing the one cache the window manager uses for
  cursors (`AGENTS.md` §2.2). The `Taskbar` model stays pure data.
  `render_library` (a `&self` method, no cache of its own) paints the open
  program-library popup — panel chrome, search field, rows with
  hover/cursor states, placeholder, scrollbar — returning `None` when
  closed.
- **Notification icon set** — the renderer draws each notification glyph from
  a `tairix-icon` `IconSet`: the built-in glyph set until `set_icons` installs
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

The crate depends only on `lib/*`: `tairix-geometry` (the shared `Point` /
`Rect` vocabulary), `tairix-input` (the shared `PointerButton`/`InputEvent`/
`Key` vocabulary the window manager also routes), `tairix-raster` (the
premultiplied-alpha `Color`/`Surface` the window manager also paints with),
`tairix-font` (the shared text rasteriser, `AGENTS.md` §16.4),
`tairix-controls` (the shared Reactive Alloy control vocabulary the buttons
and popup compose, `plans/GUI-CONTROLS-DESIGN.md`), `tairix-proglib` (the
program-library catalog the popup lists), and `tairix-theme` (the single
shared theme definition). It does **not** depend on
the window manager or any sibling userland crate, and nothing depends on it in
turn (`AGENTS.md` §17.4, §17.3): the desktop is an optional, one-way-dependent
frontend, so a headless image omits it cleanly.

It is `no_std` (with `alloc`). No `unsafe`, and no `unwrap`/`expect`/`panic!`
in production paths (`AGENTS.md` §2.9).

## Still to come (Stage 7, `plans/NEW-TASKBAR.md`)

The pin strip (T6/T7 — with the popup's right-click *Pin to taskbar*
landing whole beside its store), the upgraded notification area (T8/T9),
the always-rightmost Switchboard icon (T9), and selecting a font face from
the theme's `FontSpec` roles once installed fonts exist.
