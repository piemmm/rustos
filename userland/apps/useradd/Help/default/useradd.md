## NAME

useradd — create a user account

## SYNOPSIS

`useradd [-u UID] -g GID [-G GID[,GID...]] [-c COMMENT] [-d HOME] [--] NAME`

## DESCRIPTION

Adds a single account to the user database. The login name must match
`[a-z_][a-z0-9_-]*`; the primary group (`-g`) is required and every group
or user reference is a decimal id. Creating an account is an
administrative operation: the database refuses a caller without the
user-administration capability.

The created account has **no usable password**: no password matches it
until an administrator sets one (and none can be guessed), exactly as the
GNU tool creates a disabled account. Set a password afterwards with the
`users` tool's `passwd` command.

When `-u` is omitted the user id is allocated automatically, one above
the highest existing id. When `-d` is omitted the home directory is the
standard `/Users/NAME` layout. The account starts the system default
shell and the ordinary session capability ceiling; an administrator
widens it afterwards with the `users` tool's `grant` command.

`--` ends option parsing: every later argument is an operand.

## OPTIONS

- `-u, --uid UID` — numeric user id; allocated automatically when
  omitted (one above the highest existing id).
- `-g, --gid GID` — numeric primary group id. Required: there is no
  default-group policy to guess.
- `-G, --groups LIST` — comma-separated numeric supplementary group ids.
- `-c, --comment TEXT` — account comment / full display name.
- `-d, --home PATH` — home directory; `/Users/NAME` when omitted.
- `-h, -?, --help` — show this command's own short help.

## EXAMPLES

- `useradd -g 100 alice` — create `alice` in primary group `100` with an
  auto-allocated id.
- `useradd -u 1000 -g 100 -G 10,20 -c 'Alice A' alice` — every field
  spelled out.

## EXIT STATUS

- `0` — the account was created.
- `1` — the database refused or failed the creation (for example a
  missing capability, a duplicate id, or an unknown group); the reason
  is printed on standard error.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `groupadd`
- `users`
