# `rustos-fstree`

The RustOS `fstree`: the full-screen, keyboard-driven tree file manager,
drawn with the OS curses library (`lib/curses`). It shows a persistent
directory-tree pane beside a file pane over the storage forest. This crate
delivers the plan's S1 model core; the file operations, tagging, search,
and the text/hex/disassembly viewers are staged, stage by stage, in
`.junie/fstree-next-plan.md`.

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
- **Help**: `-h`/`-?` and the `?` overlay render the bundle's own `Help/`
  document through the shared `lib/help` engine — nothing embedded.

## Keys

`↑↓←→`/`h j k l` navigate; `Enter` expands/collapses (tree) or descends
(files); `Tab` switches panes; `s` sorts; `.` toggles hidden entries;
`?` shows help; `q` quits.

## Design

An I/O-free model (`src/model.rs`) over two injected seams — the `Fs`
directory/space seam and the curses `Tty` — drawn by a pure renderer and
driven by a blocking event loop (no polling; the kernel parks each read).
A refused listing fails closed onto the message line and never disturbs
the previous state. The `Run` binary (`src/run.rs`) wires the seams to the
kernel-authorised `fs_*` syscalls and the inherited standard streams; it
holds no ambient authority and names no console device.

## Capabilities

`CAP_CONSOLE_WRITE`, `CAP_CONSOLE_READ`, `CAP_FS_ACCESS` — the kernel
still authorises every path per-inode under the caller's attested
identity; a denial is a message-line answer, not a crash.
