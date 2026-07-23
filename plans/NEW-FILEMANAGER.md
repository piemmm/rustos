# NEW-FILEMANAGER.md — the `files` app becomes a first-class file manager

Binding under `AGENTS.md`. This is the staged build plan that takes the
Stage 7 `files` app (`userland/apps/files`, `tairix-files`) from the
keyboard-only, single-fixed-window directory browser it is today into a
first-class graphical file manager: clickable file/folder icons that open
directories, launch `.app` bundles, and hand files to the right viewer;
in-place rename; move/copy/delete; and make-directory — all done cleanly,
with a coherent, best-in-class UI and **without** the bloat of Windows
Explorer or the per-panel inconsistency of the typical Linux file manager.

Read first, in order: `AGENTS.md` (all of it), `plans/APPWIN.md` (AW1–AW5
— the window channel, the shared `lib/browse` engine, the CU6
one-shot `fd_grant`/`fd_redeem` delegation this builds directly on),
`plans/GUI-CONTROLS-DESIGN.md` (the `lib/controls` widget vocabulary every
surface here composes — no second control implementation, §2.2),
`plans/APPS.md` (command-word resolution / bundle lookup the "open with"
path reuses), `plans/CAPABILITY_USE.md` (CU6 trusted-UI picker sizing),
`docs/src/filesystem/drives.md` (the storage-forest path model — `/` is a
view, not the root of storage), and `plans/DISPLAY.md` (the seat/display
model). Every rule in all of them applies here without exception.

**Note:** `abi-v1` is *not* frozen (the standing task direction supersedes
the `AGENTS.md`/`PLAN.md` language). A `lib/abi` change today is allowed;
it requires regenerating the C header (`cargo xtask c-header --write`),
which the drift guard enforces.

## Status

`in progress` — **FM1, FM2a, FM2b, FM3, FM4a, FM4b's pure chrome model, FM4b's
drawn breadcrumb path bar + pointer routing, FM4b's drawn clickable toolbar +
`Alt+←/→/↑` + `F5` accelerators, FM5, FM6a, FM6b's pure association
model, FM7a's selection + clipboard model, and FM7b's pure paste-execution model
are done**; the rest of the FM4b drawn chrome (the context menu, and the New
Folder tool which lands with FM7's `fs_mkdir`), the FM6b app-side
spawn/delegation, the FM7b app-side move/copy/delete verbs, and FM8–FM9 are
`planned`. The starting point is
`plans/APPWIN.md` AW3/AW5 (done): the
`files.app` `Run` binary composes the shared `lib/browse` `Browser` model +
`render` renderer over the AW2 window channel, parks on its event mailbox, and
navigates by keyboard; the renderer-mirroring point hit-test
(`render::entry_index_at`) and the kernel one-shot read delegation
(`fd_grant`/`fd_redeem`) the viewer consumes are in place.

FM2 was split (§2.19) into FM2a (the list item view) and FM2b (the icon-grid
view, the runtime view toggle, and the drawn `ScrollBar`); both are done. FM4 is
split the same way: **FM4a** (the engine navigation model — bounded back/forward
history + breadcrumb navigation) is done; **FM4b** paints that model as drawn
chrome. Its **breadcrumb path bar is now drawn and clickable**, with the app's
pointer routing wired onto it (a primary-button press climbs a crumb via
`navigate_to_depth` or selects an item via `select`) — landed now because its
action (breadcrumb navigation) already exists (§2.4). Its **drawn clickable
toolbar** is now done too — its commands (Back/Forward/Up/Refresh/ToggleView/
Sort) and their actions already exist, so it needs no speculative surface. The
remaining FM4b chrome (the context menu) still lands *with* the actions its
entries gate (FM5 rename, FM6 open/open-with, FM7 clipboard verbs), so no menu
entry is built as speculative surface ahead of the behaviours it invokes (§2.4).

FM6 is split (§2.19) the same way: **FM6a** (the engine `activate` dispatch-by-kind
decision — descend / launch a bundle / open a file, host-proven) is done, and
so now is **FM6b's pure type→bundle "open with" association model** (the
`lib/browse::open_with` module — the `BundleSource` enumeration seam, the
extension→MIME `mime_for_name` classifier, and `applications_for`, host-proven
like FM6a). The rest of **FM6b** (the app-side spawn of a launched bundle, the
CU6 `fd_grant` hand-off of a file to its associated viewer wired onto that
model, and the async non-blocking launch) is `planned`, since it needs the
`CAP_PROC_SPAWN` manifest grant and the spawn/delegation wiring that the pure
engine model does not.

## 0. Scope and decisions (binding for this plan)

