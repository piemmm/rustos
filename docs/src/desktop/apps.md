# Default desktop apps

The default graphical applications live under `userland/apps/`. They are
ordinary `.app` bundles (`AGENTS.md` §16.5) that consume the shared desktop
`lib/*` crates — `rustos-geometry`, `rustos-theme`, `rustos-raster`,
`rustos-font` — exactly as the taskbar does, and never depend on the window
manager (`AGENTS.md` §17.4).

## Filesystem browser (`rustos-files` over `lib/browse`)

The filesystem browser navigates the §16 filesystem layout and renders the
current directory through the active theme. It is split into a navigation
**model** and a **renderer**, both driven by an injected directory-read seam,
so the security-relevant logic is testable without a kernel (`AGENTS.md` §7).
The engine — the model, the renderer and its row hit-test, and the validated
path spelling described below — lives in the shared `lib/browse` crate
(`rustos-browse`), because the desktop session's trusted file picker
(`plans/APPWIN.md` AW5) drives exactly the same engine; the `rustos-files`
package is only the `Run` binary that composes it over the live syscalls.

### The directory-read seam

`DirectorySource::list(components)` returns the children of an absolute path
(root-first components; the empty slice is `/`). On a running system the seam
is a capability-checked VFS directory read, so the §5.3 permission decision and
the §16 path policy live in the VFS, not in the app. The browser shows exactly
the entries the source returns — it never fabricates a `/proc`/`/sys`-style
synthetic entry (`AGENTS.md` §16.1). Each entry is an `Entry` carrying a name
and an `EntryKind` (directory or regular file).

### The production source (`vfs`)

`VfsDirectorySource` is the shipping `DirectorySource` (`plans/APPWIN.md`
AW1). It composes three pieces, each host-proven:

- `spell_absolute_path` — the app's one path spelling, shared by the
  browser's displayed path, the tests' tree keys, and the VFS fetch, so the
  three can never disagree (`AGENTS.md` §2.2).
- `absolute_path` — validation before spelling: a component that is empty,
  `.`, `..`, or carries `/`/NUL is refused (`OutOfRange`) *before* any
  syscall, and the spelled path is bounded by the kernel's `FS_PATH_MAX`
  (`LengthOutOfRange`) — validate every input, fail closed (`AGENTS.md`
  §5.4).
- `entries_from_dir_stream` — the packed `fs_readdir` stream mapped onto
  `Entry` values through the shared `rustos_abi::fs::DirEntries` walker (the
  same walker `ls` lists through); one malformed record or non-UTF-8 name
  refuses the whole listing, never a partial one.

The directory fetch itself is injected (`fetch(path) -> stream`): the
shipping program passes `rustos_rt::read_dir_all` — the kernel-authorised
`fs_open` + grow-to-`FS_IO_MAX` `fs_readdir` transfer under the app's own
attested identity — while tests pass an in-memory tree of encoded streams and
drive a `Browser` over it end to end. The engine adds no authority and makes
no permission decision of its own.

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

### The `Run` bundle

`files.app`'s entry point (`plans/APPWIN.md` AW3) wires `VfsDirectorySource`
over `rustos_rt::read_dir_all`, creates and grants the zero-copy window
frame region, parks on its window-event mailbox, and drives the browser
with the keyboard (`Down`/`Up` select, `Enter` opens a directory,
`Backspace` climbs); a `CloseRequested` from the desktop ends it cleanly,
and every bring-up refusal exits fail-loud with its reason on `stderr`. The
desktop session's start menu carries a `Files` entry that spawns the
bundle.

## Terminal emulator (`rustos-terminal`)

The terminal emulator hosts the system shell and shows its output on a
character-cell screen rendered through the active theme. Like the browser it is
split into a screen **model** and a **renderer**, both driven by an injected
shell I/O seam, so the parsing and rendering logic is testable without a kernel
(`AGENTS.md` §7).

### The shell seam

`ShellSource::read()` returns the bytes the shell has produced since the last
call (an empty read is not an error) and `ShellSource::write(bytes)` forwards
the user's keystrokes. On a running system the seam is
`spawned::PipeShellSource` (`plans/APPWIN.md` AW4): two kernel pipes to a
shell child the terminal spawned under its own `CAP_PROC_SPAWN`, wired at
spawn through `spawned::shell_wires` — the child's stdin is the keystroke
pipe and its stdout *and* stderr land on the one output pipe a terminal
renders (fd 3 is closed; advisory records are best-effort by contract).
Reads drain one bounded chunk per wait-set wake and surface end-of-stream as
the typed "shell has exited" refusal; writes loop over short writes and fail
closed on a wedged channel. The process-spawn authority lives in the `Run`
binary, behind the seam, not in the screen model.

### The screen model

`Grid` is a fixed `cols`×`rows` rectangle of `lib/vt` `Cell`s — a glyph plus
its folded `Attributes` — with a cursor and a rendition pen. It exposes the
cursor-relative operations a terminal needs: writing a glyph with the pen
(wrapping and scrolling at the edges), the C0 moves, absolute/relative cursor
positioning, the ANSI erase operations, the scroll region and explicit
scrolling, the alternate screen, cursor visibility, the saved cursor, the
window title, and clear.

