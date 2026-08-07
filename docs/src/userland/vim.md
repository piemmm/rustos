# The `vim` editor

`tairix-vim` is the system's modal text editor: an implementation of the
core of the well-known vim editor, drawn with the OS curses library
(`lib/curses`) and shipped as the `vim.app` command bundle in the system
app store. The staged plan for the features beyond this core is
`plans/VIM.md`; the user-facing manual is the bundle's own `Help/` tree
(`man vim`).

## Scope of the core

The editor implements the daily-use vim command set:

- **Modes** — normal, insert, replace, visual (character and line), and
  the `:`/`/`/`?` command line, with `Esc`/`Ctrl-C` returning to normal.
- **Motions and text objects** — `h j k l`, words (`w W b B e E`), line
  positions (`0 ^ $`), line finds (`f F t T` with `;`/`,`), buffer jumps
  (`gg G H M L { } %`), and the `iw aw i( a( i[ i{ i" i' i<` object
  family, all count-aware.
- **Operators** — `d c y` over any motion or object, doubled linewise
  forms, and the shorthands `x X s S D C Y r ~ J`, with named registers
  (`"a`–`"z`, capitals appending) and `p`/`P` puts.
- **History** — grouped undo/redo (`u`, `Ctrl-R`) and dot-repeat (`.`)
  that replays a whole change including its insert-mode text.
- **Search and substitute** — `/ ? n N *` over a bounded vim-subset
  pattern engine, plus `:[range]s/pat/rep/[g]`.
- **The ex core** — `:w :q :wq :x :e :enew :r :n :prev :noh :set nu`,
  line addresses, and ranges.

## Design

The crate is an I/O-free state machine between injected seams, so the
whole editor is host-testable:

- `Editor` (`editor.rs`) owns the buffer, cursor, mode, registers, and
  search state; `normal.rs` is the key grammar; `excmd.rs` the `:`
  language; `render.rs` the one drawing path into a curses `Window`.
- `FileIo` (`fileio.rs`) is the only route to files. The `Run` binary
  backs it with the kernel-authorised `fs_*` syscalls; every per-inode
  and mount check stays kernel-side and a refusal is a status-line
  message (the editor fails closed, never crashes).
- The buffer (`buffer.rs`) records span-based inverse edits into grouped
  undo steps, so undo memory scales with the lines a change touched,
  never the file.
- The pattern engine (`pattern.rs`) compiles to a node list and matches
  under a fixed backtracking budget, failing closed on pathological
  patterns rather than stalling the session.
- The session loop blocks in the kernel for each keystroke; there is no
  polling loop anywhere.
- A terminal resize is handled by that loop, never by the key grammar:
  the session window takes the new geometry and repaints, and the
  renderer re-derives the view scroll so the cursor stays on screen.

The editor drives the terminal through the shared `lib/curses` screen
model. Modal editing required two input events the decoder did not carry
before: the bare Escape key (`Event::Esc`, resolved when a read ends on a
dangling `ESC` — the same boundary ncurses resolves with `ESCDELAY`) and
control-chorded letters (`Event::Ctrl`). Both live in `lib/vt`/
`lib/curses` and are shared by every curses consumer.

## Capabilities

The bundle manifest requests `CAP_CONSOLE_WRITE`, `CAP_CONSOLE_READ`, and
`CAP_FS_ACCESS` — the display, the raw keystrokes, and the file operands.
There is no ambient authority: the secured VFS authorises every path
per-inode under the caller's attested identity, and `-R`/readonly is
enforced editor-side on top of, never instead of, the kernel's checks.

## Testing

`cargo test -p tairix-vim` covers the buffer's undo/redo groups, the
motion and text-object families, operators with counts and registers,
dot-repeat, the search engine and its budget, the ex command set over an
in-memory file seam, command-line editing, readonly enforcement, and the
renderer's cells, highlights, and scrolling against a real curses window.
