## NAME

seq — print a sequence of numbers

## SYNOPSIS

`seq [-f format] [-s string] [-w] [first [increment]] last`

## DESCRIPTION

Prints the numbers from `first` to `last`, in steps of `increment`, one
per line by default. An omitted `first` or `increment` defaults to 1 —
including when `last` is smaller than `first`, so `seq 5 1` prints
nothing. The sequence ends when adding `increment` would pass `last`.

All three operands are read as floating point values; `increment` is
usually positive when `first` is below `last` and negative when it is
above, and may not be zero. `last` may be `inf` to count forever. The
default output precision follows the operands' spellings (`seq 1 0.25 2`
prints two decimal places), and plain integer runs are generated exactly,
however large the numbers.

Option scanning stops at the first operand, and a leading negative
number is an operand, not an option: `seq -5 5` counts from -5.

## OPTIONS

- `-f, --format <format>` — print every number through the printf-style
  floating-point `<format>` (one `%` directive of type `e`, `f`, `g`, or
  `a`, upper or lower case, with the usual flags, width, and precision).
  Cannot be combined with `-w`.
- `-s, --separator <string>` — separate numbers with `<string>` instead
  of a newline. The output still ends with a newline.
- `-w, --equal-width` — pad every number with leading zeros to a common
  width. Cannot be combined with `-f`.
- `-h, -?` — show this command's own short help.
- `--` — end option parsing; every later argument is an operand.

## EXAMPLES

- `seq 5` — print 1 through 5.
- `seq 2 5` — print 2 through 5.
- `seq 1 2 10` — print the odd numbers 1 through 9.
- `seq 5 -1 1` — count down from 5 to 1.
- `seq -w 8 10` — print `08`, `09`, `10`.
- `seq -s , 3` — print `1,2,3`.
- `seq -f %.2f 3` — print `1.00`, `2.00`, `3.00`.

## EXIT STATUS

- `0` — the sequence (or a requested short help) was written.
- `1` — the output stopped accepting bytes.
- `2` — the command line was not understood (an unrecognised option, an
  invalid number, a zero increment, or a bad format).

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `yes`
- `man`
