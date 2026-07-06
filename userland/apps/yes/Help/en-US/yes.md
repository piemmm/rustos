## NAME

yes — repeatedly output a line of text

## SYNOPSIS

`yes [string...]`

## DESCRIPTION

Writes its operands, joined by single spaces — or `y` when none are
given — followed by a newline, over and over until its output stops
accepting bytes (a closed pipe) or the process is terminated. Its
historical job is feeding an affirmative answer to a prompting command;
its modern one is being a cheap source of repeated text.

Option scanning stops at the first operand, so `yes a -x` prints
`a -x`. An unrecognised option before the operands is an error; write
`yes -- -x` to print a string that looks like an option.

## OPTIONS

- `-h, -?` — show this command's own short help.
- `--` — end option parsing; every later argument is an operand.

## EXAMPLES

- `yes` — print `y` until interrupted.
- `yes hello world` — print `hello world` until interrupted.
- `yes -- -x` — print `-x` (after `--`, operands may look like
  options).

## EXIT STATUS

- `0` — a requested short help was served.
- `1` — the output stopped accepting bytes (the tool's one stop
  condition).
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `true`
- `man`
