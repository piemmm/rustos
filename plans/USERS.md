# USERS.md — Default system/service accounts and process-identity separation

This is the staged build plan for the OS-owned account set and the
identity separation of system processes and services: system processes
run as the `system` user (uid 0, group `system`), each system service
runs as its **own** dedicated user with primary group `services`, and
none of these accounts can log in or needs a home directory.

`AGENTS.md` is binding — read it, `PLAN.md`, `plans/CAPABILITY_USE.md`
(grants/ceilings), and `plans/SPAWN.md` (the spawn syscall this builds
on) first. Every rule in this file is binding too. One fully-gated
increment (one `U`-stage) per landing.

Status: **U1–U4 done**.

---

## 0. Scope and binding decisions

1. **One user per service, one shared `services` group — never a shared
   service user.** `devmgr`, `sysinfod`, `seatmgr`, `login` each get
   their own uid, all with primary group `services`. The uid is what
   §19.4 per-service log partitioning, IPC peer attestation, and
   blast-radius containment key off; a single shared service identity
   would defeat all three. The shared group exists only for common read
   access to service-facing paths. Each service account's capability
   ceiling (`lib/users/src/grants.rs`) holds only that service's needs,
   so the §5.2 ceiling∩manifest intersection does real work: a
   compromised service cannot borrow a sibling's authority even if its
   manifest lied.

2. **The system identity is compiled into the kernel — never stored on,
   or read from, a volume.** The OS-owned accounts (`system`, `devmgr`,
   `sysinfod`, `seatmgr`, `login`) and groups (`system`, `services`) are
   kernel policy, defined once in `lib/users/src/provision.rs`
   (`system_accounts()` / `system_groups()`, built from one const spec
   table) and compiled into the kernel's identity table. Rationale:
   - **Tamper-proof by construction.** The records are protected exactly
     as the kernel text is; there is no on-disk artefact to corrupt,
     replace, or shadow. (The service *ceilings* are OS policy tied to
     the shipped binaries — like the compiled-in program manifests —
     not admin-editable data.)
   - **Available from first boot on every architecture.** The users
     volume is passphrase-encrypted and unlocks interactively, but
     services must start (and drivers autoload — the discovered-tier
     input driver *types* that passphrase, `plans/DEVICES.md`) before it
     is readable. Storing service identity there would deadlock the
     boot; compiling it in resolves the ordering with no waiting at all.
   - The kernel's `sec` boot phase builds and verifies the compiled
     table (`rustos_kernel_core::system_identity_table`) and installs it
     into the port's identity cell (`BootInfo::with_spawn_identity`,
     wired by every port boot), so spawn-as-user and filesystem group
     resolution for system accounts work before any volume exists.

