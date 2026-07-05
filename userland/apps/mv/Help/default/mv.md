## NAME

mv — move (rename) files and directories

## SYNOPSIS

`mv [-finvT] [-t dir] [--] source... dest`

## DESCRIPTION

Moves each source operand to a destination. With a single source and
a destination that does not name a directory, the source is renamed
to that exact path. When the destination names an existing directory
— and always when there is more than one source — each source is
moved *into* that directory under its own base name.

A move within one volume is an atomic rename that preserves the
node's identity. A move whose source and destination lie on different
volumes cannot be atomic: it falls back to copying the source to the
destination and then removing the source (directories are reproduced
recursively).

An existing destination is overwritten by default, skipped under
`-n`, and asked about on the standard error stream under `-i` (a
declined question skips that move without error; an unreadable reply
is never treated as consent). The first failure stops the run before
any later operand. `--` ends option parsing: every later argument is
a path.

## OPTIONS

- `-f, --force` — remove a blocking destination and retry the rename;
  never prompt. The later of `-f`, `-i`, and `-n` wins.
- `-i, --interactive` — ask before overwriting an existing
  destination; only a reply beginning `y`/`Y` consents.
- `-n, --no-clobber` — never overwrite an existing destination.
- `-v, --verbose` — report each move as `renamed 'source' -> 'dest'`.
- `-t dir, --target-directory=dir` — move every source into `dir`,
  which must be an existing directory. The value follows attached
  (`-tdir`, `--target-directory=dir`) or as the next argument.
- `-T, --no-target-directory` — treat the destination as a normal
  file; exactly one source is permitted. Cannot be combined with
  `-t`.
- `-h, -?, --help` — show this command's own short help.

## EXAMPLES

- `mv draft.txt final.txt` — rename one file.
- `mv -v a.txt b.txt Archive` — move both files into `Archive`,
  reporting each move.
- `mv -n new.cfg current.cfg` — install a file only if the
  destination does not already exist.

## EXIT STATUS

- `0` — every move succeeded (a `-n` skip and a declined `-i`
  question are not failures).
- `1` — a filesystem, prompt, or output failure; the reason is
  printed on standard error.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `cp`
- `ls`
- `rm`
