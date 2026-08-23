# `tairix-fstree`

The TAIRiX `fstree`: the full-screen, keyboard-driven tree file manager,
drawn with the OS curses library (`lib/curses`). Modelled on XTree Gold,
it shows a disk-statistics header over a persistent directory-tree
window stacked above a file window, over the storage forest. It carries
the model core, the file operations, the tagging surface (multi-file
tags, batch operations, the flattened branch view, and disk-usage
statistics), name/content search, and the text/hex/disassembly viewers.

Stability tier: **experimental**.

## What it implements

- **Tree window**: a lazily populated directory tree — a directory is
  read only when first shown or expanded, never a whole-volume scan —
  drawn with `├─`/`└─` box-drawing branch lines, a `+`/`-` fold marker,
  and cursor + scroll state.
- **File window**: the highlighted directory's entries with name, size,
  and modification-stamp columns (the stamp rides the `fs_readdir`
  listing; a backing with no stored stamp shows `-`, never a fabricated
  date).
- **Sorting**: name, extension, size, or modification stamp, ascending or
  descending, with directories always grouped first (`s` opens the menu).
- **Hidden entries**: dotfiles are hidden by default; `H` toggles them.
  When the file pane omits hidden entries, one advisory record per
  change goes out on the Standard Information Stream (fd 3),
  best-effort and ignorable.
- **Header/status/command lines**: a top disk-statistics header (path,
  volume free space via the System Information API's `MOUNT_LIST` query,
  item and tagged counts), a status band (active window, sort order,
  hidden/filter state), and a context command menu with errors and key
  hints.
- **File operations**: copy (`c`), move (`m`), rename (`r`), delete (`d`,
  confirmed — the confirmation is a persisted setting), and mkdir (`M`).
  Destinations are parsed by the shared `lib/path` grammar and Tab
  completes them through the shared `lib/complete` engine; a transfer
  onto itself or into its own subtree is refused before any I/O; a move
  renames atomically on one volume and falls back to copy-then-remove
  across volumes; an existing target file pauses the operation for a
  per-file overwrite/skip/cancel answer; a failure mid-copy removes the
  partial target and surfaces the errno. `.` repeats the last operation
  on the current selection.
- **Tagging and batches**: `t` toggles a tag (marked `*`), `T` tags by a
  `lib/glob` pattern or a `size:`/`date:` range, `i` inverts, `C` clears;
  while anything is tagged,
  `c`/`m`/`d` run over the whole tagged set in tag order, continuing past
  per-entry failures and listing every failure on a report overlay — a
  batch is never silently partial. Succeeded entries untag; failures stay
  tagged for a retry. The status band carries the tagged count and bytes.
- **Walks**: `u` counts files/bytes/dirs under the focused directory and
  `v` flattens its branch into one file list — both driven by one bounded,
  cancellable walker (`src/walk.rs`) that reads a few directories per
  timed tick (the kernel parks each wait; never a busy poll), records
  unreadable directories instead of stopping, and pages the flattened
  list so a huge branch fills only as far as asked (`Space` loads more).
- **Volumes**: `V` lists the mounted volumes (target, filesystem type,
  free/total when reported) over the System Information API's mount
  walk; Enter re-roots the session at the chosen root.
- **Settings**: `S` toggles the delete confirmations, persisted in this
  application's own app-data store through `lib/appdata` — gated on the
  kernel-attested bundle identity, so no other application the user
  launches can read or rewrite them, and no store path is spelled here.
  Reading fails safe: an unreachable store, an absent key, or a value
  that is not a boolean leaves every confirmation **on**, and a refused
  value is named rather than swallowed.
- **Help**: `-h`/`-?` and the `?` overlay render the bundle's own `Help/`
  document through the shared `lib/help` engine — nothing embedded.

## Layout and navigation

The screen follows XTree Gold: a disk-statistics header across the top
(path, free/total space, item and tagged counts), a boxed
directory-tree window (with `├─`/`└─` branch lines) above a boxed file
window listing the highlighted directory's entries, and a context
command menu along the bottom.

The tree window is primary. `↑↓`/`k j` move the highlight (and the file
window follows the highlighted directory); `→`/`l`/`+` fold a branch
open, `←`/`h`/`-` fold it shut (or step to the parent when already
shut); `Enter` or `Tab` step into the file window; `Esc`, `Tab`, or `←`
step back out. In the file window `↑↓` move, `Enter` opens the entry (a
directory descends, a file views), and `→`/`l` descends a directory.
`PageUp`/`PageDown` move by a screenful and `Home`/`End` jump to the
ends.

## Keys

`s` sorts; `c` copies; `m` moves; `r` renames; `d` deletes; `M` makes a
directory; `a` edits the mode bits and attributes; `o` opens a file in a
chosen viewer; `t` tags; `T` tags by glob or range; `i` inverts tags;
`C` clears tags; `f` filters; `/` finds by name; `F` searches contents;
`u` counts disk usage; `v` flattens the branch (`Space` loads the next
page, `Esc` returns); `H` toggles hidden entries; `.` repeats the last
operation; `V` lists volumes; `S` opens settings; `?` shows help; `q`
quits.

## Design

An I/O-free model (`src/model.rs`) over two injected seams — the `Fs`
filesystem seam and the curses `Tty` — drawn by a pure renderer and
driven by an event loop whose every wait parks in the kernel: blocking
normally, and bounded by a short timeout only while a walk is live so an
elapsed wait advances the walk one bounded tick (never a busy poll). A
terminal resize is handled by the loop itself, never as a key: the window
takes the new geometry and the pane split, page height, and scroll are
re-derived from it.
The operations planner/executor (`src/ops.rs`) validates before any I/O
and drives a resumable work stack that reads directories incrementally
and pauses on each overwrite conflict, so the per-file question runs
through the same key loop. A refused step fails closed onto the message
line and never disturbs consistent state. The `Run` binary (`src/run.rs`)
wires the seams to the kernel-authorised `fs_*` syscalls and the
inherited standard streams; it holds no ambient authority and names no
console device.

## Symbolic links

A link is shown as the link it is — the size column reads `<link>`, whose own
byte count would only be the length of the path it stores — and every verb
acts on the **name**, never on what the name points at:

- **Copy** recreates the link with the same stored target, verbatim. Streaming
  its bytes would leave a regular file holding the target's text, and
  following it would copy something the link only points at. A dangling link
  duplicates fine: a link is data.
- **Move** and **delete** act on the link. `fs_unlink` and `fs_rename` keep
  the name as typed, so what the link named survives untouched.
- A **link already at a destination** is a real loss, so the overwrite
  question says so (`… exists as a symbolic link`) and an approved
  replacement *removes* the link before creating the new object. That is what
  keeps a create or truncate — both of which follow a final link — from acting
  on whatever the link pointed at, anywhere on the volume. A directory is
  never transferred onto a link: a link is a leaf however it resolves.

The destination probe is therefore a `NO_FOLLOW` stat: the name as typed. A
following probe would let anyone who can plant a name inside a destination
tree redirect every later write out of it.

## Capabilities

`CAP_CONSOLE_WRITE`, `CAP_CONSOLE_READ`, `CAP_FS_ACCESS` — the kernel
still authorises every path per-inode under the caller's attested
identity; a denial is a message-line answer, not a crash.
