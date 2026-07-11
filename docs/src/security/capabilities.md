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
`CAP_FS_ACCESS`, `CAP_PROC_SPAWN`, `CAP_CONSOLE_READ`,
`CAP_CONSOLE_WRITE`, and the graphical-session class — `CAP_DISPLAY`,
`CAP_INPUT_READ`, and `CAP_SHM`, so a graphical login is an ordinary
session, not an administrative act; real reach stays per-inode,
per-descriptor, and per-lease (the kernel owner-gates every seat
acquire, input drain, and present against the live lease, and every
shared-memory region against its owner). An
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
live process. The per-invocation form is the shell's
`elevate <user> <program>` builtin (`plans/CAPABILITY_USE.md` CU5): it
posts one synchronous IPC call to its console's login supervisor over the
reserved per-console rendezvous (`lib/abi/src/elevate.rs`, derived from
the caller's kernel-attested `Origin::console`), the supervisor
re-authenticates the offered credentials with the same timing-equalised
authenticator as the login prompt and spawns the program as the target
account, and the shell blocks until it exits. The elevated child's set is
`its manifest ∩ the target account's ceiling`; the requesting shell's set
is untouched, refusal causes are audited but indistinguishable to the
requester, and binding the reserved rendezvous requires
`CAP_IPC_BIND_PRIVILEGED` so a squatter can never receive an elevation
request.

The two sets are policy with one definition: `lib/users` (the `grants`
module) defines `SESSION_BASELINE`, `ADMINISTRATIVE_SET`, and the
`administrator_ceiling()` union beside the account record that stores a
grant, and every author imports them — the image builder's debug `root`
account (`tools/mkimage::debug_users_db`) and the QEMU disk fixtures seed
exactly `administrator_ceiling()` (`plans/CAPABILITY_USE.md` CU3). The
baseline is a **ceiling**, never a program's manifest: the shell
requests its own exercised set (`SHELL_MANIFEST` — the console pair,
`CAP_FS_ACCESS`, `CAP_PROC_SPAWN`), the desktop session requests the
graphical class, and every
program's manifest is sized to every gated syscall the program has
a code path to issue — **including capability-gated optional features
that degrade gracefully when the intersection strips them** — and to
nothing it has no code path for (`plans/CAPABILITY_USE.md` §4.5, CU7).
The manifest describes what the code *can* do; the account ceiling
describes what the user *may* do; the intersection does the security
work, so requesting an optional privileged feature is safe by
construction. `top` requests `CAP_SYSINFO_KERNEL` (the memory summary
line) and `CAP_SYSINFO_GLOBAL` (the `a` system-wide toggle), `ps`
requests `CAP_SYSINFO_GLOBAL` (`-e`/`-A`), and `sysinfo` requests the
three global observability capabilities its query surface exercises: an
administrator's intersection arms these features, while a baseline
account's strips them and each tool reports the refusal and continues
with its self-scoped core function. Each list is one shared definition
per program in the kernel's `program_manifests` module, pinned by an
exact-set unit test, with the above-baseline subset of every session
tool additionally pinned as its own audited, reviewed set (CU2, CU7).
The whole lifecycle is proven end to end by the
`rustos-test-session-ceiling-qemu-aarch64` QEMU vertical: unlock, root
login, `cd`/`pwd`/`ps` under the seeded ceiling, and a `ulimit`
hard-bound raise refused because the shell's manifest does not request
`CAP_RLIMIT_RAISE` even though the account ceiling carries it.

## User management (`users_admin`, CU4)

A running system edits its accounts through one `CAP_USER_ADMIN`-gated,
audited syscall, `users_admin` (`lib/abi::users_admin`): a versioned,
typed request per operation — create/modify/delete an account,
lock/unlock, replace its grant ceiling or stored password record,
create/delete a group, or list either database's non-secret fields.
There is no raw-text edit path: the databases' salted password records
never leave the kernel (`CAP_USERS_READ` stays login's alone), and every
decision is audited per operation (`USER_ADMIN_APPLIED` /
`USER_ADMIN_REJECTED`, ids 4045/4046) with the caller's kernel-attested
uid.

The kernel engine (`kernel/core::useradmin::UserAdminEngine`) applies
each operation whole-or-not-at-all through one commit path:

- **Delegation narrows.** A grant edit may add only capabilities the
  *caller's own* effective set holds — an administrator cannot mint an
  account more powerful than themselves.
- **User management cannot be bricked.** The last active account holding
  `CAP_USER_ADMIN` can be neither deleted, locked, nor stripped of that
  grant.
- **The boot checks re-run.** Every candidate state passes the same
  `lib/users` validation and identity-table verification the boot load
  runs (group referential integrity included), and the serialised texts
  are bounded by the on-disk format maxima the next boot's parser
  enforces.
- **Disk first, then live.** The edited `users-v1`/`groups-v1` texts are
  persisted crash-safely to `/System/Security` (a temp node carrying the
  original's security record, renamed over it) through a dedicated
  writable window onto the encrypted root, and only then are the live
  users-database text and identity table swapped — so a change binds at
  the **next** spawn/login while running processes keep the sets they
  were derived with (the revoke model above). Creating an account also
  provisions its `/Users/<name>` home, owned by the new account,
  owner-only.

The first holder is the interactive `users` tool
(`userland/shell/users`, `/System/Apps/users.app/Run`), whose manifest requests
the console pair plus `CAP_USER_ADMIN` — deliberately above the session
baseline, so the intersection arms it only for an administrator account
and leaves it inert for everyone else. Passwords are hashed client-side
into salted PBKDF2 records (salt from the kernel CSPRNG via
`sys:random`), so no plaintext crosses the syscall boundary.
