# tairix-taskbar

The TAIRiX traditional desktop **taskbar** (`AGENTS.md` §10, `PLAN.md`
Stage 7, `plans/NEW-TASKBAR.md`): a GNOME/Windows-style bar pinned to a
configured screen edge.

This crate is the Stage 7 **layout, model, and rendering** increment. It
owns:

- **Layout** — `TaskbarConfig` (edge, thickness, per-region extents) and
  `BarLayout::compute`, which lays the bar out along its main axis: the two
  permanent leading launcher buttons (**Library**, then **Files** — fixed
  order, never removable), the **pin strip** between the launchers and tasks,
  the running-task list in the middle, the notification-icon area packed
  before the clock, and the clock anchored to the trailing end. All
  arithmetic saturates, so a degenerate screen size fails closed inside the
  bar (`AGENTS.md` §2.9).
- **Variable DPI** — the bar's extents, thickness, and corner radius are
  *logical* pixels (the screen dimensions are physical). `BarLayout::compute`
  takes a `tairix-geometry` `Scale` and converts the logical lengths to
  physical pixels through the one shared `Scale::scale_length` (`AGENTS.md`
  §10, §2.2). The `Taskbar` stores **no** scale: the desktop density belongs
  to the output and is owned by the compositor, so `layout`, `hit_test`, and
  `library_layout` take the `Scale` as a parameter and the presenter supplies
  `Compositor::scale` at present time. A runtime DPI change is therefore just
  a re-present at the new density — transparent to the bar, no model rebuild.
- **The owned theme** — the bar owns a copy of the active `Theme`
  (`Taskbar::theme`, swapped by `apply_theme`), so layout, hit-testing, and
  painting read one definition.
- **The per-surface repaint latch** — pixel-only state changes (a launcher
  hover, a popup scroll or edit, a theme switch) set a repaint latch the
  embedder drains with `take_repaint`, so one present follows each visual
  change — never a per-frame busy repaint (`AGENTS.md` §2.16). The latch is a
  `TaskbarRepaint`: one flag per rendered surface (`bar`, `library`, `menu`,
  `notifications`, `readout`), composed with `|` / `|=`, never a single
  "something changed" bit. The five surfaces cost wildly different amounts to
  produce — measured on the host in release, the bar renders in 1655 µs and
  the library popup in 1001 µs against the context menu's 104 µs — so one bit
  forced the embedder to re-render and re-push all five for any change at all:
  a pointer drifting between two rows of a small open menu cost ~2.8 ms of
  rendering and a recomposite of five window rectangles where 104 µs and one
  small rectangle would do. That was the desktop's pointer lag. The contract
  every mutator upholds is exact: **every change latches every surface it
  touches**, and a change touching several latches all of them — opening the
  popup also presses the bar's Library button, a raised or dismissed
  notification changes the popover *and* the bar's notification icon, a tray
  summary changes the bar *and* the readout while it is expanded, and a theme
  swap or a `set_config` edge/resize changes all five. Latch sites **err
  toward latching more, never less**: an extra latch costs one redundant
  repaint, while a missing one leaves stale pixels on screen, which is a
  correctness bug. That holds even for a borrow the bar cannot see into: each
  `&mut` sub-model accessor latches as it hands the borrow out (`tasks_mut`
  and `clock_mut` the bar, `library_mut` the popup *and* the bar). The
  embedder may therefore present strictly from the drained latch, and present
  nothing at all when it is empty; only the compositor-supplied `Scale` stays
  the embedder's own.
- **Hit-testing** — `BarLayout::hit_test` maps a pointer to the `Hit` element
  under it (the Library button, the Files button, a pin, a task, a
  notification icon, the clock, or the Switchboard capsule) for input
  routing. A region slot that does not fit is `Rect::EMPTY` and is never hit.
- **Input routing** — `TaskbarInput` consumes the shared `tairix-input`
  `InputEvent` stream (the same one the window manager routes) and turns a
  primary press into a typed `TaskbarResponse`: `OpenLibrary` (the Library
  button opened the popup), `OpenFiles`, pin activation (`ActivatePin`), the
  task activate/minimise rule (`TaskActivated`), or a notification-icon / clock
  press. While the context menu OR the program-library popup is open it is
  modal and consumes the whole stream — presses, releases, scroll, and keys
  all route into the modal surface; a press on the Library button toggles the
  popup shut and a press anywhere else dismisses it without acting on what it
  landed on (`LibraryDismissed`, `BarMenu::Dismissed`) — one click does one
  thing (`AGENTS.md` §2.1). Anything else, or a press that misses every
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
- **Task list and pin strip** — `TaskList` tracks one entry per top-level
  window; `PinStrip` holds the resolved views of the user's pinned shortcuts.
  Both use the familiar click-to-activate / minimise-restore rule. Both pins
  and tasks are rendered as `TaskbarItem` controls. `PinView` matched running
  windows and per-application artwork are handed in by the session.
  `set_focused` mirrors focus into the highlight. A slot's `TaskVisibility`
  states itself with the presence mark on its lower edge: the full-width accent
  seam for the active window, a short muted mark for one merely running, and
  nothing at all for `TaskVisibility::Closed` — a pinned application that is
  not running rests as its bare icon on the bar until hovered or focused, so it
  can never masquerade as a running task.