- **One engine, two consumers, no divergence (§2.2).** All navigation,
  selection, layout, hit-testing, and file-operation *modelling* lives in
  the shared `lib/browse` crate (`tairix-browse`) — the same engine the
  desktop session's trusted CU6 file picker (`plans/APPWIN.md` AW5)
  drives. The `files.app` `Run` binary stays "only the program": it wires
  syscalls to the engine and paints; it never grows a private copy of a
  behaviour the picker also needs. A capability the picker must *not* have
  (write/delete) is gated in the app's own privileged tail, not by forking
  the engine.

- **The app is its own process with its own bounded authority (§4, §5.2).**
  `files.app` holds exactly its manifest ∩ ceiling set. Today that is
  `CAP_FS_ACCESS` (read/list) + `CAP_SHM` + `CAP_CONSOLE_WRITE`. Write-side
  operations (rename/move/copy/delete/mkdir) are ordinary §5.3-checked VFS
  calls under the launching user's own identity — they need **no new
  capability**: the per-inode owner/mode/ACL model already gates them, and
  a refused write fails closed with a stated reason (§2.24), never a
  fabricated success. Launching another app is a `CAP_PROC_SPAWN` request
  added to the manifest **only** in the stage (FM6) that first uses it,
  never ahead of it (§2.4).

- **No ambient authority; every operation is the user's own (§4, §5.4).**
  The file manager performs a write only through a path the user directly
  acted on (selected + invoked). There is no daemon doing work on the
  user's behalf with wider authority, no setuid, no "run as system". A
  drag-drop move is the same authorised `fs_rename`/copy the user could
  type; the GUI is a spelling of the user's intent, not an escalation.

- **Coherent UI, zero bloat (§2.3, best-in-class mandate).** One window,
  one consistent layout, built entirely from `lib/controls` widgets over
  the shared theme (`lib/theme`) — a toolbar, a path/breadcrumb bar, one
  scrollable item view (list *or* icon-grid, a view toggle, not two
  code paths), a selection model, and a small honest set of operations.
  No ribbon, no property-sheet sprawl, no modal-dialog maze. Every action
  is discoverable from the toolbar/context-menu and has a keyboard
  equivalent. A feature earns its place or it is not built (§2.3).

- **Destructive actions are honest and reversible where cheap (§2.24).**
  Delete asks once (a `lib/controls` `Dialog` with honest action warmth),
  reports refusals in-UI (a denied delete is an answer, not a crash), and
  — where the backing supports it cheaply — prefers a recoverable move to
  a per-user trash location over an irreversible unlink (staged FM7).

- **Fail closed, park never poll, no busy loops (§5.4, §2.23).** The event
  loop parks on the wait-set exactly as today; a long copy is chunked and
  interruptible and never spins; a refused listing/operation leaves the
  view exactly where it was (the `lib/browse` transactional discipline).

- **Not in this plan:** the compositor window furniture
  (`plans/COMPOSITOR-WORK.md`), display acceleration
  (`plans/FIX-DISPLAY-ACCELERATION.md`), the storage-namespace resolver
  internals (`docs/src/filesystem/drives.md`), and network/remote volumes.
  This plan consumes those surfaces; it does not build them.

## 1. Stages

Each stage is one fully-gated increment: it lands with its host tests, its
docs, and a green whole-project validation gate (§7), and — where the
behaviour is observable end-to-end — extends the autoload QEMU vertical
rather than a faked run (§2.1). The engine work (FM1–FM3, FM7 modelling)
is host-proven in `lib/browse` against injected sources exactly as the AW1
model was; the app work (painting, click routing, spawn) rides the desktop
autoload vertical the AW3/AW5 interaction contract already drives.

### FM1 — richer entries: metadata, kinds, and a stable sort `[x]`

Done. `lib/browse::Entry` now carries `size: u64` and `modified: Time64`
alongside its name and kind, mapped straight from the existing `fs_readdir`
`DirEntry` stream (no new syscall); a bad record still refuses the *whole*
listing (§5.4). `EntryKind` gained a `Bundle` variant — a `<Name>.app`
directory is a sealed unit, so `Entry::is_directory` is `false` for it and
`Browser::open_index` refuses to descend; the engine only models the
distinction (FM6 owns the launch). `EntryKind::for_listing` / `is_bundle_name`
are the one pure classifier both views share. `lib/browse::sort` adds
`SortMode` (`SortKey` name/size/modified × `SortDirection`) and the pure
`sort_entries` — directories first, then the key, with an alloc-free
case-insensitive name tiebreak; the `Browser` applies it to every listing and
`set_sort_mode` re-orders in place keeping the selection on the same entry
(default: name-ascending). Host-tested in `lib/browse/src/tests.rs` (metadata
mapping/refuse, `is_bundle_name`, bundle-not-descendable, the three sort keys +
direction + empty, `set_sort_mode` selection-preserve); the order-dependent
existing tests were updated to the sorted order. Docs:
`docs/src/desktop/apps.md`, `lib/browse/README.md`. No app-behaviour change
(the app repaints in FM2).

