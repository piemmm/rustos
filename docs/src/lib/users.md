# `rustos-users`

The user-account database **and** the first-class group registry: the single
definition of a RustOS account, of a RustOS group, and of the versioned text
formats persisted at `/System/Security/Users` and `/System/Security/Groups`
(`AGENTS.md` §5.1, §16.2). The installer (`AGENTS.md` §11) and the image
builder (`tools/mkimage`) *author* the databases; the kernel's boot-time
root-volume read paths (`rustos_kernel_core::users::load_users_db` and
`rustos_kernel_core::groups::load_groups_db`, see the
[kernel page](../architecture/kernel.md)) and the login path
(`userland/session/login`) *read* them — one format each, defined once
(`AGENTS.md` §2.2). The crate is `no_std` + `alloc`, has no `unsafe`, and
depends only on `rustos-abi`, `rustos-caps`, and `rustos-crypto`.

## The format (`rustos-users-v1`)

Line one is exactly the header `rustos-users-v1`; every other line is
blank, a `#` comment, or one record of ten `:`-separated fields:

```text
rustos-users-v1
root:1000:1000::System Administrator:/Users/root:/System/Apps/elsh.app/Run:CAP_USER_ADMIN:active:pbkdf2-sha256$600000$<salt>$<hash>
devmgr:10:101::Device Manager:none:none:CAP_DRV_LOAD:nologin:*
```

A record carries the full §5.1 identity: username, uid, primary gid, the
comma-separated supplementary gids, a display name, the absolute home
directory, the user's **shell of choice** (the program their text session
runs), the comma-separated `CAP_*` capability grant ceiling (`AGENTS.md`
§5.2 — spelled with the canonical `abi-v1` names), the account state
(`active` / `locked` / `nologin`), and the stored password field.

The password field is `pbkdf2-sha256$<iterations>$<salt-hex>$<hash-hex>`:
a per-record 16-byte random salt and a PBKDF2-HMAC-SHA256 hash
(`lib/crypto`) at a per-record cost bounded into `1_000..=10_000_000`
(default `600_000`). The password itself is never stored. Alternatively
the field is the explicit `*` marker — the typed statement that the
account has no password and never authenticates (`StoredPassword`).

A `nologin` record is a system/service identity that never starts a
session (`plans/USERS.md`): it spells its absent home and shell as the
explicit `none` marker and its password as `*`, and the constructor and
parser both enforce the pairing — an `active`/`locked` account carries
all three real values, a `nologin` account none of them
(`ParseError::AccountShape`). No fake `/Users/<service>` path, no
throwaway hash that "should" never match: the record is structurally
incapable of a session, fail closed by construction.

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

## The group registry (`rustos-groups-v1`)

Groups are first-class objects (`AGENTS.md` §5.1): every group a user may
belong to is declared once in the group registry, by name and numeric gid.
Line one is exactly the header `rustos-groups-v1`; every other line is
blank, a `#` comment, or one record of two `:`-separated fields
`groupname:gid`:

```text
rustos-groups-v1
wheel:0
ada:1000
```

A group name obeys the same identifier grammar as a username
(`[a-z_][a-z0-9_-]*`, ≤ 32 bytes — one shared charset definition, never two
copies). **Membership is not stored here**: a user's primary and
supplementary groups live in that user's `UserRecord`
(`/System/Security/Users`), so a membership fact has a single home and the
two files can never disagree about who is in a group. The registry answers
only *which groups exist, and what they are called* — the authoritative set
every user's group references are checked against when the kernel assembles
its identity table (`rustos_kernel_core::groups::build_identity_table`): a
user naming a group with no registry record is refused, fail closed
(referential integrity, `AGENTS.md` §5.4).