3. **The on-disk databases hold human accounts only, inside the user
   band — enforced fail-closed at the identity merge.** The
   encrypted-root unlock replaces the boot table with the merge of the
   compiled half and the loaded `/System/Security/{Users,Groups}`
   records (`rustos_kernel_core::build_identity_table`, also the
   `users_admin` engine's re-verification path). The merge refuses any
   on-disk user outside `IdRange::User` (uid ≥ `FIRST_USER_UID` = 1000)
   or carrying a reserved account name, and any on-disk group outside
   the user band or carrying a reserved group name — so a tampered or
   misprovisioned volume can never shadow, widen, or displace a system
   identity. The sole system-band on-disk record is the well-known
   removable-storage group, pinned to its exact `storage:100` pairing
   (storage membership is admin-managed data about human accounts,
   `plans/DEVICES.md` D3d); either half of that pairing repurposed is
   refused.

4. **No home, no shell — honestly absent, never faked.** A
   non-interactive account carries the explicit `none` spelling for
   both `home` and `shell`, and the constructor and parser enforce the
   pairing — an `Active`/`Locked`, login-capable account requires both;
   a no-login account carries neither. No fake `/Users/system`
   directory, no dangling path.

5. **No password record either — a typed never-authenticates marker.**
   A service account with a random throwaway hash would be a lie
   waiting to be brute-forced or misread. The record carries the
   explicit typed "no password / never authenticates" marker (the
   principled `*` of `/etc/shadow`); `UsersDb::authenticate` treats it
   as an unconditional, timing-equalised refusal. Combined with the
   dedicated no-login `AccountState` variant, a system/service account
   is structurally incapable of starting a session — fail closed by
   construction, not by configuration.

6. **The kernel's compiled-in bootstrap credential stays.** The boot
   floor's uid 0 / gid 0 / no-capabilities credential
   (`kernel/core/src/driver_store.rs::bootstrap_credentials`) is the
   identity PID 1 and the boot readers run under; the compiled `system`
   record is merely the *name* that identity resolves to in listings
   and audit output, carrying the same gid 0 and an empty ceiling.

7. **PID 1 resolves service accounts at config-parse time and spawns
   with a concrete `target_uid`.** The startup config names each
   entry's account (`service <path> <account>` / `session <path>
   <account>`); the parser resolves the name through the pure,
   allocation-free `rustos_users::system_account_uid()` and rejects the
   whole config on an unknown or missing account
   (`ConfigError::UnknownAccount` / `MissingAccount`) — nothing spawns
   from a config whose identities cannot all be resolved. No syscall,
   no waiting, no compiled-in kernel table of services: the account
   name lives in the same config that names the service's path. `init`
   holds `CAP_SPAWN_AS_USER` (its manifest:
   `kernel/rustos-kernel/src/program_manifests.rs::INIT_MANIFEST`), and
   the kernel resolves each switch's group set and capability ceiling
   from the boot-installed identity table, failing closed on an
   unresolvable uid (`resolve_spawn_credential`). The login *session*
   is account-named too: `login` runs as its own unprivileged service
   account and uses its ceiling's `CAP_SPAWN_AS_USER` to drop the
   authenticated session into the target user — authority from
   ceiling∩manifest, never from identity.

8. **The user directory lists both halves.** The ungated
   `USER_DIRECTORY` introspection serves the compiled `(uid, username)`
   rows first (`rustos_users::system_account_directory()`), then the
   on-disk human records, so `ls -l`/`ps`/`top` render system-account
   names with no volume mounted and nothing is fabricated beyond the
   two real halves. The `users_admin` `ListUsers` view remains the
   *editable* (on-disk, human) set only.

## 1. The account set

Compiled into the kernel (`lib/users/src/provision.rs`):

| Account | uid | primary group | state | home/shell | ceiling |
|---|---|---|---|---|---|
| `system` | 0 | `system` (gid 0) | no-login | none | empty |
| `devmgr` | 10 | `services` (gid 101) | no-login | none | devmgr's needs only |
| `sysinfod` | 11 | `services` | no-login | none | sysinfo capabilities only |
| `seatmgr` | 12 | `services` | no-login | none | seat capabilities only |
| `login` | 13 | `services` | no-login | none | incl. `CAP_SPAWN_AS_USER` |

Seeded on disk (human data): the debug image's `root` (uid
`FIRST_USER_UID` = 1000, primary group `wheel` gid `FIRST_USER_GID` =
1000, administrator ceiling, `tools/mkimage::DEBUG_UID`); the installer
image seeds an empty users database (the first human user is a
first-boot job). Both profiles seed the `storage:100` group. `login` is
the instructive shape: it needs `CAP_SPAWN_AS_USER` (to drop the
authenticated session into the user) while itself being an unprivileged
no-login service account.

## 2. Stages

- **U1 — `lib/users` format + policy (host-testable, the foundation).**
  Status: **done**. What now holds:
  - `StoredPassword::{Password, NeverAuthenticates}` (on-disk `*`,
    `NO_PASSWORD_MARKER`); `AccountState::NoLogin` (`nologin`); optional
    home/shell spelled `none` (`NO_PATH_MARKER`). Constructor and parser
    enforce the pairing (`ParseError::AccountShape`): active/locked ⇒
    home+shell+password, nologin ⇒ none of them. `UsersDb::authenticate`
    refuses a no-login account timing-equalised (the dummy-derivation
    burn covers records without a cost).
  - Range-aware `next_id(IdRange::{System,User}, …)` +
    `FIRST_USER_UID`/`FIRST_USER_GID` (§0.3); `useradd`/`groupadd`
    allocate from the user band.
  - Per-service ceilings in `grants.rs` (`DEVMGR_CEILING`,
    `SYSINFOD_CEILING`, `SEATMGR_CEILING`, `LOGIN_CEILING`,
    `NETSTACK_CEILING`), pinned and sibling-disjoint by test;
    `capability_set` builds the stored set.
  - The `users_admin` `ListUsers` entry reports the truthful tri-state
    (`AccountStateCode::{Active, Locked, NoLogin}`, fail-closed decode)
    and spells absent home/shell as `none`; the `users` tool renders all
    three states.

- **U2 — the compiled system identity and human-only provisioning.**
  Status: **done** (reworked in place by U3's design decision, §2.13).
  What now holds:
  - `lib/users/src/provision.rs` defines the compiled identity from one
    const spec table: `system_accounts()`, `system_groups()`, the
    alloc-free `system_account_uid()` name→uid lookup, the
    `is_system_account_name()`/`is_system_group_name()` reserved-name
    guards, and `system_account_directory()` — all pinned by tests
    (record set, ceilings, lookup↔records agreement).
  - `tools/mkimage` seeds human accounts only: debug ⇒ `root` + `wheel`
    + `storage`; installer ⇒ empty users database + `storage`. The QEMU
    disk fixture (`tests/integration/rustfs_image`) seeds the same
    human-only shape; both are pinned by tests.

- **U3 — spawn-side wiring.** Status: **done**. What now holds:
  - The kernel `sec` phase builds, verifies, and installs the compiled
    identity into the port's cell on every architecture
    (`kernel/core/src/init.rs`; `BootInfo.spawn_identity` is
    `Option<&'static LateIdentity>`, wired by the aarch64/riscv64/x86_64
    boots to `root_mount::LATE_IDENTITY`). The unlock **replaces** the
    held table with the `build_identity_table` merge; the `users_admin`
    engine's edits re-verify through the same merge, so the band/name
    enforcement of §0.3 binds every path that can publish a table.
  - `init`'s startup config names each entry's account and the parser
    resolves it at parse time (§0.7); the supervisor's slots carry the
    uid and every launch/relaunch spawns through `spawn_as` with the
    slot's concrete `target_uid` — services and the login session run
    as their own accounts from their first instruction.
  - `INIT_MANIFEST` += `CAP_SPAWN_AS_USER` (pinned); the user directory
    lists compiled + human halves with stable cross-half paging (§0.8).

