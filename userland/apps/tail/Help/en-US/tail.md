## NAME

tail — output the last part of files

## SYNOPSIS

`tail [option...] [file...]`

## DESCRIPTION

Prints the last 10 lines of each `file` to standard output. With more
than one `file`, each part is preceded by a `==> file <==` header. With
no `file`, or when `file` is `-`, standard input is read.

`-n` and `-c` change how much is printed: a plain count (or one written
with a leading `-`) prints the last `num` lines or bytes; a count
written with a leading `+` prints everything **from** line or byte
`num` (counting from 1) to the end. A count may carry a multiplier
suffix: `b` (512), `kB` (1000), `K` (1024), `MB`, `M`, `GB`, `G`, and
so on for `T`, `P`, `E`, `Z`, `Y`, `R`, `Q` (a lone letter multiplies
by powers of 1024; with `B` by powers of 1000; with `iB` by powers of
1024).

The historical first-argument form `tail -num` / `tail +num` (with an
optional trailing `b`/`c`/`l` letter) is accepted, as in the GNU tool.

Follow mode (`-f`, `-F`, `--follow`, `--retry`, `--pid`,
`--sleep-interval`, `--max-unchanged-stats`) is not yet available and
is reported as an unrecognised option: it needs a file-change wake
source the system does not expose yet, and a busy-poll is not shipped
in its place.

When leading content is not shown, an advisory record is written to the
standard information stream (fd 3); it never changes the output or the
exit status. A file that cannot be read is reported on standard error
and the run continues with the next file.

## OPTIONS

- `-c, --bytes <num>` — print the last `num` bytes of each file; with a
  leading `+`, everything from byte `num` onward.
- `-n, --lines <num>` — print the last `num` lines of each file; with a
  leading `+`, everything from line `num` onward.
- `-q, --quiet, --silent` — never print the `==> file <==` headers.
- `-v, --verbose` — always print the `==> file <==` headers.
- `-z, --zero-terminated` — lines are NUL-delimited instead of
  newline-delimited.
- `-h, -?` — show this command's own short help.

## EXAMPLES

- `tail log.txt` — print the last 10 lines of `log.txt`.
- `tail -n 3 a b` — print the last 3 lines of `a` and of `b`, each
  under its header.
- `tail -c 1K image` — print the last 1024 bytes of `image`.
- `tail -n +5 notes` — print `notes` from its 5th line to the end.

## EXIT STATUS

- `0` — every file was printed (or the short help was written).
- `1` — a file could not be read, or the output could not be delivered.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `head`
- `cat`
- `wc`
- `man`
