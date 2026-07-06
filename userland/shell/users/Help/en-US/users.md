## NAME

users — administer user accounts and groups

## SYNOPSIS

`users [-h | -?]`

## DESCRIPTION

Runs the interactive account-administration session over the gated
`users_admin` interface. Every operation is decided kernel-side under
your kernel-attested identity: without `CAP_USER_ADMIN` in your
account's ceiling every operation is refused at dispatch. Passwords are
read with terminal echo off and hashed client-side into a salted
record; plaintext never crosses the interface and is never echoed or
logged.

The tool takes no operands: accounts are administered with commands
typed inside the session.

- `list` — list user accounts.
- `groups` — list groups.
- `create <name> <uid> <gid>` — create an account.
- `passwd <name>` — set an account's password.
- `lock <name>`, `unlock <name>` — disable or re-enable an account.
- `grant <name> <CAP_...>`, `revoke <name> <CAP_...>` — edit an
  account's capability grants.
- `deluser <name>` — delete an account.
- `addgroup`, `delgroup` — create or delete a group.
- `help` — list the session commands.
- `exit`, `quit` — end the session.

## OPTIONS

- `-h, -?` — show this command's own short help and exit.

## EXIT STATUS

- `0` — the session ended cleanly, or the short help was shown.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `man`
