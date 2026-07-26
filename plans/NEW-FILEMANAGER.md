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

`done` — FM1–FM12 plus a UI-polish increment are all landed. **The UI-polish
increment (latest)** makes the browser window **resizable/maximizable**
(`files.app` opens `resizable` and re-maps its zero-copy frame region on a
`WindowEvent::Resized`, laying the shared renderer out to the new viewport;
fail-closed re-map, min-size clamp), replaces the cramped, overlapping,
unlabelled inline permission toggles with a **labelled permissions grid popup**
(`render::PermGrid` — `Read`/`Write`/`Exec` × `Owner`/`Group`/`Other`, drawn in
the taller `properties_editable_panel_rect`, with the default `WIN_HEIGHT`
raised so it fits), and enlarges **icon-only buttons** to fill their plate (the
one `lib/controls` `paint_content` icon path, so every icon-only button across
the desktop benefits). Host tests: the `lib/browse` permission-grid non-overlap
regression + updated hit-test scan; freestanding app builds + lints clean.

**FM12 (double-click activation)**: a double-click on an item now activates it (descend / launch a bundle /
open a file), driven by the shared pure `lib/browse::click::DoubleClickTracker`
over the capability-free monotonic clock, through the very same `activate`
dispatch a keyboard `Enter` uses (so pointer and keyboard never diverge, §2.2).
The `files.app` primary-press routing is factored into `apply_primary_press`
(manager tool → item single-select vs same-item double-activate → read-only
chrome via the trimmed `apply_chrome_press`), the tracker reset on any
tool/chrome press so a click through the chrome and back never mis-pairs; nine
host tests in `lib/browse`, freestanding app builds + lints clean. FM1–FM11
remain landed, including **FM11c (the empty-Trash QEMU
witness)**: the aarch64 `autoload_input` vertical now proves the empty-Trash
click-through end to end (after FM10's move-to-Trash `op=rename`, the runner
clicks Go to Trash → Empty Trash → confirm *Delete Permanently*, and the guest
PASS latches an eleventh witness — `FsNodeMutated op=rmdir` whose `path` is
under `Library/Trash`, gated on the move having latched via the one-shot
`FM11_TRASH_FILLED_MARKER`, so no earlier removal can satisfy it — fail
closed). **FM11b lands the app-side Empty Trash
verb + the navigable Trash view.** Two manager-only toolbar tools join the
`chrome::ManagerTool` set (drawn only for the write-capable file manager): **Go
to Trash** (`ManagerTool::Trash`) navigates the browser to the user's
`Library/Trash` via the new `Browser::navigate_to` jump-to-arbitrary-location
primitive, and **Empty Trash** (`ManagerTool::EmptyTrash`) — enabled only in a
non-empty Trash via the new `chrome::ManagerToolModel` threaded through
`render`/`manager_tool_at` — builds `empty_trash_plan`, confirms with the
`DeleteDisposition::Permanent` dialog, and drives the plan's `DeleteWalk`
through the same interleaved progress/cancel runner a delete uses
(`ProgressOp::Delete`). Each tool carries a new built-in `lib/icon` glyph
(`IconKind::Trash` / `IconKind::EmptyTrash`). Host-tested in `lib/browse`
(`navigate_to` off-spine/no-op/fail-closed; the Empty Trash enable-gate
hit-test) and `lib/icon`; the freestanding files app builds + lints clean.
**FM11a (the pure empty-Trash model) is complete.** `lib/browse::trash::empty_trash_plan` turns the Trash
directory's `fs_readdir` listing into a `delete::DeletePlan` over its *contents*
(never the Trash directory itself, so emptying leaves the now-empty folder in
place), carried out by the same recursive `DeleteWalk` a permanent delete uses
(no second removal engine, §2.2). Emptying is always permanent, so the app
confirms it with `DeleteDisposition::Permanent`; it returns `None` for an
already-empty Trash (a no-op the app just does not offer, never an error) and is
fail closed — a root Trash dir (`RootTrash`) or an invalid child leaf
(`InvalidName`) refuses the whole empty rather than remove outside Trash or
silently skip an item (§5.4). It touches no filesystem and holds no authority
(the app drives the plan with its own `fs_readdir`/`fs_unlink` under the user's
identity), so composing it grants nothing and the read-only picker never builds
one. Host-tested in `lib/browse` (contents-not-the-dir removal, empty=no-op,
root-trash refusal, invalid-child refusal). Now justified rather than
speculative: the move that fills the Trash (FM10) has landed, so the way back to
a permanent removal is real surface (§2.4). **FM11c is complete**: the
end-to-end empty-Trash click-through on the aarch64 `autoload_input` QEMU
vertical latches a new eleventh witness (`FsNodeMutated op=rmdir` under
`Library/Trash`, gated after the FM10 move via the one-shot
`FM11_TRASH_FILLED_MARKER`). **FM10 (recoverable delete:
move to Trash) is complete.** FM10a landed the pure `lib/browse::trash` model
(`trash_strategy` same-volume-move-vs-unlink + collision-safe `trash_dest_path`);
**FM10b now lands the app-side Trash verb and its QEMU witness.** On a confirmed
delete the `files.app` `Run` binary resolves the user's home from the exported
`HOME`, ensures the fixed `Library/Trash` subtree (shared `trash::trash_dir`),
and — when Trash and every target share a volume — carries the removal out as a
recoverable **move to Trash** (`Job::Trash`: one `fs_rename` per target into its
collision-free `trash_dest_path`, driven by the same interleaved
progress/cancel runner), falling back fail-closed to the irreversible
`DeleteWalk` unlink when Trash is unavailable or cross-volume. The confirmation
`Dialog` is disposition-aware (`DeleteDisposition` threaded into the shared
`render::build_delete_dialog`): a recoverable *Move to Trash* vs an irreversible
*Delete Permanently*, so the wording always matches what will happen (§2.24).
The desktop session now forwards the **user environment** (incl. `HOME`) to its
launched apps (`spawn_app` → `spawn_with`), the prerequisite that lets the file
manager locate the per-user Trash. The aarch64 `autoload_input` QEMU vertical's
tenth witness changed from `FsNodeMutated op=rmdir` to `op=rename` with a
destination under `Library/Trash` (still gated after the FM9-b `fd_redeem`, so
no earlier mutation can satisfy it — fail closed). **FM1, FM2a, FM2b, FM3, FM4a, FM4b's pure chrome model, FM4b's
drawn breadcrumb path bar + pointer routing, FM4b's drawn clickable toolbar +
`Alt+←/→/↑` + `F5` accelerators, FM5, FM6a, FM6b's pure association
model, FM7a's selection + clipboard model, FM7b's pure paste-execution model,
FM7b's pure delete model, FM7b's pure recursive-delete execution model
(`DeleteWalk`), FM7b's pure recursive-copy execution model (`CopyWalk`),
FM7b's pure new-folder (`fs_mkdir`) model, FM7b's drawn New Folder tool +
`Ctrl+Shift+N` (create + inline-rename, wired end-to-end),
FM8a's properties view model, FM8b's drawn read-only properties panel +
its `Alt+Enter`/`Escape` app wiring, FM8b's pure permission-edit model,
FM8b's drawn permission (mode) control + its click-to-toggle/commit app wiring,
FM8b's ownership-change model + its privileged `fs_set_owner`/`CAP_FS_CHOWN`
kernel primitive, FM8b's drawn ownership control + its click-to-edit/commit app
wiring,
FM4b's pure context-menu chrome model,
FM7b's app-side move/copy verbs (`Ctrl+X`/`Ctrl+C`/`Ctrl+V` cut/copy/paste
driving `plan_paste`→`paste_strategy`→`fs_rename` / `CopyCursor`+`CopyWalk` /
copy-then-delete over the user's own VFS seams, fail-closed and fail-loud),
and FM7b's app-side Delete verb (the `Delete`-key modal confirmation `Dialog`
+ the end-to-end `DeleteWalk` drive over the user's own `fs_readdir`/`fs_unlink`),
FM7b's app-side **progress + cancel** for both Delete and copy/paste (each
confirmed operation handed to one interleaved `advance_operation` runner — a
`Job::Delete` `DeleteWalk` or a `Job::Paste` state machine — the event loop
drives a bounded slice at a time, drawing the shared `lib/browse::progress`
panel and honouring a non-blocking mid-run cancel, so even a large recursive
delete or a multi-gigabyte copy never freezes the window, §2.23),
FM6b's app-side bundle launch (`Enter` → `Browser::activate_selected` →
descend a directory or spawn a `<Name>.app` bundle's own `Run` through the
signed load gate under `CAP_PROC_SPAWN`, async and non-blocking, with launched
children reaped on an any-child wait-set member),
and FM4b's drawn context menu (a secondary-button press opens a
`lib/controls::Menu` painted from the shared `ContextMenuModel`, routed through
`dispatch_context_command` to the *same* Open/Rename/Cut/Copy/Paste/Properties
verbs the toolbar and keyboard drive, fail-closed on a disabled row or a press
off the menu)
are done** — completing FM4b's drawn chrome and all of FM7b's app-side verbs,
plus **FM6b's app-side `OpenFile` hand-off** (opening a data file in its
associated viewer via the inherited-document `DOCUMENT_ROLE_ARG` + `STDIN`
spawn-time hand-off, with the viewer's own inherited-document startup path and
the signed `AppInfo` MIME associations that resolve the viewer), and
**FM6b's explicit "Open With…" chooser** (`OpenWith` re-joins the context menu
for a regular file; the drawn `render::build_open_with_menu` chooser offers the
full `applications_for` result and launches the picked bundle through the same
`DOCUMENT_ROLE_ARG`+`STDIN` hand-off, where the default open picks the first).
**FM6b is complete. FM9-pre (the `FsNodeMutated`/
`FsMutationDenied` filesystem-mutation audit events every write syscall emits,
the robust serial witnesses the vertical keys on and a §5.4/§19.4 requirement
in their own right), FM9-a, FM9-b, and FM9-c are all landed.** FM9-a appends the New-Folder +
inline-rename click-through to the aarch64 `autoload_input` QEMU vertical after
the AW4 terminal round trip: it descends into `/Users/root` by
layout-reconstructed pointer clicks (`render::selection_rect` for rows, the new
forward `render::manager_tool_rect` over the new `Toolbar::tool_rect` for the
New Folder tool, offset by the WM's `WindowFrame::insets` client inset) and
seat-keyboard `Enter`s, creates+names a folder, and the guest PASS latches two
new `FsNodeMutated` `op=mkdir`→`op=rename` witnesses (post-terminal-round-trip,
fail-closed) plus a "named folder" screendump. **FM9-b is now landed too**
(open a file into the viewer via the CU6 one-shot delegation): the trusted
picker now opens at the user's home (`Browser::open_at` over the session's
`HOME`, falling back to `/`), the fixture plants a readable document in
`/Users/root`, and the aarch64 `autoload_input` vertical launches the Viewer
from the start menu, lets the auto-opened picker read the home, clicks the
document row, and latches two new guest PASS witnesses — `SyscallInvoked
sc=fd_grant` then `sc=fd_redeem` (after the FM9-a rename, so no earlier
delegation can satisfy them). The pick-click is gated on a test-kernel
picker-open marker (the session's first post-rename `comm=desktop sc=fs_open`,
the picker's `open_at` home read), so it lands only once the picker is
composited — no `MessageDelivered` fires for the session-internal picker and
the user-authority session cannot `log_emit`, so the test sink turns that
unique read into the deterministic gate. **FM9-c (delete with confirm) is now
fully landed.** A clickable **Delete** joins the context menu (its
`begin_delete` action already existed, so this is not speculative surface,
§2.4), routed through `dispatch_context_command` to the same confirm-and-remove
verb the `Delete` key opens. Delivering the right-click needed a real compositor
fix that also makes the *whole* context menu usable in the desktop: the
secondary (right) button was **dropped** — `tairix_wm`'s router ignored it and
the desktop session's router had a catch-all that swallowed it. Now the WM
router raises+focuses and returns `InputResponse::SecondaryActivated`, the
session forwards `PointerPressed {Secondary}`, and the session delivers
`WindowEvent::Pointer Pressed(Secondary)` to the app (host-tested in
`tairix-wm` and `tairix-desktop-session`). The earlier "the injected
right-click never arrives" was a `tools/qemu` harness bug (QEMU's HMP
`mouse_button` help string mislabels the state bits — `0x2` is right, `0x4` is
middle — so the harness sent a right-press as the *middle* button); the
`MouseButton::mask_bit` fix sends `0x2` and the dedicated
`pointer_button_virtio_mmio_qemu_aarch64` vertical proves it (`BTN_RIGHT`,
fails-before/passes-after). The full delete click-through is now wired into the
aarch64 `autoload_input` vertical: gated on the Viewer's `sc=fd_redeem` (the
last FM9-b serial event), the runner right-clicks the FM9-a folder row (opening
the context menu — every point reconstructed through the shared `selection_rect`
/ `context_menu_command_rect` / `delete_dialog_rect` + `Dialog::action_rects`
geometry, §2.2), clicks the drawn **Delete** row, and clicks the confirmation
dialog's Delete button; the guest PASS latches a tenth witness,
`FsNodeMutated op=rmdir` (gated after `fd_redeem`, so no earlier removal can
satisfy it — fail closed). The starting point was
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
**drawn context menu is now done** — a secondary-button press paints the shared
`ContextMenuModel` as a `lib/controls::Menu` routed to the existing
Open/Rename/Cut/Copy/Paste/Properties verbs; `OpenWith` **now re-joins**
`CONTEXT_COMMANDS` with FM6b's chooser verb (enabled only for a regular file),
and **Delete** joined it with FM9-c's confirm-and-remove verb (enabled on any
selection). New Folder stays off this menu — it is a *write* toolbar tool, not
a menu command shared with the read-only picker.