- **The context menu** — `BarMenu`, the bar's one right-click surface. A
  secondary press on a pin or a library entry opens this menu. Choosing a row
  reports a typed `TaskbarResponse` (`ActivatePin`, `Unpin`, `PinEntry`, or
  `LibraryLaunch`); the session performs the action.
- **Notification area** — `NotificationArea`, an ordered set of status icons.
- **Clock** — `Clock` holds the display label the caller sets (formatting a
  `Time64` value is an upstream concern, `AGENTS.md` §21).
- **Switchboard capsule** — `SwitchboardTray`, the immovable trailing-most
  slot (`plans/NEW-TASKBAR.md` T9/T11). One pure derive turns the summary
  the Switchboard service publishes, plus the session's count of
  unresponsive applications, into the shared `tairix-controls` `TraySignal`
  capsule: the dominant state with its badge, label, and value, and the
  orthogonal heat seam, pressure rail, and recovery posture composed
  beneath it. Hover expands the instrument readout, a scroll over the
  capsule or the readout cycles the running tasks, and a middle press
  switches to the previous task.
- **The capsule's tap-or-hold gesture** — a primary press and quick release
  reports `OpenSwitchboard { section: Section::Tasks }`; a press held past
  `LONG_PRESS_AFTER_NS` (half a second) reports
  `OpenSwitchboard { section: Section::Recovery }`; and the readout's one
  safe action, "Open Switchboard", reports the same response as the quick
  press. The session asks the Switchboard service to open — or revive and
  open — its window at that section. The threshold is resolved against the
  monotonic time the caller passes to `TaskbarInput::handle`, on whichever
  event the router next handles (a motion sample taken while the press is
  held, or the release), so the gesture never polls or sleeps
  (`AGENTS.md` §2.23). A hold that has already fired never also fires on
  release, and a press dragged off the capsule opens nothing (fail closed,
  `AGENTS.md` §5.4).
- **Rendering** — `TaskbarRenderer::render` paints the bar into a
  `tairix-raster` `Surface` using the taskbar's own theme, taking the caller's
  `tairix_icon::IconArtwork` lookup as its last argument: the two leading
  `IconButton`s draw with their live hover state over the shipped `Library`
  and `Folder` artwork, then each **pin** and **task slot** is drawn as a
  shared `TaskbarItem` (pins use the `Icon` presentation; tasks
  `IconAndLabel`). Every icon on the bar is **bar-seated**
  (`tairix_controls::PlateSeating::Bar`): it wears no perimeter in any state and
  no plate at all while it has nothing of its own to state, so the strip reads
  as one bar rather than a row of boxed buttons, and a hover shows as the shared
  pointer wash under that one slot. The rule itself lives in `lib/controls`; the
  bar only chooses the seating (`AGENTS.md` §2.2). Each remaining region is
  filled with its theme role, then
  the notification icons, clock, and titles are drawn on top. `pin_icon_side`
  exposes the exact pixel geometry so owners rasterise at the drawn size. The
  surface is rectangular — the window manager rounds it
  — and the colour/blit algebra is reused from `lib/*` (`AGENTS.md` §2.2).
  The renderer holds a `tairix-reclaim` `ReclaimCache` of the rasterised
  notification glyphs across frames, built by `icon_cache` from the shared
  desktop cache policy (`tairix_reclaim::desktop::disposable_ui_cache`,
  `plans/SMARTRAM.md` SMART5): owned by the seat, bounded by a budget
  derived from the real framebuffer byte size, dropped under memory
  pressure, and wiped on release. `render_library` paints the open popup —
  each entry row drawing the artwork the session resolved for it — and
  `render_menu` paints the open context menu.
- **Icon artwork** — every icon the bar draws resolves through one rule,
  expressed once in `render`: the application's own artwork when the owner
  supplied it, else the artwork the lookup holds for the slot's `IconKind`,
  else the control's built-in vector glyph. The bar never reads or decodes a
  file; it asks the lookup the session owns, at exactly the pixel side the
  slot will be drawn at (`pin_icon_side`, the controls' `icon_side`). A
  lookup that answers nothing — `tairix_icon::NoArtwork`, a machine with no
  `/System/Graphics`, a store under memory pressure — therefore renders every
  element from glyphs rather than leaving a blank slot.
- **The popup's per-row artwork** — `LibraryPopup::visible_icon_requests`
  reports the rows the viewport actually shows, each with its entry id and
  pixel side, so the session decodes only icons a user can see;
  `set_row_artwork` files the answers and `row_artwork` serves them to the
  renderer. A rebuild clears them, so a stale index can never draw another
  application's icon.
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
program-library catalog the popup lists), `tairix-theme` (the single
shared theme definition), `tairix-abi` (the `PowerAction`, notification, and
Switchboard-tray IPC vocabulary the capsule and power controls consume),
`tairix-reclaim` (the shared reclaimable-cache policy the notification-glyph
cache is built from, `plans/SMARTRAM.md` SMART5), and `tairix-log` (the
audit sink the cache reports through). It does **not** depend on the window
manager or any sibling userland crate, and nothing depends on it in turn
(`AGENTS.md` §17.4, §17.3): the desktop is an optional, one-way-dependent
frontend, so a headless image omits it cleanly.

It is `no_std` (with `alloc`). No `unsafe`, and no `unwrap`/`expect`/`panic!`
in production paths (`AGENTS.md` §2.9).

## Still to come (Stage 7, `plans/NEW-TASKBAR.md`)

Selecting a font face from the theme's `FontSpec` roles once installed fonts
exist.