`Parser` is a thin **consumer** of the shared `lib/vt` ANSI/VT/xterm
vocabulary (`plans/CURSES.md` C2): it lets `lib/vt`'s streaming parser turn
shell output bytes into the shared `Op` vocabulary and applies each `Op` to
the grid, so there is exactly one escape-sequence definition in the tree, not
a second divergent one (`AGENTS.md` §2.2). The emulator is xterm-class —
printable text and Unicode, the C0 controls, SGR rendition with the
16/256/truecolour colour models, cursor addressing, the erase operations, the
scroll region (`DECSTBM`), the alternate screen (`?1049`), cursor visibility
(`?25`), the saved cursor (`ESC 7`/`ESC 8`), and the OSC window title — and it
honestly advertises `xterm-256color` because every capability that name
implies is really parsed (the compiled-in capability database is the next
`plans/CURSES.md` stage, `lib/termcap`). Because `lib/vt`'s parser is total, an
unrecognised, oversized, or malformed sequence is consumed without disturbing
the screen, so an unfamiliar stream degrades to dropped control rather than a
corrupted display or a panic (`AGENTS.md` §2.9).

`Terminal` ties the grid, the parser, and the seam together: `pump` reads the
shell's output and applies it to the screen, and `send` / `send_str` forward
input. The terminal never echoes input itself — echo, line editing, and job
control are the shell's responsibility, exactly as on a real tty — and a
failing seam call surfaces the boundary `Errno` while leaving the screen
unchanged (`AGENTS.md` §5.4).

### Rendering

`render(terminal, theme, viewport)` paints the grid into a `rustos-raster`
`Surface` sized to the viewport, using the theme's palette and the shared
`rustos-font` monospace family (Inconsolata EX plus the M PLUS 1 Code Japanese
and Noto Sans Hebrew companions). Hebrew and Yiddish letters, final forms,
punctuation, and marks occupy individual terminal cells; a Japanese full-width
bitmap paints its lead and continuation cells as one unit, so a
continuation-cell background cannot erase half the glyph.
Each cell is drawn with its own rendition: its
`lib/vt` `Attributes` select the foreground and background, resolved one way
(`AGENTS.md` §2.2) — a `Default` colour takes the theme's `on_surface` /
`surface` roles, the 16 basic colours and the 256-colour palette map through
the standard ANSI tables, truecolour is used directly, `reverse` swaps the
pair, and `bold` brightens a basic colour. The visible cursor cell is
highlighted with the accent role. The surface is rectangular; the compositor
places and rounds it through its single anti-aliased rounded-corner path, so
there is no rounding in the app. Every length saturates so a viewport smaller
than the grid paints what fits rather than panicking (`AGENTS.md` §2.9).

### The `Run` bundle

`terminal.app`'s entry point creates the two pipes, spawns the user's default
shell (`rustos_users::policy::DEFAULT_SHELL`) with `TERM` exported and the
child-side pipe ends closed after the spawn (so each side observes the
other's end-of-file honestly), creates and grants the zero-copy window frame
region, and **parks** on one wait-set with three members — its window-event
mailbox (`Port`), the shell-output pipe's read end (`Stream`, the AW4 kernel
addition: ready on buffered bytes or end-of-stream), and the shell child
(`Child`) — dispatching on the woken member's token, never a poll loop. Key
presses are encoded through the one shared `lib/keymap` rule and written to
the shell (releases send nothing); shell output is pumped into the grid and
the repainted frame presented. The shell exiting, or a `CloseRequested` from
the desktop, ends the session cleanly; every bring-up refusal exits
fail-loud with a reserved code and its reason on `stderr`. The desktop
session's start menu carries a `Terminal` entry that spawns the bundle, and
the autoload QEMU vertical types a real command into the served window at
the seat keyboard, PASSing only on the kernel-attested keyboard → session →
terminal → pipe → shell → spawn round trip.

## File viewer (`rustos-viewer`)

The read-only text viewer is the first consumer of the desktop's trusted
file picker and the CU6 one-shot file delegation (`plans/APPWIN.md` AW5,
`plans/CAPABILITY_USE.md`). Its manifest requests `CAP_CONSOLE_WRITE` and
`CAP_SHM` and deliberately **no filesystem capability**: on its own the
viewer can open, list, and stat nothing.

At startup the `Run` binary creates its window and immediately asks the
session's picker (`WindowClient::pick_file`). The user browses in the
*session's* UI under the *session's* authority; the viewer receives
exactly one conclusion on its authenticated event channel — a
`FilePicked` carrying the kernel's one-shot `fd_grant` handle, or a
`PickCancelled`. Redeeming the handle (`fd_redeem`, unprivileged)
installs a read-only descriptor whose reads the kernel re-authorises
under the session's captured identity, so the viewer reads exactly the
one file the user chose and nothing else — the user-mediated file
capability of `AGENTS.md` §16.5, end to end.

The host-tested view engine keeps untrusted content honest:
`content_lines` bounds the shown bytes (`CONTENT_MAX`), splits on line
feeds, and sanitises **every** non-printable byte to a placeholder before
anything reaches the renderer, so a hostile picked file can neither pin
unbounded memory nor smuggle control sequences; `render_status` /
`render_lines` paint through the active theme and the shared monospace
face. `Enter` asks for another pick; a cancelled pick leaves the viewer
open with a notice; a `CloseRequested` ends it cleanly. The start menu
carries a `Viewer` entry that spawns the bundle.