One principal is kernel-defined rather than database-defined: the **system
principal** (`uid 0`). It exists before any table can be read (PID 1 and
the boot services load their `/System` store bundles before the encrypted
root is unlocked) and an installer image's table never defines it, so the
kernel's filesystem group resolution falls back to the capability-less
bootstrap identity (`gid 0`, no supplementary groups) whenever the table is
absent or holds no `uid 0` record — a table record for `uid 0` (the debug
image's seeded administrator) wins when present. The fallback grants no
ambient power (`AGENTS.md` §5.1): every per-inode owner/mode/ACL and
mount-flag check still applies, non-zero uids stay strictly fail-closed,
and a spawn-as-user *switch* always requires the installed table.

One group name is **well-known**: `STORAGE_GROUP` (`"storage"`, seeded as
gid `STORAGE_GID` = 100 by the image builder and the installer). At root
unlock the kernel resolves it **by name** from the loaded registry and
arms the removable-volume identity map with its gid
(`rustos_kernel::volume_policy`, `plans/DEVICES.md` D3d): a hotplug volume
whose filesystem stores no owner model (FAT32) then appears system-owned
under this group with group read/write, so any member uses the medium
without ambient authority. A registry without the group simply leaves
foreign volumes restrictively system-owned — the kernel never invents a
gid.

`GroupsDb::parse` is held to the identical fail-closed discipline as the
user database — it bounds the file (64 KiB), each line (128 bytes), and the
record count (1024) before reading anything, validates every field,
enforces group-name and gid uniqueness, and rejects the whole file on the
first defect — and its own deterministic fuzz harness
(`tests/fuzz_groups.rs`, enrolled in `cargo xtask fuzz`) drives it under the
never-panic + round-trip invariants.

## Authentication without information leaks

`UsersDb::authenticate(username, password)` exposes exactly one refusal,
`AuthError::InvalidCredentials`, whether the account is unknown, locked,
no-login, or the password is wrong (`AGENTS.md` §5.4). The stored-hash
comparison is constant-time (`lib/crypto`'s `ct_eq`, `AGENTS.md` §19.1),
and a refusal for an unknown, locked, or no-login account still pays one
PBKDF2 derivation at the highest cost any record carries (the default
cost when no record carries one), so response timing reveals neither
whether an account exists nor whether it holds a password at all. An
over-long password (> 256 bytes) is rejected without
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

The shared **account-authoring policy** lives beside the format so every
author agrees on it (`AGENTS.md` §2.2): `DEFAULT_SHELL` (the default
shell's store-bundle `Run` binary, drift-pinned to the `lib/abi` store
spellings), `default_home` (the §16 `/Users/<name>` layout), and the
range-aware `next_id` (auto-allocation of a free uid/gid inside a
reserved band: one above the highest taken id in the band, fail closed on
band exhaustion — ids below the current maximum are deliberately not
re-used, and an allocation never spills into the neighbouring band).
System uids/gids occupy `0..=999` (`IdRange::System`); interactive users
start at `FIRST_USER_UID`/`FIRST_USER_GID` = 1000 (`IdRange::User`). The
interactive `users` session, the one-shot `useradd`/`groupadd` command
apps (user range), and the image builder all import these definitions
rather than carrying private copies.

The canonical **default account set** (`plans/USERS.md`) is defined here
too: `default_system_accounts()` — the no-login `system` record (uid 0,
group `system` gid 0, empty ceiling; the *name* the loaded registry gives
the kernel's bootstrap identity) plus one no-login account per system
service (`devmgr` 10, `sysinfod` 11, `seatmgr` 12, `login` 13, primary
group `services` gid 101), each carrying exactly its own service's grant
ceiling (`DEVMGR_CEILING`, `SYSINFOD_CEILING`, `SEATMGR_CEILING`,
`LOGIN_CEILING`) so the §5.2 ceiling∩manifest intersection does real
work — and `default_groups()` (`system:0`, `services:101`,
`storage:100`). Every author of a fresh `/System/Security` pair imports
this one definition; the uid/gid constants seed provisioning only, and
runtime consumers resolve accounts and groups by name, failing closed
when a record is missing (the `storage` precedent).
