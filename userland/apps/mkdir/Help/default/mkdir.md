## NAME

mkdir — make directories

## SYNOPSIS

`mkdir [-pv] [--] directory...`

## DESCRIPTION

Creates each directory operand, in order. Without `-p` every operand's
parent must already exist and the operand itself must not; the first
failure stops the run before any later operand.

With `-p` every missing ancestor is created first, outermost first, and
an operand (or ancestor) that already exists as a directory is not an
error. An ancestor that exists as a file still fails: nothing is ever
silently replaced.

GNU `mkdir`'s `-m`/`--mode` is not yet accepted: directories are created
with the filesystem's default mode until the mode-setting facility
lands, and the switch will arrive with it rather than being ignored.
`--` ends option parsing: every later argument is a path.

## OPTIONS

- `-p, --parents` — make missing parent directories; an operand that is
  already a directory is not an error.
- `-v, --verbose` — report each created directory as
  `mkdir: created directory 'dir'`.
- `-h, -?` — show this command's own short help (also `--help`).

## EXAMPLES

- `mkdir Notes` — create one directory in the current directory.
- `mkdir -p Projects/os/build` — create the whole chain, skipping the
  parts that already exist.
- `mkdir -pv Home:/tools/bin` — create under an alias root, reporting
  each new directory.

## EXIT STATUS

- `0` — every directory was created (or, under `-p`, already existed).
- `1` — a filesystem or output failure; the reason is printed on
  standard error.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

rmdir, rm, ls