Deliberately deferred to a later stage (not FM1): a `Symlink`/`Special`
variant is added only when the VFS surfaces such a kind (a new variant, never
overloading the existing ones).

### FM2a — the list item view over `lib/controls` `[x]`

Done. The ad-hoc row painter in `lib/browse::render` is replaced with a real
list item view built from the shared collection controls, so the manager and
the trusted picker share one coherent, themed surface (§2.2, §17.4). No app-
behaviour change: the `files.app`/picker `render` and `entry_index_at`
signatures are unchanged, so both get the new look for free.

- **List view**: each entry is a `lib/controls` `TableRow` with a leading
  name cell (a directory suffixed `/`), a trailing numeric size cell, and a
  modified-date cell; the selected row carries the shared row chrome's
  selection state (raised surface + accent selection rail), not a browser-
  private accent fill. The column layout is one definition (`render::COLUMNS`),
  scaled proportionally into the content width by `TableRow::render`.
- **Item-view geometry** (`lib/browse::layout::ListView`): the one pure
  definition of the visible-row window, each row's `Rect`, and the pixel→index
  hit-test, built on the shared `lib/controls` `scroll::ScrollRange` clamp
  rather than a re-derived anchor. Both `render` (paint) and `entry_index_at`
  (hit-test) consume it, so they can never disagree (§2.2).
- **Column formatting** (`lib/browse::format`): `format_size` (binary units)
  and `format_date` (`Time64` → ISO `YYYY-MM-DD`, blank at the epoch so a
  stampless file is never given a fabricated date, §21) — the file-listing
  convention shared by both views, deliberately distinct from the `top`/
  `sysinfo` figure spellings in `lib/procinfo` (a browser engine does not
  depend on the System Information client crate).
- Host tests: `format` size/date (bytes, binary scaling, huge-size no-overflow,
  epoch-blank, pre-1970/post-2038, leap day); `layout` (visible window excludes
  the header, degenerate viewport/zero row height show nothing, row rects and
  the mirroring hit-test at normal sizes, selection-anchored scroll, the
  `ScrollRange` offset clamp); the updated render selection-chrome assertion.

### FM2b — the icon-grid view, the view toggle, and the drawn `ScrollBar` `[x]`

Done. The engine now owns a `ViewMode` (`List`/`Grid`) and a single scroll
offset, and both views land complete (§27) behind one `layout::ViewLayout`
dispatch that the renderer and the pointer hit-test share (§2.2):

- **Two views, one model.** `layout::ListView` (full-width rows) and
  `layout::GridView` (a wrapped grid of `lib/controls` `Card` tiles) take an
  explicit scroll offset and share one `reveal` rule + the `scroll::ScrollRange`
  clamp. `Browser::set_view_mode` toggles the view keeping the selection on the
  same entry and re-reading nothing; the icon glyph above each grid tile's
  label is FM3 (the tile is complete without it here).
- **Scrolling** is the drawn `lib/controls` `ScrollBar` in a reserved
  right-edge gutter over that same `ScrollRange`; the wheel routes through the
  shared `scroll::ScrollModel` (`render::scroll_lines`), and a selection-moving
  key reveals the selection the least it can (`render::reveal_selection`).
  Interactive thumb-drag arrives with the FM4 pointer routing; the browser owns
  the one offset both the bar and the views read.
- **Hit-testing** is `render::entry_index_at`, a point (x, y) test through
  `ViewLayout` that resolves list rows and grid tiles alike (rejecting the
  header, inter-tile gaps, and the scrollbar gutter). The picker adopts it.
- Host-tested in `lib/browse` (list + grid layout/hit-test at degenerate and
  normal sizes, `reveal` in both units, the view-toggle selection-preserve, the
  wheel-scroll clamp, and the drawn scrollbar thumb tracking the offset); the
  FM2a `ListView` tests were updated to the explicit-offset API. Docs:
  `docs/src/desktop/apps.md`, `lib/browse/README.md`.

### FM3 — file-type icons `[x]`

Done. `lib/icon::IconKind` gained the file-manager kinds `Folder`,
`FolderOpen`, `File` (generic), `AppBundle`, `Text`, `Image`, `Archive`, and
`Executable`, each a built-in vector glyph on the shared 24-unit design grid
resolved (like every kind) through the SVG-first theme-asset path, with
`Generic` the fail-closed fallback (§2.9). `IconSet` was refactored to store
one slot per kind indexed by the new `IconKind::index`, so adding a kind is a
new `ICON_KINDS` entry rather than a new field (§2.2); `builtin()` stays
`const`. The existing audio `Volume` glyph is left as-is: reusing an audio
speaker for a *storage* volume would be a semantic defect, so a storage-volume
icon is deferred to the stage that actually draws one (FM4 breadcrumb/root
view), not forced onto the audio kind here.

