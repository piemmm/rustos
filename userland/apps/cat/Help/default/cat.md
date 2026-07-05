## NAME

cat — concatenate files to standard output

## SYNOPSIS

`cat [-n] [--] [file...]`

## DESCRIPTION

Reads each file operand in order and writes its bytes to standard
output. The operand `-` names standard input, and with no operand
standard input is the single source.

With `-n` output lines are numbered continuously across every source,
so a line that straddles two sources is numbered exactly once, when
its first byte appears.

A source that cannot be read stops the command before any later
source is touched; the bytes already written stay written.

## OPTIONS

- `-n, --number` — number output lines, continuously across every
  source.
- `-h, -?` — show this command's own short help.

## EXAMPLES

- `cat notes.txt` — write `notes.txt` to standard output.
- `cat a.txt - b.txt` — write `a.txt`, then standard input, then
  `b.txt`.
- `cat -n log.txt` — number every output line.
- `cat -- -n` — write the file named `-n`.

## EXIT STATUS

- `0` — every source was written.
- `1` — a source could not be read, or the output could not be
  delivered.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `ls`
- `man`
