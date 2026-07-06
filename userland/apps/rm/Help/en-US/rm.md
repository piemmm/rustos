## NAME

rm — remove files and directories

## SYNOPSIS

`rm [-dfiIrRv] [--] file...`

## DESCRIPTION

Removes each file operand, in order. A non-directory operand is
unlinked; a directory operand is removed only with `-r` (which removes
its contents depth-first and then the directory itself) or, when
empty, with `-d`.

With `-f` an operand that does not exist is skipped silently and no
question is ever asked. `-i` asks on the standard error stream before
every removal and before descending into a directory; `-I` asks once
up front before removing more than three operands or before a
recursive removal. A declined question skips the object (or the whole
run, for `-I`) without error; an unreadable reply is never treated as
consent. The later of `-f`, `-i`, and `-I` wins.

The operand `/` is refused under `--preserve-root`, the default. The
first failure stops the run before any later operand. `--` ends
option parsing: every later argument is a path.

## OPTIONS

- `-r, -R, --recursive` — remove directories and their contents.
- `-f, --force` — ignore operands that do not exist; never prompt.
- `-d, --dir` — remove empty directories.
- `-i, --interactive` — prompt before every removal; only a reply
  beginning `y`/`Y` consents.
- `-I` — prompt once before removing more than three operands, or
  before a recursive removal.
- `-v, --verbose` — report each removal as `removed 'file'`.
- `--preserve-root` — refuse to remove `/` (the default).
- `--no-preserve-root` — allow removing `/`.
- `-h, -?, --help` — show this command's own short help.

## EXAMPLES

- `rm notes.txt` — remove one file.
- `rm -r Scratch` — remove the `Scratch` tree and everything in it.
- `rm -I a b c d` — ask once, then remove all four files on a `y`.

## EXIT STATUS

- `0` — every removal succeeded (a declined question and a `-f` skip
  are not failures).
- `1` — a filesystem, prompt, or output failure; the reason is
  printed on standard error.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `cp`
- `ls`
- `mv`
