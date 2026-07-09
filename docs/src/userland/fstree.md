# The `fstree` file manager

`fstree` (`userland/apps/fstree`, crate `rustos-fstree`) is the full-screen,
keyboard-driven **tree file manager** for the terminal: a persistent
directory-tree pane beside a file pane over the storage forest, drawn with
the OS curses library (`lib/curses`). It is a command app in the system app
store — a sealed `.app` bundle with `AppInfo`, `Run`, and a `Help/` locale
tree, discovered from disk like every other bundle. The staged plan for the
whole tool lives in `.junie/fstree-next-plan.md`; this page describes what
is built.

## What is built (stages S1 — the model core — and S2 — file operations)

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
- **The mode editor.** `a` edits the focused selection's permission bits
  (the file pane's entry, or the tree pane's directory): a modal octal
  prompt on the message line, pre-filled with the entry's current bits
  (a resolve-only stat through the seam). Octal digits and Backspace
  edit — a non-octal key and a fifth digit are refused at the prompt,
  matching the kernel's `FS_MODE_MASK` ceiling — Enter applies through
  `fs_set_mode` (the kernel's owner-only rule decides; a refusal is
  surfaced verbatim and nothing changes), and Esc cancels.
- **File operations.** `c` copies and `m` moves the focused selection to
  a typed destination, `r` renames it in place (prompt pre-filled with the
  current name), `d` deletes it after an explicit confirmation (a
  directory with everything under it — the question says so), and `M`
  creates a directory in the listed one. The destination spelling is
  parsed by the one shared path grammar (`lib/path`): a relative spelling
  is joined onto the listed directory, and an existing directory receives
  the transfer inside it under the source's name. A transfer onto itself
  or into the source's own subtree is refused before any I/O. A move is
  an atomic `fs_rename` on one volume and falls back to copy-then-remove
  across volumes (the kernel's honest `CrossVolume` report drives the
  fallback — the target is probed *before* the rename, so an existing
  file is asked about, never silently replaced).
- **The overwrite question.** When a transfer would overwrite an existing
  file the operation pauses per file: `o` overwrites, `s` skips (the
  skipped source stays in place, and the emptied-source-directory removal
  above it is withheld so a skip never fails a move), `c` cancels the
  remaining steps — applied work stays, and the completion report counts
  entries and skips. Copy streams through a fixed bounded buffer (never a
  whole-file slurp); a mid-stream failure removes the partial target and
  surfaces the kernel's errno, so a half-written file never masquerades
  as a copy.
- **Help.** `-h`/`-?` print the bundle's own Help document through the
  shared `lib/help` engine; the in-session `?` overlay shows the same
  document decoded to plain text through the one `lib/vt` parser. Nothing
  is embedded in the binary.

## Design

The charter's seam pattern (as `vim` and `top`): an I/O-free state machine
(`src/model.rs`) the pure renderer (`src/render.rs`) draws and the key
grammar (`src/app.rs`) mutates, over two injected seams —

- `Fs` (`src/fs.rs`) — `list_dir`, `volume_space`, the mode editor's
  `stat_mode`/`set_mode`, and the S2 mutating operations (`stat_kind`,
  streamed `read`/`write`, `create`, `mkdir`, `remove_file`,
  `remove_dir`, `rename` with its `RenameOutcome::CrossDevice` report);
  the `Run` binary implements it over the kernel-authorised `fs_*`
  syscalls and the sysinfo mount walk (with one-slot source/destination
  handle caches hoisting the per-chunk open off the copy path), the
  tests over an in-memory tree. The trait carries exactly the operations
  the landed stages perform; each later stage extends it in place with
  the operations it introduces, together with their callers.
- The operations themselves live in `src/ops.rs`: `resolve_destination`
  and `plan_target` validate before any I/O, and the resumable `FileOp`
  executor drives a work stack that reads directories incrementally as
  the walk reaches them (memory follows the frontier, never the subtree)
  and pauses on each overwrite conflict, handing the decision to the key
  loop — a paused operation holds no filesystem handle, only the paths
  still to process.
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
help overlay, the sort prompt, the mode prompt), the mode-editor flows
(pre-filled prompt, digit/Backspace editing with non-octal and
fifth-digit refusal, apply, Esc cancel, refused stat, kernel-refused
change, emptied prompt), and a scripted end-to-end session
(browse → expand → pane switch → sort → hidden toggle → help → quit).
The S2 operations are covered end to end through the same in-memory
seam: mkdir (creation, invalid-name refusal, Esc cancel), rename (in
place, onto an existing file via the overwrite question, session-root
refusal), delete (file, recursive directory with the file pane climbing
to the surviving ancestor, declined confirmation), copy (absolute and
relative destinations, tree reproduction, self/subtree/bad-spelling
refusals), the overwrite matrix (overwrite, skip, cancel, per-file
questions in a merging directory copy), move (same-volume rename,
cross-volume copy-then-remove, skip preserving the skipped source), and
failure injection (a read or write failure mid-copy removes the partial
target and surfaces the errno with the panes consistent).
