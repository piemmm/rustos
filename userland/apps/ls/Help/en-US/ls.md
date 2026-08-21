## NAME

ls — list directory contents

## SYNOPSIS

`ls [-aABbCcdFfGghikIlmNnopQqrRsSTtUuvXx1] [-w cols] [-I PATTERN]`
`[--block-size=SIZE] [--si] [--format=WORD] [--indicator-style=WORD]`
`[--hide=PATTERN] [--time=WORD] [--time-style=STYLE] [--sort=WORD]`
`[--quoting-style=STYLE] [--full-time] [--author] [--file-type]`
`[--group-directories-first] [--zero] [--color[=WHEN]] [--] [path...]`

## DESCRIPTION

Lists each path operand: a directory operand's entries are read and
listed (unless `-d` names the directory itself), any other operand is
listed as itself. With no operand the current directory (`.`) is
listed.

Entries are sorted by name (or by size, largest first, with `-S`; by
timestamp, newest first, with `-t`; by extension with `-X`; by natural
"version" order with `-v`; not at all — directory order — with `-U`;
chosen by name with `--sort`; reversed with `-r`).
`--group-directories-first` floats directories to the top of the sort.
When the output is a terminal they are laid out in columns sized to the
terminal width, filled top-to-bottom (`-C`); when it is not (a pipe or
a file), they are listed one name per line.
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
The link-count column reports how many directory entries name the file,
as the filesystem itself records it; a format that keeps no count
answers `1`.

A symbolic link renders with the type letter `l` and, in the long format,
as `name -> target` — the target exactly as it is stored, unresolved, which
is what the link holds. A dangling link therefore lists normally; only a
posture that resolves it (`-L`, or `-H` for an operand) reports the target
as unreachable.

Names are quoted so that awkward characters are visible and safe to
paste back into a shell: at a terminal the default is `shell-escape`
(a name with spaces or metacharacters is quoted, control characters
shown as `$'…'` escapes), and elsewhere it is `literal` (names
verbatim). `-N`, `-Q`, `-b`, and `--quoting-style` choose the style,
and `-q` shows control characters as `?`.

At a colour terminal names are coloured by kind — directories,
executables, and plain files each in the standard scheme's role —
controlled by `--color`. Colour is presentation only: it is added to the
terminal render and never to piped or redirected output, which is
byte-for-byte identical to the plain listing apart from the escape
sequences, so it never changes columns, ordering, or a script's view.

When more than one operand is given — and always under `-R` — each
directory's listing is preceded by a `path:` header, and blocks are
separated by a blank line.

## OPTIONS

- `-t` — sort by the timestamp shown, newest first.
- `-c` — use the metadata-change time (ctime): with `-l` show it, and
  with `-t` sort by it; without `-l`, sort by it.
- `-u` — like `-c`, but the access time (atime).
- `-i, --inode` — print each entry's node number.
- `-B, --ignore-backups` — do not list entries whose name ends with
  `~`, in every mode (backups are hidden even under `-a`).
- `-I, --ignore=PATTERN` — do not list entries matching the shell glob
  `PATTERN` (repeatable); applies in every mode. `*` and `?` also match
  a leading `.`.
- `--hide=PATTERN` — like `--ignore`, but has no effect when `-a` or
  `-A` is given.
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
- `-h, --human-readable` — print sizes like `1.1K`, `23M` (powers of
  1024), for both the long-format file sizes and the `-s` blocks.
- `-l` — long format: permission bits, owner, group, size, then
  name.
- `-m` — comma-separated names, wrapped to the output width.
- `-n, --numeric-uid-gid` — long format with numeric owner and
  group; implies `-l`. Owner and group are always numeric here (see
  above), so this matches `-l`.
- `-o` — long format without the group column; implies `-l`.
- `-p` — append `/` to directories.
- `-N, --literal` — print names verbatim, without quoting
  (`--quoting-style=literal`).
- `-Q, --quote-name` — C-style quoting: double-quote each name,
  escaping quotes, backslashes, and control characters
  (`--quoting-style=c`).
- `-b, --escape` — like `-Q` but without the surrounding quotes and
  with spaces escaped (`--quoting-style=escape`).
- `--quoting-style=WORD` — how names are quoted: `literal` (`-N`),
  `shell`, `shell-always`, `shell-escape`, `shell-escape-always`, `c`
  (`-Q`), or `escape` (`-b`). The default is `shell-escape` at a
  terminal and `literal` otherwise; the `locale` and `clocale` styles
  are not supported.
- `-q, --hide-control-chars` — show nongraphic characters as `?` (the
  default at a terminal); affects only the non-escaping styles.
