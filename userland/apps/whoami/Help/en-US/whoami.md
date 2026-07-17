## NAME

whoami — print the current user's account name

## SYNOPSIS

`whoami`

## DESCRIPTION

Prints the user name associated with this process's identity, followed
by a newline, and nothing else.

TAIRiX has no `/etc/passwd`: the user identifier comes from the
kernel's own record of the calling process, and the matching account
name comes from the System Information API's public account directory.
If the directory holds no name for the identifier, the command reports
`cannot find name for user ID <uid>` and fails.

The command takes no operands; an argument is an `extra operand`
error.

## OPTIONS

- `-h, -?` — show this command's own short help.
- `--` — end option parsing; any later argument is still an extra
  operand (`whoami` takes none).

## EXAMPLES

- `whoami` — print the name of the account running the command.

## EXIT STATUS

- `0` — the name (or a requested short help) was written.
- `1` — the identity read, the directory lookup, or the output failed,
  or the directory holds no name for the user ID.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `users`
- `ps`