FM6 is split (§2.19) the same way: **FM6a** (the engine `activate` dispatch-by-kind
decision — descend / launch a bundle / open a file, host-proven) is done, and
so now is **FM6b's pure type→bundle "open with" association model** (the
`lib/browse::open_with` module — the `BundleSource` enumeration seam, the
extension→MIME `mime_for_name` classifier, and `applications_for`, host-proven
like FM6a). **FM6b's app-side bundle launch is now done too**: `Enter` on the
selection dispatches through `Browser::activate_selected`, and a `LaunchBundle`
spawns the `<Name>.app` bundle's own `Run` through the ordinary signed load
gate under the `CAP_PROC_SPAWN` grant this stage added (async and
non-blocking, with launched children reaped on an any-child wait-set member).
**Handing a data file to its associated viewer (`OpenFile`) is now done too**:
`Activation::OpenFile` resolves the viewer from the installed bundles' signed
`AppInfo` MIME associations (`RtBundleSource` + `applications_for`) and hands
the file over through the race-free spawn-time inheritance — `fs_open`
read-only + `spawn_attached` with the descriptor wired onto the child's `STDIN`
(`FdWire::Handle`) and the reserved `DOCUMENT_ROLE_ARG` token, so the viewer
reads its document with no filesystem capability of its own. This supersedes
the earlier fd_grant-after-spawn sketch (`fd_grant`/`fd_redeem` remain the
picker's post-hoc delegation to an already-running window owner). **The
explicit "Open With…" chooser over the full `applications_for` result is now
done too** — the default open picks the first association, the chooser lets the
user pick any. See FM6b below. FM6b is complete.

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
  `files.app` holds exactly its manifest ∩ ceiling set: `CAP_FS_ACCESS`
  (read/list) + `CAP_SHM` + `CAP_CONSOLE_WRITE`, plus `CAP_PROC_SPAWN` (added
  in FM6b, the stage that first launches a bundle). Write-side
  operations (rename/move/copy/delete/mkdir) are ordinary §5.3-checked VFS
  calls under the launching user's own identity — they need **no new
  capability**: the per-inode owner/mode/ACL model already gates them, and
  a refused write fails closed with a stated reason (§2.24), never a
  fabricated success. Launching another app is the `CAP_PROC_SPAWN` request
  added to the manifest **only** in the stage (FM6b) that first uses it,
  never ahead of it (§2.4); the child still loads through the ordinary signed
  load gate and runs as the launching user (no ambient authority).

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

### FM4b — the drawn chrome: toolbar, breadcrumb bar, context menu `[x]`

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
current directory (`is_current`), whose jump is the documented no-op.
Host-tested in `lib/browse` (toolbar enable/disable at root / after descend /
after go-back, the active-view/sort report, the `TOOLBAR_COMMANDS` order, the
breadcrumb crumb list + depth + `is_current`, and a crumb depth climbing to its
ancestor). Docs: `docs/src/desktop/apps.md`, `lib/browse/README.md` + rustdoc.

**The pure context-menu chrome model is done** (§2.19 — host-proven ahead of
the drawn menu, exactly as `ToolbarModel` landed ahead of the drawn toolbar):
`chrome::ContextMenuModel::for_browser(browser, has_clipboard)` +
`ContextCommand` + `CONTEXT_COMMANDS`. It reports which right-click command is
actionable: Open/Rename/Cut/Copy/Properties over the selection (an empty
directory offers none), Open With… over a regular file only (a directory
descends and a bundle launches itself, so neither has an app to choose), and
Paste over the app's held clipboard (threaded in, since the clipboard lives in
the app — `Browser::clipboard` *captures* one from the selection rather than
storing it). This is no longer speculative surface: every modelled command maps
to an engine action that already exists (§2.4). Delete and New Folder, whose
engine action does not exist yet, are deliberately absent from `CONTEXT_COMMANDS`
and land with the stage that first wires them. Host-tested in `lib/browse`
(no-selection disables the item commands, a directory enables all but Open
With…, a bundle disables Open With…, a file enables it, Paste tracks the
clipboard flag, and the `CONTEXT_COMMANDS` order/coverage). Docs:
`docs/src/desktop/apps.md`, `lib/browse/README.md` + rustdoc.

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

**The New Folder tool is done** (its `fs_mkdir` action already exists, §2.4),
and it exposed — and settled — a real design point: the drawn read-only
toolbar (`chrome::ToolbarCommand` / `apply_command` / `render`) is composed by
**both** the file manager *and* the trusted read-only picker, so a *write*
action cannot live on it without handing the picker write authority. New Folder
is therefore a separate **manager-only write-tool vocabulary**,
`chrome::ManagerTool` (with `MANAGER_TOOLS` + `ManagerTool::icon()`), that only
a write-capable consumer hands to `render` — the file manager passes
`MANAGER_TOOLS`, the picker passes `&[]`, so the picker cannot draw or resolve
a write tool (the separation is by type, not a runtime flag). `render` draws
the write tools in their own toolbar group after the read-only commands, and
`render::manager_tool_at` is the mirror hit-test (a read-only command's
position is unchanged whether or not tools follow, so `toolbar_command_at`
needs no `tools` argument). The `files.app` `Run` binary routes a toolbar click
(and the `Ctrl+Shift+N` keyboard equivalent) to `begin_new_folder`, which names
a non-clashing placeholder (`mkdir::suggest_new_dir_name`), creates it through
`Browser::create_directory` over the `fs_mkdir` seam under the user's own
identity (**no new capability**), and opens the inline rename on the new folder;
a refused create states its reason on `stderr` and leaves the listing put
(§2.24, §5.4). Host-tested in `lib/browse` (`manager_tool_at` resolves New
Folder and stays disjoint from the read-only commands, the empty-`tools` picker
never resolves a write tool, and `suggest_new_dir_name` disambiguation) and
`lib/icon` (the `NewFolder` glyph). Docs: `docs/src/desktop/apps.md`,
`lib/browse`/`lib/icon` README + rustdoc.

