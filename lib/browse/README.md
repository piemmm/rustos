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
- **Item-view geometry** (`layout::ListView`): the one definition of
  which entry rows are visible for a given selection, the rectangle each
  occupies, and the pixel-to-index hit-test — built on the shared
  `lib/controls` `scroll::ScrollRange` clamp rather than a re-derived
  anchor, so the renderer and the pointer hit-test can never disagree.
- **Column formatting** (`format`): `format_size` (binary units — `1.5
  MiB`) and `format_date` (`Time64` → an ISO `YYYY-MM-DD`, blank at the
  epoch so a stampless file is never given a fabricated date), the
  file-listing convention shared by both views.
- **Renderer** (`render`): paints the path bar and the scrolling entry
  list into a `lib/raster` `Surface`; each entry is a shared
  `lib/controls` `TableRow` (name/size/modified columns) so the file
  manager and the trusted picker render one coherent themed surface, the
  selected row carrying the row chrome's selection state. `WIN_WIDTH`/
  `WIN_HEIGHT` are the one browser-view geometry the files app, the
  picker, and the QEMU vertical's host-side assertions share.
- **Path spelling** (`vfs`): `absolute_path` (root-first components into
  a bounded, validated absolute path — refusing an empty, `/`-bearing,
  or NUL-bearing component before any syscall) and `VfsDirectorySource`,
  the composition over an injected `fetch(path) -> stream` primitive so
  the engine is host-proven end to end without a kernel.

`no_std` (with `alloc`); depends only on `lib/abi`, `lib/geometry`,
`lib/theme`, `lib/raster`, `lib/font`, and `lib/controls` — never a
kernel, driver, or window-manager crate. No `unsafe`.
