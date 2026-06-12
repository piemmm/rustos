# `rustos-users`

The user-account database: the single definition of a RustOS account and of
the versioned text format persisted at `/System/Security/Users`
(`AGENTS.md` §5.1, §16.2). The installer (`AGENTS.md` §11) and the image
builder (`tools/mkimage`) *author* the database; the login path
(`userland/session/login`) *reads* it — one format, defined once
(`AGENTS.md` §2.2). The crate is `no_std` + `alloc`, has no `unsafe`, and
depends only on `rustos-abi`, `rustos-caps`, and `rustos-crypto`.

## The format (`rustos-users-v1`)

Line one is exactly the header `rustos-users-v1`; every other line is
blank, a `#` comment, or one record of ten `:`-separated fields:

```text
rustos-users-v1
root:0:0::System Administrator:/Users/root:/Apps/Shell.app/Run:CAP_USER_ADMIN:active:pbkdf2-sha256$600000$<salt>$<hash>
```

A record carries the full §5.1 identity: username, uid, primary gid, the
comma-separated supplementary gids, a display name, the absolute home
directory, the user's **shell of choice** (the program their text session
runs), the comma-separated `CAP_*` capability grant ceiling (`AGENTS.md`
§5.2 — spelled with the canonical `abi-v1` names), the account state
(`active` / `locked`), and the stored password record.

The password record is `pbkdf2-sha256$<iterations>$<salt-hex>$<hash-hex>`:
a per-record 16-byte random salt and a PBKDF2-HMAC-SHA256 hash
(`lib/crypto`) at a per-record cost bounded into `1_000..=10_000_000`
(default `600_000`). The password itself is never stored.

## Fail-closed parsing

The database text is untrusted input (`AGENTS.md` §19.5/§19.6):
`UsersDb::parse` bounds the file (64 KiB), each line (512 bytes), and the
record count (512) before reading anything; validates every field's length,
charset, and shape; enforces username and uid uniqueness; and rejects the
whole file on the first defect (`ParseError`). `UsersDb::serialise` emits
text that parses back to an equal database, and the deterministic fuzz
harness (`tests/fuzz_users.rs`, enrolled in `cargo xtask fuzz`) drives the
parser with mutated, truncated, spliced, and noise inputs under the
never-panic + round-trip invariants.

## Authentication without information leaks

`UsersDb::authenticate(username, password)` exposes exactly one refusal,
`AuthError::InvalidCredentials`, whether the account is unknown, locked, or
the password is wrong (`AGENTS.md` §5.4). The stored-hash comparison is
constant-time (`lib/crypto`'s `ct_eq`, `AGENTS.md` §19.1), and a refusal
for an unknown or locked account still pays one PBKDF2 derivation at the
database's highest record cost, so response timing does not reveal whether
an account exists. An over-long password (> 256 bytes) is rejected without
deriving anything — a work-factor bound, not a capacity (`AGENTS.md`
§24.4).

## Authoring

`UserRecord::with_password` hashes a fresh password under a caller-supplied
random salt (the crate stays deterministic; entropy belongs to the caller),
and `UsersDb::new` enforces the whole-database invariants over records
built in memory. `tools/mkimage` uses exactly this path: a **debug** image
seeds the single `root`/`root` bring-up account, an **installer** image
seeds no accounts at all (the §11 installer authors the database on first
boot).