Kind→icon is the pure `lib/browse::icon` classifier (`icon_for(entry)` /
`icon_for_name(name)`): by `EntryKind` first (directory→`Folder`,
bundle→`AppBundle`), then a small, documented, ASCII-case-insensitive
filename-extension table (text/image/archive/executable) with the generic
`File` glyph as the fallback for an unknown/extensionless/dotfile name — one
definition shared by manager and picker (§2.2). It is a display *hint* only; it
gates no operation (authority stays in the VFS and the launcher, §4/§5.4). The
glyph is now drawn: `lib/controls::Card` gained an optional `with_icon`
identifying glyph rendered above a centred title (a card with no icon is
unchanged, so notification/resource cards are unaffected), and `render`'s grid
tile sets it from the classifier — so the FM2b grid tile is complete.

Host-tested: `lib/icon` (the new glyphs draw, `index`↔`ICON_KINDS` round-trip,
`for_asset` mappings, per-kind SVG load/fallback over the full set),
`lib/controls` (a card icon draws above the label, no-icon card unchanged), and
`lib/browse` (classifier: kind-before-extension, known extensions per class,
case-insensitivity, unknown/extensionless/dotfile/trailing-dot → generic,
last-extension-wins). Docs: `docs/src/desktop/apps.md`,
`plans/GUI-CONTROLS-DESIGN.md` §11.15, `lib/icon`/`lib/browse` README + rustdoc.

### FM4a — the engine navigation model: history + breadcrumb `[x]`

Done. The host-testable navigation *model* the FM4b chrome will drive, added to
`lib/browse::Browser` (§2.2 — the picker gets it for free):

- **Navigation history**: a bounded back/forward stack (`go_back`/`go_forward`,
  with `can_go_back`/`can_go_forward` supplying the Back/Forward toolbar enable
  state). Every fresh navigation — descend, climb, or a breadcrumb jump —
  records the directory it left on the back stack and clears the forward branch
  (standard browser semantics). The history is a bounded ring (`HISTORY_MAX`)
  that drops the *oldest* location rather than growing without bound: it is a
  UX convenience, not a hardware-scaled resource, so a deliberate defensive cap
  is the right shape (§24 — a bound, not a discovered capacity), and reaching
  it never fails a navigation.
- **Breadcrumb navigation**: `navigate_to_depth(depth)` jumps to the ancestor
  `depth` path components deep (`0` = root, `components().len()` = current = a
  no-op, as is a depth past the end), the primitive the FM4b breadcrumb bar
  will bind each clickable component to. Honours the storage-forest model — the
  root view is whatever the source lists (the four view bindings), never a
  fabricated POSIX tree (`docs/src/filesystem/drives.md`).
- Every one of these is the same transactional, fail-closed navigation as
  descend/climb: the target is listed *before* any state *or history* changes,
  so a move to a directory that has become unreadable leaves the browser and
  its history exactly where they were (§5.4).
- Host-tested in `lib/browse/src/tests.rs` (descend→back→forward, no-op on empty
  history, `go_up` records history, fresh-navigation clears forward, breadcrumb
  climb + current/past-end no-op, `go_back` transactional when the target
  becomes unreadable, and the bounded drop-oldest cap); `MockFs` gained a
  read-count-driven `deny_after_first` to model a revoked directory without a
  test-only source accessor. Docs: `docs/src/desktop/apps.md`,
  `lib/browse/README.md`.

### FM4b — the drawn chrome: toolbar, breadcrumb bar, context menu `[~]`

The app frame, entirely `lib/controls`/`lib/browse::render` widgets over the
theme, painting the FM4a model. **A drawn surface lands with the action it
invokes** so no menu/toolbar entry is built ahead of the behaviour it calls
(§2.4) — the breadcrumb path bar lands now (its navigation already exists), the
toolbar and context menu with their verbs.

**The drawn, clickable breadcrumb path bar is done**: `lib/browse::breadcrumb`
is the pure placement (`layout` + `crumb_at`) that positions the `chrome::breadcrumbs`
crumbs left-to-right and **right-anchors** the strip so the current directory
stays visible, clipping overflowing leading ancestors rather than dropping any
crumb; it is font-agnostic (measured pixel widths) and shared by the painter
and the hit-test (§2.2). `render::draw_path_bar` draws the crumbs (ancestors in
the accent colour, the current directory solid and inert, muted separators) and
`render::crumb_at` is the app-facing hit-test returning the clicked ancestor's
`depth`. The `files.app` `Run` binary routes a primary-button `Pointer` press
through `crumb_at`→`navigate_to_depth` (a path-bar crumb) or
`entry_index_at`→`select` (an item), the same transactional navigation the
keyboard drives (a refused re-listing leaves the browser put). Host-tested in
`lib/browse` (layout fit/overflow right-anchor, `crumb_at` gaps / off-screen /
out-of-range, empty, and the `render::crumb_at` mirror: root inert, ancestor
navigable, current inert, path-bar-row guard). Docs: `docs/src/desktop/apps.md`,
`lib/browse/README.md` + rustdoc.