**The drawn context menu is done.** A secondary-button (right-click) press
selects the item under the pointer (or clears the selection on empty space, so
only the directory-scoped Paste is offered) and opens a `lib/controls::Menu`
painted from the done `ContextMenuModel`: `render::build_context_menu` builds one
`MenuItem` per `chrome::CONTEXT_COMMANDS` entry (its `ContextCommand::label()` +
`shortcut()` caption, rendered *disabled* — not hidden — when the model reports
it inapplicable, so the menu's shape is stable), `render::context_menu_rect`
anchors it at the click and clamps it inside the window, `render::draw_context_menu`
paints it last (topmost), and `render::context_menu_command_at` is the mirror
hit-test returning **only an enabled command** (fail closed on a disabled row
or a press off the menu, §5.4). The `files.app` `Run` binary routes a chosen
command through `dispatch_context_command` to the *exact same* app verbs the
toolbar and keyboard already drive — Open (`activate`), Rename (FM5),
Cut/Copy/Paste (FM7), Properties (FM8) — so the menu can never diverge from them
(§2.2) and adds no authority (every verb is the user's own §5.3-checked action).
`Escape` or a press off the menu dismisses it. Host-tested in `lib/browse`
(`build_context_menu` labels + model-mirrored enablement, `context_menu_rect`
anchor/clamp/degenerate, `draw_context_menu` paints/no-panic, and the
`context_menu_command_at` full-window mirror + fail-closed on a disabled row and
off the menu). Docs: `docs/src/desktop/apps.md`, `lib/browse/README.md` + rustdoc.

`OpenWith` was **removed** from `ContextCommand`/`CONTEXT_COMMANDS` (and the now
unused `ContextMenuModel::selection_is_file` deleted with it, §2.14): the drawn
menu has no verb to invoke for it until the FM6b file→viewer hand-off lands, so
carrying a clickable-but-dead Open With… row would be speculative surface
(§2.4). It rejoins the command set in that stage, exactly as Delete and New
Folder join with the stages that first wire their behaviour. The drawn context
menu therefore has no `planned` remainder.

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

### FM6b — the app: launch `.app`, open a file, "Open With…" `[x]`

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

**The app-side bundle launch is done.** The `files.app` `Run` binary now
dispatches a plain `Enter` on the selection through the shared
`Browser::activate_selected` (the one dispatch-by-kind decision the trusted
picker also acts on, §2.2): `Descended` reveals the selection and repaints (as
a breadcrumb navigation does), and `LaunchBundle { path }` launches the
`<Name>.app` bundle through the ordinary signed app-load gate — the manager's
own `Launcher` spawns the bundle's own `Run` (`<path>/Run`, never a private
path) via `tairix_rt::spawn`, under the launching user's identity, with the
`CAP_PROC_SPAWN` grant added to `AppInfo.toml` (and the kernel
`FILES_BROWSER_REQUEST` pin) in *this* stage (§2.4). The launch is **async and
non-blocking** (`plans/FIX-DESKTOP.md`): `spawn` admits the child and returns
its PID before the image loads, so the event loop never freezes behind a load;
a synchronous refusal (a stripped capability, a malformed path) is stated
fail-loud on `stderr` at once, and a load refusal that only shows once the image
is read surfaces later as the child's reserved `LOAD_*` exit status, named by
the reap (the shared `tairix_abi::load_failure_reason` wording, §2.2, §2.24).
The manager **reaps** every launched child on a new any-child wait-set member
(`CHILD_TOKEN`), drained in the event source's park branch the instant it fires,
so a launched app is never left a zombie and the wake never degrades into a
busy-poll (§2.23). Only the write/spawn-capable file manager builds and drives
this; the read-only picker composes the same `Browser` and never launches.
The app wiring rides the FM9 autoload vertical; the freestanding `Run` builds
and clippy-clean cross-compiled, and the manifest grant is pinned by the kernel
`appinfo_sources_match_the_embedded_registry` host test.

**Opening a data file in its associated viewer (`OpenFile`) is now done** —
the defining "make items *do* something" behaviour. The inherited-document
hand-off is the TAIRiX spelling of `viewer < file`, race-free at spawn:

- **The launch convention** is `tairix_abi::DOCUMENT_ROLE_ARG` (a reserved
  launch-argument token, modelled on `SPAWN_SELF`) plus the `STDIN` stream: a
  launcher that opens a document for a viewer opens the file read-only in its
  **own** table, spawns the viewer with a `SpawnAttach` block wiring that
  descriptor onto the child's `STDIN` slot (`FdWire::Handle`), and passes the
  token as an argument. The kernel clones the read-only *open description* into
  the child owner-checked, so the viewer reads its document with **no
  filesystem capability of its own** (least privilege, §5.2) and there is no
  post-spawn channel, handle-forwarding, or ordering race. This **supersedes**
  the earlier fd_grant-to-attested-PID sketch (`fd_grant`/`fd_redeem` stay the
  picker's *post-hoc* delegation to an already-running window owner; §2.13).
- **The signed `AppInfo` now carries file-type associations.** The bundle
  composer parses an optional `associations` MIME array from `AppInfo.toml`
  and emits the signed MIME table the ABI already reserved (`mime_count` /
  `mime_type_at`); the whole body — capabilities then MIME table — is under the
  signature, so a tampered association breaks the bundle. `viewer.app` declares
  the text/structured-config types it displays.
- **The running-system `BundleSource` is `files.app`'s `RtBundleSource`**: a
  bounded recursive walk of `/System/Apps` then `/Apps`, reading each
  `<Name>.app/AppInfo` through the shared, host-tested
  `lib/browse::association_from_appinfo` decode (fail-closed — a corrupt
  manifest is skipped, never offered). `Activation::OpenFile { path }` resolves
  the associated bundle via `applications_for` (keyed off the file's leaf
  name — never a hard-coded viewer path), and `Launcher::open_file` /
  `launch_viewer` `fs_open`s the file read-only and `spawn_attached`es the
  bundle's `Run` with the `STDIN` wire + `DOCUMENT_ROLE_ARG` + the leaf-name
  title, closing its own descriptor and reaping the child on the same any-child
  member. A file no installed application claims is stated fail-loud on
  `stderr`, never a fabricated open (§2.24). Only the write/spawn-capable file
  manager does this; the read-only picker composes the same `Browser` and never
  launches. Host-tested (`association_from_appinfo` valid / empty / fail-closed);
  the app wiring rides the FM9 vertical and builds clippy-clean cross-compiled.
- **The viewer's inherited-document startup path is done**: `viewer.app`
  detects `DOCUMENT_ROLE_ARG` and reads its document from the inherited `STDIN`
  descriptor (titling its window from the leaf name), distinct from its
  interactive picker path (its standalone launch is unchanged).

**The explicit "Open With…" chooser is now done.** `OpenWith` re-joins
`chrome::ContextCommand`/`CONTEXT_COMMANDS` (enabled only for a regular file —
a directory descends and a bundle launches itself, so `ContextMenuModel` tracks
`selection_is_file` again) and the drawn context menu offers it. Choosing it
resolves the file's absolute path (the shared `selected_target_path`),
enumerates the full `applications_for` candidate list over `RtBundleSource`,
and — when at least one application claims the type — paints it as a
`lib/controls` `Menu` (`render::build_open_with_menu`, one row per candidate in
source order, anchored where the context menu was via the shared
`context_menu_rect`). The chooser owns input while open: a primary-button press
resolves through `render::open_with_index_at` (the *same* enabled-row hit-test
the context menu uses — `menu_enabled_row_at` — so paint and click cannot
disagree, §2.2) and launches the chosen candidate through the **same**
`DOCUMENT_ROLE_ARG` + `STDIN` hand-off `open_file` already uses; `Escape` or a
press off the menu dismisses it. A file no installed application claims is
stated fail-loud on `stderr` and opens nothing (§2.24). The default open still
picks the first association; the chooser lets the user pick any. Host-tested in
`lib/browse` (the context-menu `OpenWith` enablement over file/directory/bundle/
empty, `build_open_with_menu` labels + order, and the `open_with_index_at`
full-window mirror + fail-closed off the menu); the app wiring rides the FM9
vertical and builds clippy-clean cross-compiled. Docs: `docs/src/desktop/apps.md`,
`lib/browse/README.md` + rustdoc.

Double-click activation was deferred from FM6b to its own pointer pass; it is
now landed as **FM12** below (the shared `click::DoubleClickTracker` over the
capability-free monotonic clock, driving the same `activate` dispatch `Enter`
does).

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

### FM7b — move, copy, paste, delete, new folder `[x]`

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

**The pure new-folder model is done** (§2.19 — host-proven ahead of the drawn
New Folder tool, exactly as the paste-execution model landed ahead of the app
verbs): the `lib/browse::mkdir` module (`MkdirError` + `validate_new_dir_name`)
plus `Browser::create_directory`. `validate_new_dir_name` spells the typed name
through the one shared `lib/path::validate_file_name` rule and refuses a name a
sibling already carries (`Clash`), both before any syscall. `create_directory`
spells the new folder's absolute path through the one shared
`Browser::spell_child` helper the launch/open targets also use (de-duplicated
from the two former private copies, §2.2), applies it through an injected
`fs_mkdir` seam under the user's own identity (**no new capability**), then
re-lists and follows the selection onto the new folder ready for the inline
rename — transactional and fail closed, a VFS refusal leaving the listing put
and surfacing as `MkdirError::Refused` (§2.24, §5.4). The read-only picker
composes the same `Browser` and never calls it. Host-tested in `lib/browse`
(commit creates + selects the new folder, each invalid-name class refused
before any syscall, clash refused, VFS refusal surfaced leaving the listing
put, create in an empty directory needs no selection, failed post-create
re-list surfaced, `validate_new_dir_name` purity, every `MkdirError` message
non-empty). Docs: `docs/src/desktop/apps.md`, `lib/browse/README.md` + rustdoc.

**The pure delete model is done** (§2.19 — host-proven ahead of the app verb,
exactly as the paste-execution and new-folder models landed): the
`lib/browse::delete` module (`DeletePlan` + `DeleteTarget`) plus
`Browser::plan_delete`. `plan_delete` captures the current multi-selection into
a `DeletePlan` (`None` when nothing is selected), one `DeleteTarget` per marked
entry in listing order, each carrying the entry's absolute component path (the
one shared spelling, so a target names exactly the node the browser shows) and
whether it is directory-backed on disk — a directory *or* a sealed `<Name>.app`
bundle, via the new `EntryKind::is_directory_backed`, so either is removed with
`UnlinkFlags::DIRECTORY` and recursed into while a regular file is a leaf.
`DeletePlan::new` is fail closed: an empty selection, or any target naming the
root (an empty component list), yields no plan rather than one that could
remove nothing or the root itself (§5.4). `len`/`has_directories` are the
honest figures a delete confirmation reports (§2.24). The model names *what*
would be removed; the app performs each `fs_unlink` under the user's own
identity (**no new capability**), so composing it grants nothing and the
read-only picker never builds one. Host-tested in `lib/browse` (capture +
per-target path/kind, none-on-empty-selection, a bundle marked
directory-backed, a files-only plan reporting no directories, and the
fail-closed `DeletePlan::new` empty/root refusals). Docs:
`docs/src/desktop/apps.md`, `lib/browse/README.md` + rustdoc.

**The pure recursive-delete *execution* model is done** (§2.19 — host-proven
ahead of the app verb, the delete-side analogue of the paste-side
`execute::CopyCursor`): the `lib/browse::delete::DeleteWalk` driven cursor.
`DeleteWalk::from_plan` begins a removal of a `DeletePlan`; `next_action` yields
the next `DeleteAction` — `List(path)` (the app reads that directory and reports
its children with `expand`, so contents are removed before their container) or
`Remove { path, is_directory }` (the app `fs_unlink`s the leaf or now-empty
directory and reports it with `complete_removal`), depth-first. It does no I/O,
keeps its own explicit stack (so a deep tree cannot overflow the call stack),
is bounded by `MAX_DELETE_DEPTH` (a fail-closed defence — a deeper tree is
`DeleteError::TooDeep`, never descended without limit, §26.6/§24.4), and holds
its exact position between steps so the app can cancel or be preempted without
losing or repeating work (§2.23 — no unbounded buffer, no spin); driving it
against the wrong step is `DeleteError::OutOfStep`, leaving the walk unchanged.
`removed` is the honest rising count a progress indicator shows (the total is
unknown until the reads reveal it, so no fabricated percentage, §2.24). It is
the browser engine's own component-path traversal, deliberately distinct from
`rm`'s coreutils removal engine — two consumers with two data models, not one
algorithm copied twice (§2.2). Host-tested in `lib/browse` (single file, empty
directory listed-then-removed, contents-before-container depth-first order,
multiple targets in listing order, the `TooDeep` bound, out-of-step fail-closed
refusals leaving the walk put, and the interruption/resume holding its exact
position). Docs: `docs/src/desktop/apps.md`, `lib/browse/README.md` + rustdoc.

**The app-side Delete verb is done.** The `files.app` `Run` binary binds the
`Delete` key to a modal confirmation before any removal. `begin_delete` captures
the selection with `Browser::plan_delete` (a no-op when nothing is selected —
the plan is `None`, fail closed) and opens a `lib/controls::Dialog` built by the
shared `render::build_delete_dialog`: the title names a single target or reports
the honest count, the message warns when folders (and their contents) are among
the removals, and the honest Action Warmth sits on the safe recommended
**Cancel**, never the destructive **Delete** (§2.24). While the dialog is up it
owns the window (`apply_modal_event` handles it first): `Escape`/Cancel dismiss,
`Enter`/Delete confirm; a primary press routes through the mirror
`render::delete_dialog_action_at` (over the dialog's own `Dialog::action_rects`),
fail closed off a button. On confirm the app drives a `DeleteWalk` to completion
— reading each directory with the same capability-checked listing call and
shared decode the browser navigates with (`tairix_browse::vfs`), and
`fs_unlink`ing each node depth-first (with `UnlinkFlags::DIRECTORY` for a
directory-backed target) under the user's own identity, **no new capability** —
then re-lists so a partial removal is shown honestly. It is bounded and fail
closed: the first refused read or unlink stops the removal, states the reason on
`stderr` (fail loud, §2.24), and leaves what was already removed removed rather
than a fabricated success (§5.4). Only the write-capable file manager builds and
drives this; the read-only picker never deletes. Host-tested in `lib/browse`
(the confirm dialog's honest title/count + folder warning, the destructive/
recommended action roles, `delete_dialog_rect` centering/clamp,
`draw_delete_dialog` paint/degenerate-no-panic, and the `delete_dialog_action_at`
full-window mirror + fail-closed) and `lib/controls` (`Dialog::action_rects`
matching `on_pointer`'s geometry). Docs: `docs/src/desktop/apps.md`,
`lib/browse`/`lib/controls` README + rustdoc.

**The pure recursive-copy *walk* model is done** (§2.19 — host-proven ahead of
the app move/copy verbs, the copy-side analogue of the delete-side
`delete::DeleteWalk`): the `lib/browse::execute::CopyWalk` driven cursor. Where
`execute::CopyCursor` streams a single *file*, `CopyWalk` copies a whole *tree*:
`from_items` begins a copy of resolved `(source, dest, is_directory)` items (the
app supplies each item's kind, which the path-only `Clipboard` does not carry)
and is fail closed — an empty set, or a source/dest naming the root, yields no
walk (§5.4). `next_action` yields the next `CopyAction` — `MakeDir { dest }` (the
app `fs_mkdir`s the destination *before* its contents, so a child always has a
parent, reported with `created`), `List { source }` (the app reads it and
reports children with `expand`), or `CopyFile { source, dest }` (the app streams
the bytes with a `CopyCursor`, reported with `copied_file`), depth-first. It
does no I/O, keeps its own explicit stack (so a deep tree cannot overflow the
call stack), is bounded by `MAX_COPY_DEPTH` — the one shared `MAX_WALK_DEPTH`
recursion bound `DeleteWalk` also obeys, hoisted so the two walks cannot drift
(§2.2, §26.6) — and holds its exact position between steps so the app can cancel
or be preempted without losing or repeating work (§2.23); a deeper tree is
`CopyWalkError::TooDeep` and driving it against the wrong step is
`CopyWalkError::OutOfStep`, both leaving the walk unchanged. `copied` is the
honest rising count a progress indicator shows (§2.24). The model holds no
authority, so the read-only picker never runs one. Host-tested in `lib/browse`
(single file, container-before-contents depth-first order, multiple items in
order, empty-directory create-then-empty-list, the `TooDeep` bound, the
out-of-step fail-closed refusals leaving the walk put, the interruption/resume
holding its exact position, `from_items` empty/root fail-closed, and the error
messages). Docs: `docs/src/desktop/apps.md`, `lib/browse/README.md` + rustdoc.

**The app-side move/copy verbs are done.** The `files.app` `Run` binary holds
one `Clipboard` in its overlay state, captured by `Ctrl+X` (a move clipboard)
or `Ctrl+C` (a copy clipboard) from the current selection
(`Browser::clipboard(op)`; nothing selected is a fail-closed no-op), and pasted
into the current directory by `Ctrl+V`. Paste validates the plan with
`plan_paste` (a paste of a folder into itself is refused outright, nothing
touched), stats the destination directory for its `VolumeId`, and carries out
each item under the user's own identity — **no new capability**:
`execute::paste_strategy` picks `fs_rename` for a same-volume move,
copy-then-delete for a cross-volume move (the source removed through the shared
delete path only after its copy fully succeeds), and a stream for a copy — a
file through an `execute::CopyCursor` and a directory (or sealed `.app` bundle)
through an `execute::CopyWalk`, over `fs_read`/`fs_write`/`fs_mkdir`/`fs_readdir`
with one reused, fixed-size (`FS_IO_MAX`) buffer, so a copy of any size holds no
unbounded buffer and never spins (§2.23, §26.6). It is bounded and fail closed:
the first refused operation stops the paste, states the reason on `stderr`
naming the item (fail loud, §2.24), and leaves what already landed in place
(§5.4); the view is re-listed so a partial paste is shown honestly. A completed
`Cut` clears the clipboard; a `Copy` keeps it. A destination is created
*exclusively*, so a pre-existing name is refused rather than clobbered, and a
`Copy` back into an item's own directory is refused rather than duplicated onto
itself — overwrite/merge confirmation is a deliberate v1 scope boundary, not a
silent overwrite. Only the write-capable file manager builds and drives this;
the read-only picker never pastes. The engine models are host-tested in
`lib/browse` (FM7a + the `execute` model above); the app wiring rides the FM9
autoload vertical. Docs: `docs/src/desktop/apps.md`, `files.app` README +
`run.rs` rustdoc.

**Progress + cancel is done for both the Delete verb and copy/paste.** Neither
interactive verb drives its walk to completion in one blocking pass: the
confirmed work is handed to an interleaved **operation** the event loop advances
a bounded slice at a time (`advance_operation`, up to `OPERATION_STEP_BUDGET`
units of work per turn — one directory read, one unlink, one `fs_mkdir`, one
copy chunk, or one rename), repainting a modal progress panel and polling the
event mailbox *non-blocking* for a mid-run cancel or a close between slices, so
even a large recursive delete or a multi-gigabyte copy never freezes the window
and never busy-spins — the walk is genuine pending work, so continuously
stepping it is not a spin (§2.23). One `Operation` carries either a `Job::Delete`
(a `DeleteWalk`) or a `Job::Paste` (the app-side `Paste` state machine), so both
drive through the one interleaving path (§2.2). The drawn surface is the shared
`lib/browse::progress` model (`ProgressModel` — op kind, the honest rising count
from the walk's own figure, and a *latched* cancel) painted by
`render::draw_progress_dialog` as a `lib/controls` `Panel` + an indeterminate
`Progress` trace (a "working" bar, no fabricated percentage since the total is
unknown until the reads reveal it, §2.24) + a Cancel `Button`;
`render::progress_cancel_at` is the mirror hit-test resolving a click to the
drawn button (fail closed off it, §2.2/§5.4). A latched cancel stops the walk at
the next unit boundary (never mid-node, and never mid-chunk), and a completed or
cancelled/refused run alike re-lists so a partial result is shown honestly
(§2.24). The blocking and the non-blocking event paths share one `accept_frame`
sender-attestation (§2.2).

The **copy/paste** slice is app-side interleaving only, over the *existing*
engine models and drawn surface (§2.19): `Ctrl+V` no longer drives the copy
synchronously. `run_paste` validates the plan, stats the destination volume, and
hands a `Paste` to the `Job::Paste` operation; the event loop then advances it
through `advance_paste`. The `Paste` machine holds its exact position between
slices in a `PasteStage` (`Idle` → begin the next item; `Copying` — a per-item
`CopyWalk` with an in-flight leaf-file `Transfer` streamed one bounded chunk at a
time so a single huge file cannot block the loop; `Deleting` — a cross-volume
move's source removal over the *same* shared `DeleteWalk`, so a move's cleanup
and an interactive delete can never diverge, §2.2), decides each item's mechanism
with `paste_strategy` as it runs, and reuses one fixed-size `FS_IO_MAX` buffer
(§2.23, §26.6). It is fail closed: the first refusal stops the paste, states the
reason on `stderr` naming the item (fail loud, §2.24), and leaves what already
landed in place (§5.4). Initiating a `Cut` paste clears the clipboard (its
sources are being moved); a `Copy` keeps it. The now-dead synchronous
`run_paste_item`/`copy_tree`/`copy_file`/`copy_dir`/`delete_source` helpers are
deleted (§2.14).

The engine models are host-tested in `lib/browse` (FM7a + the `execute` model
above: strategy per op × volume, chunked-copy completion/short-transfer/resume/
overrun, the `CopyWalk`/`DeleteWalk` order + `TooDeep` + out-of-step, and the
`ProgressModel` count/verb/no-percentage + latched cancel + `progress_cancel_at`
mirror); the app-side drive interleaving (`advance_operation`/`advance_paste`)
rides the FM9 autoload vertical. Docs: `docs/src/desktop/apps.md`, `lib/browse`
README + rustdoc, `files.app` `run.rs` rustdoc.

(**New Folder is done** — its drawn manager-only tool + `Ctrl+Shift+N` +
create-then-inline-rename wiring landed with FM4b's chrome; see that stage.)

### FM8a — the properties view model `[x]`

Done (§2.19 — the pure model host-proven ahead of the drawn panel, exactly as
FM6a/FM6b/FM7a/FM7b's pure models landed): the `lib/browse::properties` module.
`Properties::from_stat(name, kind, stat)` turns an entry's name, its browser
`EntryKind`, and the node's `fs_stat` `FileStat` into the display-ready fields
the panel renders — a human kind label (`Folder`/`File`/`Application`, so a
sealed `<Name>.app` bundle reads distinctly from an ordinary directory), the
apparent `size` and on-disk `allocated` bytes (both via the shared
`format_size`, never one derived from the other), the raw mode + its
four-digit octal spelling, the ten-character permission string, the owning
uid/gid, and the four `Time64` stamps rendered as `YYYY-MM-DD HH:MM:SS` by the
new `format::format_datetime`. Every field is straight from `fs_stat` (§21,
64-bit-native), no fabricated field: a stamp the backing does not keep is the
epoch and renders blank, never a made-up `1970-01-01` wall time.

- **The permission spelling is one shared definition (§2.2).** The
  `drwxr-xr-x` mapping is the new `tairix_abi::fs::mode_string` — an alloc-free
  `[u8; 10]` producer in the ABI crate that owns the mode bits — so the
  properties view and `ls -l` can never disagree on what a mode means. The
  private duplicate that lived in the `ls` app is deleted and `ls` now
  delegates to it (§2.14). The permission string's leading type indicator
  reads from the structural `FileStat::kind`, so a bundle is *labelled*
  "Application" yet honestly shows a directory's `d`.
- **The model holds no authority.** The app performs the one
  capability-checked `fs_stat` under the user's own identity and hands the
  result here; the model reads nothing, so composing it grants nothing and the
  read-only picker builds the same view (§4, §5.4).
- Host-tested: `lib/browse` (regular-file / directory / bundle summaries,
  timestamp render + epoch-blank, octal masking) and `lib/browse::format`
  (`format_datetime` date+time, epoch-blank, sub-second, pre-1970/post-2038),
  and `lib/abi` (`mode_string` kind indicator + triads + empty/full/private +
  higher-bit masking). Docs: `docs/src/desktop/apps.md`, `lib/browse`
  README + rustdoc, `tairix_abi::fs::mode_string` rustdoc.

### FM8b — the drawn properties panel + permission editing `[x]`

Done. Split (§2.19) into the drawn read-only panel, the drawn permission (mode)
control, the ownership-change model + its privileged kernel primitive, and the
drawn ownership control — all landed.

**The drawn Properties panel is done.** `render::draw_properties` paints the
done FM8a `Properties` model as a shared `lib/controls` `Panel` centered over
the view — name (title), kind, size + on-disk `allocated`, permissions
(symbolic + octal), owner uid/gid, and the four `Time64` stamps — all straight
from `fs_stat` (§21, 64-bit-native throughout), no fabricated fields.
`render::properties_rows` is the one host-tested definition of which fields
appear and how each reads, so the drawn panel and its tests never disagree
(§2.2); the panel clips so a too-small window shows what fits rather than
panicking (§2.9). The `files.app` `Run` binary opens the overlay with
`Alt+Enter` on the selected item — spelling its path through the new public
`Browser::selected_target_path` and reading its metadata with one
capability-checked `fs_stat` under the user's own identity (**no new
capability**) — and dismisses it with `Escape`; while open the overlay owns the
window (keys do not navigate behind it). Showing properties is an incidental,
refusable action: a stat the VFS refuses is stated on `stderr` and leaves the
overlay closed — an answer, not a crash, never a fabricated summary (§2.24,
§5.4). Host-tested in `lib/browse` (`properties_rows` field set/order + bundle
labelling, `properties_panel_rect` centering/clamp, `draw_properties` paints /
degenerate no-panic, and `selected_target_path` spelling + empty-directory
`None`). Docs: `docs/src/desktop/apps.md`, `lib/browse` README + rustdoc.

**The pure permission-edit model is done** (§2.19 — host-proven ahead of the
drawn control, exactly as FM8a's properties model landed ahead of the drawn
panel): the `lib/browse::mode_edit` module + `Browser::set_mode_selected`.
`validate_mode` fails closed on any bit above `tairix_abi::fs::FS_MODE_MASK`
(the settable `rwx`/setuid/setgid/sticky word) — refused, never masked into a
different mode, so the mode committed is always exactly the one asked for.
`set_mode_selected` names the selected node through the shared
`Browser::selected_target_path` spelling, validates the mode *before* any
syscall, and applies it through an injected `fs_set_mode` seam under the user's
own identity (**no new capability**); a VFS refusal leaves the node's mode
unchanged and surfaces as `ModeError::Refused` (§2.24, §5.4). The listing
carries no mode, so a success re-reads nothing — the app re-stats to refresh
the panel. The model holds no authority, so the read-only picker composes the
same `Browser` and never calls it. Host-tested in `lib/browse` (commit applies
the mode to the selected node's path, an out-of-mask mode refused before any
syscall, a VFS refusal surfaced leaving the listing put, empty-directory
`NoSelection`, `validate_mode` purity across the whole mask + above it, every
`ModeError` message non-empty). Docs: `docs/src/desktop/apps.md`, `lib/browse`
README + rustdoc.

**The drawn permission (mode) control is done.** The Properties overlay is
editable in the file manager: `render::draw_properties_editable` draws the
metadata fields (as the read-only `render::draw_properties`) and, below them, a
labelled permissions grid — `Read`/`Write`/`Exec` column headers over three
`Owner`/`Group`/`Other` triad rows of clickable `lib/controls` `Checkbox`
toggles reflecting the current mode. The grid is drawn in the taller
`render::properties_editable_panel_rect` (the read-only picker keeps the shorter
`render::properties_panel_rect`), and the shared `render::PermGrid` geometry
places the painted grid, its headers/row-labels, and the hit-test from one
definition (§2.2), so the toggles sit on a real grid pitch under their own
labels — replacing the earlier cramped single-row layout whose nine boxes were
a glyph apart (overlapping) with no label. `render::PERMISSION_BITS` /
`permission_cells` are the one definition of which
of the nine owner/group/other bits each toggle carries, and
`render::permission_cell_at` is the mirror hit-test returning the bit a click
flips (nothing off a toggle, fail closed). Only the write-capable file manager
draws the editable overlay; the trusted read-only picker draws `draw_properties`
and never resolves a toggle, so the write surface is separated by call site, not
a runtime flag (the manager-only write-tool precedent, §2.2). The `files.app`
`Run` binary routes an overlay primary-press through `permission_cell_at`, flips
that `rwx` bit while preserving the current setuid/setgid/sticky bits (the
settable word masked by `FS_MODE_MASK`), and commits through
`Browser::set_mode_selected` over `fs_set_mode` under the user's own identity
(**no new capability**); on success it re-stats to refresh the panel, and a VFS
refusal is stated on `stderr` leaving the mode untouched (§2.24, §5.4). The
setuid/setgid/sticky bits stay visible in the octal/symbolic display and are
edited via `chmod` — a deliberate scope boundary, not an omission. Host-tested
in `lib/browse` (`PERMISSION_BITS`/`permission_cells` mapping incl. high-bit
independence, `draw_properties_editable` paints / degenerate no-panic, and the
full-panel scan proving `permission_cell_at` mirrors exactly the nine distinct
bits and fails closed off-grid). Docs: `docs/src/desktop/apps.md`, `lib/browse`
README + rustdoc.

**The ownership-change model and its privileged kernel primitive are done.**
Reassigning a file's *owner* is genuinely unlike the other write verbs (rename,
mode, mkdir), which are the user's own §5.3-checked writes needing no new
capability: it is a privilege operation, so it is gated by a new dedicated
capability `CAP_FS_CHOWN` (id 39, the Unix `CAP_CHOWN` analogue), carried by
the `ADMINISTRATIVE_SET` ceiling and by nothing an ordinary session holds
(§5.2 — a new capability guarding a real class of authority, added with its
live holder and enforcement point). The kernel primitive is the new
`fs_set_owner` syscall (no. 96): the whole authority rule lives in the secured
VFS (`DelegatedFs::set_owner` over the frozen driver `set_security`, so no
driver-trait change) — reassigning the uid, or setting a gid the caller is not
a member of, requires `CAP_FS_CHOWN`; otherwise only the node's owner may
change the group, and only to a group they belong to (the unprivileged
`chgrp`); any change strips the set-*id* bits (the `chown(2)` safety
behaviour). Dispatch keeps the coarse `CAP_FS_ACCESS` gate; the privileged
check is per-inode, in the VFS, under the caller's kernel-attested credential,
and audited, fail closed (§5.4). Wired end to end: `lib/rt::fs_set_owner`, the
C stub `tairix_sys_fs_set_owner`, and the generated `include/` header. The pure
engine model is `lib/browse::owner_edit` (`OwnerChange`/`OwnerError`/
`validate_owner`) + `Browser::set_owner_selected`: it names *what* to change
(each field `None` = unchanged, `Some(id)` = set), refuses the reserved
`FS_OWNER_UNCHANGED` sentinel as an explicit target before any syscall, spells
the node through the shared `absolute_path`, and surfaces a VFS refusal
(including the missing-`CAP_FS_CHOWN` denial) as `OwnerError::Refused` leaving
the ownership untouched. The model holds no authority, so the read-only picker
composes the same `Browser` and never calls it. Tested in `kernel/core`
(privileged/unprivileged uid, member/non-member group, setid-strip, no-op,
read-only, not-implemented), `kernel/syscall` (dispatch + `CAP_FS_ACCESS`
gate), `lib/rt`/`lib/abi-sys` (marshalling), and `lib/browse` (the engine
model). Docs: `docs/src/architecture/syscalls.md`, `docs/src/security/
capabilities.md`, `docs/src/desktop/apps.md`, `lib/browse` README + rustdoc.

**The drawn ownership control is done.** The Properties overlay's owner row is
editable in the file manager, but only where the launching user holds
`CAP_FS_CHOWN` — read once from the kernel-attested `self_origin` at start-up,
so a session that cannot reassign ownership is never shown a control it cannot
use (§2.24). `render::draw_owner_control` overlays the uid and gid values of
`render::draw_properties`' owner row: an accent underline marks each as
clickable, and while one is being edited the shared `lib/controls` `TextField`
is drawn over it. `render::OwnerField` + `render::owner_field_at` are the mirror
hit-test resolving a click to exactly the uid or gid value it edits (measured
from the same `uid N / gid N` spelling the panel draws, §2.2; nothing off a
value, fail closed). Like the permission control, the write surface is
separated by call site — only the file manager calls `draw_owner_control` (the
trusted read-only picker never does) — *and* additionally gated on the runtime
capability, since owner reassignment is privileged. The `files.app` `Run`
binary opens the inline id editor on a click, pre-filled and bounded to a
`u32`'s ten digits, live-validates the typed id, and on `Enter` commits through
`Browser::set_owner_selected` over `fs_set_owner` under the user's own identity
(the kernel enforces `CAP_FS_CHOWN` and the group-membership rule); `Escape`
cancels. A non-numeric/out-of-range id or a VFS refusal (including the
missing-`CAP_FS_CHOWN` denial) states its reason in the field and keeps the
editor open — an honest answer, never a silent or fabricated result (§2.24,
§5.4). On success the panel is re-stat'd to reflect the new owner. Host-tested
in `lib/browse` (`owner_field_at` full-panel scan proving it mirrors exactly
the two distinct value cells and fails closed off-grid / on a too-small window,
and `draw_owner_control` painting the affordances and the active editor without
panicking on a degenerate viewport). Docs: `docs/src/desktop/apps.md`,
`lib/browse` README + rustdoc.

### FM9 — the autoload QEMU vertical + docs (FM9-a done)

FM9 is split (§2.19) so the vertical's robust, non-fragile gates exist before
the click-through that keys on them is written.

- **FM9-pre — filesystem-mutation audit gates `[x]` (done).** The write
  syscalls the vertical must observe (`fs_mkdir`, `fs_unlink`, `fs_rename`,
  `fs_set_mode`, `fs_set_owner`) emitted **no** audit record, so there was no
  kernel-attested serial witness for a New-Folder / rename / delete step to
  gate on — and, independently, mutating on-disk state is a security-relevant
  decision the charter (§5.4(4)/§19.4) requires be logged. Landed: two stable
  events in `kernel/core` — `FsNodeMutated` (id 4100, `Info`) on a successful
  mutation and `FsMutationDenied` (id 4101, `Warn`, carrying the refusal's
  `errno`) — emitted by every write handler after the secured VFS decides,
  under the caller's kernel-attested uid, via the free `emit_fs_mutation`
  (with `audit_path_field` bounding the path on a char boundary so an
  over-long path can never drop the record, and `format_mode_octal` for the
  chmod field). Fields: `op` (`mkdir`/`rmdir`/`unlink`/`rename`/`chmod`/
  `chown`), `uid`, `path`, plus `to` (rename dest), `mode` (chmod), and
  `owner`/`group` (chown); read-only ops are not audited; no token/secret is
  logged. Host-tested in `kernel/core` (`fs_audit_tests`: allow+deny id/level/
  fields per op, path-bounding incl. multibyte boundary + always-emitted, octal
  mode). Docs: `docs/src/architecture/kernel.md` audit catalogue.
- **The manager already has a writable place to act — no extra fixture
  volume is needed (corrected §2.3).** An earlier draft of FM9 said the
  autoload fixture "must first give the fixture a **writable** volume
  because `/System` is read-only". That prerequisite is redundant and was
  dropped: the autoload vertical boots the production path, which on a
  successful unlock publishes the encrypted `ARXFSRoot` partition
  **read-write** as `/` and its writable sub-mounts (`/Users`, `/Storage`,
  `/System/Logs`, `/System/Settings`) — see
  `tairix_kernel::unlock_orchestrate::WritableStateSink` /
  `system_mount::register_writable_state`. The shared users-root fixture
  already carries `/Users/root/`, owned by the logged-in account
  (`tairix_users::FIRST_USER_UID`/`FIRST_USER_GID`, mode `0700`;
  `tairix_test_arxfs_image::build_users_root_image_with_key`), and the
  desktop session (and the files bundle it spawns) run as that account.
  So the manager can create/rename/delete under `/Users/root` with the
  authority it already holds. Adding a fourth writable partition would
  duplicate an already-writable tree (§2.3) and is forbidden; FM9 acts on
  `/Users/root`.

- **FM9 is split (§2.19) into three mutation increments**, each a complete,
  fully-gated landing appended **after** the AW4 terminal round trip (so the
  existing delivery counts 2/4/7/16 do not shift; each new stage adds its own
  kernel-attested PASS witness rather than re-deriving the terminal gates):
  - **FM9-a — New Folder + inline-rename `[x]` (done).** The aarch64
    `autoload_input` vertical, after the AW4 terminal round trip, drives the
    served files window to create and name a folder in `/Users/root` by
    coordinate-computed pointer clicks (the seat-keyboard injection
    `tairix_qemu::qkeycode_for` covers only printable ASCII + `ret`/`tab`/
    `spc`, no arrows). Every click point is reconstructed from the browser's
    own layout code — `render::selection_rect` for rows, the new forward
    `render::manager_tool_rect` (over the new `Toolbar::tool_rect`) for the
    New Folder tool — over a `Browser::open_root` on a fake `DirectorySource`,
    so it obeys the same default sort (directories first, name ascending:
    `Apps`, `Storage`, `System`, `Users`) and resolves each row by name; the
    points are offset by the WM's client inset (`WindowFrame::insets`) so a
    click lands on the client, not the server-side title bar/border. The
    first click hits the `Users` row at a left-biased column clear of the
    (raised, large) terminal window — raising+focusing files — then a seat
    `Enter` descends; a `root`-row click + `Enter` descends into
    `/Users/root`; the New Folder tool click makes `New Folder` (`fs_mkdir`)
    and opens the inline rename; a typed suffix + `Enter` commits a distinct
    name (`fs_rename`). Sequencing is the window-event (`MessageDelivered`)
    delivery counter (a focus-changing click = 3, a same-window press = 1, a
    typed key = 2 edges), so pointer and seat-keyboard cursors interleave
    deterministically; the contract counts live in the vertical's `src/lib.rs`
    (`FM9_*`). Two new guest PASS witnesses latch from the FM9-pre
    `FsNodeMutated` (id 4100) `op=mkdir` then `op=rename` serial lines,
    counted only after the terminal round trip so no boot/login mutation can
    satisfy them (fail closed — `FsMutationDenied` is id 4101 and never
    latches), plus a "named folder" screendump asserting the files window is
    composited at its slot.
  - **FM9-b — open a file into the viewer via CU6 delegation `[x]` (done).**
    The trusted picker now opens at the user's home (`Browser::open_at` over
    the session's `HOME`, parsed with the shared
    `vfs::components_from_absolute_path`, falling back to `/`), and the shared
    users-root fixture plants a readable document (`HOME_DOC_NAME`) in
    `/Users/root`. After FM9-a, the vertical opens the start menu and clicks
    the **Viewer** launcher (session-internal taskbar clicks, gated on the
    FM9-a folder-dump delivery count and held behind that dump); the Viewer,
    handed no document, asks the picker, which opens the home. A single
    pointer click on the document row (reconstructed through the same
    `render::selection_rect`, at `PICKER_ORIGIN` with **no** frame inset —
    the picker is undecorated session chrome) concludes the pick, so the
    session `fd_grant`s the chosen file to the Viewer and the Viewer
    `fd_redeem`s it. Two new guest PASS witnesses latch from `SyscallInvoked`
    `sc=fd_grant` then `sc=fd_redeem` (after the FM9-a rename, so no earlier
    delegation can satisfy them; the picker is the only `fd_grant` caller and
    the Viewer the only `fd_redeem` caller in the image). **Sequencing the
    pick was the hard part** and is solved without any production change: the
    session-internal picker delivers no `MessageDelivered`, maps no `shm`
    frame, and — being user-authority — cannot `log_emit`, so there is no
    kernel-audited event at picker-open. But the picker's `open_at` home
    listing is the session's *first* directory read after the FM9-a rename, a
    `SyscallInvoked` `comm=desktop sc=fs_open`; the test kernel's audit sink
    turns that unique event into the deterministic `FM9B_PICKER_OPEN_MARKER`
    serial line, and the runner gates the pick-click on it (the `fs_open`
    happens synchronously inside the `PickFile` serve, so the click lands in a
    later wake with the picker composited). Non-flaky across repeated runs.
  - **FM9-c — delete with confirm — product `[x]` (done, host-tested);
    right-click delivery in QEMU `[x]` (done, proven); the full delete
    click-through in `autoload_input` `[x]` (done).** A
    clickable **Delete** joins the context menu (`ContextCommand::Delete`,
    enabled on any selection), routed through the app's
    `dispatch_context_command` to the *same* `begin_delete` the `Delete` key
    opens — the action already existed, so this is not speculative surface
    (§2.4). Delivering the right-click needed a real compositor fix that also
    makes the *whole* context menu (Open/Rename/Cut/Copy/Paste/Properties/
    Delete) usable in the desktop: the secondary (right) button was being
    **dropped** — `tairix_wm`'s input router ignored it and the desktop
    session's router had a catch-all that swallowed it. Now the WM router
    raises+focuses and returns `InputResponse::SecondaryActivated` for a
    client-area right-press, the session router forwards `PointerPressed`
    `{Secondary}` to the WM, and the session delivers `WindowEvent::Pointer`
    `Pressed(Secondary)` to the app so it opens its menu — host-tested in
    `tairix-wm` (`secondary_press_activates_and_delivers_to_the_client`) and
    `tairix-desktop-session` (`secondary_press_over_a_window_routes_to_the_window_manager`).
    The shared `Menu::row_rect` + `render::context_menu_command_rect` give a
    caller the drawn Delete row's rect (§2.2).
    - **The earlier "emulation gap" was a harness bug, now fixed and proven.**
      A prior draft recorded that a scripted right-click "never arrives in the
      guest" and blamed the emulator. The real cause was in the QEMU test
      harness (`tools/qemu`): QEMU's HMP `mouse_button` help string
      ("1=L, 2=M, 4=R") is **wrong**. `hmp_mouse_button` feeds the state mask
      to `qemu_input_update_buttons` through a `bmap` of the legacy
      `MOUSE_EVENT_*` bits (`MOUSE_EVENT_RBUTTON = 0x2`,
      `MOUSE_EVENT_MBUTTON = 0x4`), so state bit `0x2` is the **right** button
      and `0x4` the **middle**. The harness trusted the help string and sent a
      secondary press as bit `0x4`, which QEMU delivered to the guest as a
      *middle*-button event — so every OS layer decoded a correct (but wrong)
      button and the right-click context menu was unreachable in QEMU.
      `MouseButton::mask_bit` now sends the bit QEMU actually decodes as the
      right button (`0x2`). A dedicated aarch64 vertical,
      `tairix-test-pointer-button-virtio-mmio-qemu-aarch64`, proves it: it
      attaches a `virtio-mouse-device`, injects a secondary press+release, and
      the shared `virtio_input_button` tail asserts the driver decodes
      `BTN_RIGHT` (`0x111`), never the middle button (`0x112`). It **fails
      (times out) with the old mask and passes with the fix** — the
      fails-before/passes-after regression guard (§2.18).
    - **The full `autoload_input` delete click-through (done).** Appended after
      FM9-b, gated on the Viewer's `sc=fd_redeem` serial line (the last FM9-b
      event, and the image's only `fd_redeem`), so it runs strictly after the
      CU6 delegation without relying on the app-ward delivery counter (the
      FM9-b Viewer window delivers its own focus event, leaving that counter
      statically unknown). The runner right-clicks the FM9-a folder row
      (`secondary_press` → `open_context_menu` selects it and anchors the menu),
      clicks the drawn **Delete** row, and clicks the confirmation `Dialog`'s
      Delete button — every point reconstructed from the app's own layout code
      (`render::selection_rect`, `render::context_menu_command_rect` over the
      menu built exactly as the app builds it, and
      `render::delete_dialog_rect` + `Dialog::action_rects`[`DELETE_CONFIRM_INDEX`]),
      never a hand-copied coordinate (§2.2). The whole burst shares the one
      gate: the guest applies the queued pointer events in order and each
      overlay (menu, then dialog) is handled synchronously on its press. A
      tenth guest PASS witness latches from `FsNodeMutated op=rmdir` gated on
      the FM9-b delegation being redeemed, so no earlier removal can satisfy it
      (fail closed — `FsMutationDenied` is a different id). Non-flaky across
      repeated runs.
- **Docs** kept current in the same changes (§2.8, §13):
  `docs/src/desktop/apps.md` (the manager's design as each stage lands),
  the `lib/browse`/`lib/icon`/`lib/controls` rustdoc + `README.md`
  stability tiers (§6), and the app's 13-locale `Help/` tree (§16.5 —
  authored in the bundle, discovered by `tools/syshelp`, never hardcoded).

### FM10 — recoverable delete: move to Trash

The §0 scope promises a delete that "prefers a recoverable move to a per-user
trash location over an irreversible unlink … where the backing supports it
cheaply" (§2.24). FM9 shipped delete as an irreversible recursive `fs_unlink`;
FM10 makes it recoverable in the cheap case. Split (§2.19) into the pure engine
model and the app wiring, exactly as FM6/FM7/FM8 were.

- **FM10a — the pure move-to-Trash model `[x]` (done).** `lib/browse::trash`,
  host-proven ahead of the app verb exactly as the pure delete/paste-execution
  models landed. `trash_strategy(item, trash)` makes the one recoverable-vs-
  irreversible decision from the item's and the user's Trash directory's
  `execute::VolumeId`s — `TrashStrategy::Move` (a single same-volume `fs_rename`
  carrying the item into Trash intact, recoverable until emptied) when they
  share a volume, else `TrashStrategy::Unlink` (the existing `DeleteWalk` path,
  since a rename cannot cross a volume, exactly as `mv` decides from `st_dev`) —
  reusing the same volume identity `paste_strategy` compares, one definition
  (§2.2). `trash_dest_path(trash_dir, leaf, taken)` resolves a collision-free
  home inside Trash: the original leaf when free, else the smallest ` (n)`
  disambiguation inserted before the extension (`notes (2).txt`) via the one
  shared `icon::extension` split (§2.2). It never clobbers an existing trashed
  item (§2.24) and is fail closed — `RootTrash` (an empty/root Trash dir),
  `InvalidName` (a bad original leaf), `TooLong` (a disambiguation past
  `FS_NAME_MAX`), and `NoFreeName` past the fixed `MAX_TRASH_NAME_ATTEMPTS`
  bound (§5.4, §24.4). The model touches no filesystem and holds no authority
  (the app performs the `fs_stat`/`fs_rename` under the user's own identity, no
  new capability), so composing it grants nothing and the read-only picker never
  runs it. Host-tested in `lib/browse` (same-/cross-volume strategy, free-name
  passthrough, extension-aware and whole-name and dotfile disambiguation,
  suffix-skipping over taken names, and each fail-closed refusal). Docs:
  `docs/src/desktop/apps.md`, `lib/browse/README.md` + rustdoc.
- **FM10b — the app-side Trash verb + the QEMU witness `[x]` (done).** The
  `files.app` `Run` binary, on a confirmed delete, decides one disposition for
  the whole plan (a selection lives in one directory, hence one volume): it
  resolves the user's home from the exported `HOME`, spells the fixed
  `Library/Trash` subtree with the shared `trash::trash_dir` (honouring the
  fixed `/Users/<u>/` shape — Trash is *inside* `Library/`, never a new
  sibling, §16.3), ensures that directory exists (`fs_mkdir` of `Library` then
  `Trash`, the user's own authority), and — when Trash and every target share a
  volume — resolves each target's collision-free `trash_dest_path`. On confirm
  a recoverable removal is a `Job::Trash` (one `fs_rename` per target into
  Trash, driven by the same interleaved progress/cancel runner as delete/paste,
  `ProgressOp::Trash`); an unavailable or cross-volume Trash falls back, fail
  closed, to the existing `DeleteWalk` unlink. The confirmation `Dialog` is
  disposition-aware (`DeleteDisposition` threaded into the shared
  `render::build_delete_dialog`): a safe, recoverable *Move to Trash* vs the
  destructive *Delete Permanently*, so the wording always matches what will
  happen (§2.24). The prerequisite — the desktop session forwarding the user
  environment (incl. `HOME`) to its launched apps (`spawn_app` → `spawn_with`);
  plain `spawn` gave a child an empty environment — is done in the same change
  (§2.19). Rides the aarch64 `autoload_input` QEMU vertical: its tenth witness
  changed from `FsNodeMutated op=rmdir` to `op=rename` whose `to` is under
  `Library/Trash` (still gated after the FM9-b `fd_redeem`, so no earlier
  mutation can satisfy it — fail closed).

### FM11 — emptying the Trash

FM10 made a delete recoverable by moving items into the per-user Trash; FM11
gives the user the deliberate way back to a permanent removal — emptying it.
Because the move that fills the Trash now exists, emptying it is real surface,
not speculative (§2.4). Split (§2.19) into the pure engine model and the app
wiring, exactly as FM6/FM7/FM8/FM10 were.

- **FM11a — the pure empty-Trash model `[x]` (done).**
  `lib/browse::trash::empty_trash_plan(trash_dir, children)`, host-proven ahead
  of the app verb exactly as the pure delete/paste/trash models landed. It turns
  an `fs_readdir` listing of the Trash directory into a `delete::DeletePlan`
  over its *contents* — one target per immediate child, in
  listing order — never the Trash directory itself, so emptying removes the
  contents and leaves the now-empty folder in place. The removal is carried out
  by the *same* recursive `DeleteWalk` an ordinary permanent delete uses, so
  there is no second removal engine (§2.2), and it is always permanent (there is
  no trash-of-the-trash), so the app confirms it with
  `DeleteDisposition::Permanent`. It returns `None` for an already-empty Trash —
  a no-op the app simply does not offer, never an error — and is fail closed: a
  root Trash dir (`RootTrash`) or an invalid child leaf (`InvalidName`) refuses
  the whole empty rather than remove outside Trash or silently skip an item
  (§5.4). The model touches no filesystem and holds no authority (the app drives
  the plan's walk with its own `fs_readdir`/`fs_unlink` under the user's own
  identity, no new capability), so composing it grants nothing and the read-only
  picker never builds one. Host-tested in `lib/browse` (contents-not-the-dir
  removal preserving listing order and directory-backed flags, empty=no-op
  `None`, root-trash refusal, invalid-child refusal across `""`/`.`/`..`/`a/b`/
  `a:b`). Docs: `docs/src/desktop/apps.md`, `lib/browse/README.md` + rustdoc.
- **FM11b — the app-side empty-Trash verb + the navigable Trash view `[x]`
  (done).** Two manager-only toolbar tools join the `chrome::ManagerTool`
  vocabulary (drawn only for the write-capable file manager, never the read-only
  picker), each carrying a new `lib/icon` built-in glyph (`IconKind::Trash` /
  `IconKind::EmptyTrash`, host-tested as the FM3 file-type glyphs were):
  - **Go to Trash** (`ManagerTool::Trash`) — the navigable Trash view. The
    `files.app` `Run` binary resolves the user's home from `HOME`, ensures the
    fixed `Library/Trash` subtree (shared `trash::trash_dir`, §16.3), and
    navigates there with the new `Browser::navigate_to(components)` — the
    jump-to-an-arbitrary-location primitive (neither an ancestor nor a listed
    child), transactional and fail closed like every other navigation, so the
    Trash's contents show like any directory. Always offered; an absent home or
    unreachable Trash is stated on `stderr`, not hidden (§2.24).
  - **Empty Trash** (`ManagerTool::EmptyTrash`) — enabled only when the current
    directory *is* the user's Trash and it is non-empty (a new
    `chrome::ManagerToolModel` the app computes from `HOME`, since the engine
    does not know it; threaded through `render`/`manager_tool_at` so a disabled
    tool renders muted and a click on it resolves to nothing, §5.4). Clicking it
    re-reads the Trash (recomputing the location so a stale click can never
    empty the wrong directory), builds `empty_trash_plan`, confirms with the
    `DeleteDisposition::Permanent` dialog, and drives the plan's `DeleteWalk`
    through the same interleaved progress/cancel runner a delete uses
    (`ProgressOp::Delete`), under the user's own `fs_readdir`/`fs_unlink` (no new
    capability). An already-empty Trash is a silent no-op; a refusal is stated
    fail-loud on `stderr`.

  Host-tested in `lib/browse` (`navigate_to` off-spine/no-op/fail-closed; the
  Empty Trash tool disabled-vs-enabled hit-test gating) and the freestanding
  files app builds and lints clean. Docs: `docs/src/desktop/apps.md`,
  `lib/browse/README.md`, `lib/icon/README.md` + rustdoc.
- **FM11c — the QEMU witness for the empty-Trash click-through `[x]` (done).**
  Proves the Empty Trash verb end-to-end on the aarch64 `autoload_input` QEMU
  vertical with a new eleventh witness. After FM10b's move-to-Trash `op=rename`,
  the host runner clicks the **Go to Trash** tool (navigating the front files
  window into `Library/Trash`, now holding the trashed folder), the **Empty
  Trash** tool (opening the *Delete Permanently* confirmation), and the dialog's
  Delete button — each point reconstructed from the app's own layout
  (`render::manager_tool_rect` for the tools, `empty_trash_plan` →
  `build_delete_dialog(Permanent)` → `Dialog::action_rects` for the confirm
  button, §2.2). The guest PASS latches a further `FsNodeMutated op=rmdir`
  whose `path` is under `Library/Trash`, gated on the FM10 move having latched.
  The whole empty burst is gated on the one-shot `FM11_TRASH_FILLED_MARKER` the
  test kernel emits the first time it observes the move latch, so the clicks
  land only after the folder is provably in the Trash (Empty Trash enabled) and
  no earlier removal can satisfy the witness — fail closed. Contract markers
  live beside the guest PASS gate (`FM11_TRASH_FILLED_MARKER` in the vertical
  crate's `lib.rs`), so the script and its observer cannot drift (§2.2).

### FM12 — double-click activation `[x]`

Done. The pointer pass FM6b deferred: a double-click on an item now activates
it (descend / launch a bundle / open a file), exactly as a keyboard `Enter`
does. Not speculative surface — the activation behaviour already exists
(`Enter`, context-menu Open), so this only adds the pointer *gesture* that
drives it (§2.4).

- **The pure detector `[x]`.** `lib/browse::click::DoubleClickTracker` is the
  one host-proven rule that turns a stream of primary presses into single- and
  double-click gestures (`ClickKind`). `register(now_ns, index)` pairs a press
  with the previous one only when it lands on the **same** item within
  `DOUBLE_CLICK_INTERVAL_NS` (half a second); a completed double *consumes*
  both presses (a third quick press begins a fresh single — standard
  triple-click semantics), a non-monotonic clock reading fails closed to a
  single, and `reset` breaks the pair when an intervening interaction (a chrome
  click) interrupts it. It holds no authority and does no I/O — the caller
  supplies the hit-test index and the timestamp — so it is fully host-tested
  and the read-only picker can compose it for free (§2.2). Host-tested in
  `lib/browse` (lone single, quick same-item double, exactly-at-interval pair,
  slow-second single, different-item single, double-consumes-both,
  reset-breaks-pair, backwards-clock fail-closed, custom interval).
- **The app wiring `[x]`.** The `files.app` `Run` binary threads a
  `DoubleClickTracker` in its `Overlays` state and reads the capability-free
  monotonic clock (`tairix_rt::clock_get`) at each primary press. Primary-press
  routing is factored into `apply_primary_press` (manager write tool → item
  single-select vs same-item double-**activate** via the shared `activate` →
  read-only chrome) with `apply_chrome_press` the trimmed toolbar/crumb router;
  the tracker is `reset` on any tool or chrome press so a click through the
  chrome and back never mis-pairs. The freestanding binary builds and lints
  clean cross-compiled. Docs: `docs/src/desktop/apps.md`, `lib/browse/README.md`
  + rustdoc.

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
selection/menu. FM8 is split the same way: FM8a models the properties view
(host-proven); FM8b paints it and adds permission editing. FM9 closes out the
core with the autoload QEMU vertical. FM10 makes delete recoverable (move to
Trash), and FM11 gives the way back — emptying the Trash (FM11a the pure model,
FM11b the app verb + navigable Trash view, FM11c the QEMU witness), each split
the same way and depending on the FM7 delete walk it reuses (§2.2). FM12 adds
the pointer double-click gesture (the pointer pass FM6b deferred), reusing the
FM6 `activate` dispatch so pointer and keyboard never diverge. Each lands
fully gated; a stage that turns out larger than one clean increment is split and
staged here, never shipped half-done "for now" (§2.19).

## 3. What this explicitly refuses to become

To stay best-in-class and bloat-free (§2.3), the file manager will **not**
grow: a built-in text/image editor (that is what associated apps and CU6
delegation are for), a search-indexer daemon, cloud/account integration, a
ribbon or customisable-toolbar framework, per-file-type plug-in surfaces,
or a second theming/rendering path. Anything that belongs to another
subsystem (viewers, the shell, the storage resolver) is *reached*, not
reimplemented here.
