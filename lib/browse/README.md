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
- **File-type icons** (`icon`): `icon_for(entry)` / `icon_for_name(name)` —
  the one classifier both views share, mapping an entry to a `lib/icon`
  `IconKind` by kind first (folder / app-bundle) then a small, documented
  filename-extension table (text / image / archive / executable) with the
  generic file glyph as the fail-closed fallback. A **display hint only**: it
  decides a glyph, never an operation; authority stays in the VFS and the
  launcher.
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
  controls) and `navigate_to_depth`, the breadcrumb-click primitive that
  jumps to an ancestor by path depth (`0` is the root, `components().len()`
  is the current directory — a no-op). Every one of these is the same
  transactional, fail-closed navigation as descend / climb: the target is
  listed before any state or history changes, a fresh navigation clears the
  forward branch, and the history is a bounded ring that drops the oldest
  location rather than growing without bound.
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
  none), Open With… applies only to a regular file (a directory descends and a
  bundle launches itself), and Paste needs only the app's held clipboard —
  threaded in, since the clipboard lives in the app, not the browser. Every
  command maps to an engine action that already exists, so it is not
  speculative; Delete and New Folder, whose action does not exist yet, are
  absent from `CONTEXT_COMMANDS` and land with the stage that first wires them.
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
- **"Open With…" association** (`open_with`, `plans/NEW-FILEMANAGER.md` FM6b):
  the pure type→bundle model behind offering a file to a chosen application.
  `mime_for_name` derives a file's content type from its filename extension —
  the one bridge from a name to the MIME vocabulary a bundle declares its
  associations in, recognising exactly the extensions the `icon` classifier
  draws a typed glyph for. `BundleSource` is the injected installed-bundle
  enumeration seam (the "Open With…" analogue of `DirectorySource`), and
  `applications_for` selects the `AppAssociation`s whose declared MIME set
  handles a file's type, in the source's order. No match is an honest empty
  answer — a "no application" notice, never a fabricated default — and the type
  decision is a display hint only: the load gate still verifies and
  capability-checks whichever bundle the user picks. The engine never spawns.
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
  read-only picker composes the same `Browser` and never calls it.
- **Breadcrumb placement** (`breadcrumb`, `plans/NEW-FILEMANAGER.md` FM4b):
  the pure geometry of the drawn, clickable path bar. `layout` places each
  `Crumb`'s label left to right from measured widths and **right-anchors** the
  strip so the current directory stays visible, letting overflowing leading
  ancestors scroll off the left (clipped) rather than dropping any crumb;
  `crumb_at` is the mirror hit-test over that same placement. Font-agnostic
  (it works in measured pixel widths), so it is host-proven with synthetic
  widths and shared by the painter and the pointer hit-test (one definition).
- **Renderer** (`render`): paints the path bar and the current directory
  into a `lib/raster` `Surface` in the browser's `ViewMode` — the path bar as
  a clickable breadcrumb trail (ancestors in the accent colour, the current
  directory drawn solid and inert) over the `breadcrumb` placement, list
  entries as shared `lib/controls` `TableRow`s (name/size/modified columns),
  grid entries as shared `Card` tiles, each tile carrying its `icon`-classified
  file-type glyph above the label — so the file manager and the trusted
  picker render one coherent themed surface, the selected item carrying the
  shared selection state. A vertical `lib/controls` `ScrollBar` is drawn in
  a reserved right-edge gutter over the same `ScrollRange`; `scroll_lines`
  routes the wheel through the shared `scroll::ScrollModel`, `reveal_selection`
  keeps the selection visible, and `entry_index_at` is the shared item point
  hit-test (`crumb_at` its path-bar counterpart). Above the path bar it draws
  the command toolbar (`chrome::TOOLBAR_COMMANDS` as a `lib/controls`
  `Toolbar` of themed `IconButton`s, disabled tools muted from the
  `ToolbarModel`); `toolbar_command_at` is that strip's hit-test, returning
  only an *enabled* command (fail closed). `chrome_height` (the toolbar strip
  plus the path bar) is the one header offset the item views, the scrollbar
  gutter, and every hit-test share. `selection_rect` is
  `entry_index_at`'s inverse — the rectangle the selected item is drawn in, so
  an overlay (the in-place rename editor) sits exactly over it.
  `selection_rect`'s sibling `draw_properties` draws the FM8b Properties
  overlay — a centered `lib/controls` `Panel` painting `properties_rows` for
  the selected node's `Properties`, clipped so a too-small window shows what
  fits rather than panicking. `WIN_WIDTH`/`WIN_HEIGHT` are the one
  browser-view geometry the files app, the picker, and the QEMU vertical's
  host-side assertions share.
- **Path spelling** (`vfs`): `absolute_path` (root-first components into
  a bounded, validated absolute path — each component checked by the shared
  `tairix_path::validate_file_name` rule, the same rule the rename editor
  spells a new name through, before any syscall) and `VfsDirectorySource`,
  the composition over an injected `fetch(path) -> stream` primitive so
  the engine is host-proven end to end without a kernel.

`no_std` (with `alloc`); depends only on `lib/abi`, `lib/path`,
`lib/geometry`, `lib/theme`, `lib/raster`, `lib/font`, `lib/controls`, and
`lib/icon` — never a kernel, driver, or window-manager crate. No `unsafe`.
