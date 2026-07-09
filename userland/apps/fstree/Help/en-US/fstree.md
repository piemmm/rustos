## NAME

fstree — the full-screen tree file manager

## SYNOPSIS

`fstree [directory]`

## DESCRIPTION

Browses the filesystem in a full-screen, keyboard-driven session: a
directory-tree pane on the left and a file pane on the right listing
the selected directory's entries with their sizes and modification
stamps. The session starts at `directory` (the root view `/` when
omitted).

The tree is read lazily: a directory's contents are fetched only when
it is first shown or expanded, so browsing a huge volume costs only
the directories actually opened. A directory the caller may not list
is refused in place — the error appears on the message line and the
previous view is kept; nothing is fabricated.

Keys:

- `Up`/`Down` or `k`/`j` — move the focused pane's cursor. Moving the
  tree cursor lists the newly selected directory in the file pane.
- `Left`/`Right` or `h`/`l` — collapse/expand the tree row under the
  cursor.
- `Enter` — in the tree, toggle expansion; in the file pane, descend
  into the selected directory (both panes follow).
- `Tab` — switch the focused pane.
- `s` — open the sort menu: `n` name, `e` extension, `s` size,
  `m` modification stamp, `r` reverse the direction, `Esc` cancels.
  Directories always group before files.
- `.` — toggle hidden (dot-named) entries in both panes.
- `?` — show this help over the panes; any key dismisses it.
- `q` — quit, restoring the terminal.

The status line shows the listed path, its visible entry count, the
sort order, the backing volume's free/total bytes (when the System
Information service can report them), and whether hidden entries are
shown. A file whose backing format stores no modification stamp shows
`-` in the stamp column.

The file operations (copy, move, rename, delete), tagging, search,
and the text/hex/disassembly viewers arrive in later stages of the
tool's plan.

## OPTIONS

- `directory` — the directory the session starts in; the default is
  the root view `/`.
- `-h`, `-?` — print this document's short form and exit.

## EXIT STATUS

- `0` — the session ended by the user's `q`.
- `1` — the starting directory could not be listed, or the terminal
  path failed.
- `2` — the arguments could not be understood.

## SEE ALSO

ls, du, df
