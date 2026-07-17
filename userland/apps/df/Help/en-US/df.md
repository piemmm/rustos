## NAME

df — report filesystem space usage

## SYNOPSIS

`df [option...] [file...]`

## DESCRIPTION

Reports, one row per mounted filesystem, the volume's size, the space
used, the space available, the percentage used, and the mount point.
With `file` operands it reports the filesystem containing each operand
instead (one row per filesystem, however many operands it covers).

The numbers come from the System Information API's mount listing, as
each mounted filesystem driver reports its own accounting. By default
the report hides mounts with no capacity of their own (the system's
synthetic view bindings) and further mounts of an already-listed
volume; `-a` shows everything, and the hidden count is noted on the
standard information stream (fd 3), never in the table.

Sizes are printed in 1024-byte blocks unless a unit option selects
otherwise; a later unit option overrides an earlier one, and block
counts round up. A filesystem whose format allocates inodes on demand
reports zero inode figures under `-i` — the honest "untracked" answer.

A `file` operand that does not exist, or that is a relative path
(mount points are absolute; `df` never guesses a resolution), is
reported on standard error and the report continues with the rest.
The GNU `--output`, `--sync`, and `--no-sync` options are not yet
available.

## OPTIONS

- `-a, --all` — include the capacity-less and duplicate mounts the
  default hides.
- `-T, --print-type` — add the filesystem-type column.
- `-t, --type <type>` — report only filesystems of `type`
  (repeatable).
- `-x, --exclude-type <type>` — omit filesystems of `type`
  (repeatable).
- `-i, --inodes` — report inode counts instead of block usage.
- `-P, --portability` — the POSIX portable format (`1024-blocks` and
  `Capacity` header wording).
- `-l, --local` — restrict the report to local filesystems (every
  TAIRiX mount today, so nothing is filtered away).
- `--total` — append a row labelled `total` summing the displayed
  figures.
- `-k` — 1024-byte blocks (the default).
- `-h, --human-readable` — human-readable sizes in powers of 1024
  (`1.0K`, `23M`).
- `-H, --si` — human-readable sizes in powers of 1000 (`1.0k`, `23M`).
- `-B, --block-size <size>` — report in blocks of `size` bytes (`512`,
  `1K`, `1MiB`, `1GB`, `human-readable`, `si`).
- `-?, --help` — show this command's own short help.

## EXAMPLES

- `df` — every real volume's usage in 1024-byte blocks.
- `df -h` — the same, in human-readable sizes.
- `df /Users/jo` — the filesystem containing `/Users/jo`.
- `df -aT` — every mount, with its filesystem type.
- `df --total -k` — the volumes plus a summed `total` row.

## EXIT STATUS

- `0` — the report covered everything asked for (or the short help
  was written).
- `1` — an operand could not be reported, the filters left nothing,
  or the query/output failed.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `du`
- `mount`
- `man`
