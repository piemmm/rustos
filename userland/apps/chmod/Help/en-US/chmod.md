## NAME

chmod — change file mode bits

## SYNOPSIS

`chmod [-cfRv] [--] MODE file...`

## DESCRIPTION

Changes the permission bits of each file operand to `MODE`, in order.
`MODE` is either an absolute octal value (`644`, `0755`, …) that
replaces the permission bits outright, or a comma-separated list of
symbolic clauses `[ugoa]*[-+=][rwxXst]*` (`g+w`, `o-rx`, `a=rx`,
`u+s`) that transform the file's current bits. The symbolic `X` grants
execute only to a directory or to a file that already carries an
execute bit.

Only a file's owner may change its mode; the kernel refuses anyone
else, and holding a capability grants no override. With `-R` a
directory operand is changed and then its contents are changed
recursively. The first failure stops the run before any later operand.
`--` ends option parsing: every later argument is an operand. To set a
mode beginning with `-`, write it without the dash (`a-w`) or end
options first (`chmod -- -w file`).

## OPTIONS

- `-R, --recursive` — change files and directories recursively.
- `-c, --changes` — report only files whose mode actually changed.
- `-v, --verbose` — report every file processed.
- `-f, --silent, --quiet` — suppress most error messages; the run
  still fails and the exit status reports it.
- `-h, -?, --help` — show this command's own short help.

## EXAMPLES

- `chmod 644 notes.txt` — owner read/write, everyone else read-only.
- `chmod g+w shared.txt` — add group write to the current bits.
- `chmod -R a=rx Docs` — make the `Docs` tree world-readable and
  traversable.

## EXIT STATUS

- `0` — every mode change succeeded.
- `1` — a filesystem or output failure; the reason is printed on
  standard error (suppressed under `-f`).
- `2` — the command line was not understood, or the mode operand
  parsed as neither octal nor symbolic.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `ls`
- `mkdir`
- `rm`
