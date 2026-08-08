# tairix-browse

Stability tier: **experimental**.

The shared directory-browser engine (`plans/APPWIN.md` AW5): the one
navigation model and themed listing renderer every directory browser in
the system composes, so the `files.app` windowed file manager and the
desktop session's trusted file picker (`plans/CAPABILITY_USE.md` CU6)
can never diverge in navigation semantics, listing policy, or look.

- **Entries** (`Entry`/`EntryKind`): each listed child carries its name,
  its kind, and the display metadata a file manager needs — apparent
  `size` and the modification `Time64` — mapped straight from the one
  `fs_readdir` stream the source already produced, so no child is opened
  and statted to fill a listing. `EntryKind` refines the VFS's
  file/directory split with the one distinction a manager must make
  structurally: a `<Name>.app` directory is a `Bundle` — a sealed unit the
  user launches, not a folder to descend into.
- **Content-type registry** (`media`): the one closed registry both the drawn
  file-type icon and the "Open With…" association vocabulary read, so the two
  can never drift apart. `MediaType` names a type by its IANA (or TAIRiX
  vendor) media-type spelling (`as_str` / `from_media_str`, round-tripping
  case-insensitively); `media_for_name(name)` maps a filename extension
  (ASCII-case-insensitive, allocating nothing) and `media_for_entry(entry,
  parent)` classifies a listed entry — `inode/directory` for a directory
  whatever its name, `application/x-tairix-service` for a `<Name>.app` listed
  from the system service store (`tairix_abi::SYSTEM_SERVICE_STORE`) and
  `application/x-tairix-app` elsewhere, and the extension's type for a regular
  file, falling closed to `application/octet-stream`. `MediaType::icon` is the
  `lib/icon` `IconKind` drawn for the type: deliberately **many-to-one** and
  the only part of the registry allowed to be, so two types are never merged
  merely because they draw alike (an application declaring the vanished type
  would silently stop matching its own files). `MediaType::parent` is the
  **subclass relation** the freedesktop.org shared-mime-info database models
  (`text/x-csrc` is a subclass of `text/plain`): every readable-text type names
  `text/plain` as its broader type — `image/svg+xml` reaches it through
  `application/xml` — while everything binary names none, the chain is finite
  and acyclic, and association matching walks it, so naming a format precisely
  never narrows what can open it. The whole registry is a **display and offer
  hint only**: it decides a glyph and which applications are *offered*, never
  an operation; authority stays in the VFS and the launcher.
- **Sort** (`SortMode`/`sort_entries`): the one listing order both views
  share — directories first, then a `Name`/`Size`/`Modified` key with a
  direction, with a case-insensitive name tiebreak so the result never
  depends on the source's incidental order. The default is name-ascending;
  `Browser::set_sort_mode` re-orders in place, keeping the selection on the
  same entry.
- **Model** (`Browser` over the injected `DirectorySource` seam):
  transactional, fail-closed navigation — descend, climb to the parent,
  refresh, a selection cursor, and the shared sort applied to every
  listing. The new directory is listed *before* any state changes, so a
  refused or failing read leaves the browser exactly where it was, and the
  engine never fabricates an entry: it shows exactly what its source
  returns, with every permission decision staying in the VFS behind the
  source under the composing process's own identity.
- **Navigation history and breadcrumbs** (`Browser`): a bounded back /
  forward stack (`go_back` / `go_forward`, with `can_go_back` /
  `can_go_forward` supplying the enable state of the Back / Forward toolbar
  controls), `navigate_to_depth`, the breadcrumb-click primitive that
  jumps to an ancestor by path depth (`0` is the root, `components().len()`
  is the current directory — a no-op), and `navigate_to(components)`, the
  jump-to-an-arbitrary-location primitive (neither an ancestor nor a listed
  child — the file manager's "go to Trash" location uses it to reach
  `Library/Trash`). Every one of these is the same transactional, fail-closed
  navigation as descend / climb: the target is listed before any state or
  history changes, a fresh navigation clears the forward branch, and the
  history is a bounded ring that drops the oldest location rather than growing
  without bound.
