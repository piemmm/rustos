# `tairix-edit`

A full-screen text editor for TAIRiX in the spirit of the classic
QuickBasic / MS-DOS editor: a menu bar (`File`, `Search`) across the top,
the text area below it (white on blue on a colour terminal), and a status
line carrying the file name, cursor position, and key hints. It edits one
buffer, loads and saves whole files, and searches forward with
wrap-around, drawing through `lib/curses`'s screen model rather than
emitting escape sequences by hand (`AGENTS.md` §2.2).

## Keys

| Key             | Action                                        |
| --------------- | --------------------------------------------- |
| typing          | insert (or overwrite) at the cursor           |
| `Insert`        | toggle insert ↔ overwrite (`OVR` in status)   |
| `Enter`         | split the line                                |
| `Backspace` / `Delete` | delete; join lines at line ends        |
| arrows, `Home`, `End`, `PgUp`, `PgDn` | move the cursor         |
| `Tab`           | insert spaces to the next eight-column stop   |
| `F1`            | key-summary overlay                           |
| `F2`            | save                                          |
| `F3`            | repeat the last find                          |
| `F10`           | open / close the menu (also cancels prompts)  |

The menus: `File` (`New`, `Open...`, `Save`, `Save As...`, `Exit`) and
`Search` (`Find...`, `Repeat Last Find`). An action that would discard
unsaved changes asks first (`y` save, `n` discard, `c`/`F10` cancel).

A terminal resize is not a keystroke: the text area and status line are
re-laid-out at the new size and the view re-clamped over the cursor, while
an open menu, prompt, or key-summary overlay stays exactly as it was.

The available key set is the one the shared terminal vocabulary can
deliver (`lib/vt` drops bare `Esc` and C0 control combinations), so the
bindings are function-key-driven — `F10` where the DOS editor used `Alt`,
`F10`/`c` where it used `Esc`.

## Honest file handling

- Input must be UTF-8 text within a 16 MiB bound; a binary file, a lone
  `\r`, or an over-large file is refused with the reason stated
  (`AGENTS.md` §2.9 fail closed), never opened as garbage. A failed
  *initial* load aborts before the screen is taken over; a failed load or
  save inside the session posts a status-line notice and keeps the buffer.
- Tabs are expanded to spaces at eight-column stops and CRLF becomes LF —
  each conversion announced on the status line, never silent. The file's
  final-newline presence is preserved so an untouched buffer round-trips
  byte for byte.
- Saving writes through the kernel-authorised `fs_*` syscalls
  (`WRITE|CREATE|TRUNCATE`); a short write fails closed rather than
  reporting a truncated file as saved. The editor adds no authority of
  its own — every path and per-inode check is kernel-side under the
  caller's attested identity (`AGENTS.md` §5.4).

## How it is built

- **`buffer`** — the `TextBuffer`: fail-closed decoding, line storage,
  and the editing primitives. Pure and exhaustively unit-tested.
- **`model`** — the I/O-free state machine (`Mode`: edit, menu, prompt,
  confirm) plus the file and search operations over the injected `Fs`
  seam, so the whole editor runs against an in-memory filesystem in tests.
- **`app`** — `render` draws the model into curses windows (the drop-down
  menu and the F1 overlay are windows composited on top through the same
  renderer) and `run` is the blocking event loop; the kernel parks the
  input read, never a poll (`AGENTS.md` §2.23).
- **`command`** — the `edit [file] [-h | -?]` argument grammar.
- **`run.rs`** — the freestanding `Run` binary: `tairix-rt` runtime, the
  shared `tairix_curses::StreamTty` byte channel over fd 0/1, the `RtFs`
  whole-file seam, raw input for the session (cooked restored on exit),
  and the `-h`/`-?` short help through the shared `lib/help` engine.

The bundle's `Help/` tree (six locales, `en-US` canonical) is the single
source of the command's documentation (plans/APPS.md); nothing is
compiled into the binary.

## Capabilities

`CAP_CONSOLE_WRITE` (the full-screen display on fd 1), `CAP_CONSOLE_READ`
(raw-mode keystrokes on fd 0), and `CAP_FS_ACCESS` (the load/save and
own-Help reads through the secured VFS, still authorised per-inode by the
kernel). See `AppInfo.toml`.

## Stability

Experimental, like every `userland/apps` crate before the first release.
