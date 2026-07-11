# `rustos-fstree`

The RustOS `fstree`: the full-screen, keyboard-driven tree file manager,
drawn with the OS curses library (`lib/curses`). It shows a persistent
directory-tree pane beside a file pane over the storage forest. This crate
delivers the plan's S1 model core, the S2 file operations, and the S3
tagging surface (multi-file tags, batch operations, the flattened branch
view, and disk-usage statistics); search and the text/hex/disassembly
viewers are staged, stage by stage, in `.junie/fstree-next-plan.md`.

Stability tier: **experimental**.

## What it implements

- **Tree pane**: a lazily populated directory tree — a directory is read
  only when first shown or expanded, never a whole-volume scan — with
  expansion markers, indentation, and cursor + scroll state.
- **File pane**: the selected directory's entries with name, size, and
  modification-stamp columns (the stamp rides the `fs_readdir` listing;
  a backing with no stored stamp shows `-`, never a fabricated date).
- **Sorting**: name, extension, size, or modification stamp, ascending or
  descending, with directories always grouped first (`s` opens the menu).
- **Hidden entries**: dotfiles are hidden by default; `H` toggles them.
  When the file pane omits hidden entries, one advisory record per
  change goes out on the Standard Information Stream (fd 3),
  best-effort and ignorable.
- **Status/message lines**: the listed path, entry count, sort order,
  volume free space (via the System Information API's `MOUNT_LIST` query,
  best-effort), and errors or key hints.
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
  tagged for a retry. The status line carries the tagged count and bytes.
- **Walks**: `u` counts files/bytes/dirs under the focused directory and
  `v` flattens its branch into one file list — both driven by one bounded,
  cancellable walker (`src/walk.rs`) that reads a few directories per
  timed tick (the kernel parks each wait; never a busy poll), records
  unreadable directories instead of stopping, and pages the flattened
  list so a huge branch fills only as far as asked (`Space` loads more).
- **Volumes**: `V` lists the mounted volumes (target, filesystem type,
  free/total when reported) over the System Information API's mount
  walk; Enter re-roots the session at the chosen root.
- **Settings**: `S` toggles the delete confirmations, persisted in the
  user's own `Settings/fstree/` through the `Fs` seam (fail-safe parse:
  a corrupt file leaves every confirmation on).
- **Help**: `-h`/`-?` and the `?` overlay render the bundle's own `Help/`
  document through the shared `lib/help` engine — nothing embedded.

## Keys

`↑↓←→`/`h j k l` navigate; `Enter` expands/collapses (tree) or descends
(files); `Tab` switches panes; `s` sorts; `c` copies; `m` moves; `r`
renames; `d` deletes; `M` makes a directory; `a` edits the mode bits;
`t` tags; `T` tags by glob or range; `i` inverts tags; `C` clears tags;
`u` counts disk usage; `v` flattens the branch (`Space` loads the next
page, `Esc` returns); `H` toggles hidden entries; `.` repeats the last
operation; `V` lists volumes; `S` opens settings; `?` shows help; `q`
quits.

## Design

An I/O-free model (`src/model.rs`) over two injected seams — the `Fs`
filesystem seam and the curses `Tty` — drawn by a pure renderer and
driven by an event loop whose every wait parks in the kernel: blocking
normally, and bounded by a short timeout only while a walk is live so an
elapsed wait advances the walk one bounded tick (never a busy poll).
The operations planner/executor (`src/ops.rs`) validates before any I/O
and drives a resumable work stack that reads directories incrementally
and pauses on each overwrite conflict, so the per-file question runs
through the same key loop. A refused step fails closed onto the message
line and never disturbs consistent state. The `Run` binary (`src/run.rs`)
wires the seams to the kernel-authorised `fs_*` syscalls and the
inherited standard streams; it holds no ambient authority and names no
console device.

## Capabilities

`CAP_CONSOLE_WRITE`, `CAP_CONSOLE_READ`, `CAP_FS_ACCESS` — the kernel
still authorises every path per-inode under the caller's attested
identity; a denial is a message-line answer, not a crash.
