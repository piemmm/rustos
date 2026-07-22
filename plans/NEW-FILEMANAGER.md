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

`in progress` — **FM1 is done**; FM2–FM9 are `planned`. The starting point
is `plans/APPWIN.md` AW3/AW5 (done): the `files.app` `Run` binary composes
the shared `lib/browse` `Browser` model + `render` renderer over the AW2
window channel, parks on its event mailbox, and navigates by keyboard;
AW5 already added the renderer-mirroring row hit-test
(`render::entry_index_at`) and the kernel one-shot read delegation
(`fd_grant`/`fd_redeem`) the viewer consumes.

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

### FM2 — the item view: list and icon grid over `lib/controls` `[ ]`

Replace the ad-hoc row painter in `lib/browse::render` with a real item
view built from the shared collection controls, so the manager and picker
share one coherent, themed surface (§2.2, §17.4).

- **List view**: `lib/controls` `TableRow`/`TableCell` with an icon rail,
  name, size, and modified columns (the shared row chrome keeps the rail
  gutter aligned as row state changes). Column layout is one definition.
- **Icon-grid view**: a wrapped grid of `Card`-framed items (icon + label)
  over the same selection model — a *view* toggle, one model, **not** a
  second code path (§2.2). The view toggle is a toolbar control (FM4).
- **Scrolling** is the shared `lib/controls` `ScrollBar` over the one
  `ScrollRange`/`ScrollModel` geometry (§2.2) — the browser stops
  re-deriving a scroll anchor; the selection-follows-scroll behaviour and
  wheel handling route through it.
- **Hit-testing** stays the one renderer-mirroring definition
  (`entry_index_at` generalised to the grid), so a click resolves to
  exactly the item the user saw (§2.2). The picker adopts the same view.
- Host tests: layout/hit-test for both views at degenerate and normal
  sizes, scroll geometry, view-toggle invariants (selection preserved).

### FM3 — file-type icons `[ ]`

`lib/icon` today carries only notification glyphs. File management needs a
small, themeable, vector file-type icon set — rasterised once per theme/
scale like every other desktop asset (§10), never on the hot path.

- **Extend `lib/icon::IconKind`** with the file-manager kinds: `Folder`,
  `FolderOpen`, `File` (generic), `AppBundle`, `Text`, `Image`, `Archive`,
  `Executable`, and `Volume` (already present). Each is an SVG-first
  vector glyph (`lib/icon::VectorIcon`) resolved through the theme, with
  `Generic` the fail-closed fallback for an unknown kind (§2.9).