**The pure chrome model is done** (§2.19 — host-proven ahead of the drawn
widgets, exactly as FM6a/FM6b/FM7a/FM7b's pure models landed): the
`lib/browse::chrome` module. `ToolbarModel::for_browser` snapshots which
`ToolbarCommand` (Back/Forward/Up/Refresh/ToggleView/Sort) is actionable —
Back/Forward/Up over `can_go_back`/`can_go_forward`/`!is_root`, the rest always
available — plus the active view/sort so a tool renders disabled, not hidden,
when it cannot apply; `TOOLBAR_COMMANDS` is the one command order the chrome
iterates. `breadcrumbs` turns the root-first `Browser::components` into the
ordered `Crumb`s of the path bar, each carrying the ancestor `depth` the drawn
crumb binds to `navigate_to_depth` (`0` = root); the terminal crumb is the
current directory (`is_current`), whose jump is the documented no-op. Only the
surfaces whose actions already exist are modelled — the context menu is *not*,
so it lands with the verbs it invokes rather than as speculative surface (§2.4).
Host-tested in `lib/browse` (toolbar enable/disable at root / after descend /
after go-back, the active-view/sort report, the `TOOLBAR_COMMANDS` order, the
breadcrumb crumb list + depth + `is_current`, and a crumb depth climbing to its
ancestor). Docs: `docs/src/desktop/apps.md`, `lib/browse/README.md` + rustdoc.

**The drawn, clickable toolbar is done.** `render` paints `TOOLBAR_COMMANDS`
as a `lib/controls::Toolbar` of themed `IconButton`s in the top strip (each
glyph from the new `ToolbarCommand::icon()` — six new `lib/icon::IconKind`
glyphs NavBack/NavForward/NavUp/Refresh/ViewToggle/Sort), each enabled or
disabled from `ToolbarModel` (muted, never hidden). `render::toolbar_command_at`
is the strip's mirror hit-test returning **only an enabled command** (fail
closed); the app routes a primary-button press through it, then the breadcrumb,
then item selection. Both the click and the keyboard accelerators run through
the one shared read-only `chrome::apply_command(browser, cmd)` (Back/Forward/
Up/Refresh + `ViewMode::toggled` / `SortMode::next`), so they cannot diverge
and the picker can drive the same toolbar. Accelerators: **`Alt+←/→`**
(Back/Forward), **`Alt+↑`** (Up), **`F5`** (Refresh). One `render::chrome_height`
(toolbar strip + path bar) is the single header offset the item views, the
scrollbar gutter, and every hit-test share (§2.2). Host-tested in `lib/browse`
(`ViewMode::toggled`, the `SortMode::next` six-mode cycle, `ToolbarCommand::icon`
distinctness, `apply_command` navigation/view/sort + fail-closed refresh, and
`toolbar_command_at` enabled-resolution + disabled-fail-closed) and `lib/icon`
(the new glyphs draw + round-trip). Docs: `docs/src/desktop/apps.md`,
`lib/browse`/`lib/icon` README + rustdoc.

The view-toggle and sort commands are toolbar (pointer) commands; they have no
conventional single-key accelerator, and a uniform keyboard path for every tool
awaits the later toolbar keyboard-focus pass (the `lib/controls::Toolbar`
`on_key` focus model), not invented chords now.

The remaining drawn chrome (still `planned`):

- **Context menu** (`lib/controls::Menu`): one menu definition whose entries
  land as their stages do — Open/Open With… (FM6), Rename (FM5), Cut/Copy/
  Paste/Delete (FM7), Properties (FM8) — each disabling when inapplicable.
- **New Folder** toolbar tool arrives with FM7 (`fs_mkdir`).

### FM5 — in-place rename `[x]`

Done — the first write operation, and the model for the rest. The edit is
modelled in `lib/browse` (host-tested without a kernel); the `files.app` `Run`
binary supplies the inline text editor and the `fs_rename` seam.

- **Shared name rule**: the typed name is spelled through the new
  `lib/path::validate_file_name` — the *one* leaf-name rule (non-empty, not
  `.`/`..`, no `/`, no control/NUL, no `:`, within `FS_NAME_MAX`), also now
  the per-component check inside `lib/browse::vfs::absolute_path`, so the
  rename target and every path component obey one definition (§2.2). Two new
  `PathError` variants (`ReservedName`, `SeparatorInName`) name the leaf-only
  failures.
