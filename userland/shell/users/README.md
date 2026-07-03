# `rustos-users-cli` — interactive account administration

The `users` tool (`/System/Apps/users.app/Run`) is the first holder of the
`CAP_USER_ADMIN`-gated `users_admin` syscall
(`plans/CAPABILITY_USE.md` CU4). It administers the system's user
accounts and groups interactively: an administrator's shell spawns it,
and every operation is validated, persisted, and made live by the
kernel's account-administration engine.

Stability tier: **experimental**.

## Commands

```
list                       list accounts (non-secret fields only)
groups                     list groups
create <name> <uid> <gid>  create an account (prompts for display name + password)
passwd <name>              replace an account's password
lock <name> / unlock <name>
grant <name> <CAP_...>     add one capability to an account's ceiling
revoke <name> <CAP_...>    remove one capability from it
deluser <name>             delete an account
addgroup <name> <gid> / delgroup <name>
help / exit
```

A created account starts from the shared session baseline
(`rustos_users::SESSION_BASELINE`) with `/Users/<name>` as its home
(provisioned kernel-side, owned by the new account) and the default
shell; `grant` widens it afterwards.

## Security shape

- **No ambient authority.** The tool's manifest requests only the
  console pair plus `CAP_USER_ADMIN` — deliberately above the session
  baseline, so the `manifest ∩ ceiling` intersection arms it only for an
  administrator account; for anyone else every call is refused at
  dispatch. It holds no `CAP_FS_ACCESS`.
- **The kernel decides everything.** Never-widen (a grant edit is
  bounded by the *caller's own* effective set), the last-administrator
  guard, and the `users-v1` format rules are enforced in
  `kernel/core::useradmin`, not here; a change binds at the next
  spawn/login.
- **Password hygiene.** Passwords are read with echo off, hashed
  client-side into a salted PBKDF2 record (`lib/users`, salt from the
  kernel CSPRNG via `sys:random`), zeroised after use, and never sent or
  logged in plaintext; no operation returns stored password material.

## Layout

`src/session.rs` is the whole behaviour behind three seams (`ToolIo`,
`AdminChannel`, `SaltSource`), host-tested by scripted sessions in
`src/session_tests.rs`; `src/run.rs` is the freestanding pure-Rust `Run`
binary binding the seams to the inherited standard streams, the
`users_admin` wrapper, and `sys:random`.

`cargo test -p rustos-users-cli` drives the command grammar, the exact
typed requests submitted (decoded and asserted field by field), the
password-record round trip, the grant merge/removal flow, the response
rendering, and the fail-closed usage/refusal paths.
