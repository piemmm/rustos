# Default desktop apps

The default graphical applications live under `userland/apps/`. They are
ordinary `.app` bundles (`AGENTS.md` §16.5) that consume the shared desktop
`lib/*` crates — `rustos-geometry`, `rustos-theme`, `rustos-raster`,
`rustos-font` — exactly as the taskbar does, and never depend on the window
manager (`AGENTS.md` §17.4).

## Filesystem browser (`rustos-files`)

The filesystem browser navigates the §16 filesystem layout and renders the
current directory through the active theme. It is split into a navigation
**model** and a **renderer**, both driven by an injected directory-read seam,
so the security-relevant logic is testable without a kernel (`AGENTS.md` §7).

### The directory-read seam

`DirectorySource::list(components)` returns the children of an absolute path
(root-first components; the empty slice is `/`). On a running system the seam
is a capability-checked VFS directory read, so the §5.3 permission decision and
the §16 path policy live in the VFS, not in the app. The browser shows exactly
the entries the source returns — it never fabricates a `/proc`/`/sys`-style
synthetic entry (`AGENTS.md` §16.1). Each entry is an `Entry` carrying a name
and an `EntryKind` (directory or regular file).

### The navigation model

`Browser` holds the current directory's path and entries plus a selection
cursor:

- `open_root` opens at `/` and lists it.
- `open_index` / `open_selected` descend into a directory entry; `go_up`
  climbs to the parent and returns `Ok(false)` at the root (no parent is not
  an error).
- `refresh` re-reads the current directory, clamping the selection into the
  new listing.
- `select`, `select_next`, and `select_previous` move the selection, clamping
  at both ends.

Every directory-listing move is **transactional and fails closed**
(`AGENTS.md` §5.4): the target is listed *before* any state changes, so a
refused or failing read leaves the browser on the directory it was already
showing. The fail-closed outcomes are the `BrowseError` variants: `Source`
(the wrapped boundary `Errno`, e.g. `PermissionDenied`), `NoSuchEntry`, and
`NotADirectory`.

### Rendering

`render(browser, theme, viewport)` paints a path bar plus a scrolling entry
list into a `rustos-raster` `Surface` sized to the viewport, using the theme's
palette for every colour and the shared `rustos-font` face for every label.
Directory names carry a trailing `/`, and the selected row is filled with the
accent role. The surface is rectangular; the compositor places and rounds it
through its single anti-aliased rounded-corner path, so there is no rounding —
and no colour algebra — in the app (`AGENTS.md` §2.2). Label truncation reuses
`BitmapFont::truncate_to_width`, the same fit-to-width path the taskbar uses,
rather than a second copy (§2.2). The list scrolls so the selected entry stays
visible, and every length saturates so a degenerate viewport paints what it
can rather than panicking (`AGENTS.md` §2.9).

### Still to do

The browser model and renderer are complete and headless-tested; wiring the
VFS-backed `DirectorySource` and the live window-manager surface is deferred
until the userland VFS client and the taskbar↔WM event glue land. The terminal
emulator is the other Stage 7 default app and is not yet implemented.
