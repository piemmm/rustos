# The capability lifecycle

How kernel class capabilities are granted, delegated, exercised, and
revoked across a real session. The staged build plan behind this page is
`plans/CAPABILITY_USE.md`; the per-task registry mechanics are
[Per-task capability registry](./captable.md).

## The layers

1. **Identity** — every process runs as a kernel-attested
   `(uid, gid, supplementary gids)` credential plus a capability record.
   The kernel supplies identity; a caller never does. `uid 0` carries no
   ambient power: powers come from capabilities, not from the uid.
2. **Class capabilities (`CAP_*`)** — the coarse "may use this subsystem
   at all" gates (`lib/abi/src/capability.rs`): `CAP_FS_ACCESS` admits a
   caller to the filesystem syscalls, `CAP_PROC_SPAWN` to `spawn`, and so
   on. They are checked at dispatch, before any state is touched, and
   confer no reach by themselves.
3. **Fine-grained authority** — the actual reach over any one object: the
   per-inode owner/mode/ACL/`required_cap` model for files, per-resource
   device grants for drivers, the per-fd descriptor table for streams. A
   `CAP_FS_ACCESS` holder is still refused any file the inode model
   denies.
4. **The intersection invariant** — a task's effective set is
   `user ceiling ∩ manifest request`, derived once by
   `TaskCapabilities::derive` at process admission and immutable for the
   life of the process. Neither side can widen the other.

## Where each ceiling comes from

- **System programs** (PID 1 `init` and every program the kernel launches
  before or outside an authenticated session) run as the system
  principal. There is no users-db row for it: the program's registered
  manifest *is* its ceiling, so the boot path passes the manifest as both
  derive bounds — the one legitimate manifest-as-ceiling shape.
- **User sessions** run under an authenticated account. The ceiling is
  the account's `capability_grants` field, stored in
  `/System/Security/Users` (`users-v1`, `lib/users`), verified and
  installed into the kernel `IdentityTable` at boot.
- **Driver processes** keep their own model: manifest-declared class
  capabilities plus per-node device-resource grants minted from the
  matched hardware-tree node.

## Delegation: spawn is the only transfer point

Authority is assigned exactly once, at process creation
(`KernelSpawnCtx::admit_process`); a running process can never change its
identity or grow its own set.

- **Inherit spawn** (no target uid): the child keeps the caller's
  credential **and the caller's stored user ceiling** — never the
  caller's narrower effective set. Its effective set is
  `child manifest ∩ that ceiling`, so a shell that launches `ps` hands it
  the account's ceiling and `ps` ends up with just what its own manifest
  requests within it. A **system-principal** caller
  (`TaskCapabilities::user_ceiling()` answers `None`) hands no account
  ceiling at all: PID 1's inherit-spawned boot services are system
  programs too, each bounded by its *own* registered manifest — login
  holds `CAP_USERS_READ`/`CAP_SPAWN_AS_USER` although init's manifest
  never did — and the shape propagates to their own children.
- **Spawn-as-user** (`CAP_SPAWN_AS_USER`, login only): the kernel
  resolves the target account's full credential **and ceiling** from the
  `IdentityTable` (`LateIdentity::resolve_credential`) and derives
  `child manifest ∩ target ceiling`. The caller chooses *which* account;
  it fabricates nothing. Before the table is installed the switch fails
  closed with `NotImplemented`; an unknown uid is denied with
  `PermissionDenied`, never a guessed identity.

Both shapes ride on `SpawnCredential`: the ceiling is an immutable
kernel-side snapshot on the task record, never a caller-supplied value,
so delegation can only narrow. Every derivation emits the
`TaskCapabilitiesDerived` audit event carrying the derived count.

## Exercise, release, revoke

- **Exercise.** Every syscall/IPC dispatch checks the caller's effective
  set before touching state; fine-grained authority is then checked by
  the owning layer. Holding a class capability is necessary, never
  sufficient.
- **Release.** A process's set is released implicitly and atomically at
  process exit. There is no runtime capability-drop syscall: sandboxed
  parsers get their narrow sets by being *spawned* narrow, which the
  manifest side of the intersection already expresses.
- **Revoke.** Editing an account's grant (or locking the account) takes
  effect at the **next spawn/login**; running processes keep the set they
  were derived with, exactly as POSIX processes keep their open fds. A
  compromised session that must die now is killed by an administrator,
  not live-revoked.

## The session baseline and the administrator

Every account that may start an interactive session is granted at least
`CAP_FS_ACCESS`, `CAP_PROC_SPAWN`, `CAP_CONSOLE_READ`, and
`CAP_CONSOLE_WRITE`; real reach stays per-inode and per-descriptor. An
administrator is an account whose grant additionally includes the
administrative capabilities (`CAP_USER_ADMIN`, `CAP_FS_MOUNT`,
`CAP_RLIMIT_RAISE`, `CAP_AUDIT_READ`, the global `CAP_SYSINFO_*` queries,
`CAP_TIME_SET`, `CAP_TIME_HIRES`) — no uid is special, there is no admin
flag, and group membership conveys file reach through ACLs, never class
capabilities. Driver-class and service-class capabilities
(`CAP_MEM_DMA`, `CAP_SPAWN_AS_USER`, …) are never part of any account
ceiling: they belong to the specific system program whose manifest
requests them.

Elevation is starting a new process under a more-privileged account
through the one `CAP_SPAWN_AS_USER` holder (login) after
re-authentication; there is no setuid and no "enter admin mode" for a
live process.

The two sets are policy with one definition: `lib/users` (the `grants`
module) defines `SESSION_BASELINE`, `ADMINISTRATIVE_SET`, and the
`administrator_ceiling()` union beside the account record that stores a
grant, and every author imports them — the image builder's debug `root`
account (`tools/mkimage::debug_users_db`) and the QEMU disk fixtures seed
exactly `administrator_ceiling()`, and the kernel's shell manifest
re-exports `SESSION_BASELINE` (`plans/CAPABILITY_USE.md` CU3). Every
other embedded program's manifest is sized to exactly the gated syscalls
it calls — one shared definition per program in the kernel's
`program_manifests` module, each list pinned by an exact-set unit test
(CU2). The whole lifecycle is proven end to end by the
`rustos-test-session-ceiling-qemu-aarch64` QEMU vertical: unlock, root
login, `cd`/`pwd`/`ps` under the seeded ceiling, and a `ulimit`
hard-bound raise refused because the shell's manifest does not request
`CAP_RLIMIT_RAISE` even though the account ceiling carries it.
