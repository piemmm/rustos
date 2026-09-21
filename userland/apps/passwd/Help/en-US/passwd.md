## NAME

passwd — set an account's password

## SYNOPSIS

`passwd [--record RECORD] [--] NAME`

## DESCRIPTION

Replaces the named account's stored password. Setting a password is an
administrative operation: the database refuses a caller without the
user-administration capability.

A plaintext password never crosses the system call. The tool prompts twice
with terminal echo off, hashes what was typed into a salted PBKDF2 record
with a salt drawn from the kernel random source, and sends the record; both
plaintext buffers are zeroised the moment the record exists.

The account name is required. GNU `passwd` with no operand changes the
caller's own password, which on TAIRiX would need an unprivileged
self-service path that does not exist: the whole account-administration
interface is capability-gated, and carving out "your own record" is a
security-model change rather than a convenience.

A caller with no terminal — a graphical program, whose standard input is
closed when it runs under the elevation broker — hashes the password itself
and hands the finished record over with `--record`, so no plaintext exists
on either side. The record is checked as a well-formed one before it is
stored.

`--` ends option parsing: every later argument is an operand.

## OPTIONS

- `--record RECORD` — a ready salted PBKDF2 record, for a caller with no
  terminal to prompt at.
- `-h, -?, --help` — show this command's own short help.

## EXAMPLES

- `passwd ada` — prompt twice and set the account's password.

## EXIT STATUS

- `0` — the password was replaced.
- `1` — the database refused or failed the replacement, the prompts
  disagreed, no randomness was available, or the record was malformed; the
  reason is printed on standard error.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such as `fr-FR`).

## SEE ALSO

- `useradd`
- `usermod`
- `userdel`
- `users`
