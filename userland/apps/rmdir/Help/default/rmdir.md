## NAME

rmdir — remove empty directories

## SYNOPSIS

`rmdir [-pv] [--ignore-fail-on-non-empty] [--] directory...`

## DESCRIPTION

Removes each directory operand, in order. Only an **empty directory**
is removed: the filesystem itself refuses a file (or any
non-directory) and a populated directory, atomically, so nothing else
can ever be unlinked in its place. Use `rm` for files and `rm -r` for
populated trees.

With `-p` each operand's ancestors are removed too, innermost first:
`rmdir -p a/b/c` removes `a/b/c`, then `a/b`, then `a`. The bare root
of a path (`/` or an alias root such as `Home:/`) is never asked to be
removed.

With `--ignore-fail-on-non-empty` a "directory not empty" refusal is
not an error — the operand (or the `-p` walk) simply stops there. No
other refusal is tolerated. The first genuine failure stops the run
before any later operand. `--` ends option parsing: every later
argument is a path.

## OPTIONS

- `-p, --parents` — remove each operand's ancestors too, innermost
  first.
- `-v, --verbose` — report each removal attempt as
  `rmdir: removing directory, 'dir'`.
- `--ignore-fail-on-non-empty` — a directory that is not empty is not
  an error; with `-p` the upward walk stops there.
- `-h, -?` — show this command's own short help (also `--help`).

## EXAMPLES

- `rmdir Scratch` — remove one empty directory.
- `rmdir -p Projects/os/build` — remove the chain, innermost first.
- `rmdir -p --ignore-fail-on-non-empty a/b` — remove `a/b`, and `a`
  too if that leaves it empty.

## EXIT STATUS

- `0` — every removal succeeded (a refusal tolerated by
  `--ignore-fail-on-non-empty` is not a failure).
- `1` — a filesystem or output failure; the reason is printed on
  standard error.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

mkdir, rm, ls
