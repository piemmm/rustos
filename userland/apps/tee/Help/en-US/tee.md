## NAME

tee — read from standard input and write to standard output and files

## SYNOPSIS

`tee [option...] [file...]`

## DESCRIPTION

Copies standard input to standard output and to each named file, so a
pipeline's data can be seen and captured at once. Each file is created
if absent and overwritten unless `-a` appends. A file that cannot be
opened or written is reported and the run continues with the remaining
outputs, per the selected `--output-error` mode.

RustOS has no `SIGPIPE`: a consumer going away surfaces as a write
error on standard output — the one output of this command that can be a
pipe — so the "pipe" of the GNU modes means exactly that output here.
Without `--output-error`, a failed standard output stops the run (the
equivalent of the GNU tool dying of `SIGPIPE`, with the reason stated on
standard error); with a `-nopipe` mode it is tolerated silently.

GNU `tee -i` (ignore interrupts) is not available: RustOS has no
per-process signal disposition to set. The switch arrives with that
kernel work rather than being accepted and ignored.

## OPTIONS

- `-a, --append` — append to the named files; do not overwrite them.
- `-p` — tolerate a failed standard output silently; the same as
  `--output-error=warn-nopipe`.
- `--output-error[=<mode>]` — how a failed output is treated. Without a
  value, `warn-nopipe`. The modes (an unambiguous prefix is accepted):
  `warn` — report an error writing to any output, drop that output, and
  continue; `warn-nopipe` — as `warn`, but a failed standard output is
  dropped silently and does not affect the exit status; `exit` — report
  an error writing to any output and stop; `exit-nopipe` — as `exit`,
  but a failed standard output is dropped silently.
- `-h, -?` — show this command's own short help.
- `--` — end option parsing; every later argument names a file, and a
  `-` operand names a file called `-`.

## EXAMPLES

- `ls -l | tee listing.txt` — show the listing and save a copy.
- `make 2>&1 | tee -a build.log` — append a build transcript while
  watching it.
- `cat data | tee copy1 copy2 | wc -c` — capture two copies and count
  the bytes flowing on.

## EXIT STATUS

- `0` — every output was served to end-of-input (or a requested short
  help was served); a standard-output failure tolerated by a `-nopipe`
  mode does not change this.
- `1` — an output failed in a way the selected mode counts, or input
  could not be read.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `cat`
- `head`
- `wc`
