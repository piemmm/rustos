## NAME

stat — report a file's or a filesystem's status

## SYNOPSIS

`stat [-Lft] [-c FORMAT | --printf=FORMAT] [--] file...`

## DESCRIPTION

Reports the fields of one status read per operand, in command-line order.

**Without `-L` a symbolic link is described as itself** — that is what
this tool is for beside `ls`. `%N` shows the link and the target it
stores, `%F` says `symbolic link`, and the sizes and stamps are the
link's own. `-L` resolves the final link and reports what it names.

`-f` switches to the filesystem the operand lies on: the volume's block
and inode counts, its block size, and the type its mount records. The
two readings have **different** specifier vocabularies, so a format is
checked against the one `-f` selects.

`-c`/`--format` renders one format string per operand and follows it with
a newline; `--printf` interprets backslash escapes and adds no newline.
That is the only difference between them. A directive takes the
printf-style flags and width (`%-10s`, `%06i`, `%.3n`), so a report can
line up in columns. `-t` is the one-line terse form of either reading.

An operand that cannot be read is reported on standard error and the
remaining operands are still described; the command then exits non-zero.
A field this system cannot supply — a mount snapshot it may not read, a
uid the user directory has no name for — renders as `?` or GNU's
`UNKNOWN`, never as a plausible substitute.

At least one operand is required. `--` ends option parsing.

Four specifiers name a concept TAIRiX does not have, and are **refused**
by name when a format uses one rather than answered with a fabricated
value: `%G`, because the System Information API publishes a user
directory and no group counterpart, so `%g` (the numeric id) is the
honest field; `%t` and `%T` of the file vocabulary, because there are no
device special files to have a major or minor type; and `%t` of the
filesystem vocabulary, because a volume has no numeric type magic —
`%T` names the type its mount records. The refusal happens when the
format is parsed, before any path is touched.

Two specifiers report a TAIRiX concept in place of a Linux one. A volume
is identified by a 16-byte id rather than a device number, so `%d` is
that id in decimal and `%D` in hexadecimal; comparing two files' `%d`
still answers exactly "are these on one volume?".

## OPTIONS

- `-L, --dereference` — describe what a symbolic link names, rather than
  the link itself.
- `-f, --file-system` — describe the filesystem holding each operand
  rather than the operand.
- `-c, --format=FORMAT` — render `FORMAT` for each operand, followed by
  a newline.
- `--printf=FORMAT` — as `-c`, but interpret backslash escapes and print
  no trailing newline.
- `-t, --terse` — print the fields on one space-separated line.
- `-?, --help` — show this command's own short help.

## EXAMPLES

- `stat notes.txt` — the full report for one file.
- `stat -c '%s %n' *` — size and name, one line each.
- `stat -L link` — describe what the link names, not the link.
- `stat -f .` — the volume holding the working directory.

## EXIT STATUS

- `0` — every operand was described (or the short help was written).
- `1` — at least one operand could not be read, or the output failed.
- `2` — the command line was not understood, or its format named a
  directive this system cannot serve.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such as
  `fr-FR`).

## SEE ALSO

ls, readlink, df, du
