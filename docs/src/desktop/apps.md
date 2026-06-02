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
until the userland VFS client and the taskbar↔WM event glue land.

## Terminal emulator (`rustos-terminal`)

The terminal emulator hosts the system shell and shows its output on a
character-cell screen rendered through the active theme. Like the browser it is
split into a screen **model** and a **renderer**, both driven by an injected
shell I/O seam, so the parsing and rendering logic is testable without a kernel
(`AGENTS.md` §7).

### The shell seam

`ShellSource::read()` returns the bytes the shell has produced since the last
call (an empty read is not an error) and `ShellSource::write(bytes)` forwards
the user's keystrokes. On a running system the seam is a capability-checked
pseudo-terminal channel to the shell process, so the process-spawn and
job-control authority lives behind the seam, not in the app.

### The screen model

`Grid` is a fixed `cols`×`rows` rectangle of cells with a cursor; it exposes
the cursor-relative operations a terminal needs — writing a glyph (wrapping and
scrolling at the edges), the C0 moves (backspace, tab, line feed, carriage
return), absolute/relative cursor positioning, the ANSI erase operations, and
clear. `Parser` is the streaming interpreter from shell output bytes to those
operations: it handles printable ASCII, the C0 controls, and a subset of ANSI
CSI escape sequences (cursor movement `A`/`B`/`C`/`D`, positioning `H`/`f`,
erase-in-line `K`, erase-in-display `J`). Anything else — a byte `>= 0x80`, an
unrecognised escape, or an unsupported CSI final byte — is consumed without
disturbing the screen, so an unfamiliar stream degrades to dropped control
rather than a corrupted display or a panic (`AGENTS.md` §2.9).

`Terminal` ties the grid, the parser, and the seam together: `pump` reads the
shell's output and applies it to the screen, and `send` / `send_str` forward
input. The terminal never echoes input itself — echo, line editing, and job
control are the shell's responsibility, exactly as on a real tty — and a
failing seam call surfaces the boundary `Errno` while leaving the screen
unchanged (`AGENTS.md` §5.4).

### Rendering

`render(terminal, theme, viewport)` paints the grid into a `rustos-raster`
`Surface` sized to the viewport, using the theme's palette for every colour and
the shared `rustos-font` monospace face for every glyph. Each grid cell maps to
one glyph cell and the cursor cell is highlighted with the accent role. The
surface is rectangular; the compositor places and rounds it through its single
anti-aliased rounded-corner path, so there is no rounding — and no colour
algebra — in the app (`AGENTS.md` §2.2). Every length saturates so a viewport
smaller than the grid paints what fits rather than panicking (`AGENTS.md`
§2.9).

### Still to do

The terminal model and renderer are complete and headless-tested; wiring the
pseudo-terminal `ShellSource` to a real shell process and presenting the live
window-manager surface is deferred until the userland process/IPC client and
the taskbar↔WM event glue land.
