# USERS.md — Default system/service accounts and process-identity separation

This is the staged build plan for the default account set every image
carries and the identity separation of system processes and services:
system processes run as the `system` user (uid 0, group `system`),
each system service runs as its **own** dedicated user with primary
group `services`, and none of these accounts can log in or needs a home
directory.

`AGENTS.md` is binding — read it, `PLAN.md`, `plans/CAPABILITY_USE.md`
(grants/ceilings), and `plans/SPAWN.md` (the spawn syscall this builds
on) first. Every rule in this file is binding too. One fully-gated
increment (one `U`-stage) per landing.

Status: **planned** (no stage started).

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

2. **`system` is a locked, non-authenticating uid 0 record; the debug
   human account moves off uid 0.** Today the debug login `root` is
   uid 0 / gid 0 (`tools/mkimage::DEBUG_USERNAME`, `DEBUG_PRIMARY_GID`)
   — the same principal as the kernel's boot identity and the owner of
   `/System`. Instead: `system` (uid 0, primary group `system` gid 0)
   is a no-login record, and the debug account `root` becomes the first
   interactive user (uid 1000, primary group `root`/`wheel` gid 1000)
   with the administrator ceiling. Nothing is lost — powers come from
   capabilities, not uid (§5.1) — and §12 fixes only the debug
   account's *name/password* (`root`/`root`), not its uid. `/System`
   stays owned by uid 0, which then visibly resolves to `system` in
   `ls -l`, audit output, and `ps`.

3. **Reserved id ranges.** System uids/gids occupy 0–999; interactive
   users start at 1000. `lib/users/src/policy.rs` gains
   `FIRST_USER_UID` / `FIRST_USER_GID` constants and range-aware
   allocation (system-range for service accounts, user-range for
   `useradd`/the installer/the `users` app), replacing the current
   unpartitioned `next_id` (one-above-highest from 0). Allocation fails
   closed on range exhaustion, exactly as `next_id` already does at
   `u32::MAX`.

4. **No home, no shell — honestly absent, never faked.** `UserRecord`
   today makes `home` and `shell` mandatory absolute paths. The
   versioned, unfrozen format evolves in place (§2.13): a
   non-interactive account carries an explicit *none* spelling for
   both, and the parser enforces the pairing — an `Active`,
   login-capable account still requires both; a no-login account
   carries neither. No fake `/Users/system` directory, no dangling
   path.

5. **No password record either — a typed never-authenticates marker.**
   Every record today must carry a PBKDF2 `PasswordRecord`. A service
   account with a random throwaway hash is a lie waiting to be
   brute-forced or misread. The format gains an explicit typed
   "no password / never authenticates" marker (the principled `*` of
   `/etc/shadow`); `UsersDb::authenticate` treats it as an
   unconditional, timing-equalised refusal. Combined with a dedicated
   no-login `AccountState` variant (intent stated explicitly, not
   "administratively `Locked`"), a system/service account is
   structurally incapable of starting a session — fail closed by
   construction, not by configuration.

6. **The kernel's compiled-in bootstrap credential stays.** The boot
   floor's uid 0 / gid 0 / no-capabilities credential
   (`kernel/core/src/driver_store.rs::bootstrap_credentials`) cannot
   depend on the databases: they live on the volume the floor is trying
   to reach. The `system` record is merely the *name* the loaded
   registry later gives that identity — the same resolve-by-name
   pattern the `storage` group already uses (constants such as
   `SYSTEM_UID` / `SERVICES_GID` seed provisioning only; runtime
   consumers resolve by name against the loaded registry and fail
   closed if the record is missing).

7. **One provisioning definition, every author imports it (§2.2).** The
   canonical "default system accounts + groups" set is defined once in
   `lib/users` (beside `policy.rs` / `grants.rs`, which exist for
   exactly this reason) and imported by all three authors: `tools/
   mkimage` (both `ImageProfile`s), the installer's first-boot path
   (§11 — it authors the defaults automatically before prompting for
   the first human user), and the QEMU test fixtures
   (`tests/integration/rustfs_image`). Never three hand-maintained
   copies.

