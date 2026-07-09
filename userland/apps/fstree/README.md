# `rustos-fstree`

The RustOS `fstree`: the full-screen, keyboard-driven tree file manager,
drawn with the OS curses library (`lib/curses`). It shows a persistent
directory-tree pane beside a file pane over the storage forest. This crate
delivers the plan's S1 model core and the S2 file operations; tagging,
search, and the text/hex/disassembly viewers are staged, stage by stage,
in `.junie/fstree-next-plan.md`.

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
- **Hidden entries**: dotfiles are hidden by default; `.` toggles them.
- **Status/message lines**: the listed path, entry count, sort order,
  volume free space (via the System Information API's `MOUNT_LIST` query,
  best-effort), and errors or key hints.
- **File operations**: copy (`c`), move (`m`), rename (`r`), delete (`d`,
  confirmed), and mkdir (`M`). Destinations are parsed by the shared
  `lib/path` grammar; a transfer onto itself or into its own subtree is
  refused before any I/O; a move renames atomically on one volume and
  falls back to copy-then-remove across volumes; an existing target file
  pauses the operation for a per-file overwrite/skip/cancel answer; a
  failure mid-copy removes the partial target and surfaces the errno.
- **Help**: `-h`/`-?` and the `?` overlay render the bundle's own `Help/`
  document through the shared `lib/help` engine — nothing embedded.

## Keys

`↑↓←→`/`h j k l` navigate; `Enter` expands/collapses (tree) or descends
(files); `Tab` switches panes; `s` sorts; `c` copies; `m` moves; `r`
renames; `d` deletes; `M` makes a directory; `a` edits the mode bits;
`.` toggles hidden entries; `?` shows help; `q` quits.

## Design

An I/O-free model (`src/model.rs`) over two injected seams — the `Fs`
filesystem seam and the curses `Tty` — drawn by a pure renderer and
driven by a blocking event loop (no polling; the kernel parks each read).
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
