## NAME

groupadd — create a group

## SYNOPSIS

`groupadd [-g GID] [--] NAME`

## DESCRIPTION

Adds a single group to the group registry. The group name must match
`[a-z_][a-z0-9_-]*` and the id is a decimal value. Creating a group is
an administrative operation: the registry refuses a caller without the
user-administration capability.

When `-g` is omitted the group id is allocated automatically, one above
the highest existing id. A requested id that is already taken is
refused; the registry is the authority on collisions.

`--` ends option parsing: every later argument is an operand.

## OPTIONS

- `-g, --gid GID` — numeric group id; allocated automatically when
  omitted (one above the highest existing id).
- `-h, -?, --help` — show this command's own short help.

## EXAMPLES

- `groupadd staff` — create `staff` with an auto-allocated id.
- `groupadd -g 100 staff` — create `staff` with id `100`.

## EXIT STATUS

- `0` — the group was created.
- `1` — the registry refused or failed the creation (for example a
  missing capability or a duplicate id); the reason is printed on
  standard error.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `useradd`
- `users`
