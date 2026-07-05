## NAME

false — do nothing, unsuccessfully

## SYNOPSIS

`false [ignored arguments]`

## DESCRIPTION

Exits with status `1`, ignoring every argument. Scripts use it wherever
a command that always fails is needed — as an always-false condition or
a deliberate failure.

Only a **first** argument of `-h`, `-?`, or `--help` is honoured (the
position GNU `false` honours `--help` in); in any later position those
tokens are ignored like everything else. Unlike GNU `false --help`,
which still exits `1`, a served short help exits `0` here — the RustOS
short-help convention.

## OPTIONS

- `-h, -?` — (first argument only) show this command's own short help.

## EXAMPLES

- `false` — fail.
- `until false; do …; done` — run the body once (the condition is
  always false).

## EXIT STATUS

- `1` — always (the tool's whole purpose).
- `0` — a requested short help was served.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `true`
- `man`