- **U4 — ceiling tightening.** Status: **done**. What now holds:
  - Each service account's ceiling (`lib/users/src/grants.rs`) is exactly
    its service's needs — identical to the service's pinned manifest — so
    there was no slack to shrink; the U4 deliverable is the enforcement
    proof that the ceiling *binds* a manifest that lies.
  - Host: `spawn_as_a_service_account_strips_every_siblings_defining_capability`
    (`kernel/core/src/syscalls.rs`) drives the real spawn handler over the
    real compiled identity table for all four service accounts, each under
    a manifest padded with every sibling's defining capability
    (`CAP_DRV_LOAD`, `CAP_SYSINFO_INTROSPECT`, `CAP_SEAT_ADMIN`,
    `CAP_SPAWN_AS_USER`+`CAP_USERS_READ`): the child's effective set keeps
    the account's own ceiling and strips every borrowed sibling grant.
  - QEMU: the `service_ceiling_qemu_aarch64` vertical (fixture:
    `service_ceiling_program`) spawns a program **as `devmgr`** through the
    production spawn syscall under a deliberately over-wide registered
    manifest (devmgr's ceiling ∪ the sibling defining capabilities, const-
    concatenated from the one `DEVMGR_CEILING` definition), with the real
    compiled identity table installed. Running as devmgr, its own
    `SYSINFO_HW`-gated `hw_tree_read` succeeds while `spawn_as`,
    `users_db_read`, `seat_switch`, and `sysinfo_introspect` are each
    refused `PermissionDenied` at the audited dispatcher gate — a
    compromised service cannot borrow a sibling's authority even when its
    manifest lies.

Everything is pre-release in-place evolution (§2.13): no compatibility
shims, no dual formats — every consumer of a changed `lib/users` type
or database spelling is updated in the same change, and each stage ends
with the whole-workspace §7 validation gate.
