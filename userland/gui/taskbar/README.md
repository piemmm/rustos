# tairix-taskbar

The TAIRiX traditional desktop **taskbar** (`AGENTS.md` §10, `PLAN.md`
Stage 7, `plans/NEW-TASKBAR.md`): a GNOME/Windows-style bar at a configured
screen edge. It floats clear of the three screen-facing sides by the theme's
`Metrics::taskbar_margin` — `5` logical pixels in both built-in themes — and
keeps its normal thickness on the work-area side.

This crate is the Stage 7 **layout, model, and rendering** increment. It
owns:

- **Layout** — `TaskbarConfig` (edge, thickness, per-region extents) and
  `BarLayout::compute`, which insets the bar by `taskbar_margin` on the three
  sides facing the screen edge, then lays it out along its main axis: the two
  permanent leading launcher buttons (**Library**, then **Files** — fixed
  order, never removable) divided by the **separator** rule, the
  **application strip** in the middle, the
  notification-icon area packed before the clock, and the clock anchored to
  the trailing end. The margin is scaled through `Scale::scale_length` and
  clamped so a screen too small to hold it still lays out a bar. Every region
  is laid out *inside* the bar's own rim — the placer is pulled in by one
  `plate_border` on both axes — so a hovered or pressed slot cannot wash over
  the surface's edge; `BarLayout::bar` remains the whole rectangle, rim
  included, and a bar too thin to spare two rims keeps its content instead. All
  arithmetic saturates, so a degenerate screen size fails closed inside the bar
  (`AGENTS.md` §2.9).
- **The separator** — `BarLayout::separator`, a rule one `border_thickness`
  along the bar's main axis (floored at one physical pixel so it survives a
  small scale), spanning the cross axis inset by one `control_inset` from
  both long edges, filled in the theme's `border` colour. It sits a
  `control_gap` past the Library button and pushes Files, the applications, and every
  trailing region along by the whole gutter, so the file manager reads as a
  peer of the applications rather than of the library. It is decoration: `hit_test`
  never reports it, and a bar too short to reach it or too thin to inset it
  simply lays out `Rect::EMPTY` and draws nothing.
- **Variable DPI** — the bar's extents, thickness, corner radius, and edge
  margin are
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
  under it (the Library button, an application slot, a
  notification icon, the clock, or the Switchboard capsule) for input
  routing. A region slot that does not fit is `Rect::EMPTY` and is never hit,
  and the separator is not a region at all: a press on the rule lands on the
  bare bar.
- **Input routing** — `TaskbarInput` consumes the shared `tairix-input`
  `InputEvent` stream (the same one the window manager routes) and turns a
  primary press into a typed `TaskbarResponse`. It acts on the pointer only
  while it *holds* it: the bar can see its own geometry but not the window
  stack, so the desktop's input seat resolves which surface the pointer rests
  on and delivers the pointer events to that one router. `set_pointer_focus`
  is the other half — a `PointerFocus::Left` drops every hover and closes the
  hover picker (a window raised over the bar leaves the pointer exactly where
  it was, so no position could have told the bar), and an
  `Entered { at }` adopts the position and refreshes the hover without opening
  any hover surface. Responses: `OpenLibrary` (the Library
  button opened the popup), an application's declared default
  action (`AppDefault`) or the raise the session performs in its place
  (`AppRaise`), a chosen row of the menu an application declared
  (`AppMenuChosen`), a hover asking for the window picker
  (`ShowWindowPicker`) and a cell chosen in it (`WindowChosen`), or a
  notification-icon press. A primary press on the clock is claimed and inert;
  its menu answers a secondary press. While the context
  menu OR the program-library popup is open it is
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
  bar on every edge, clamped along the bar's own span so it cannot enter the
  wallpaper gap, chrome measured by probing the shared `Panel` rather than
  re-deriving it, §2.2).
- **Floating chrome** — the bar adopts its theme's floating form once
  (`Theme::floating`, in `Taskbar::new` and `apply_theme`), so the bar, every
  popup it opens — the program-library panel, context menu, notification
  popover, and Switchboard readout — and every control drawn on them are
  translucent by construction. The session requests the theme's
  `Metrics::chrome_backdrop_blur` — `7` logical pixels in both built-in themes
  — behind each surface, and along-bar popup placement is clamped to the bar's
  own span, so no popup enters the wallpaper gap. Each surface keeps the colour
  role it wears solid and takes the palette's `chrome_alpha` (the bar, menu and
  readout `surface_raised`; a `Panel` `surface`), so a resting row is exactly
  its panel; a plate raised on one — an icon's hover wash, the search field,
  the readout's action button, a notification card — takes the step-more-solid
  `chrome_plate_alpha`, and a scroll channel the ground's own. A surface's own
  rim is its edge rather than a mark on it, so it takes that same
  `chrome_alpha` and reads a step lighter than the ground on the dark theme, a
  step darker on the light one — the bar's 1 px border, drawn by the one shared
  `paint_surface_plate` recipe. Ink and semantic marks stay solid: icons,
  labels, a control's rim, focus rings, role fills, rails, beads.
  Each surface draws one translucent layer, so a floating panel has no separate
  header band.
- **Window registry and application strip** — `TaskList` is the one registry
  of top-level windows (id, title, minimised), read by the hover picker and by
  the Switchboard capsule's cycle and previous-window gestures; `AppStrip`
  holds the session's resolved slots, one per *running application*, each
  carrying the label, class glyph, and artwork the session resolved from the
  bundle the kernel attested owns that process, the windows it owns, and the
  declaration it made. A slot is drawn as an icon-only `TaskbarItem` with no
  presence, focus, or minimised mark at all: the bar shows which applications
  are running by showing them, and a window is reached through the picker.
