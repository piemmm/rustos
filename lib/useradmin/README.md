# `lib/useradmin` — the shared `users_admin` client

**Stability tier: `experimental`.**

The one place a command app encodes a `users_admin` request, submits it,
decodes a listing, and words a refusal. Its consumers are the account
administration command apps — `useradd`, `usermod`, `userdel`, `passwd`,
`groupadd`, `groupdel` — and the interactive `users` session.

## Why it exists

Each of those tools needs the same four things: a fixed encode buffer
bounded by `USERS_ADMIN_MAX_REQUEST`, a reply capacity sized from the
on-disk database maximum, a fail-closed walk of a list response, and a
terse wording for whatever `Errno` the kernel chose. Seven private copies
would drift, and two tools printing different words for the same refusal
is exactly the duplication the charter forbids.

## What it is not

It is not a policy point and holds no authority. The `CAP_USER_ADMIN`
dispatch gate, the never-widen grant rule, the last-administrator guard,
and every name, uid and record validity rule are enforced kernel-side
under the caller's attested identity. This crate builds and decodes; the
kernel judges, and a refusal arrives here as the `Errno` it chose.

The [`AdminChannel`] seam is the whole outside world: production binds it
to the `users_admin` syscall, tests to an in-memory database, so every
decision above it is host-tested with no kernel.

## Secret hygiene

A `CreateUser` or `SetPassword` request carries a salted PBKDF2 record.
The record is credential material even though it is not a password, so
[`submit`] scrubs its encode buffer before returning. Plaintext passwords
never reach this crate: a caller hashes with `tairix_users::PasswordRecord`
and hands over the record.
