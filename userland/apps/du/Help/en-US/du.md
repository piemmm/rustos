## NAME

du — estimate file space usage

## SYNOPSIS

`du [option...] [file...]`

## DESCRIPTION

Walks each `file` operand and reports, one row per directory (deepest
first), the on-disk storage the tree beneath it occupies, as
`size<TAB>path`. With no `file` the current directory (`.`) is walked.
A `file` operand that is not a directory is reported by itself.

The default measure is each node's real allocated storage, as the
mounted filesystem reports it, so sparse or compressed files count what
they actually occupy; `--apparent-size` (or `-b`) measures apparent
byte lengths instead. Sizes are printed in 1024-byte blocks unless a
unit option selects otherwise; a later unit option overrides an
earlier one, and block counts round up (a partially used block is a
used block).

A path that cannot be read is reported on standard error and the walk
continues with what remains; an unreadable directory contributes
nothing rather than a guessed partial sum.

A file reached through more than one name is counted **once**, so its
storage is not reported twice; `-l` counts every name instead. `-x` (one
file system) is not yet available; the `DU_BLOCK_SIZE`-family environment
variables are not read — the scale is selected by options alone.

## OPTIONS

- `-a, --all` — also report each file, not just directories.
- `-s, --summarize` — report only each operand's total (conflicts with
  `-a` and `-d`).
- `-c, --total` — append a grand-total row labelled `total`.
- `-d, --max-depth <n>` — report directories at most `n` levels below
  an operand (`0` reports the operands only); totals are unaffected.
- `-S, --separate-dirs` — a directory's row excludes its
  subdirectories.
- `-l, --count-links` — count a multiply-named file once per name
  instead of once.
- `--apparent-size` — measure apparent byte lengths, not allocated
  storage.
- `-b, --bytes` — apparent size in single bytes (`--apparent-size`
  with a block size of 1).
- `-k` — 1024-byte blocks (the default).
- `-m` — 1048576-byte blocks.
- `-h, --human-readable` — human-readable sizes in powers of 1024
  (`1.0K`, `23M`).
- `--si` — human-readable sizes in powers of 1000 (`1.0k`, `23M`).
- `-B, --block-size <size>` — report in blocks of `size` bytes (`512`,
  `1K`, `1MiB`, `1GB`, `human-readable`, `si`).
- `-0, --null` — end each row with NUL instead of newline.
- `-?, --help` — show this command's own short help.

## EXAMPLES

- `du` — the current directory's tree, one row per directory.
- `du -sh /Users/jo` — one human-readable total for `/Users/jo`.
- `du -a docs` — every file and directory under `docs`.
- `du -d1 -c /Apps /Users` — the first level of each store, then a
  grand total.

## EXIT STATUS

- `0` — every operand was walked (or the short help was written).
- `1` — a path could not be read, or the output could not be
  delivered.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `df`
- `ls`
- `man`