- **The hover window picker** — `WindowPicker`, opened over a slot whose
  application owns at least `PICKER_MIN_WINDOWS` (two) windows. The session
  supplies one `PickerEntry` per window — its title and its last presented
  frame, scaled to the cell — and a press on a cell reports
  `WindowChosen { id }`. It opens only where there is a choice to make, so no
  dwell timer is needed and single-window applications never flash a popup;
  it takes no keyboard and closes when the pointer leaves.
- **The context menu** — `BarMenu`, the bar's one right-click surface. A
  secondary press on an **application slot** opens the menu that *application*
  declared over the window channel — and nothing at all when it declared none
  — with one row the bar owns: *About*, whose submenu is a `FactList` of the
  bundle's **signed** manifest, so an application cannot state an identity
  that is not its own. A secondary press on a library entry row opens the one
  `EntryRow` list — *Open* and *Create Desktop Shortcut*, the two things the
  popup can do to a row that its own click cannot — and one on the Switchboard
  capsule opens the system menu below.
  Choosing a row reports a typed `TaskbarResponse` (`AppMenuChosen`,
  `LibraryLaunch`, `CreateDesktopShortcut`, or a system action); the session
  performs it.
- **The system menu** — the start-menu session controls, whose shape is the
  one ordered `system::ROWS` table (inspect the machine, change how it looks,
  then secure, leave, or stop it). The bar holds none of that authority: it
  renders what the session attested through `SystemPermits`, so a row whose
  backing is missing is shown non-actionable with its reason rather than
  offered and then failing. *Switch User…* is the one exception and is
  **absent** rather than refused: a desktop whose session authority never
  gave it a wake mailbox cannot be resumed, so there is no facility to
  explain the absence of (`set_switch_user_available`, `plans/NEW-DESKTOP-LOGIN.md`
  G5). Both the rendered rows and the row → command mapping read the same
  filter, so a hidden row can never stay clickable at its old index.
  *Lock Screen* reads one attestation, `set_elevation_available`: whether this
  session's console has a re-authentication broker. The clock menu's set-time
  row needs the same broker and reads the same attestation — one fact, not two
  booleans that would always be equal (`AGENTS.md` §2.2). Both default to
  refusing.
- **The clock's menu** — the same one modal menu surface, opened by a
  **secondary** press on the clock (a primary press on a reading is claimed
  and inert, like a status signal's), whose shape is the one ordered
  `clock_menu::ROWS` table: the reading the bar is drawing (a statement, not a
  command) and *Set Date & Time…* (`plans/NEW-TASKBAR.md` T17). The heading
  repeats the bar's own label rather than deriving a second time, so the two
  cannot disagree, and an unset clock says *Time not set* instead of repeating
  the bar's `--:--` or showing a fabricated one.
  Setting a clock is authority the bar does not hold, so the row reports
  a typed `SetDateTime` and the session asks for an account that may.
- **Notification area** — `NotificationArea`, an ordered set of status icons.
- **Clock** — `Clock` holds the display label the caller sets (formatting a
  `Time64` value is an upstream concern, `AGENTS.md` §21). `clock::UNSET_LABEL`
  is the one spelling of "no wall time yet" (`--:--`): a clock whose menu is
  where a time gets set must stay visible, so a caller draws that
  rather than nothing.
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
  reports `OpenSwitchboard { section: CommandSection::Tasks }`; a press held
  past `LONG_PRESS_AFTER_NS` (half a second) reports
  `OpenSwitchboard { section: CommandSection::Recovery }`; and the readout's one
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
  and `Folder` artwork, then each **application slot** is drawn as the same
  icon-only `TaskbarItem`: one centred, plate-sized icon, never a label, so a
  run of applications reads as one strip of equal
  icons. An application's label stays model data a context surface reads
  (`TaskEntry::title`), never ink on the bar. Every icon on the bar is
  **bar-seated** (`tairix_controls::PlateSeating::Bar`): it wears no perimeter
  in any state and no plate at all while it has nothing of its own to state, so
  the strip reads as one bar rather than a row of boxed buttons, and a hover
  shows as the shared pointer wash under that one slot. The rule itself lives
  in `lib/controls`; the bar only chooses the seating (`AGENTS.md` §2.2). Each
  remaining region is filled with its theme role, then the notification icons
  and the clock are drawn on top. `pin_icon_side` and `task_icon_side` expose
  the exact pixel geometry of each kind of slot, so owners rasterise at the
  drawn size; both slots are icon-only, so the two agree wherever the
  configured extents do — and by default they do. The bar's own background is
  the shared floating-surface plate (`tairix_controls::paint_surface_plate`),
  the recipe every popup it opens already wears: a rim one `plate_border` thick
  in the palette's `rim`, then the raised ground inside it, both rounded by
  `BarLayout::corner_radius`. The colour/blit algebra is reused from `lib/*`
  (`AGENTS.md` §2.2).
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
  slot will be drawn at (`pin_icon_side`, `task_icon_side`, the controls'
  `icon_side`). A lookup that answers nothing — `tairix_icon::NoArtwork`, a
  machine with no `/System/Graphics`, a store under memory pressure —
  therefore renders every element from glyphs rather than leaving a blank
  slot.
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

`BarLayout::corner_radius` carries the radius from the active theme's
`taskbar_corner_radius`. The window manager cuts the bar window to it through
its single anti-aliased rounded-corner path, the same one it uses for windows,
and the bar's own background plate is laid down at that same radius so its rim
follows the silhouette the cut leaves instead of squaring off across it. Both
round through `lib/raster`'s one coverage path; there is no second
implementation (`AGENTS.md` §2.2).

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
