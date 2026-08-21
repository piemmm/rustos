## NAME

readlink — print a symbolic link's target

## SYNOPSIS

`readlink [-fem] [-nz] [-q | -s | -v] [--] file...`

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

`-f`, `-e` and `-m` switch to **canonicalisation** instead: the one path
that names what the operand resolves to, with every link followed and
every `..` applied. Under any of them the operand need not be a link at
all, and the three differ only in how much of the path must exist. They
are alternatives rather than modifiers, so the last one given wins.

That resolution is the filesystem's — physical `..`, the hop budget, a
search-permission check on every directory passed through, and the rule
that a link cannot resolve outside what its mount projects — and this
tool *calls* it rather than walking links itself. A second copy of the
algorithm that disagreed by one rule would print a path the filesystem
resolves differently.

## OPTIONS

- `-f, --canonicalize` — print the canonical path; every component but
  the last must exist.
- `-e, --canonicalize-existing` — print the canonical path; every
  component must exist.
- `-m, --canonicalize-missing` — print the canonical path; no component
  need exist.
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
- `readlink -f alias` — print what it resolves to, links and all.
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
