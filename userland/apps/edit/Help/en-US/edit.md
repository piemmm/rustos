## NAME

edit — full-screen text editor

## SYNOPSIS

`edit [file] [-h | -?]`

## DESCRIPTION

A full-screen text editor in the spirit of the classic QuickBasic /
MS-DOS editor: a menu bar across the top, the text below it, and a
status line showing the file name, the cursor position, and the key
hints. It edits one file at a time.

Started with a `file` operand, the editor loads that file; a file that
does not exist yet opens as an empty buffer and is created on the
first save. Started without an operand, it opens an unnamed buffer and
asks for a name when it is first saved.

The menu (opened with `F10` or with `Alt` plus a title's highlighted
letter — `Alt-F` for `File`, `Alt-S` for `Search` — navigated with the
arrow keys, `Enter` selects, `Esc` or `F10` closes) carries:

- `File` — `New`, `Open...`, `Save`, `Save As...`, `Exit`.
- `Search` — `Find...`, `Repeat Last Find`.

When an action would discard unsaved changes (`New`, `Open...`,
`Exit`), the editor asks first: `y` saves and continues, `n` discards,
`c` (or `Esc`) cancels.

Keys inside the session:

- Typing inserts at the cursor; `Insert` toggles overwrite (`OVR` on
  the status line).
- `Enter` splits the line; `Backspace` and `Delete` remove characters
  and join lines at line ends.
- Arrows, `Home`, `End`, `PageUp`, `PageDown` move the cursor; the
  view scrolls, horizontally too, to follow it.
- `Tab` inserts spaces to the next eight-column stop.
- `F1` shows the key summary, `F2` saves, `F3` repeats the last find,
  `F10` (or `Alt-F` / `Alt-S`) opens the menu.

`Find...` searches forward from the cursor, literally and
case-sensitively, wrapping around at the end of the buffer; an
unmatched search reports `Match not found` and leaves the cursor
where it was.

The editor edits text files only, and says exactly what it changes:

- The file must be UTF-8 text no larger than 16 MiB; anything else
  (a binary file, a lone carriage return, an over-large file) is
  refused with the reason stated — never opened as garbage.
- Tab characters are expanded to spaces at eight-column stops on
  load, and CRLF line endings become LF; each conversion is announced
  on the status line, never applied silently.
- The presence or absence of the file's final newline is preserved.

A failed load or save inside the session is reported on the status
line and the buffer is kept; the session never dies over a refused
file. Every path is resolved and permission-checked by the kernel
under the caller's own identity — the editor holds no special
authority.

## OPTIONS

- `-h, -?` — show this command's own short help and exit.

## EXIT STATUS

- `0` — the session ended through `File > Exit`, or the short help
  was shown.
- `1` — the named file could not be loaded (not text, too large, or
  refused), or the terminal failed; the reason is printed on standard
  error.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).
- `TERM` — the terminal the session draws for; an unknown or missing
  value degrades to a safe baseline.

## SEE ALSO

- `cat`
- `man`