- **Frame model** (`chrome`, `plans/NEW-FILEMANAGER.md` FM4b): the pure
  model behind the drawn toolbar and breadcrumb path bar, host-proven ahead
  of the chrome it drives. `ToolbarModel::for_browser` snapshots which
  `ToolbarCommand` is actionable — Back / Forward / Up reflect the navigation
  history and depth (`can_go_back` / `can_go_forward` / `!is_root`); Refresh,
  the view toggle, and sort are always available — plus the active view and
  sort so a tool renders disabled (never hidden) when it cannot apply.
  `breadcrumbs` turns the root-first components into the ordered `Crumb`s of
  the path bar, each carrying the ancestor `depth` the drawn crumb binds to
  `navigate_to_depth` (`0` is the root); the terminal crumb is the current
  directory (`is_current`), whose jump is a no-op. `ToolbarCommand::icon`
  gives each command its `lib/icon` glyph, and `apply_command(browser, cmd)`
  is the one shared, **read-only** dispatch (Back / Forward / Up / Refresh /
  view toggle / sort cycle over `ViewMode::toggled` and `SortMode::next`) both
  the toolbar click and its keyboard accelerator run through, so they cannot
  diverge and the read-only picker can drive the same toolbar.
  `ContextMenuModel::for_browser(browser, has_clipboard)` snapshots which
  `ContextCommand` the right-click menu offers is actionable: Open / Rename /
  Cut / Copy / Properties act on the selection (an empty directory offers
  none), Open With… is offered only for a regular file (a directory descends
  and a bundle launches itself, so neither has an application to choose), and
  Paste needs only the app's held clipboard — threaded in, since the clipboard
  lives in the app, not the browser. Each command's `label()` and
  keyboard-`shortcut()` caption drive the drawn `MenuItem`, and
  `CONTEXT_COMMANDS` is the one top-to-bottom order the menu iterates. Only
  commands the file manager can carry out today are modelled, so none is
  speculative surface: Delete and New Folder are absent from
  `CONTEXT_COMMANDS` and each lands with the stage that first wires its
  behaviour. The drawn menu is the renderer's `build_context_menu` (one
  `MenuItem` per command, disabled when the model says so), `context_menu_rect`
  (anchored at the click and clamped to the window), `draw_context_menu`
  (painted topmost), and the mirror `context_menu_command_at` (returning
  **only** an enabled command — fail closed off the menu or on a disabled
  row); the files app opens it on a secondary-button press and routes a chosen
  command to the same verbs the toolbar and keyboard drive. The **Open With…**
  chooser itself is the renderer's `build_open_with_menu` (one row per
  `applications_for` candidate, in source order) plus the mirror
  `open_with_index_at` (the same enabled-row hit-test the context menu uses),
  and the files app launches the chosen candidate through the same document
  hand-off the default open uses.
