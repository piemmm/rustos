## NAME

userdel — delete a user account

## SYNOPSIS

`userdel [--] NAME`

## DESCRIPTION

Removes one account from the user database. Deleting an account is an administrative operation: the database refuses a caller without the user-administration capability.

The database is the authority on what may be removed. It refuses the deletion of the last active account that may administer users, so a system can never be left with no way to administer it.

`--` ends option parsing: every later argument is an operand.

## OPTIONS

- `-h, -?, --help` — show this command's own short help.

## EXAMPLES

- `userdel ada` — delete the account `ada`.

## EXIT STATUS

- `0` — the account was deleted.
- `1` — the database refused or failed the deletion (for example a missing capability, an unknown account, or the last administrator); the reason is printed on standard error.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such as `fr-FR`).

## SEE ALSO

- `useradd`
- `usermod`
- `passwd`
- `users`
