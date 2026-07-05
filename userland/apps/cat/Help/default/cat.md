## NAME

cat — concatenate files to standard output

## SYNOPSIS

`cat [-AbeEnstTuv] [--] [file...]`

## DESCRIPTION

Reads each file operand in order and writes its bytes to standard
output. The operand `-` names standard input, and with no operand
standard input is the single source.

With `-n` output lines are numbered continuously across every source,
so a line that straddles two sources is numbered exactly once, when
its first byte appears. `-b` numbers only non-empty lines and
overrides `-n`. `-s` suppresses repeated adjacent blank lines, and a
suppressed line is neither written nor numbered.

The marker options make invisible bytes visible: `-E` prints `$`
before each newline, `-T` prints TAB as `^I`, and `-v` prints other
control bytes as `^X` and non-ASCII bytes in `M-` notation. `-e`,
`-t`, and `-A` are the usual combinations `-vE`, `-vT`, and `-vET`.

A source that cannot be read stops the command before any later
source is touched; the bytes already written stay written.

## OPTIONS

- `-A, --show-all` — equivalent to `-vET`.
- `-b, --number-nonblank` — number non-empty output lines; overrides
  `-n`.
- `-e` — equivalent to `-vE`.
- `-E, --show-ends` — print `$` at the end of each line.
- `-n, --number` — number output lines, continuously across every
  source.
- `-s, --squeeze-blank` — suppress repeated adjacent blank lines.
- `-t` — equivalent to `-vT`.
- `-T, --show-tabs` — print TAB characters as `^I`.
- `-u` — accepted and ignored; output is already unbuffered.
- `-v, --show-nonprinting` — use `^` and `M-` notation for control
  and non-ASCII bytes, except line feed and TAB.
- `-h, -?` — show this command's own short help.

## EXAMPLES

- `cat notes.txt` — write `notes.txt` to standard output.
- `cat a.txt - b.txt` — write `a.txt`, then standard input, then
  `b.txt`.
- `cat -n log.txt` — number every output line.
- `cat -bs draft.txt` — number the non-empty lines and squeeze blank
  runs.
- `cat -A config.txt` — make line ends, tabs, and control bytes
  visible.
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