- **In-place rename** (`rename`, `Browser::rename_selected`): the model of
  the file manager's first write operation (`plans/NEW-FILEMANAGER.md` FM5),
  host-tested without a kernel. `validate_new_name` spells the typed name
  through the one shared `tairix_path::validate_file_name` rule and rejects a
  clash with an existing sibling or a no-op rename to the same name;
  `Browser::rename_selected` then applies it through an injected `fs_rename`
  seam and re-lists, transactional and fail-closed — validated before any
  syscall, a VFS refusal leaves the listing untouched, and the selection
  follows the entry to its new name. The engine adds no authority (the write
  is the caller's own permission-checked `fs_rename`, no new capability), so
  the read-only picker composes the same `Browser` and never calls it.
- **Activation** (`activate`, `Browser::activate_selected` /
  `activate_index`): the one dispatch-by-kind decision behind a double-click
  or `Enter` (`plans/NEW-FILEMANAGER.md` FM6), so the file manager and the
  picker act identically. Exhaustive over the three kinds: a directory is
  *descended into* by the engine itself (its own fail-closed navigation); a
  bundle is named as `Activation::LaunchBundle` for the caller to launch
  through the ordinary signed app-load gate; a file is named as
  `Activation::OpenFile` for the caller to open in the associated viewer. The
  target's absolute path is spelled through the one shared `absolute_path`, so
  a launch or open can never name a different node than the browser shows, and
  a name that cannot be spelled fails closed. The engine holds no launch or
  open authority of its own — it decides *what* and *what should happen*, never
  performs the spawn or the `fs_open` (so the read-only picker never launches).
- **Double-click detection** (`click`, `DoubleClickTracker`,
  `plans/NEW-FILEMANAGER.md` FM12): the one pure rule that turns a stream of
  primary presses into single-click and double-click gestures, so a pointer
  double-click on an item runs the very same `Activation` a keyboard `Enter`
  does (§2.2). `register(now_ns, index)` pairs a press with the previous one
  only when it lands on the *same* item within `DOUBLE_CLICK_INTERVAL_NS`
  (half a second); a completed double consumes both presses (a third quick
  press starts a fresh single), a non-monotonic clock reading fails closed to
  a single, and `reset` breaks the pair when an intervening chrome click
  interrupts it. It holds no authority and does no I/O — the app supplies the
  hit-test index and the capability-free monotonic clock and performs the
  activation itself.
- **"Open With…" association** (`open_with`, `plans/NEW-FILEMANAGER.md` FM6b):
  the pure type→bundle model behind offering a file to a chosen application.
  A file's content type is the shared registry's (`media_for_name`), so the
  association vocabulary is exactly the one the drawn glyph comes from.
  `BundleSource` is the injected installed-bundle enumeration seam (the "Open
  With…" analogue of `DirectorySource`), and `applications_for` selects the
  `AppAssociation`s whose declared MIME set handles the file's type **or any
  broader type it is a subclass of** (`MediaType::parent`) — an editor
  declaring `text/plain` is offered for a `.rs` file. Candidates are ordered by
  how specifically they claim it (a bundle declaring `text/x-rust` before one
  declaring `text/plain`), and bundles claiming at the same level keep the
  source's enumeration order, so no existing ordering is disturbed. No match is
  an honest empty answer — a "no application" notice, never a fabricated
  default — and the type decision is a display hint only: the load gate still
  verifies and capability-checks whichever bundle the user picks. The engine
  never spawns. The renderer draws the candidate list as the
  `build_open_with_menu` chooser and resolves a click through
  `open_with_index_at` (sharing the context menu's placement and row geometry),
  so the files app can offer the full list where the default open picks the
  first.
  `association_from_appinfo(bundle_path, appinfo)` is the pure, fail-closed
  decode a running-system `BundleSource` uses per bundle: it reads a manifest's
  header and declared MIME table (the same body layout the loader reads) into an
  `AppAssociation`, skipping a corrupt or non-parsing manifest rather than
  offering it on a guess. It reads the declared types as a hint only and never
  verifies the signature — the signed load gate does that at launch.
- **Multi-selection** (`select`, `Browser` selection methods,
  `plans/NEW-FILEMANAGER.md` FM7): the per-listing set of marked entries the
  management verbs act on. `Selection` models a plain click (`single`), a
  `Ctrl`-click (`toggle`), a `Shift`-click range (`range_to`, grown from the
  anchor), and Select All (`select_all`); `Browser::toggle_selection` /
  `extend_selection_to` / `select_all` / `clear_selection` bounds-check every
  index against the live listing. The selection is index-based, so any listing
  change (navigate, refresh, re-sort) collapses it to the single focused entry,
  and an unmodified keyboard move collapses it too — standard file-manager
  semantics.
- **Cut/copy clipboard** (`clipboard`, `Browser::clipboard`,
  `plans/NEW-FILEMANAGER.md` FM7): the cross-directory set the move/copy verbs
  act on. `Browser::clipboard(op)` captures the selected entries' absolute
  component paths onto a `Clipboard` (`None` when nothing is selected), so it
  survives navigating to the paste target. `plan_paste(clipboard, target)`
  resolves each source to a destination in the target directory, **fail
  closed**: a target inside one of the moved items is `PasteError::WouldRecurse`
  (an exact component-prefix test — `/a/b` is within `/a`, `/ab` is not), and a
  paste back into an item's own directory is flagged
  (`PasteItem::overwrites_source`) for the app to confirm rather than silently
  clobber. The model names *what* would move where; the app performs the
  capability-checked `fs_rename` / streamed copy under the user's own identity,
  so composing it grants nothing and the read-only picker never builds a
  clipboard.
- **Paste execution** (`execute`, `plans/NEW-FILEMANAGER.md` FM7b): the pure
  model of *how* a planned paste is carried out, host-proven ahead of the app
  verbs. `paste_strategy(op, source, dest)` decides from the clipboard
  operation and the two items' `VolumeId`s (the 16-byte `fs_stat` volume id):
  a `Copy` streams, a `Cut` within one volume is a single `Rename`, a `Cut`
  across volumes is `CopyThenDelete` — the one `mv`/`st_dev` decision.
  `CopyCursor` walks a known-length source in fixed `COPY_CHUNK_LEN` steps,
  yielding the next `CopyChunk` to transfer; the app reads/writes it and
  reports the bytes carried with `advance`, so a large copy stays bounded and
  interruptible with no unbounded buffer and no spin, and it `resume`s from a
  persisted offset after a cancel or a preemption. It is fail closed:
  advancing (or resuming) past the source length is `CopyError::Overrun`,
  never a silent wrap. The engine does no I/O and the source is deleted only
  after a cross-volume copy fully succeeds — the app performs every syscall
  under the user's own identity, so the read-only picker never runs it.
- **Recursive copy** (`execute::CopyWalk`, `plans/NEW-FILEMANAGER.md` FM7b):
  the model of *how* a whole tree is copied — the copy-side analogue of
  `delete::DeleteWalk`. Where a delete removes contents before their container,
  a copy *creates* a destination directory *before* streaming its contents into
  it, so a child always has a parent to land in. `CopyWalk::from_items` starts
  the walk from resolved `(source, dest, is_directory)` items (the app supplies
  each item's kind, which the path-only clipboard does not carry) and is **fail
  closed** — an empty set or a source/dest naming the root yields no walk.
  `next_action` yields the next `CopyAction` — `MakeDir { dest }` (the app
  `fs_mkdir`s it and reports `created`), `List { source }` (the app reads it and
  reports its children with `expand`), or `CopyFile { source, dest }` (the app
  streams the bytes with a `CopyCursor` and reports `copied_file`). It does no
  I/O, keeps its own explicit stack (so a deep tree cannot overflow the call
  stack), stays within the shared `MAX_COPY_DEPTH` (a deeper tree is
  `CopyWalkError::TooDeep`, never descended without limit — the same
  `MAX_WALK_DEPTH` bound `DeleteWalk` obeys), and holds its exact position
  between steps so the app can cancel or be preempted without losing or
  repeating work. Driving it against the wrong step is
  `CopyWalkError::OutOfStep`, leaving the walk unchanged. `copied` is the honest
  rising count a progress indicator shows; the total is unknown until the reads
  reveal it, so no fabricated percentage. The app performs every syscall under
  the user's own identity, so composing it grants nothing and the read-only
  picker never runs it.
- **Delete** (`delete`, `Browser::plan_delete`, `plans/NEW-FILEMANAGER.md`
  FM7b): the model of *what* a Delete removes, host-proven ahead of the app
  verb. `Browser::plan_delete()` captures the selection into a `DeletePlan`
  (`None` when nothing is selected) whose `DeleteTarget`s each carry the
  entry's absolute component path and whether it is directory-backed on disk
  — a directory *or* a bundle, so a sealed `<Name>.app` is removed with
  `UnlinkFlags::DIRECTORY` and recursed into as the directory it really is,
  while a regular file is a leaf. `DeletePlan::new` is **fail closed**: an
  empty selection, or any target naming the root (an empty component list),
  yields no plan rather than one that could remove nothing or the root.
  `len`/`has_directories` are the honest figures a delete confirmation reports. The
  model names the removals; the app performs each `fs_unlink` under the user's
  own identity (no new capability), so composing it grants nothing and the
  read-only picker never builds one.
- **Delete execution** (`delete::DeleteWalk`, `plans/NEW-FILEMANAGER.md` FM7b):
  the model of *how* a `DeletePlan` is carried out — the depth-first traversal
  that removes a directory's contents before the directory itself, the
  delete-side analogue of `execute::CopyCursor`. `DeleteWalk::from_plan` starts
  the walk; `next_action` yields the next `DeleteAction` — `List(path)` (the app
  reads that directory and reports its children with `expand`, so they are
  removed first) or `Remove { path, is_directory }` (the app unlinks the leaf or
  now-empty directory and reports it with `complete_removal`). It does no I/O,
  keeps its own explicit stack (so a deep tree cannot overflow the call stack),
  stays within `MAX_DELETE_DEPTH` (a fail-closed bound — a deeper tree is
  `DeleteError::TooDeep`, never descended without limit), and holds its exact
  position between steps so the app can cancel or be preempted without losing or
  repeating work. Driving it against the wrong step is `DeleteError::OutOfStep`,
  leaving the walk unchanged. `removed` is the honest rising count a progress
  indicator shows; the total is unknown until the reads reveal it, so no
  fabricated percentage. This is the browser engine's own component-path
  traversal, distinct from `rm`'s coreutils removal engine — two consumers with
  two data models, not one algorithm copied twice. The drawn verb is the
  renderer's `build_delete_dialog` (a `lib/controls` `Dialog` naming the honest
  target count and warning about folders, with the Action Warmth on the safe
  Cancel, never the destructive Delete), `delete_dialog_rect` (centred/clamped
  bounds), `draw_delete_dialog`, and the mirror `delete_dialog_action_at`
  hit-test resolving a press to the Delete or Cancel button (`DELETE_CONFIRM_INDEX`
  / `DELETE_CANCEL_INDEX`); the files app opens it on `Delete`, confirms with
  `Enter`/Delete and cancels with `Escape`/Cancel, then drives the `DeleteWalk`
  to completion over the user's own `fs_readdir`/`fs_unlink`. Only the write-capable
  file manager builds and drives it — the read-only picker never deletes.
- **Move to Trash** (`trash`, `plans/NEW-FILEMANAGER.md` FM10): the pure model
  of a *recoverable* delete — a delete is reversible when that costs nothing.
  `trash_strategy` decides, from the item's and the Trash directory's
  `VolumeId`s, whether the removal is a cheap `TrashStrategy::Move` (a single
  same-volume `fs_rename` into Trash, recoverable until emptied) or must fall
  back to the irreversible `TrashStrategy::Unlink` (a cross-volume removal — a
  rename cannot span volumes — the existing `DeleteWalk` path), reusing the same
  volume identity `execute::paste_strategy` compares. `trash_dest_path` resolves
  a collision-free home inside the Trash directory: the original leaf when free,
  otherwise the smallest ` (n)` disambiguation inserted before the extension
  (`notes (2).txt`), reusing the registry's one extension split. It is fail
  closed — it never overwrites an existing trashed item and refuses a root Trash
  dir (`RootTrash`), an invalid original name (`InvalidName`), a disambiguation
  past the per-name limit (`TooLong`), or a search past `MAX_TRASH_NAME_ATTEMPTS`
  (`NoFreeName`). It touches no filesystem and holds no authority (the app
  performs the `fs_stat`/`fs_rename` under the user's own identity), so the
  read-only picker never runs it. `trash_dir` spells the fixed `Library/Trash`
  subtree of the user's home — the one definition the app and its QEMU witness
  share so where a trashed item lands cannot drift. The confirmation dialog is
  disposition-aware: `DeleteDisposition` (threaded into `build_delete_dialog`)
  picks a safe, recoverable *Move to Trash* wording or the destructive *Delete
  Permanently* wording, so what the dialog promises always matches what the
  confirmed delete does (the app decides the disposition from the targets' and
  Trash's `VolumeId`s via `trash_strategy`, FM10b). `empty_trash_plan`
  (`plans/NEW-FILEMANAGER.md` FM11) models the irreversible counterpart —
  emptying the Trash: it turns the Trash directory's listing into a
  `DeletePlan` over its *contents* (never the Trash directory itself), carried
  out by the same recursive `DeleteWalk` a permanent delete uses (§2.2), so
  emptying leaves the now-empty Trash folder in place. It returns `None` for an
  already-empty Trash (a no-op, not an error) and is fail closed: a root Trash
  dir (`RootTrash`) or an invalid child leaf (`InvalidName`) refuses the whole
  empty rather than remove outside Trash or silently skip an item. It touches
  no filesystem and holds no authority, so the read-only picker never builds one.
  The file manager wires both verbs through the manager-only toolbar tools
  (FM11b): `ManagerTool::Trash` navigates (`navigate_to`) to the user's Trash —
  the navigable Trash location — and `ManagerTool::EmptyTrash`, enabled only in
  a non-empty Trash (`ManagerToolModel`), builds the `empty_trash_plan`,
  confirms it with the `DeleteDisposition::Permanent` dialog, and drives its
  `DeleteWalk` through the same interleaved progress/cancel runner a delete uses.
- **New folder** (`mkdir`, `Browser::create_directory`,
  `plans/NEW-FILEMANAGER.md` FM7b): the model of creating a directory in the
  current listing, host-proven ahead of the New Folder tool. `validate_new_dir_name`
  spells the typed name through the one shared `tairix_path::validate_file_name`
  rule (the same rule the rename editor uses) and refuses a name already taken
  by a sibling (`MkdirError::Clash`) — both decided before any syscall.
  `Browser::create_directory` then spells the child path through the shared
  `spell_child`, applies the create through an injected `fs_mkdir` seam, and on
  success re-lists and follows the selection onto the new folder (ready for the
  app's inline rename); a VFS refusal leaves the listing exactly as it was and
  is surfaced as `MkdirError::Refused`. The create is the caller's own
  permission-checked `fs_mkdir` (no new capability), so the read-only picker
  composes the same `Browser` and never calls it. `suggest_new_dir_name`
  supplies the non-clashing placeholder name (`New Folder`, then `New Folder 2`,
  …) the manager creates with before opening the inline rename — bounded by the
  listing (pigeonhole), never an arbitrary cap.
- **Places / devices rail** (`places`, `layout::SidebarView`,
  `plans/NEW-FILEMANAGER.md`): the shortcut column down the leading edge of a
  file-manager window — the user's own places above, every mounted volume
  below. `Places::new(home, volumes)` is **pure**: it takes the home
  directory's path components and the volumes the caller has already learned
  about and returns an ordered, validated, deduplicated list, so the model
  cannot open, stat, or list anything and is host-proven without a kernel.
  Reading the live mount table is the composing app's job — the file manager
  reads `MOUNT_LIST` through `lib/procinfo` and offers only mounts reporting
  themselves available, so a surprise-removed device is never drawn as a row
  that would fail on the first click.
  - **One fixed order**, so the rail never reshuffles between two paints of
    the same state: Home, Desktop, Documents, the application root, the system
    root — the fixed user places, offered whether or not their directories
    exist, since the model does no I/O and a shortcut that silently vanishes is
    less honest than one that says why it cannot open — then a drawn
    separation at `volume_start`, then the accepted volumes sorted stably by
    label. They are sorted *before* deduplication, so which row survives a
    duplicated target depends only on the set of volumes, never on the order
    the mount table happened to page them out in. An empty `home` drops the
    three home-derived rows rather than spelling a row that navigates nowhere.
  - **Fail closed on every offered volume.** A mount record is text this
    process did not author, so `Volume` is validated, never trusted: an empty
    label, one longer than `MAX_PLACE_LABEL`, or one carrying a control
    character is dropped; a target that is not absolute or does not parse into
    components is dropped; a target an already-accepted row (fixed or volume)
    covers is dropped. A malformed volume is never repaired, truncated, or
    guessed at into a row that would navigate somewhere else, and no stale
    volume row is ever fabricated.
  - **The medium is real data.** Each volume carries the storage medium its
    backing device actually reports (the mount record's `BlkDeviceClass`), and
    `tairix_icon::disk_icon` maps it to the shipped artwork — rotational,
    solid-state, and removable each draw their own drive icon, while a
    paravirtual or absent class draws the generic drive glyph. A USB stick can
    never masquerade as an internal disk, and nothing here classifies a device
    by its name or by guesswork.
  - **Below the toolbar, on the listing's row grid.** The command toolbar is
    *window* chrome: its band spans the full window width at the top, so it
    reaches the window's leading edge and aligns with the rest of the
    desktop's chrome. `sidebar_view` therefore insets the rail's top by the
    toolbar band, which puts the rail's first row top exactly at the path
    bar's top and every later row on the same `row_height` grid as the
    listing rows beside it — the rail reads as a navigation pane for the
    whole content region rather than a strip on a grid of its own.
  - **Geometry defined once** (`layout::SidebarView`): the rail's width is
    derived from the theme metrics and the theme's own body face — the padded
    row height plus the measured `WIDEST_FIXED_LABEL`, clamped to a third of
    the window — never a magic constant, so every fixed label fits at any UI
    density.
    `rail_rect`, `row_rect`, and `separator_rect` place the paint and
    `index_at` inverts exactly those rectangles, so the row drawn and the row a
    click resolves to can never disagree; a point outside the rail, past the
    last row, or on the separation band resolves to nothing.
  - **Drawn through the shared control.** Each row is a `lib/controls`
    `ListRow`, so it inherits the artwork seam (a volume shows its medium's
    shipped artwork and falls back to the built-in glyph) and every state the
    control offers is real: hover from the pointer, focus from the rail's own
    keyboard cursor while the rail holds the focus field, selected for the row
    matching the browser's current location (`index_of`, an **exact** component
    match — standing inside a subdirectory of a place highlights nothing rather
    than claiming the user is at the place itself), and disabled for a row
    `set_unavailable` marked after a navigation to it was actually refused,
    never a row assumed dead in advance.
  - **The interaction state lives on the model** (`cursor` / `move_cursor`,
    which clamps rather than wraps so a held arrow cannot cycle the rail
    endlessly; `is_focused` / `set_focused`; `hovered` / `set_hovered`), so the
    paint, the hit-test, and the app's key routing all read one state. There is
    **no mount-change notification** to subscribe to, so the volume rows are
    rebuilt when the user asks the window to re-read what is there — never by a
    poll or a timer.
  - The trusted file picker passes no rail (`ManagerChrome::none`), and that
    emptiness is deliberate rather than unfinished: the picker's whole purpose
    is bounded to the directory tree the requesting application was authorised
    to be shown, so a rail offering one-click jumps to arbitrary mounted
    volumes would widen the pick beyond what was asked for. With no rail the
    window is laid out exactly as it is with no sidebar at all.
- **Item-view geometry** (`layout`): two views over one selection and one
  scroll offset — `ListView` (a column of full-width rows) and `GridView`
  (a wrapped grid of icon tiles) — behind the `ViewLayout` dispatch, the
  one definition of which items are visible for a given scroll offset, the
  rectangle each occupies, and the pixel-to-index hit-test. Both clamp
  their scroll window through the shared `lib/controls` `scroll::ScrollRange`
  rather than a re-derived anchor, and share one `reveal` rule that keeps
  the selection on screen, so the renderer and the pointer hit-test can
  never disagree. `ViewMode` selects the view; toggling preserves the
  selection and re-reads nothing.
  Only whole tiles are ever laid out (`visible_lines` lines of
  `cells_per_line` tiles), so no tile is cut by an edge. `GridFill` is the
  grid's policy for the space a line has left over, and it is a property of
  the *view*: a **resizable** grid takes `Spread`, sharing the leftover width
  out along the row so the gaps widen by equal amounts and the two end margins
  match — only the space between the tiles moves, a tile never stretches —
  while a **fixed** field, the desktop's icon column, takes `FixedPitch` and
  keeps one tile-plus-gap pitch from the edge its icons hug so an icon does
  not drift when the area's extent changes. The pitch is the floor under
  either policy, so a line that fits its tiles exactly is laid out
  identically, and the axis a grid *scrolls* along is never spread: the space
  past the last whole line belongs to the next line, one scroll away.
- **Column formatting** (`format`): `format_size` (binary units — `1.5
  MiB`), `format_date` (`Time64` → an ISO `YYYY-MM-DD`, blank at the
  epoch so a stampless file is never given a fabricated date), and
  `format_datetime` (the properties view's `YYYY-MM-DD HH:MM:SS`
  date-and-time spelling, blank at the epoch for the same reason) — the
  file-listing convention shared by both views.
- **Properties** (`properties`, `plans/NEW-FILEMANAGER.md` FM8): the pure
  view model behind the Properties panel. `Properties::from_stat` turns an
  entry's name, its browser
  `EntryKind`, and the node's `fs_stat` `FileStat` into the display fields the
  panel shows — a human kind label (`Folder` / `File` / `Application`), the
  apparent and on-disk sizes (via `format_size`), the raw and octal mode, the
  ten-character permission string (the shared `tairix_abi::fs::mode_string`
  spelling, so it never disagrees with `ls -l`), the owning uid/gid, and the
  four `Time64` stamps rendered with `format_datetime` (blank when the backing
  keeps none — no fabricated wall time). It reads nothing and holds no
  authority: the app performs the one capability-checked `fs_stat` under the
  user's own identity and hands the result here, so the read-only picker
  builds the same view. The drawn overlay (FM8b) is the renderer's
  `draw_properties`: a shared `lib/controls` `Panel` centered over the view
  painting `properties_rows` (the one definition of which fields appear —
  Kind, Size, Permissions, Owner, and the four stamps), which the files app
  opens with `Alt+Enter` and dismisses with `Escape`. `Browser::selected_target_path`
  is the shared spelling of the selected node's absolute path the `fs_stat`
  acts on.
- **Permission edit** (`mode_edit`, `Browser::set_mode_selected`,
  `plans/NEW-FILEMANAGER.md` FM8b): the model of committing a new permission
  mode to the selected node, host-proven ahead of the drawn permission
  control. `validate_mode` fails closed on any bit above
  `tairix_abi::fs::FS_MODE_MASK` (the settable `rwx`/setuid/setgid/sticky
  word) — refused, never masked into a different mode, so the mode applied is
  exactly the one asked for. `Browser::set_mode_selected` spells the selected
  node's absolute path through the one shared `absolute_path`, validates the
  mode before any syscall, and applies it through an injected `fs_set_mode`
  seam; a VFS refusal leaves the node's mode unchanged and is surfaced as
  `ModeError::Refused`. The listing carries no mode, so a success re-reads
  nothing (the app re-stats to refresh the Properties view). The change is the
  caller's own permission-checked `fs_set_mode` (no new capability), so the
  read-only picker composes the same `Browser` and never calls it. The drawn
  control is a labelled permissions grid below the metadata fields:
  `render::PERMISSION_BITS` / `permission_cells` are the one definition of the
  nine owner/group/other `rwx` bits, `render::draw_properties_editable` draws
  `Read`/`Write`/`Exec` column headers over three `Owner`/`Group`/`Other` triad
  rows of clickable `lib/controls` `Checkbox`es (replacing an earlier cramped
  single-row layout whose boxes overlapped and carried no label), the shared
  `render::PermGrid` geometry places the painted grid and its hit-test from one
  definition, and `render::permission_cell_at` returns the bit a click toggles
  (fail closed off a toggle). Only the write-capable file manager draws the
  editable overlay; the picker draws the read-only `draw_properties` and
  never resolves a toggle (separated by call site, not a runtime flag). The
  setuid/setgid/sticky bits stay in the octal/symbolic display and are edited
  via `chmod` — a deliberate scope boundary, and a toggle preserves them.
- **Ownership edit** (`owner_edit`, `Browser::set_owner_selected`,
  `plans/NEW-FILEMANAGER.md` FM8b): the model of committing a new owning user
  and/or group to the selected node (the `chown(2)` / `chgrp(2)` shape),
  host-proven ahead of the drawn ownership control. Unlike rename, mode, and
  mkdir — the user's own §5.3-checked writes — reassigning the owner is a
  **privileged** operation: the kernel's secured VFS requires the dedicated
  `CAP_FS_CHOWN` to change the uid or set a group the caller is not a member
  of, and clears the set-*id* bits on any change. The engine models none of
  that policy — it names *what* to change via `OwnerChange` (each field `None`
  = unchanged, `Some(id)` = set), and `validate_owner` fails closed on a field
  set to the reserved `FS_OWNER_UNCHANGED` sentinel as an explicit target.
  `Browser::set_owner_selected` spells the selected node's absolute path
  through the one shared `absolute_path`, validates before any syscall, maps
  `None` onto the sentinel, and applies through an injected `fs_set_owner`
  seam; a VFS refusal (including the missing-`CAP_FS_CHOWN` denial) leaves the
  ownership unchanged and surfaces as `OwnerError::Refused`. The listing
  carries no ownership, so a success re-reads nothing (the app re-stats to
  refresh the Properties view). The model holds no authority, so the read-only
  picker composes the same `Browser` and never calls it. The drawn control is
  inline on the Properties owner row: `render::OwnerField` /
  `render::owner_field_at` are the mirror hit-test resolving a click to the uid
  or gid value it edits, and `render::draw_owner_control` underlines each value
  as editable and draws the active `lib/controls` `TextField` over the one being
  edited. Only the file manager calls it — and only where the launching user
  holds `CAP_FS_CHOWN` (read from the kernel-attested `self_origin`), so a
  session that cannot use it is never shown it.
- **Progress + cancel** (`progress`, `plans/NEW-FILEMANAGER.md` FM7b): the
  pure display + cancel *state* of a long file operation the file manager
  drives interleaved with its event loop. `ProgressModel` carries the
  operation kind (`ProgressOp::Delete`/`Copy`/`Trash`), the honest rising count
  the driving walk reports (`DeleteWalk::removed` / `CopyWalk::copied` / the
  move-to-Trash item count), and a
  *latched* cancel; its `title`/`status_line` never fabricate a percentage,
  since the total is unknown until the walk's reads reveal it. The drawn
  surface is the renderer's `draw_progress_dialog` (a `lib/controls` `Panel`,
  an indeterminate `Progress` "working" trace captioned with the count, and a
  Cancel `Button`), with `progress_cancel_at` the mirror hit-test resolving a
  click to the drawn Cancel (fail closed off it). The model holds no authority
  and does no I/O, so the read-only picker (which never deletes or copies)
  never builds one. The delete, copy/paste, and move-to-Trash drives are all
  wired end to end in the files app, each reusing this one panel.
- **Breadcrumb placement** (`breadcrumb`, `plans/NEW-FILEMANAGER.md` FM4b):
  the pure geometry of the drawn, clickable path bar. `layout` places each
  `Crumb`'s label left to right from measured widths and **right-anchors** the
  strip so the current directory stays visible, letting overflowing leading
  ancestors scroll off the left (clipped) rather than dropping any crumb;
  `crumb_at` is the mirror hit-test over that same placement. Font-agnostic
  (it works in measured pixel widths), so it is host-proven with synthetic
  widths and shared by the painter and the pointer hit-test (one definition).
- **Renderer** (`render`): every entry point takes the desktop's
  `tairix_geometry::Scale` and the active `Theme`, and nothing takes a
  typeface: each resolves the body face the theme's own ladder names at that
  scale, so a caller cannot substitute a face the desktop does not draw with,
  and every chrome length — row pitch, tile size, toolbar strip, scrollbar
  gutter, panel and dialog bounds, menu placement — is authored logically and
  converted through that one scale, so the chrome tracks the display density
  the text does. It paints the path bar and the current directory
  into a `lib/raster` `Surface` in the browser's `ViewMode` — the path bar as
  a clickable breadcrumb trail (ancestors in the accent colour, the current
  directory drawn solid and inert) over the `breadcrumb` placement, list
  entries as shared `lib/controls` `TableRow`s (name/size/modified columns),
  grid entries as shared `IconTile`s, each carrying the icon of its
  registry-classified type above the label and no plate of its own (only a
  hovered, selected, or focused entry paints a panel behind its icon) — so the
  file manager and the trusted picker render one coherent themed surface, the
  selected item carrying the shared selection state. The grid paints inside
  `GridView::tile_area`, so a tile can never mark the chrome above it or the
  scrollbar gutter beside it whatever it draws inside its own rectangle.
  `render`'s trailing `artwork: &mut dyn tairix_icon::IconArtwork` is the
  draw-site icon lookup: for each grid tile it is asked for the classified
  `IconKind` at exactly the side `IconTile::icon_side` reserves, and the tile
  blits what it returns or draws the built-in vector glyph when it returns
  `None` — so a missing, oversize, or refused asset degrades to a meaningful
  icon and can never blank the tile. A caller with no cache passes
  `tairix_icon::NoArtwork`; the list view is text-only and never
  consults the lookup. A vertical `lib/controls` `ScrollBar` is drawn in
  a reserved right-edge gutter over the same `ScrollRange`; `scroll_lines`
  routes the wheel through the shared `scroll::ScrollModel`, `reveal_selection`
  keeps the selection visible, and `entry_index_at` is the shared item point
  hit-test (`crumb_at` its path-bar counterpart). Above the path bar it draws
  the command toolbar (`chrome::TOOLBAR_COMMANDS` as a `lib/controls`
  `Toolbar` of themed `IconButton`s, disabled tools muted from the
  `ToolbarModel`); `toolbar_command_at` is that strip's hit-test, returning
  only an *enabled* command (fail closed). The toolbar band is the one piece
  of window chrome here, so `toolbar_bounds` and the three hit-tests that
  invert it (`toolbar_command_at`, `manager_tool_at`, `manager_tool_rect`)
  take the **whole window**, while `content_area` — the window less the rail —
  is what the path bar, the item view, the scrollbar, and every overlay
  occupy. With no rail (the picker) `content_area` *is* the window, so that
  window is laid out exactly as it always was. Because a *write* action must never
  reach the read-only picker that shares this toolbar, the manager-only write
  tools live in a separate `chrome::ManagerTool` vocabulary (`MANAGER_TOOLS`,
  `ManagerTool::icon`): New Folder, the Trash location (go to the user's Trash),
  and Empty Trash (permanently remove the Trash's contents). A write-capable
  consumer hands `render` a `ManagerChrome`: its tools, a
  `chrome::ManagerToolModel` enable snapshot, and its places rail (the file
  manager passes `MANAGER_TOOLS`, a live model, and its `Places`; the picker
  passes `ManagerChrome::none()`), grouped into one value so a caller cannot
  draw a rail's rows while hit-testing a window that has none. The tools draw
  in their own toolbar group after the read-only commands, each rendered
  disabled (muted, never hidden) when the model reports it inactive — Empty
  Trash is offered only when the current directory is the user's non-empty
  Trash, a fact the file manager computes from `HOME` and threads in.
  `manager_tool_at` is their mirror
  hit-test, resolving only an *enabled* tool (fail closed), so the picker can
  neither draw nor resolve a write tool. `chrome_height` (the toolbar strip
  plus the path bar) is the one header offset the item views, the scrollbar
  gutter, and every hit-test share. `selection_rect` is
  `entry_index_at`'s inverse — the rectangle the selected item is drawn in, so
  an overlay (the in-place rename editor) sits exactly over it.
  `selection_rect`'s sibling `draw_properties` draws the FM8b Properties
  overlay — a centered `lib/controls` `Panel` painting `properties_rows` for
  the selected node's `Properties`, clipped so a too-small window shows what
  fits rather than panicking. `draw_properties_editable` is the file manager's
  variant, drawn in the taller `properties_editable_panel_rect`: the metadata
  fields plus a labelled `Owner`/`Group`/`Other` × `Read`/`Write`/`Exec`
  permissions grid of clickable toggles (`permission_cell_at` its hit-test, the
  shared `PermGrid` geometry placing both);
  `draw_owner_control` (`owner_field_at` its hit-test) likewise makes the owner
  row's uid/gid values editable, drawn only for a `CAP_FS_CHOWN` holder.
  When the chrome carries a places rail, `render` paints the rail down the
  leading edge first and lays everything else out inside `content_area`: the
  toolbar, the path bar, the list or grid, and the scrollbar gutter are all
  inset by the rail, and every hit-test (`toolbar_command_at`, `crumb_at`,
  `entry_index_at`, the scrollbar) resolves against that same inset area, with
  `sidebar_index_at` the rail's own mirror. With no rail `content_area` is the
  viewport unchanged, so a window without one is pixel-for-pixel what it was
  before the rail existed. `WIN_WIDTH`/`WIN_HEIGHT` are the one
  browser-view geometry the files app, the picker, and the QEMU vertical's
  host-side assertions share; the files app opens its window `resizable` and
  re-maps this surface on a `WindowEvent::Resized`, laying the same renderer out
  to the new viewport.
- **Path spelling** (`vfs`): `absolute_path` (root-first components into
  a bounded, validated absolute path — each component checked by the shared
  `tairix_path::validate_file_name` rule, the same rule the rename editor
  spells a new name through, before any syscall) and `VfsDirectorySource`,
  the composition over an injected `fetch(path) -> stream` primitive so
  the engine is host-proven end to end without a kernel.

`no_std` (with `alloc`); depends only on `lib/abi`, `lib/path`,
`lib/geometry`, `lib/theme`, `lib/raster`, `lib/font`, `lib/controls`, and
`lib/icon` — never a kernel, driver, or window-manager crate. No `unsafe`.