- **Engine** (`lib/browse::rename` + `Browser::rename_selected`): `RenameError`
  (with a terse in-UI `message()`) and the pure `validate_new_name`
  (spelling + clash-with-a-different-sibling + no-op `Unchanged`).
  `rename_selected` is transactional and fail-closed — validate before any
  syscall, apply through an injected `fs_rename` seam under the user's own
  identity (**no new capability**), then re-list and follow the selection to
  the new name; a VFS refusal leaves the listing untouched and is surfaced as
  `RenameError::Refused(errno)` (§2.24, §5.4). The read-only picker composes
  the same `Browser` and never calls the write path.
- **App** (`files.app`): `F2` opens the one shared `lib/controls::TextField`
  over the selected row (via the new `render::selection_rect` / `ViewLayout::item_rect`
  overlay geometry), pre-filled and bounded by `FS_NAME_MAX`; keys route to
  the editor, edits live-validate (a clash/bad char shows in the field),
  `Enter` commits and `Escape` cancels. The window-channel wire key is mapped
  onto the `lib/input` vocabulary locally.
- Host tests (`lib/browse`, `lib/path`): valid commit-then-refresh with the
  selection following, each invalid-name class refused before any syscall,
  clash, no-op unchanged, VFS refusal surfaced, empty-directory no-selection,
  `validate_new_name` purity, every `RenameError` message non-empty, and
  `selection_rect`. Docs: `docs/src/desktop/apps.md`, `lib/browse`/`lib/path`
  README + rustdoc.

### FM6a — the engine activation decision `[x]`

Done. The pure dispatch-by-kind decision behind a double-click / `Enter`, the
one primitive both the file manager and the trusted picker act on (§2.2). Added
to `lib/browse` as the `activate` module (`Activation`) + `Browser::activate_selected`
/ `activate_index`:

- **`Activation`** is exhaustive over the three entry kinds: `Descended` (the
  entry was a directory and the engine descended into it, transactionally, via
  its own fail-closed navigation — nothing to launch), `LaunchBundle { path }`
  (a `<Name>.app` bundle, named for the caller to launch through the signed
  load gate), and `OpenFile { path }` (a regular file, named for the caller to
  open in the associated viewer).
- **The engine holds no launch or open authority.** It decides *what* the
  target is and *what should happen*; the spawn and the `fs_open` stay in the
  app's own capability-checked tail under the user's identity, so the read-only
  picker composes the same `Browser` and simply never launches.
- **The target path is spelled through the one shared `vfs::absolute_path`**,
  so a launch/open can never name a different node than the browser shows; a
  name that cannot be spelled as a valid bounded absolute path fails closed as
  `BrowseError::Source` — the same outcome descending into it already produces.
- Host tests (`lib/browse`): directory→descend (listing changes), bundle→
  `LaunchBundle` without descending, file→`OpenFile` without descending, a
  nested target's path spelling, no-selection and out-of-range refusal, and a
  descent into an unreadable directory failing closed and staying put. Docs:
  `docs/src/desktop/apps.md`, `lib/browse/README.md` + rustdoc.

### FM6b — the app: launch `.app`, open a file, "Open With…" `[~]`

Make items *do* something end-to-end — the defining first-class behaviour. The
`files.app` `Run` binary acts on the FM6a decision; this stage needs the spawn
and delegation wiring the pure engine model does not.

**The pure association model is done** (§2.19): the `lib/browse::open_with`
module lands the type→bundle "open with" model host-proven ahead of the app
wiring, exactly as FM6a landed the activation decision. `mime_for_name` derives
a file's content type from its filename extension (recognising exactly the
extensions the `icon` classifier draws a typed glyph for, sharing one
`extension` split, §2.2), `BundleSource` is the injected installed-bundle
enumeration seam mirroring `DirectorySource`, and `applications_for(name,
bundles)` returns the `AppAssociation`s whose declared `AppInfo` MIME set
handles the file's type, in source order — no match being an honest empty
answer (§2.24), never a fabricated default. The type decision is a display
hint only; the load gate still verifies and capability-checks the picked
bundle, and the engine never spawns. Host-tested (classifier per class,
case-insensitivity, unknown/dotfile fail-closed, `handles`, match / single /
none / unrecognised, seam refusal). Docs: `docs/src/desktop/apps.md`,
`lib/browse/README.md` + rustdoc.

The remaining app-side wiring (still `planned`):

- **The app dispatches Enter/double-click through `Browser::activate_selected`**:
  `Descended` repaints; `LaunchBundle`/`OpenFile` drive the launch below.
- **Launch via the app loader, never a private path (§16.5, §18).** The
  manager requests `CAP_PROC_SPAWN` (added to `AppInfo.toml` in *this*
  stage, §2.4) and spawns through the ordinary load gate — for a `.app`
  bundle, the bundle's own `Run`; for a data file, the app the file's
  type/extension associates with, resolved through the shared bundle
  lookup (`plans/APPS.md` command-word resolution + `AppInfo` MIME
  associations, §16.5) — **not** a hard-coded table in the manager.
