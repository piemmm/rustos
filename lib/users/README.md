# rustos-users

The RustOS **user-account database**: the versioned text format persisted at
`/System/Security/Users` (`AGENTS.md` §5.1, §16.2), the account identity
types (`Uid`, `Gid`, `UserRecord`), and password verification.

- `UsersDb::parse` — the fail-closed reader for the on-disk text. The
  database is untrusted input (`AGENTS.md` §19.5/§19.6): every field is
  bounds- and shape-checked, usernames and uids must be unique, and a file
  the parser cannot fully understand yields **no** database.
- `UsersDb::authenticate` — verifies a `(username, password)` pair against
  the stored PBKDF2-HMAC-SHA256 record (`lib/crypto`), in constant time with
  respect to the stored hash, and returns one indistinguishable
  `AuthError::InvalidCredentials` whether the account is unknown, locked, or
  the password is wrong — an attacker cannot probe for valid usernames.
- `UsersDb::serialise` / `UserRecord::with_password` — the writer the
  installer (`AGENTS.md` §11) and the image builder (`tools/mkimage`) use to
  author the database; `parse(serialise(db))` round-trips exactly.
- `SESSION_BASELINE` / `ADMINISTRATIVE_SET` / `administrator_ceiling()` —
  the standard account grant sets (`plans/CAPABILITY_USE.md` §4.2/§4.3):
  the session baseline every interactive account's ceiling includes (and
  the shell's whole manifest request), and the administrative set that
  makes an account an administrator. Account policy is one definition
  beside the record that stores it, imported by the image builder, the
  disk-image test fixtures, and the kernel's program manifests — never
  copy-pasted.

A record carries the full §5.1 account identity: username, uid, primary gid,
supplementary gids, display name, home directory, the user's shell of
choice, the capability grant ceiling (`CAP_*` names, `AGENTS.md` §5.2), the
account state (`active`/`locked`), and the salted PBKDF2 password record
with its per-record cost.

## Why it lives in `lib/`

The database is written by the installer and `tools/mkimage`, and read by
the login path (`userland/session/login`) — independent consumers on both
sides of the user/kernel line, so the single definition belongs in `lib/*`
(`AGENTS.md` §2.2, §6). It sits above `lib/crypto` (the audited PBKDF2 and
constant-time comparison) and `lib/caps`/`lib/abi` (capability identity) and
depends on nothing else.

## Stability tier

`experimental` — the format version line (`rustos-users-v1`) carries the
version; until the first release the record shape evolves in place
(`AGENTS.md` §2.13). The crate is `no_std` + `alloc`. No `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths: parsing and authentication
are `Result`-based and total, and malformed input is rejected, never
guessed at (`AGENTS.md` §2.9, §5.4).
