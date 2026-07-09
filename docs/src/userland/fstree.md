# The `fstree` file manager

`fstree` (`userland/apps/fstree`, crate `rustos-fstree`) is the full-screen,
keyboard-driven **tree file manager** for the terminal: a persistent
directory-tree pane beside a file pane over the storage forest, drawn with
the OS curses library (`lib/curses`). It is a command app in the system app
store — a sealed `.app` bundle with `AppInfo`, `Run`, and a `Help/` locale
tree, discovered from disk like every other bundle. The staged plan for the
whole tool lives in `.junie/fstree-next-plan.md`; this page describes what
is built.

## What is built (stage S1 — the model core)

- **The tree pane.** A lazily populated directory tree: a directory is read
  through one `fs_readdir` call when it is first shown or expanded — never
  a whole-volume scan, so browsing costs the working set and a huge volume
  costs only the directories actually opened. Rows carry expansion markers
  (`+`/`-`), indentation by depth, and a cursor with scrolling.
- **The file pane.** The selected directory's entries with name, size
  (`<dir>` for directories), and modification-stamp columns. The stamp is
  the `Time64` value the kernel's listing stream carries per entry; a
  backing format that stores no per-node stamp reports the epoch, which
  renders as `-` — an absent figure, never a fabricated 1970 date.
- **Sorting.** Name, extension, size, or modification stamp, ascending or
  descending, directories always grouped first. The `s` menu selects the
  key (`n`/`e`/`s`/`m`) or reverses (`r`); `Esc` cancels.
- **Hidden entries.** Dot-named entries are hidden by default in both
  panes; `.` toggles them, with cursors clamped to the shrunken lists.
- **Status and message lines.** The listed path, visible entry count, sort
  order, the backing volume's free/total bytes (the System Information
  API's `MOUNT_LIST` query through the shared `rustos_procinfo` mount walk,
  best-effort — an unreachable service simply omits the figure), and either
  an error, the sort prompt, or the key hints.
- **Help.** `-h`/`-?` print the bundle's own Help document through the
  shared `lib/help` engine; the in-session `?` overlay shows the same
  document decoded to plain text through the one `lib/vt` parser. Nothing
  is embedded in the binary.

## Design

The charter's seam pattern (as `vim` and `top`): an I/O-free state machine
(`src/model.rs`) the pure renderer (`src/render.rs`) draws and the key
grammar (`src/app.rs`) mutates, over two injected seams —

- `Fs` (`src/fs.rs`) — `list_dir` and `volume_space`; the `Run` binary
  implements it over the kernel-authorised `fs_*` syscalls and the sysinfo
  mount walk, the tests over an in-memory tree. The trait carries exactly
  the operations this stage performs; each later stage extends it in place
  with the operations it introduces, together with their callers.
- `Tty` (from `rustos-curses`) — the terminal byte channel over the
  inherited fd 0/1; the program names only its standard descriptors, never
  a console device.

Every wait blocks (`Screen::getch` parks the task in the kernel); there is
no polling loop. A refused listing fails closed: the error is surfaced on
the message line, and the cursor, listing, and expansion state are left
untouched — a denied directory is never shown as empty. The session runs
inside the terminal's alternate screen and restores it (and cooked input)
on every exit path, stating any abnormal reason on standard error after
the restore.

## Capabilities

`CAP_CONSOLE_WRITE`, `CAP_CONSOLE_READ`, and `CAP_FS_ACCESS` — the ambient
file authority of the launching user, nothing more. Every path the session
touches is authorised per-inode by the kernel under the caller's attested
identity; the tool holds no private channel and adds no authority of its
own.

## Testing

Host tests drive the whole session without a kernel or terminal: model
unit tests (lazy expansion counted through the seam, every sort key and
direction, hidden-toggle cursor clamping, fail-closed denial), golden-grid
renders (pane layout, the absent-stamp `-`, real stamp formatting, the
help overlay, the sort prompt), and a scripted end-to-end session
(browse → expand → pane switch → sort → hidden toggle → help → quit).