- `--show-control-chars` — print nongraphic characters as-is (the
  default when the output is not a terminal).
- `-r, --reverse` — reverse the sort order.
- `-R, --recursive` — list subdirectories recursively.
- `-L, --dereference` — show information about the file each symbolic
  link names, rather than the link itself, wherever a link appears. A link
  whose target cannot be reached is reported on standard error and the
  listing continues, with a non-zero exit status.
- `-H, --dereference-command-line` — dereference only the symbolic links
  named on the command line; links inside a listing show themselves. The
  later of `-L` and `-H` wins.
- `--dereference-command-line-symlink-to-dir` — the default when no format
  flag forces otherwise: a command-line link *to a directory* is
  dereferenced, so `ls linkdir` lists the directory, while every other link
  shows itself. `-l`, `-d`, and `-F` instead default to showing every link
  itself.
- `-s, --size` — print each entry's allocated size in blocks (scaled by
  `-h` / `--si` / `--block-size` / `-k`), with a `total` line per
  directory listing.
- `-C` — list entries in columns, filled top-to-bottom (the default
  when the output is a terminal).
- `-S` — sort by size, largest first.
- `-U` — do not sort; list entries in directory order.
- `-X` — sort by file-name extension (the text from the last `.`),
  ties by name.
- `-v` — natural "version" sort, so `f2` precedes `f10`; ties by name.
- `-f` — do not sort and show all entries: enables `-a` and `-U` and
  disables `-l` and `-s`. Applied where it appears, so a later
  `-l`/`-s`/sort flag overrides it.
- `--sort=WORD` — choose the sort key by name: `none` (`-U`), `size`
  (`-S`), `time` (`-t`), `version` (`-v`), `extension` (`-X`), or
  `name`.
- `--group-directories-first` — list directories before other entries;
  directories come first even under `-r`.
- `-w, --width <cols>` — set the output width in columns; `0` means an
  unlimited line length. Without it the terminal's width is used, or
  80 when it cannot be determined.
- `-x` — list entries in columns, filled left-to-right.
- `-1` — one name per line (the default when the output is not a
  terminal).
- `-?` — show this command's own short help (`--help` is the long
  form).
- `--file-type` — append `/` to directories, but never `*` to
  executables (`--indicator-style=file-type`).
- `--indicator-style=WORD` — choose the indicator suffix by name:
  `none`, `slash` (`-p`), `file-type` (`--file-type`), or `classify`
  (`-F`).
- `-G, --no-group` — omit the group column from the long format. Unlike
  `-o` it does not select the long format on its own.
- `--author` — with `-l`, print the author column (the owning user,
  since there is no separate author) after the owner and before the
  group.
- `--si` — like `-h` but powers of 1000 (`1.1k`, `23M`).
- `-k, --kibibytes` — use 1024-byte blocks for the `-s` cells and the
  `total`. This is already the default, so it confirms the output
  rather than changing it; a size option overrides it.
- `--block-size=SIZE` — scale the long-format file sizes and the `-s`
  blocks by SIZE: a plain integer (bytes), or a unit `K`/`M`/`G`/`T`/
  `P`/`E` (1024-based), a `KiB`-style unit (1024-based), or a
  `KB`-style unit (1000-based), optionally with a leading integer
  coefficient. A bare unit prints its suffix; a coefficient suppresses
  it. A malformed SIZE is an error.
- `--format=WORD` — choose the arrangement by name: `long` (`-l`) or
  `verbose`, `single-column` (`-1`), `vertical` (`-C`), `across` or
  `horizontal` (`-x`), or `commas` (`-m`).
- `-T, --tabsize <cols>` — set the column-grid tab stop (default 8);
  `0` pads columns with spaces only.
- `--zero` — end each output line with NUL instead of newline; also
  selects, unless overridden, the single-column arrangement, literal
  quoting, and shown control characters.

- `--color[=WHEN]` — colourise names by kind (directories, executables,
  plain files). `WHEN` is `auto` (the default: colour only when the
  output is an attested terminal), `always` (colour even when it is not,
  e.g. a serial console), or `never`; `--color` with no `WHEN` means
  `always`. Piped or redirected output is never coloured.

## EXAMPLES

- `ls` — list the current directory.
- `ls -al /System` — long-format listing of `/System`, including
  hidden entries.
- `ls -lhS` — long format, human-readable sizes, largest first.
- `ls -v` — natural sort, so `f2` comes before `f10`.
- `ls --group-directories-first` — directories first, then files.
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

- `TERM` — the terminal type, which decides the colour depth of
  `--color` output. An unset or colourless `TERM` renders plain under
  `auto`.

## SEE ALSO

- `cat`
- `man`
