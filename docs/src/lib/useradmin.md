# `tairix-useradmin` — the shared `users_admin` client

`lib/useradmin` is the one place a command app encodes a `users_admin`
request, submits it, decodes a listing, and words a refusal. Its
consumers are the account-administration command apps — `useradd`,
`usermod`, `userdel`, `passwd`, `groupadd`, `groupdel` — and the
interactive `users` session.

Stability tier: `experimental`.

## Why one client

Each of those tools needs the same four things: a fixed encode buffer
bounded by `USERS_ADMIN_MAX_REQUEST`, a reply capacity sized from the
on-disk database maximum, a fail-closed walk of a list response, and a
terse wording for whatever `Errno` the kernel chose. Seven private
copies would drift, and two tools printing different words for the same
refusal is the duplication the charter forbids.

## Not a policy point

The crate holds no authority. The `CAP_USER_ADMIN` dispatch gate, the
never-widen grant rule, the last-active-administrator guard, and every
name, uid and record validity rule are enforced kernel-side under the
caller's attested identity. A refusal arrives here as the `Errno` the
kernel chose and is rendered, never reinterpreted.

The `AdminChannel` seam is the whole outside world: production binds it
to the `users_admin` syscall, tests to an in-memory database, so every
decision above it is host-tested with no kernel.

## The relayed listing

A graphical surface cannot call `users_admin` at all — a desktop
application holds no authority — so it asks an authenticated account to
run `users --list` and reads what was printed back through the
supervisor's elevated-read seam. The `listing` module holds **both**
halves of that exchange: the renderer the tool writes with and the
parser the surface reads with, so the two cannot disagree about what a
field means.

The line form is `:`-delimited because the `users-v1` and `groups-v1`
databases already forbid `:` in every field, so no value can contain one
and no escaping is needed. A line the parser does not recognise is
skipped rather than failing the whole read, so one record a future build
spells differently never hides the rest.

## Secret hygiene

A `CreateUser` or `SetPassword` request carries a salted PBKDF2 record.
That is credential material even though it is not a password, so
`submit` scrubs its encode buffer before returning. Plaintext passwords
never reach this crate: a caller hashes with `tairix_users::PasswordRecord`
and hands over the finished record.