- **Kind→icon is a pure classifier** in `lib/browse` (by `EntryKind`
  first, then a small, documented extension/`.app` table) — one
  definition shared by manager and picker (§2.2). It is a *hint* for
  display only; it never gates an operation (authority is the VFS's job).
- Host tests: classifier table (bundle, known extensions, unknown →
  generic), icon decode/fallback.

### FM4 — the chrome: toolbar, breadcrumb path bar, context menu `[ ]`

The app frame, entirely `lib/controls` widgets over the theme.

- **Toolbar** (`lib/controls::Toolbar`): Back, Up, Refresh, New Folder,
  view-toggle (list/grid), and sort. Each tool is an `IconButton` with a
  keyboard equivalent; a tool whose action is unavailable renders
  disabled (Back at history start), never hidden-then-surprising.
- **Breadcrumb path bar**: the current path as clickable components
  (root-first, from `Browser::components`), each a click target that
  navigates to that ancestor. Honours the storage-forest model — the
  root view shows the four `System:`/`Users:`/`Apps:`/`Storage:` view
  bindings, not a fake POSIX `/` tree (`docs/src/filesystem/drives.md`).
- **Navigation history**: a bounded back/forward stack in the engine
  (§24.1 — bounded, not a fixed tiny ceiling), Back/Forward + `Alt+←/→`.
- **Context menu** (`lib/controls::Menu`): right-click (or the menu key)
  on an item → Open, Open With… (FM6), Rename (FM5), Cut/Copy/Paste
  (FM7), Delete (FM7), Properties; on empty space → New Folder, Paste,
  view/sort. One menu definition; entries disable when inapplicable.
- Host tests: toolbar enable/disable logic, breadcrumb hit→component,
  history bounds and back/forward, menu-entry applicability.

### FM5 — in-place rename `[ ]`

The first write operation, and the model for the rest.

- **Rename** = a `lib/controls::TextField` inline editor over the selected
  item's label (F2 or menu), committing via `fs_rename` under the user's
  own identity — an ordinary §5.3-checked VFS call, **no new capability**.
- **Validation before syscall** (§5.4): the new name is spelled through
  the shared `lib/path` component rules (no separator, no NUL, within
  `FS_NAME_MAX`); an invalid name refuses in-UI without touching the VFS.
- **Fail closed, honest**: a refused rename (permission, name clash,
  read-only mount) leaves the item unchanged and shows the refusal reason
  in-UI (§2.24); the view refreshes transactionally on success.
- **Engine models the edit** (begin/commit/cancel + validation) so it is
  host-tested; the app supplies the `fs_rename` seam. Host tests: valid/
  invalid names, clash, cancel, commit-then-refresh, refusal surfaced.

### FM6 — opening: double-click, launch `.app`, "Open With…" `[ ]`

Make items *do* something — the defining first-class behaviour.

- **Double-click / Enter dispatches by kind** through one engine
  `activate(entry)` decision (§2.2): a directory descends (as today); a
  `.app` bundle launches; a regular file opens in the associated viewer.
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
- **"Open With…"** lists the bundles whose `AppInfo` claims the type (from
  the same lookup) — a `Menu`, no invented registry. No match ⇒ an honest
  "no application" answer, never a crash (§2.24).
- **Async, non-blocking launch (`plans/FIX-DESKTOP.md`).** The spawn must
  not freeze the manager's window; it stays responsive and parked while
  the child starts. Host tests: the `activate` decision per kind, the
  association lookup (match / bundle / none), the grant-to-child seam.

### FM7 — clipboard operations: cut, copy, paste, delete, new folder `[ ]`

The core management verbs, modelled in the engine and executed in the app.

- **A selection + clipboard model** in `lib/browse`: multi-select
  (Shift/Ctrl-click ranges, Select All), a cut/copy set with the
  pending-op kind, and paste-target validation (no paste into a
  descendant of a moved dir, no self-overwrite without confirm) — all
  pure and host-tested.
- **Move** = `fs_rename` when source and target share a volume; otherwise
  **copy-then-delete**. **Copy** streams `fs_read`→`fs_write` in bounded,
  interruptible chunks (§2.23 — no unbounded buffer, no spin), preserving
  metadata where the target format allows and failing closed with
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
- Host tests: selection ranges, clipboard state machine, move-vs-copy
  volume decision, chunked-copy resume/cancel/partial-failure, delete
  confirm/refuse, mkdir+rename. App wires the VFS seams.

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

FM1→FM2→FM3 build the shared engine + view + icons (all host-proven, no
app-behaviour change until FM2 repaints). FM4 adds the chrome. FM5 is the
first write and the template for FM7. FM6 (launch/open) depends on FM3
(bundle/file kinds) and reuses AW5 delegation. FM7 depends on FM4's
selection/menu. FM8 and FM9 close out. Each lands fully gated; a stage
that turns out larger than one clean increment is split and staged here,
never shipped half-done "for now" (§2.19).

## 3. What this explicitly refuses to become

To stay best-in-class and bloat-free (§2.3), the file manager will **not**
grow: a built-in text/image editor (that is what associated apps and CU6
delegation are for), a search-indexer daemon, cloud/account integration, a
ribbon or customisable-toolbar framework, per-file-type plug-in surfaces,
or a second theming/rendering path. Anything that belongs to another
subsystem (viewers, the shell, the storage resolver) is *reached*, not
reimplemented here.
