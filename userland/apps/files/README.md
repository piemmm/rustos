# `rustos-files` — filesystem browser

Stage 7 deliverable (`AGENTS.md` §10, `PLAN.md` Stage 7). The default
graphical file manager: it navigates the §16 filesystem layout and renders the
current directory through the shared desktop theme. Installed as a `.app`
bundle under `/Apps` (`AGENTS.md` §16.5).

## What this crate is

A navigation **model** (`Browser`) plus a **renderer** (`render`), both driven
by an injected `DirectorySource`. It is a graphical app, so it consumes the
same `lib/*` building blocks the taskbar does — `lib/geometry`, `lib/theme`,
`lib/raster`, `lib/font` — and never depends on the window manager
(`AGENTS.md` §17.4).

## Navigation model (`Browser`)

`Browser::open_root` opens at `/` and lists its children. From there:

- `open_index` / `open_selected` descend into a directory entry.
- `go_up` climbs to the parent (`Ok(false)` at the root — not an error).
- `refresh` re-reads the current directory.
- `select` / `select_next` / `select_previous` move the selection cursor,
  clamping at both ends.

Every move that lists a directory is **transactional and fails closed**
(`AGENTS.md` §5.4): the target is listed *before* any state changes, so a
refused or failing read leaves the browser exactly where it was. Opening a
regular file is rejected (`BrowseError::NotADirectory`); an out-of-range index
is `BrowseError::NoSuchEntry`; a source failure surfaces
`BrowseError::Source(Errno)` (most often `PermissionDenied`).

## No `/proc`, no fabrication

RustOS has no `/proc` and no `/sys` (`AGENTS.md` §16.1). The browser shows
exactly the entries its `DirectorySource` returns — it never injects a
synthetic entry — and makes no permission decision of its own: the §5.3 check
and the §16 path policy live in the VFS behind the source.

## Rendering (`render`)

`render(browser, theme, viewport)` paints a path bar plus the (scrolling)
entry list into a `lib/raster` `Surface` sized to the viewport, using the
active theme's palette and the shared `lib/font` face. Directory names carry a
trailing `/`; the selected row is filled with the accent role. The surface is
the compositor's to place and round — there is no rounding and no colour
algebra here (`AGENTS.md` §2.2). Label truncation goes through the shared
`BitmapFont::truncate_to_width`, the same fit-to-width path the taskbar uses,
so it is not duplicated (§2.2).

## Seam

`DirectorySource::list(components) -> Result<Vec<Entry>, Errno>` is the one
thing the browser needs from outside. On a running system it is a
capability-checked VFS directory read; tests wire an in-memory tree, so the
navigation and rendering logic is exhaustively testable without a kernel
(`AGENTS.md` §7). The binary that ships as the file manager wires the
VFS-backed source (deferred until the userland VFS client lands).

## Layering & safety

`no_std` (with `alloc`); depends only on `rustos-abi` and the shared `lib/*`
desktop libraries, so this app never links a kernel, driver, or window-manager
crate (`AGENTS.md` §17.4). No `unsafe`, no `unwrap`/`expect`/`panic!` in
production paths (`AGENTS.md` §2.9).

## Test surface

`cargo test -p rustos-files` (14 unit tests): the four top-level directories at
the root; descend/climb path + entry tracking; fail-closed root open, file
open, out-of-range index, and unreadable-directory descent; the empty-directory
no-selection case; `open_selected`; selection clamping; refresh re-listing with
selection clamping; and the renderer (viewport sizing, accent highlight, and a
degenerate tiny viewport).
