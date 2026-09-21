## NAME

groupdel — delete a group

## SYNOPSIS

`groupdel [--] NAME`

## DESCRIPTION

Removes one group from the group registry. Deleting a group is an administrative operation: the registry refuses a caller without the user-administration capability.

The registry is the authority on what may be removed. It refuses the deletion of a group an account still references, so no account is ever left naming a group that does not exist.

`--` ends option parsing: every later argument is an operand.

## OPTIONS

- `-h, -?, --help` — show this command's own short help.

## EXAMPLES

- `groupdel staff` — delete the group `staff`.

## EXIT STATUS

- `0` — the group was deleted.
- `1` — the registry refused or failed the deletion (for example a missing capability, an unknown group, or a group an account still references); the reason is printed on standard error.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such as `fr-FR`).

## SEE ALSO

- `groupadd`
- `usermod`
- `users`
