## NAME

true — do nothing, successfully

## SYNOPSIS

`true [ignored arguments]`

## DESCRIPTION

Exits with status `0`, ignoring every argument. Scripts use it wherever
a command that always succeeds is needed — as a placeholder command, an
always-true condition, or the body of a loop.

Only a **first** argument of `-h`, `-?`, or `--help` is honoured (the
position GNU `true` honours `--help` in); in any later position those
tokens are ignored like everything else.

## OPTIONS

- `-h, -?` — (first argument only) show this command's own short help.

## EXAMPLES

- `true` — succeed.
- `while true; do …; done` — loop until interrupted.

## EXIT STATUS

- `0` — always (the tool's whole purpose).
- `1` — a requested short help could not be written.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `false`
- `man`
