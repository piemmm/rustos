## NAME

ls — list directory contents

## SYNOPSIS

`ls [-aAdFghlmnopQrRsS1] [--] [path...]`

## DESCRIPTION

Lists each path operand: a directory operand's entries are read and
listed (unless `-d` names the directory itself), any other operand is
listed as itself. With no operand the current directory (`.`) is
listed.

Entries are sorted by name (or by size, largest first, with `-S`;
reversed with `-r`), one name per line by default. Entries whose name
begins with `.` are hidden unless `-a` or `-A` is given; when entries
are hidden, a note is emitted on the standard information stream
(fd 3), never in the listing itself.

The long format (`-l`) renders the type and permission bits, the
owner and group, the size, then the name. Owner and group are numeric
ids: resolving account names requires the capability-gated user
database, which a listing must not demand, so the output matches the
GNU tool's numeric fallback (`-n` renders identically). There is no
link-count or timestamp column because the filesystem contract does
not carry hard links or timestamps yet; the columns will appear when
it does.

When more than one operand is given — and always under `-R` — each
directory's listing is preceded by a `path:` header, and blocks are
separated by a blank line.

## OPTIONS

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
- `-m` — comma-separated names on one line.
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
- `-S` — sort by size, largest first.
- `-1` — one name per line (the default).
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
