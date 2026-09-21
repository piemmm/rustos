## NAME

usermod — modify a user account

## SYNOPSIS

`usermod [-c COMMENT] [-d HOME] [-s SHELL] [-g GID] [-G LIST] [-L | -U]
[--grants LIST] [--] NAME`

## DESCRIPTION

Changes one account's identity fields, its lock state, or its capability
grant ceiling. Modifying an account is an administrative operation: the
database refuses a caller without the user-administration capability.

An identity edit replaces the account's whole non-security field set, so the
tool first reads the account's current record and resends every field nobody
named unchanged. An account the database does not list is refused before
anything is sent.

Each switch is its own database operation, applied one at a time and
whole-or-nothing. A command line asking for several issues several, in a
fixed order — fields, then grants, then lock state — and stops at the first
refusal, naming the step and warning that an earlier change may already be
in effect.

`-G` replaces the whole supplementary set rather than appending to it: the
database takes whole sets, and an append built on a stale reading would be
worse than an explicit replacement. `--grants` is a TAIRiX concept with no
coreutils counterpart, so it is spelled long-only; the database refuses any
capability the calling account does not itself hold.

`--` ends option parsing: every later argument is an operand.

## OPTIONS

- `-c, --comment COMMENT` — the account comment / full name.
- `-d, --home HOME` — the home directory.
- `-s, --shell SHELL` — the login shell.
- `-g, --gid GID` — the numeric primary group id.
- `-G, --groups LIST` — the comma-separated numeric supplementary group ids,
  replacing the current set. An empty list clears it.
- `-L, --lock` — bar the account from logging in.
- `-U, --unlock` — let it log in again.
- `--grants LIST` — the comma-separated capability names forming the
  account's whole grant ceiling. An empty list clears it.
- `-h, -?, --help` — show this command's own short help.

## EXAMPLES

- `usermod -c 'Ada Lovelace' ada` — set the account's full name.
- `usermod -L ada` — lock the account.
- `usermod --grants LIST` — replace the grant ceiling.

## EXIT STATUS

- `0` — every requested change was made.
- `1` — the database refused or failed a change; the step and the reason are
  printed on standard error.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such as `fr-FR`).

## SEE ALSO

- `useradd`
- `userdel`
- `passwd`
- `groupadd`
- `users`
