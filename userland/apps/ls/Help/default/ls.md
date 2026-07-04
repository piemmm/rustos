## NAME

ls — list directory contents

## SYNOPSIS

`ls [-a] [-l] [--] [path...]`

## DESCRIPTION

Lists each path operand in order. A directory operand has its entries
listed, sorted by name; a non-directory operand is listed by its name.
With no operand the current directory is listed.

Entries whose name begins with `.` are hidden unless `-a` is given. When
the default filter hides entries, `ls` notes how many on the advisory
stream (fd 3); the listing itself is unchanged.

With more than one operand, non-directory operands are listed first
(sorted by name), then each directory operand under a `path:` header,
with blocks separated by a blank line.

The long format prints, per entry: a type character (`d` for a
directory, `-` otherwise), the nine permission bits, the size in bytes
right-aligned across the block, then the name.

## OPTIONS

- `-a, --all` — do not hide entries whose name begins with `.`.
- `-l, --long` — long format: type and permission bits, size, then name.
- `-h, -?` — show this command's own short help.

## EXAMPLES

- `ls` — list the current directory.
- `ls -la /System/Apps` — list every entry of `/System/Apps`, hidden
  ones included, in the long format.
- `ls -- -a` — list the file or directory named `-a`.

## EXIT STATUS

- `0` — every operand was listed.
- `1` — an operand could not be inspected, a directory could not be
  read, or the listing could not be delivered.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `man`