8. **Services are spawned as their own user via the existing ABI.**
   Identity is fixed at creation: `spawn` already takes a `target_uid`
   (`rustos_abi::SPAWN_UID_INHERIT` or a concrete uid), gated by
   `CAP_SPAWN_AS_USER`; there is no setuid-self. PID 1 `init` holds
   `CAP_SPAWN_AS_USER` and spawns each `/System/Services/<name>.app`
   with a concrete `target_uid` resolved **by name** from the loaded
   identity table, the name coming from the same startup config that
   already supplies the service's path — never a compiled-in kernel
   table (§16.5/§18 discovery discipline). A service account missing
   from the database is a fail-closed spawn refusal with a logged
   event (§19.4), never a silent fall-back to uid 0.

## 1. The default account set

Seeded identically by the debug image, the installer image, and the
installer's first-boot provisioning:

| Account | uid | primary group | state | home/shell | ceiling |
|---|---|---|---|---|---|
| `system` | 0 | `system` (gid 0) | no-login | none | empty |
| `devmgr` | system range (1–999) | `services` | no-login | none | devmgr's needs only |
| `sysinfod` | system range | `services` | no-login | none | sysinfo capabilities only |
| `seatmgr` | system range | `services` | no-login | none | seat capabilities only |
| `login` | system range | `services` | no-login | none | incl. `CAP_SPAWN_AS_USER` |
| `root` (debug profile only) | 1000 | `wheel` (gid 1000) | active | `/Users/root`, `DEFAULT_SHELL` | administrator ceiling |

Groups: `system:0`, `services:<SERVICES_GID>` (system range, following
the `storage:100` precedent in `lib/users/src/groups.rs::STORAGE_GID`),
`storage:100`, plus the debug account's primary group. `login` is the
instructive shape: it needs `CAP_SPAWN_AS_USER` (to drop the
authenticated session into the user) while itself being an unprivileged
no-login service account — authority from ceiling∩manifest, never from
identity.

## 2. Stages

- **U1 — `lib/users` format + policy (host-testable, the foundation).**
  The typed no-password marker; optional (none) home/shell with the
  parser-enforced pairing against account state; the no-login
  `AccountState` variant with `authenticate` refusing it
  timing-equalised; `FIRST_USER_UID`/`FIRST_USER_GID` and range-aware
  id allocation; the canonical default-accounts/groups provisioning
  definition; per-service grant-ceiling sets in `grants.rs`. Parser
  round-trip, authenticate-refusal, and range-allocation tests; rustdoc
  and the `docs/src/security/` page updated in the same change.
  Status: planned.

- **U2 — provisioning consumers.** `tools/mkimage` authors the U1
  default set for **both** profiles (debug additionally appends the
  uid-1000 `root`); the QEMU fixtures import the same definition; every
  test that assumes `root` is uid 0 is updated. The installer inherits
  the same definition when it lands (§11). Status: planned.

- **U3 — spawn-side wiring.** `init`'s startup config names each
  service's account; `init` resolves the uid by name from the loaded
  identity table and spawns with a concrete `target_uid`; missing
  account ⇒ fail-closed refusal + stable log event. Kernel identity-
  table build resolves `system`/`services` by name (storage-group
  pattern) and fails closed when absent. Status: planned.

- **U4 — ceiling tightening.** With each service running as itself,
  shrink each service account's ceiling to exactly its needs and assert
  the intersection in the per-service QEMU verticals (a service cannot
  exercise a sibling's capability). Status: planned.

Everything is pre-release in-place evolution (§2.13): no compatibility
shims, no dual formats — every consumer of a changed `lib/users` type
or database spelling is updated in the same change, and each stage ends
with the whole-workspace §7 validation gate.
