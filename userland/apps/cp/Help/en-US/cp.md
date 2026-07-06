## NAME

cp — copy files and directories

## SYNOPSIS

`cp [-finrRvT] [-t dir] [--] source... dest`

## DESCRIPTION

Copies each source operand to a destination. With a single source and
a destination that does not name a directory, the source is copied to
that exact path. When the destination names an existing directory —
and always when there is more than one source — each source is copied
*into* that directory under its own base name.

A directory source is copied only with `-r`, which reproduces the
whole subtree; without `-r` a directory operand is refused. An
existing destination file is overwritten by default, skipped under
`-n`, and asked about on the standard error stream under `-i` (a
declined question skips that copy without error; an unreadable reply
is never treated as consent).

The first failure stops the run before any later operand. `--` ends
option parsing: every later argument is a path.

## OPTIONS

- `-r, -R, --recursive` — copy directories and their contents.
- `-f, --force` — when a destination file cannot be created, remove
  it and retry the copy once.
- `-i, --interactive` — ask before overwriting an existing file; only
  a reply beginning `y`/`Y` consents.
- `-n, --no-clobber` — never overwrite an existing file. The later of
  `-i` and `-n` wins.
- `-v, --verbose` — report each copy as `'source' -> 'dest'`.
- `-t dir, --target-directory=dir` — copy every source into `dir`,
  which must be an existing directory. The value follows attached
  (`-tdir`, `--target-directory=dir`) or as the next argument.
- `-T, --no-target-directory` — treat the destination as a normal
  file; exactly one source is permitted. Cannot be combined with
  `-t`.
- `-h, -?, --help` — show this command's own short help.

## EXAMPLES

- `cp notes.txt backup.txt` — copy one file to a new name.
- `cp -r Projects Archive` — reproduce the `Projects` tree inside
  `Archive` (or as `Archive` if it does not exist).
- `cp -v -t Backup a.txt b.txt` — copy both files into `Backup`,
  reporting each copy.

## EXIT STATUS

- `0` — every copy succeeded (a `-n` skip and a declined `-i`
  question are not failures).
- `1` — a filesystem, prompt, or output failure; the reason is
  printed on standard error.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `ls`
- `mv`
- `rm`
