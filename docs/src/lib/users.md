# `tairix-users`

The user-account database **and** the first-class group registry: the single
definition of a TAIRiX account, of a TAIRiX group, and of the versioned text
formats persisted at `/System/Security/Users` and `/System/Security/Groups`
(`AGENTS.md` §5.1, §16.2). The installer (`AGENTS.md` §11) and the image
builder (`tools/mkimage`) *author* the databases; the kernel's boot-time
root-volume read paths (`tairix_kernel_core::users::load_users_db` and
`tairix_kernel_core::groups::load_groups_db`, see the
[kernel page](../architecture/kernel.md)) and the login path
(`userland/session/login`) *read* them — one format each, defined once
(`AGENTS.md` §2.2). The crate is `no_std` + `alloc`, has no `unsafe`, and
depends only on `tairix-abi`, `tairix-caps`, and `tairix-crypto`.

## The format (`tairix-users-v1`)

Line one is exactly the header `tairix-users-v1`; every other line is
blank, a `#` comment, or one record of ten `:`-separated fields:

```text
tairix-users-v1
root:1000:1000::System Administrator:/Users/root:/System/Commands/elsh.app/Run:CAP_USER_ADMIN:active:pbkdf2-sha256$600000$<salt>$<hash>
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

## The group registry (`tairix-groups-v1`)

Groups are first-class objects (`AGENTS.md` §5.1): every group a user may
belong to is declared once in the group registry, by name and numeric gid.
Line one is exactly the header `tairix-groups-v1`; every other line is
blank, a `#` comment, or one record of two `:`-separated fields
`groupname:gid`:

```text
tairix-groups-v1
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
its identity table (`tairix_kernel_core::groups::build_identity_table`): a
user naming a group with no registry record is refused, fail closed
(referential integrity, `AGENTS.md` §5.4).

The **system identity is compiled in, not stored on disk**
(`plans/USERS.md`): `system_accounts()` / `system_groups()` define the
OS-owned records — the no-login `system` account naming uid 0 plus one
no-login account per system service — as kernel policy, tamper-proof
exactly as the kernel text is. The kernel's sec boot phase builds and
installs that half into the live identity cell before any volume exists
(`tairix_kernel_core::system_identity_table`), so spawn-as-user and
filesystem group resolution for the system accounts work from first boot
on every architecture; the encrypted-root unlock later replaces the held
table with the merge of the same compiled half and the on-disk human
records (`build_identity_table`). The merge **fails closed on any on-disk
record that collides with the compiled identity** — a system-band uid or
gid (`IdRange::System`), a reserved account or group name, or a
repurposed storage-group pairing — so a tampered or misprovisioned volume
can never shadow, widen, or displace a system identity. The kernel's
filesystem group resolution still falls back to the capability-less
bootstrap identity (`gid 0`, no supplementary groups) for `uid 0` when no
table is installed (a host harness); the fallback grants no ambient power
(`AGENTS.md` §5.1): every per-inode owner/mode/ACL and mount-flag check
still applies, non-zero uids stay strictly fail-closed, and a
spawn-as-user *switch* always requires the installed table.

One group name is **well-known**: `STORAGE_GROUP` (`"storage"`, seeded as
gid `STORAGE_GID` = 100 by the image builder and the installer). At root
unlock the kernel resolves it **by name** from the loaded registry and
arms the removable-volume identity map with its gid
(`tairix_kernel::volume_policy`, `plans/DEVICES.md` D3d): a hotplug volume
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
built in memory. `tools/mkimage` uses exactly this path, seeding **human
accounts only** (the system identity is compiled into the kernel, never
written to disk): a **debug** image seeds the interactive `root`/`root`
bring-up administrator (uid `FIRST_USER_UID`, primary group `wheel` gid
`FIRST_USER_GID`); an **installer** image seeds an *empty* database (the
§11 installer authors the first human user on first boot). Both profiles
seed the storage group into the on-disk registry beside them.

The shared **account-authoring policy** lives beside the format so every
author agrees on it (`AGENTS.md` §2.2): `DEFAULT_SHELL` (the default
shell's store-bundle `Run` binary, drift-pinned to the `lib/abi` store
spellings), `default_home` (the §16 `/Users/<name>` layout), the home's
own layout (below), and the range-aware `next_id` (auto-allocation of a
free uid/gid inside a reserved band: one above the highest taken id in the
band, fail closed on band exhaustion — ids below the current maximum are
deliberately not re-used, and an allocation never spills into the
neighbouring band).
System uids/gids occupy `0..=999` (`IdRange::System`); interactive users
start at `FIRST_USER_UID`/`FIRST_USER_GID` = 1000 (`IdRange::User`). The
interactive `users` session, the one-shot `useradd`/`groupadd` command
apps (user range), and the image builder all import these definitions
rather than carrying private copies.

### The shape of a home

A home is not just its top directory. `HOME_SUBDIRS` is the fixed set the
installed-system contract requires inside `/Users/<name>` — `Apps`,
`Desktop`, `Documents`, `Library`, `Settings` (`AGENTS.md` §16.3) — and
`HOME_MODE` (`0o700`) is the owner-only mode the home and each of those
directories is stamped with, so an account's storage is private by
construction rather than by per-file hardening.

They are created **with the account**, not on first use, because the
per-user paths the system writes to sit one level deeper — a settings
store under `Settings/<App>/`, an app cache under `Library/<App>/`, the
user's own bundles under `Apps/` — and a writer that creates only its
immediate parent would fail on a brand-new account the first time anything
was saved. Every route that lays a home down reads this one definition:
the `CAP_USER_ADMIN` provisioning path (`RootAdminBacking::provision_home`,
which also fills in a missing directory on a later provisioning and never
rewrites what the account itself put there), the image builder's seeded
home, and the QEMU users-root fixture. Provisioning fills in the shape
only inside a home the account **owns**: an administrator pointing a new
account at an existing directory never has one principal's storage laid
out inside another's.

The compiled-in **system identity** (`plans/USERS.md`) is defined here
too: `system_accounts()` — the no-login `system` record (uid 0, group
`system` gid 0, empty ceiling; the *name* the kernel's bootstrap identity
resolves to) plus one no-login account per system service (`devmgr` 10,
`sysinfod` 11, `seatmgr` 12, `login` 13, `netstack` 14, `fontd` 15, primary
group `services` gid 101), each carrying exactly its own service's grant
ceiling (`DEVMGR_CEILING`, `SYSINFOD_CEILING`, `SEATMGR_CEILING`,
`LOGIN_CEILING`, `NETSTACK_CEILING`, `FONTD_CEILING`) so the §5.2
ceiling∩manifest intersection does real
work — and `system_groups()` (`system:0`, `services:101`). The kernel is
the sole consumer: the set is compiled into its identity table, never
authored to a volume. PID 1 resolves a startup-config account name onto
its uid through the pure, allocation-free `system_account_uid()` at
config-parse time and spawns each service with that concrete
`target_uid`; `is_system_account_name()` / `is_system_group_name()` back
the kernel merge's reserved-name refusals, and
`system_account_directory()` supplies the `(uid, username)` rows the
user-directory introspection lists ahead of the on-disk half.
