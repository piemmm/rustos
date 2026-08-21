## NAME

unlink — remove one name

## SYNOPSIS

`unlink [--] file`

## DESCRIPTION

Removes exactly one name, through the one filesystem call the POSIX
`unlink` function names. There is deliberately no recursion, no force,
no prompting and no verbose reporting: a script that must remove one
name and nothing else has a tool that cannot do more. Use `rm` when you
want those options and `rmdir` for a directory.

The name is removed **as typed**. A symbolic link is removed itself and
is never followed, so a link planted at the name cannot redirect the
removal to what it points at.

A **directory** is refused by the filesystem, in the same locked walk
that would have removed the entry — so no check-then-remove race exists
here, and a directory swapped in for a file cannot be unlinked instead.

Exactly one operand is required: no operand and two or more operands are
both usage errors, and nothing is removed. `--` ends option parsing, so
a name that begins with a dash is removable.

## OPTIONS

- `-?, --help` — show this command's own short help.

## EXAMPLES

- `unlink stale.log` — remove one name.
- `unlink Home:/Documents/alias` — remove a symbolic link itself, not
  what it points at.
- `unlink -- -weird-name` — remove a name that begins with a dash.

## EXIT STATUS

- `0` — the name was removed (or the short help was written).
- `1` — the filesystem refused the removal, or the output failed; the
  reason is printed on standard error.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

rm, rmdir, ln, link, readlink
