# tairix-browse

Stability tier: **experimental**.

The shared directory-browser engine (`plans/APPWIN.md` AW5): the one
navigation model and themed listing renderer every directory browser in
the system composes, so the `files.app` windowed file manager and the
desktop session's trusted file picker (`plans/CAPABILITY_USE.md` CU6)
can never diverge in navigation semantics, listing policy, or look.

- **Model** (`Browser` over the injected `DirectorySource` seam):
  transactional, fail-closed navigation — descend, climb to the parent,
  refresh, and a selection cursor. The new directory is listed *before*
  any state changes, so a refused or failing read leaves the browser
  exactly where it was, and the engine never fabricates an entry: it
  shows exactly what its source returns, with every permission decision
  staying in the VFS behind the source under the composing process's own
  identity.
- **Renderer** (`render`): paints the path bar and the scrolling entry
  list into a `lib/raster` `Surface` through the active `lib/theme`
  palette and the shared `lib/font` face; `WIN_WIDTH`/`WIN_HEIGHT` are
  the one browser-view geometry the files app, the picker, and the QEMU
  vertical's host-side assertions share.
- **Path spelling** (`vfs`): `absolute_path` (root-first components into
  a bounded, validated absolute path — refusing an empty, `/`-bearing,
  or NUL-bearing component before any syscall) and `VfsDirectorySource`,
  the composition over an injected `fetch(path) -> stream` primitive so
  the engine is host-proven end to end without a kernel.

`no_std` (with `alloc`); depends only on `lib/abi`, `lib/geometry`,
`lib/theme`, `lib/raster`, and `lib/font` — never a kernel, driver, or
window-manager crate. No `unsafe`.
