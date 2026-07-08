## NAME

printf — format and print data

## SYNOPSIS

`printf format [argument...]`

## DESCRIPTION

Prints `argument`(s) under the control of `format`, like the C `printf`
function. The format holds three kinds of item: plain characters, copied
to standard output; backslash escapes; and `%` conversion directives,
each converting the next argument.

The escapes are `\a` (alert), `\b` (backspace), `\c` (end all output
immediately), `\e` (escape), `\f` (form feed), `\n` (newline), `\r`
(carriage return), `\t` (tab), `\v` (vertical tab), `\\`, `\"`, `\NNN`
(one to three octal digits), `\xHH` (one or two hex digits), and
`\uHHHH` / `\UHHHHHHHH` (Unicode code points, four or eight hex digits).

The conversions are `%d`/`%i` (signed decimal), `%u` (unsigned decimal),
`%o`/`%x`/`%X` (octal and hexadecimal), `%e`/`%E`/`%f`/`%F`/`%g`/`%G`/
`%a`/`%A` (floating point), `%c` (the argument's first character), `%s`
(string), `%b` (string with its own backslash escapes interpreted, octal
written `\0NNN`), `%q` (string quoted for reuse as shell input), and
`%%` (a literal `%`). A directive takes the C flags `-`, `+`, space,
`#`, `0`, and `'`, a field width, and a precision; width and precision
may each be `*`, reading their value from the next argument. `%b` and
`%q` take no flags, width, or precision.

The format is reused as necessary until every argument is consumed; a
conversion with no argument left prints zero or the empty string. A
numeric argument is read like a C number (`0x` hex, leading-`0` octal,
floating point, `inf`, `nan`); a leading `'` or `"` converts the next
character's code point. An argument that is not a number, only partially
a number, or out of range is diagnosed on standard error and converted
as far as it goes — the run continues and exits `1`. An unknown
conversion, a flag on a conversion that does not accept it, or a
malformed escape ends the run with a diagnostic.

Two deliberate divergences from GNU `printf`: floating point is computed
in IEEE 754 double precision (GNU uses `long double`), so a value beyond
double's range prints `inf`; and a *first* argument of `-h` or `-?`
shows this short help — spell such a format `printf -- -h...`.

## OPTIONS

- `-h, -?` — show this command's own short help (first argument only).
- `--` — end option parsing; the next argument is the format.

## EXAMPLES

- `printf '%s\n' hello` — print `hello` and a newline.
- `printf '%d\n' 0x10` — print `16`.
- `printf '%5.2f|\n' 3.14159` — print ` 3.14|`.
- `printf '%s=%q\n' greeting 'hi there'` — print `greeting='hi there'`.
- `printf '%b' 'one\ntwo\n'` — print two lines from one argument.
- `printf '%s-' a b c` — reuse the format: `a-b-c-`.

## EXIT STATUS

- `0` — everything (or the requested short help) was written.
- `1` — a conversion problem was diagnosed, the format was missing or
  invalid, an escape was malformed, or the output stopped accepting
  bytes.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `seq`
- `man`
