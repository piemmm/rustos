## NAME

ls — list directory contents

## SYNOPSIS

`ls [-aACcdFghilmnopQrRsStux1] [-w cols] [--time=WORD]`
`[--time-style=STYLE] [--full-time] [--] [path...]`

## DESCRIPTION

Lists each path operand: a directory operand's entries are read and
listed (unless `-d` names the directory itself), any other operand is
listed as itself. With no operand the current directory (`.`) is
listed.

Entries are sorted by name (or by size, largest first, with `-S`; by
timestamp, newest first, with `-t`; reversed with `-r`). When the
output is a terminal they are laid out
in columns sized to the terminal width, filled top-to-bottom (`-C`);
when it is not (a pipe or a file), they are listed one name per line.
`-x` fills the columns left-to-right instead, `-m` lists them
comma-separated, `-1` forces one per line, and `-w` sets the width
explicitly. Entries whose name begins with `.` are hidden unless `-a`
or `-A` is given; when entries are hidden, a note is emitted on the
standard information stream (fd 3), never in the listing itself.

The long format (`-l`) renders the type and permission bits, the
owner and group, the size, a timestamp, then the name. Owner and
group are numeric ids: resolving account names requires the
capability-gated user database, which a listing must not demand, so
the output matches the GNU tool's numeric fallback (`-n` renders
identically). The timestamp is the modified time by default; `-c`,
`-u`, and `--time` select which of the four timestamps is shown (and
sorted by), and `--time-style` — or `--full-time` — sets its format.
There is still no link-count column because the filesystem contract
does not carry hard links yet; it will appear when it does.

When more than one operand is given — and always under `-R` — each
directory's listing is preceded by a `path:` header, and blocks are
separated by a blank line.

## OPTIONS

- `-t` — sort by the timestamp shown, newest first.
- `-c` — use the metadata-change time (ctime): with `-l` show it, and
  with `-t` sort by it; without `-l`, sort by it.
- `-u` — like `-c`, but the access time (atime).
- `-i, --inode` — print each entry's node number.
- `--time=WORD` — which timestamp to show and sort by: `atime`
  (`access`, `use`), `ctime` (`status`), `mtime` (`modification`), or
  `birth` (`creation`).
- `--time-style=STYLE` — timestamp format: `locale` (the default),
  `long-iso`, `full-iso`, or `iso`. A custom `+FORMAT` is not
  supported.
- `--full-time` — like `-l --time-style=full-iso`.
- `-a, --all` — do not hide entries whose name begins with `.`.
- `-A, --almost-all` — like `-a`, but never list `.` or `..`.
- `-d, --directory` — list directory operands themselves, not their
  contents.
- `-F, --classify` — append `/` to directories and `*` to
  executables.
- `-g` — long format without the owner column; implies `-l`.
- `-h, --human-readable` — with `-l`, print sizes like `1.1K`,
  `23M` (powers of 1024).
- `-l` — long format: permission bits, owner, group, size, then
  name.
- `-m` — comma-separated names, wrapped to the output width.
- `-n, --numeric-uid-gid` — long format with numeric owner and
  group; implies `-l`. Owner and group are always numeric here (see
  above), so this matches `-l`.
- `-o` — long format without the group column; implies `-l`.
- `-p` — append `/` to directories.
- `-Q, --quote-name` — double-quote each name, escaping quotes,
  backslashes, and control characters.
- `-r, --reverse` — reverse the sort order.
- `-R, --recursive` — list subdirectories recursively.
- `-s, --size` — print each entry's allocated size in 1024-byte blocks
  (scaled by `-h`), with a `total` line per directory listing.
- `-C` — list entries in columns, filled top-to-bottom (the default
  when the output is a terminal).
- `-S` — sort by size, largest first.
- `-w, --width <cols>` — set the output width in columns; `0` means an
  unlimited line length. Without it the terminal's width is used, or
  80 when it cannot be determined.
- `-x` — list entries in columns, filled left-to-right.
- `-1` — one name per line (the default when the output is not a
  terminal).
- `-?` — show this command's own short help (`--help` is the long
  form).

## EXAMPLES

- `ls` — list the current directory.
- `ls -al /System` — long-format listing of `/System`, including
  hidden entries.
- `ls -lhS` — long format, human-readable sizes, largest first.
- `ls -R Documents` — recurse through `Documents`, one header per
  directory.
- `ls -F` — mark directories with `/` and executables with `*`.
- `ls -d Documents` — list the `Documents` entry itself, not its
  contents.

## EXIT STATUS

- `0` — every operand was listed.
- `1` — an operand could not be inspected or a directory could not
  be read, or the output could not be delivered.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `cat`
- `man`
