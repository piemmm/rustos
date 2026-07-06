## NAME

wc — print newline, word, and byte counts for each file

## SYNOPSIS

`wc [option...] [file...]`

`wc [option...] --files0-from <file>`

## DESCRIPTION

Counts, for each `file`, its lines (newline characters), words, and
bytes, and prints them in one row followed by the file name. With no
`file`, or when `file` is `-`, standard input is read (and no name is
printed for the no-operand form). With more than one input, a final
`total` row is printed as `--total` selects.

The selectors `-l`, `-w`, `-m`, `-c`, and `-L` choose which counts are
printed; without any, the newline, word, and byte counts are printed.
Counts always appear in the fixed order: lines, words, characters,
bytes, maximum line width. A word is a maximal run of non-whitespace
characters. `-m` counts UTF-8 characters (a byte that is not valid
UTF-8 counts as a byte but not as a character); `-L` measures each
line's display width in terminal columns, with tabs advancing to the
next multiple of 8.

`--files0-from <file>` reads the operand list, NUL-separated, from
`file` (`-` means standard input); it cannot be combined with `file`
operands.

An input that cannot be read is reported on standard error and the run
continues with the next input.

## OPTIONS

- `-c, --bytes` — print the byte count.
- `-m, --chars` — print the character count.
- `-l, --lines` — print the newline count.
- `-w, --words` — print the word count.
- `-L, --max-line-length` — print the maximum display width of a line.
- `--files0-from <file>` — read the NUL-separated operand list from
  `file` (`-` reads it from standard input).
- `--total <when>` — when to print the `total` row: `auto` (the
  default: only with more than one input), `always`, `only` (only the
  total, unlabelled), or `never`.
- `-h, -?` — show this command's own short help.

## EXAMPLES

- `wc notes.txt` — print the line, word, and byte counts of
  `notes.txt`.
- `wc -l a b` — print the line count of `a` and of `b`, then the
  total.
- `wc -L table.txt` — print the widest line of `table.txt` in
  terminal columns.
- `wc -c --total=only a b` — print just the summed byte count.

## EXIT STATUS

- `0` — every input was counted (or the short help was written).
- `1` — an input could not be read, or the output could not be
  delivered.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `cat`
- `head`
- `man`