- **Handing a file to a viewer reuses CU6 delegation (`plans/APPWIN.md`
  AW5).** The manager `fs_open`s the file read-only and `fd_grant`s the
  one-shot descriptor to the spawned viewer, exactly as the session
  picker does — the viewer needs no filesystem capability of its own
  (least privilege, §5.2). Write-capable "open" is a future, separately
  gated concern (not built speculatively, §2.4).
- **"Open With…"** draws the bundles the done `open_with` model returns as a
  `lib/controls` `Menu` — no invented registry, no crash on an empty result
  (§2.24). The remaining work is only the app-side `BundleSource` backed by the
  real app store (each bundle's `AppInfo` MIME table) and the menu that lists
  `applications_for`'s result; the matching model itself is landed above.
- **Async, non-blocking launch (`plans/FIX-DESKTOP.md`).** The spawn must
  not freeze the manager's window; it stays responsive and parked while
  the child starts. Host tests: the app-store `BundleSource`, the grant-to-child seam.

### FM7a — the selection + clipboard model `[x]`

Done (§2.19 — the pure model host-proven ahead of the app verbs, exactly as
FM6a/FM6b's pure model landed). The two `lib/browse` modules the management
verbs are built on:

- **Multi-selection** (`select::Selection` + `Browser` methods): the
  per-listing set of marked entries plus the range anchor. `single` (plain
  click / unmodified keyboard move), `toggle` (`Ctrl`-click), `range_to`
  (`Shift`-click from the anchor), and `select_all`; `Browser::select`,
  `toggle_selection`, `extend_selection_to`, `select_all`, `clear_selection`
  bounds-check every index against the live listing (`NoSuchEntry` otherwise).
  Because members are indices, every listing change (navigate / refresh /
  re-sort) and every unmodified move collapses the selection to the single
  focused entry, so it never points at a stale row.
- **Cut/copy clipboard** (`clipboard` module + `Browser::clipboard`):
  `ClipboardOp` (`Copy`/`Cut`) and a `Clipboard` capturing the selected
  entries' absolute component paths (so it survives navigating to the paste
  target); `None` when nothing is selected. `plan_paste(clipboard, target)`
  resolves each source to a destination under the target and is fail closed
  (§5.4): a target inside one of the moved items is `PasteError::WouldRecurse`
  (an exact component-prefix test), and a paste back into an item's own
  directory is flagged (`PasteItem::overwrites_source`) for the app to confirm
  rather than silently clobber (§2.24). The `Empty` case is the *absence* of a
  clipboard (`Option::None`), not a paste error, so a constructed `Clipboard`
  is never empty and no dead variant lingers (§2.14). The model names *what*
  would move where; the app performs the move/copy under the user's own
  identity, so composing it grants nothing and the picker never builds one.
- Host-tested in `lib/browse/src/tests.rs` (each `Selection` gesture + anchor,
  the bounds refusals, the listing-change/keyboard collapse, the empty-directory
  empty selection, clipboard capture + `None`, `Clipboard::new` empty/root
  refusal, `plan_paste` mapping / self-overwrite flag / recurse-into-self +
  descendant + sibling-prefix, and the error message). Docs:
  `docs/src/desktop/apps.md`, `lib/browse/README.md` + rustdoc.

### FM7b — move, copy, paste, delete, new folder `[~]`

The core management verbs on top of the FM7a model.

**The pure paste-execution model is done** (§2.19 — host-proven ahead of the
app verbs, exactly as FM6a/FM6b/FM7a's pure models landed): the
`lib/browse::execute` module. `paste_strategy(op, source, dest)` makes the
move-vs-copy decision from the clipboard op and the two items' `VolumeId`s (the
16-byte `fs_stat` volume identity) — `Copy` streams, a same-volume `Cut` is one
`Rename`, a cross-volume `Cut` is `CopyThenDelete` — the one `mv`/`st_dev`
definition (§2.2). `CopyCursor`/`CopyChunk` model the bounded, resumable,
interruptible streamed copy: a known-length source is walked in fixed
`COPY_CHUNK_LEN` steps, `advance`d by the bytes actually carried (so short reads
and cancellation between chunks both work), `resume`d from a persisted offset,
and fail closed — advancing/resuming past the source length is
`CopyError::Overrun`, never a silent wrap (§2.23, §5.4). The engine does no I/O
and the cross-volume source is deleted only after its copy fully succeeds, so
composing it grants nothing and the read-only picker never runs it. Host-tested
(strategy per op × volume, `VolumeId` round-trip, empty/small/large chunking to
completion, short-transfer advance, resume, resume/advance overrun, error
message). Docs: `docs/src/desktop/apps.md`, `lib/browse/README.md` + rustdoc.

The remaining app-side verbs (still `planned`):

- **Move** = `fs_rename` when source and target share a volume (the
  `PasteStrategy::Rename` case); otherwise
  **copy-then-delete** (the `PasteStrategy::CopyThenDelete` case). **Copy**
  streams `fs_read`→`fs_write` driving the landed `execute::CopyCursor` in
  bounded, interruptible chunks (§2.23 — no unbounded buffer, no spin),
  preserving metadata where the target format allows and failing closed with
  `TimestampOutOfRange`-style honesty on a narrowing target (§21). A
  directory copy recurses depth-bounded; an error mid-copy stops, reports,
  and leaves a partial-copy marker rather than a silent half-result
  (§2.24, §5.4).
- **Delete** asks once (a `lib/controls::Dialog`, honest warmth), then
  `fs_unlink`/recursive remove under the user's identity; a refusal is an
  in-UI answer. **New Folder** = `fs_mkdir` + inline-rename the new item.
- **Progress + cancel** for long operations: a bounded progress indicator
  (`lib/controls` `Progress`), a Cancel that stops at the next chunk
  boundary; the window stays parked/responsive throughout (§2.23).
- Host tests: the engine-side selection ranges, clipboard state machine,
  move-vs-copy volume decision, and chunked-copy resume/cancel/overrun are
  **done** in `lib/browse` (FM7a + the `execute` model above); the app-side
  work adds partial-failure recovery, delete confirm/refuse, and mkdir+rename
  over the VFS seams.

### FM8 — properties and permissions `[ ]`

- **A Properties panel** (`lib/controls` `Panel`) for the selected item:
  name, kind, size + on-disk `allocated`, the four `Time64` stamps,
  owner uid/gid, and mode bits — all straight from `fs_stat` (§21,
  64-bit-native throughout), no fabricated fields.
- **Editing mode/ownership** where the user is authorised: mode via a
  clear permission control, committed through `fs_set_mode`; a refused
  change is an honest in-UI answer (§2.24). Ownership change is shown but
  only offered when the user holds the authority (no ambient escalation,
  §4). Host tests: stat rendering (incl. epoch stamps), mode edit
  commit/refuse.

### FM9 — the autoload QEMU vertical + docs `[ ]`

- **Extend the desktop autoload vertical** (the AW3/AW5 interaction
  contract) with a file-manager stage: start menu → Files → click a
  folder icon (descend) → New Folder → inline-rename → open a file into
  the viewer via CU6 delegation → delete with confirm — each step gated
  on kernel-attested serial records (window replies, `fd_grant`/
  `fd_redeem` audit ids, the `fs_rename`/`fs_mkdir`/`fs_unlink` audit
  events), never a faked screendump (§2.1). Every delivery count / reply
  index / cascade slot the new steps shift is re-derived in the contract's
  lib target, landed as its own increment (the AW5 "remaining" discipline).
- **Docs** kept current in the same changes (§2.8, §13):
  `docs/src/desktop/apps.md` (the manager's design as each stage lands),
  the `lib/browse`/`lib/icon`/`lib/controls` rustdoc + `README.md`
  stability tiers (§6), and the app's 13-locale `Help/` tree (§16.5 —
  authored in the bundle, discovered by `tools/syshelp`, never hardcoded).

## 2. Sequencing and dependencies

FM1→FM2a→FM2b→FM3 build the shared engine + views + icons (host-proven;
FM2a repaints the list, FM2b adds the icon grid). FM4a adds the engine
navigation model (history + breadcrumb); FM4b paints the chrome and grows the
context menu alongside the actions it invokes (FM5–FM8), so no menu entry is
built ahead of its behaviour (§2.4). FM5 is the first write and the template for
FM7. FM6a models the activation decision (host-proven); FM6b (launch/open) acts
on it, depends on FM3 (bundle/file kinds), and reuses AW5 delegation. FM7 is
split (§2.19): FM7a models the selection + clipboard in the engine
(host-proven); FM7b executes the verbs in the app and depends on FM4b's
selection/menu. FM8 and FM9 close out. Each
lands fully gated; a stage that turns out larger than one clean increment is
split and staged here, never shipped half-done "for now" (§2.19).

## 3. What this explicitly refuses to become

To stay best-in-class and bloat-free (§2.3), the file manager will **not**
grow: a built-in text/image editor (that is what associated apps and CU6
delegation are for), a search-indexer daemon, cloud/account integration, a
ribbon or customisable-toolbar framework, per-file-type plug-in surfaces,
or a second theming/rendering path. Anything that belongs to another
subsystem (viewers, the shell, the storage resolver) is *reached*, not
reimplemented here.
