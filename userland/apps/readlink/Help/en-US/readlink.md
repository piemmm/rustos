## NAME

readlink — print a symbolic link's target

## SYNOPSIS

`readlink [-nz] [-q | -s | -v] [--] file...`

## DESCRIPTION

Prints the target each operand stores, one per operand, in command-line
order.

The target is printed **exactly as stored**. A link's target is data, not
a path resolved when the link was made: it may be relative, may carry
`..`, and may name nothing at all. So `readlink` shows the spelling, and
`ls -l` shows a link beside what it currently names.

An operand that is **not** a symbolic link has no target to print — a
file and a directory are both refused with the same "value out of range"
reason — and an absent name is "not found". Either way the remaining
operands are still read and the command exits non-zero. Quiet is the
default, as in the GNU tool: `-v` turns the per-operand diagnostics on.

`-n` drops the delimiter after the last target. With more than one operand
it is ignored, and that is reported, because the delimiters between
targets are what separate them.

At least one operand is required. `--` ends option parsing.

GNU's canonicalisation switches `-f`, `-e` and `-m` are **refused**, not
approximated. Resolving every component of a path — following each link,
handling `..` physically, enforcing the hop budget and the rule that a
link cannot escape the volume that stores it — is the filesystem's one
implementation. A second copy of it in this tool could print a path the
filesystem resolves differently, so it fails closed until the filesystem
offers that resolution itself.

## OPTIONS

- `-n, --no-newline` — do not print the delimiter after the last target
  (ignored, with a report, for more than one operand).
- `-z, --zero` — end each target with NUL instead of newline.
- `-q, -s` — do not diagnose a refused read (the default; also
  `--quiet`, `--silent`).
- `-v, --verbose` — diagnose a refused read on standard error.
- `-?, --help` — show this command's own short help.

## EXAMPLES

- `readlink Home:/Desktop/Notes` — print what one shortcut stores.
- `readlink -v alias` — print it, and say why if it is not a link.
- `readlink -z a b | tr '\0' '\n'` — NUL-separated targets for a script.

## EXIT STATUS

- `0` — every operand's target was printed (or the short help was
  written).
- `1` — at least one read was refused, or the output failed.
- `2` — the command line was not understood, or named a
  canonicalisation switch.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such as
  `fr-FR`).

## SEE ALSO

ln, link, unlink, ls
