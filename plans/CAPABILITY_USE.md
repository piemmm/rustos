# CAPABILITY_USE.md — The capability lifecycle: how a user actually uses TAIRiX

This is the binding specification and staged build plan for how kernel
capabilities are **granted, delegated, exercised, and revoked** across a real
session: logging in, navigating the filesystem, running programs, reading and
writing files, administering the system, and (later) using the desktop.
`AGENTS.md` is binding — read it, `PLAN.md`, `plans/SPAWN.md`,
`plans/SHELL.md`, and `.junie/PREREQUISITES2.md` first. Every rule in this
file is binding too. One fully-gated increment (one `CU`-stage) per landing.

**Note:** `abi-v1` is *not* frozen — the standing task direction supersedes
the `AGENTS.md`/`PLAN.md` "frozen" language. Capability-model changes are
made **in place** (`AGENTS.md` §2.13), with the C header regenerated
(`cargo xtask c-header --write`) where `lib/abi` types change.

## Motivating defect

On the debug image (`root`/`root`), login succeeds but the session is
useless: every `ls`, `cd`, file open, and program launch fails with
`PermissionDenied`. The root cause is threefold (§3 below):

1. *(fixed — CU1)* The kernel already had the right primitive —
   `TaskCapabilities::derive` (`kernel/sec/src/captable.rs`) computes
   *effective = user grant ∩ manifest request* — but every runtime spawn fed
   the program manifest as both sides, so the per-account grant stored in
   `/System/Security/Users` (`users-v1` `capabilities` field, `lib/users`)
   was dead data. The ceiling is now threaded through spawn (CU1).
2. *(fixed — CU2)* The shell's registered manifest requested only
   `CAP_CONSOLE_WRITE` + `CAP_CONSOLE_READ` — no `CAP_FS_ACCESS`, no
   `CAP_PROC_SPAWN` — so the spawned shell could not touch the filesystem
   or launch anything. The shell now requests its own exercised set,
   `SHELL_MANIFEST`: the console pair plus `CAP_FS_ACCESS` and
   `CAP_PROC_SPAWN` (`kernel/tairix-kernel/src/program_manifests.rs`,
   CU2; decoupled from the baseline when CU6 widened the ceiling with
   the graphical-session class the shell never exercises).
3. *(fixed — CU3)* The debug root account's seeded grant (`tools/mkimage`)
   omitted `CAP_FS_ACCESS`, so even with the intersection wired the shell
   was still denied the filesystem. The seeded grant is now the shared
   administrator ceiling (`tairix_users::administrator_ceiling`, CU3).

This plan defines the lifecycle properly, then fixes the defect through it —
never by widening a check or special-casing `uid 0` (`AGENTS.md` §2.17,
§15.9).

## Status legend

`planned` / `in progress` / `done`, per `AGENTS.md` §13.

---

## 1. The model (normative recap)

TAIRiX authority is layered. Nothing here is new; this section fixes the
vocabulary the rest of the plan uses.

1. **Identity** — every process runs as a kernel-attested
   `(uid, gid, supplementary gids)` credential plus a capability record.
   The kernel supplies identity; a caller never does. `uid 0` carries **no**
   ambient power: powers come from capabilities, not from the uid
   (`AGENTS.md` §5.1).
2. **Class capabilities (`CAP_*`)** — the coarse "may use this subsystem at
   all" gates (`lib/abi/src/capability.rs`): `CAP_FS_ACCESS` admits a caller
   to the filesystem syscalls, `CAP_PROC_SPAWN` to `spawn`, and so on. They
   are checked before any state is touched — at dispatch, or as the
   handler's first act where *which* capability is required depends on the
   request's own content (`stream_write`'s console arm; `spawn`, which a
   canonical parser-sandbox block lets the narrow `CAP_SANDBOX_SPAWN`
   admit and anything else needs `CAP_PROC_SPAWN` for) — and they confer no
   reach by themselves.
3. **Fine-grained authority** — the actual reach over any one object:
   the per-inode owner/mode/ACL/`required_cap` model for files
   (`AGENTS.md` §5.3), per-endpoint/per-region/per-window device-resource
   grants for drivers, the per-fd descriptor table for streams. A
   `CAP_FS_ACCESS` holder is still refused any file the inode model denies.
4. **The intersection invariant** — a task's effective class-capability set
   is `user grant ∩ manifest request` (`AGENTS.md` §5.2), derived once by
   `TaskCapabilities::derive` and immutable for the life of the process.
   The **user grant** is the account's ceiling (the `capability_grants`
   field of the kernel `IdentityTable` record, sourced from
   `/System/Security/Users`); the **manifest request** is what the program
   declares it needs (today the `EmbeddedProgram` registry entry; later the
   signed `rxe`/`AppInfo` manifest). Neither side can widen the other.
5. **No setuid-self, no runtime raise** — a running process can never
   change its own identity or grow its own capability set. The only place
   authority is assigned is **process creation**; the only privileged
   identity transition is a `CAP_SPAWN_AS_USER` spawn (PID 1 switching
   the boot services and the login session onto their own service
   accounts, and login dropping the authenticated session into the target
   user — `plans/USERS.md`), which resolves the target credential from
   the kernel identity table.
6. **Fail closed, audit the decision** — every denial is a typed
   `PermissionDenied`, never a fallback, and every security-relevant grant
   or refusal is logged with a stable event ID (`AGENTS.md` §5.4, §19.4).

---

## 2. The capability lifecycle (binding)

The life of a capability, from disk to exercise to revocation:

1. **Grant (at rest).** A *human* account's ceiling lives in its
   `/System/Security/Users` record (`users-v1` `capabilities` field),
   authored by the image builder (debug), the installer (first user), or a
   `CAP_USER_ADMIN` holder (user management, CU4). A *system/service*
   account's ceiling is compiled into the kernel with its record
   (`tairix_users::system_accounts`, `plans/USERS.md`) — OS policy,
   tamper-proof as the kernel text, never volume data. The *system*
   principal (PID 1 `init`) has no account ceiling; its manifest is its
   ceiling (§4.1).
2. **Install (at boot + unlock).** The kernel's `sec` boot phase builds,
   verifies, and installs the compiled system identity into the live
   `LateIdentity` cell before any volume exists; the encrypted-root unlock
   then replaces the held table with the verified merge of that compiled
   half and the on-disk human records — refusing, fail-closed, any on-disk
   record that collides with the compiled identity (a system-band id or a
   reserved name, `plans/USERS.md`). With no table installed every
   identity resolution fails closed.
3. **Delegate (at spawn).** Spawn is the **only** delegation point:
   - *Inherit spawn* (no target uid): the child keeps the caller's
     credential **and the caller's user ceiling**; its effective set is
     `child manifest ∩ that ceiling`. A shell that launches `ps` hands it
     the user's ceiling, and `ps` ends up with just what its own manifest
     requests within it — never the shell's effective set, never more.
   - *Spawn-as-user* (`CAP_SPAWN_AS_USER` — PID 1 for the boot services
     and the login session, login for the authenticated user's session):
     the kernel resolves the target account's full credential **and
     ceiling** from the `IdentityTable` and derives `child manifest ∩
     target ceiling`. The caller chooses *which* account; it fabricates
     nothing.
   Delegation can only narrow (`AGENTS.md` §5.2): the ceiling travels with
   the credential as an immutable kernel-side snapshot on the task record,
   never a caller-supplied value.
4. **Exercise.** Every syscall/IPC dispatch checks the caller's *effective*
   set before touching state; fine-grained authority (inode model, resource
   grants) is then checked by the owning layer. Holding a class capability
   is necessary, never sufficient.
5. **Release.** A process's class-capability set is released implicitly and
   atomically at process exit; there is no partial release. A deliberate
   runtime *drop* ("give up `CAP_X` before parsing untrusted input") is a
   sound future narrowing, but it has no consumer today and is **not**
   invented ahead of one (`AGENTS.md` §2.3, §5.2) — sandboxed parsers get
   their narrow sets by being *spawned* narrow (§19.5), which the manifest
   side of the intersection already expresses.
6. **Revoke.** Editing an account's grant (or locking the account) in the
   users database takes effect at the **next spawn/login** — running
   processes keep the set they were derived with, exactly as POSIX processes
   keep their open fds. Live revocation of a *fine-grained* grant (a seat, a
   device window) is the owning subsystem's job (`plans/DISPLAY.md`,
   `hw_remove_node`), not a class-capability concern. If a compromised
   session must die *now*, the answer is `signal`/kill by an administrator,
   not a novel live-revocation mechanism (declined as speculative surface).

---

## 3. Current state (what exists, what is broken)

**Exists and is correct:**

- The 32-capability `abi-v1` vocabulary with frozen names/ids
  (`lib/abi/src/capability.rs`), the 256-bit `CapabilitySet` (`lib/caps`).
- `TaskCapabilities::derive(user_grant, manifest_request)` with private
  fields, the intersection invariant, and its audit event (`kernel/sec`).
- The `users-v1` on-disk account record carrying a per-account
  `CapabilitySet` grant (`lib/users`), verified and installed into the
  kernel `IdentityTable` (whose `UserRecord.capability_grants` documents
  itself as "the maximum capability set this user may ever exercise").
- `CAP_SPAWN_AS_USER` held only by PID 1 and login (and `CAP_USERS_READ`
  only by login); `resolve_credential` resolving the target's groups
  **and capability ceiling** from the table, fail closed with no table /
  on unknown uid.
- Per-inode owner/mode/ACL enforcement under kernel-attested credentials in
  the secured VFS; capability-gated dispatch on every syscall.
- The user ceiling threaded through spawn (CU1): `SpawnCredential` carries
  the account's `capability_grants` snapshot and
  `KernelSpawnCtx::admit_process` derives `ceiling ∩ manifest` — B1 is
  fixed.
- The audited, pinned per-program manifests — the shell's own exercised
  set (`SHELL_MANIFEST`), sized per §4.5, among them
  (`kernel/tairix-kernel/src/program_manifests.rs`, CU2) — B2 is fixed.
- The §4.2/§4.3 sets defined once in `lib/users` (`grants`:
  `SESSION_BASELINE`, `ADMINISTRATIVE_SET`, `administrator_ceiling()`)
  and seeded as the debug root grant (`tools/mkimage`'s profile-keyed
  `users_db`) and the QEMU users-root fixture account — B3 is fixed (CU3).

All three defects are fixed; user management (CU4) and per-invocation
elevation (CU5) are live; the desktop's session/ceiling slice (CU6) is
live with `plans/DISPLAY.md` D7d, and its picker-issued one-shot
descriptors landed with `plans/APPWIN.md` AW5.

---

## 4. Binding design decisions

### 4.1 Principals and where each ceiling comes from

- **System programs** (PID 1 `init`, `login`, `devmgr`, `sysinfod` — every
  program the kernel launches *before or outside* an authenticated session)
  run as the system principal. Their ceiling **is** their registered
  manifest: the boot path passes the manifest as both sides of `derive`,
  which is correct *for them* — there is no users-db row for the system
  principal, and inventing one would add an unauditable ambient identity.
  This is the one legitimate `derive(caps, caps)` shape.
- **User sessions** (the shell login spawns, and everything below it) run
  under an authenticated account. Their ceiling is the account's
  `capability_grants` snapshot, resolved by the kernel at the
  spawn-as-user switch and inherited by every descendant (§2.3).
- A **driver process** keeps its existing model: manifest-declared class
  capabilities plus per-node device-resource grants minted from its matched
  hardware node — nothing in this plan touches that path.

### 4.2 The session baseline (what every interactive account gets)

Every account that may start an interactive session is granted at least:

| Capability | Why it is baseline |
|---|---|
| `CAP_FS_ACCESS` | "May use the filesystem at all." Real reach stays per-inode (§5.3): home is writable, `/System` is not. |
| `CAP_PROC_SPAWN` | "May run programs at all." What runs is bounded by the program registry / bundle store; the child is bounded by its own manifest ∩ this same ceiling. |
| `CAP_CONSOLE_READ`, `CAP_CONSOLE_WRITE` | An interactive session's streams are console-backed; the fine authority stays the inherited descriptor table. |
| `CAP_DISPLAY`, `CAP_INPUT_READ`, `CAP_SHM` | The graphical-session class (§4.6, `plans/DISPLAY.md` D7): acquiring a seat's exclusive revocable lease, draining the *owned* seat's input, and creating/granting the zero-copy frame region. A graphical login is an ordinary session; the kernel still owner-gates every acquire, drain, and present against the live lease, and every region against its owner. |

Nothing else is baseline. Self-scoped `sysinfo` queries, `resource_open` of
`sys:*`, `stream_*` on inherited pipes/files, lowering one's own rlimits,
and `fs_getcwd` already require no capability, so an unprivileged account
with only the baseline can do everything an ordinary user expects — and a
*sandboxed* process (a parser) still gets **none** of the baseline, because
its manifest requests none of it.

### 4.3 The administrator (what "admin" means)

An administrator is **an account whose grant includes administrative
capabilities** — nothing more. No uid is special, there is no admin flag,
and there is no wheel-group backdoor: membership in a group conveys file
reach through ACLs, never class capabilities. The administrative set, on
top of the session baseline:

| Capability | Administrative power |
|---|---|
| `CAP_USER_ADMIN` | Create/modify/delete/lock accounts and edit grants (CU4). |
| `CAP_FS_MOUNT` | Mount/unmount volumes; relax per-mount flags is its own gate. |
| `CAP_RLIMIT_RAISE` | Raise hard resource limits above an inherited ceiling. |
| `CAP_AUDIT_READ` | Read the hash-chained security audit log. |
| `CAP_SYSINFO_GLOBAL`, `CAP_SYSINFO_KERNEL`, `CAP_SYSINFO_HW` | System-wide observability (all processes, kernel memory, hardware tree). |
| `CAP_TIME_SET` | Adjust the wall clock. |
| `CAP_TIME_HIRES` | Full-resolution monotonic clock (diagnostics/profiling). |
| `CAP_MEM_PIN` | Exempt a process's anonymous memory from the swap tiers (`mem_pin`, bounded by the `pinned-memory-bytes` limit; `plans/STRESSTEST.md` ST2). |

The **debug image's root account** is seeded with exactly the session
baseline plus this administrative set — the "administrative capability
ceiling a bring-up session needs" its rustdoc already promises. Driver-class
(`CAP_MEM_DMA`, `CAP_IRQ_BIND`, `CAP_MMIO_MAP`, `CAP_HW_EMIT`, …) and
service-class (`CAP_SPAWN_AS_USER`, `CAP_USERS_READ`,
`CAP_SYSINFO_INTROSPECT`, `CAP_INPUT_INJECT`, `CAP_SANDBOX_SPAWN`, …)
capabilities are **never**
part of an *interactive* account ceiling: they belong to the specific system
program whose manifest requests them — and, through that service's own
no-login account (`plans/USERS.md`), to its dedicated per-service ceiling
(`tairix_users::{DEVMGR_CEILING, SYSINFOD_CEILING, SEATMGR_CEILING,
LOGIN_CEILING, NETSTACK_CEILING}`), which holds exactly that one service's needs so the
ceiling∩manifest intersection does real work. An administrator administers
the system; they do not impersonate its services.

### 4.4 Elevation (running with more than your ceiling)

There is no setuid, no self-elevation, and no "enter admin mode" for a live
process. Elevation is **starting a new process under a more-privileged
account, through the one spawn-as-user holder, after re-authentication**:

- Day 1 (after CU1–CU3): elevation is *logging in as an administrator
  account* — on another console, or by exiting the session. The debug image
  has exactly one account and it is the administrator, so bring-up needs
  nothing further.
- CU5 adds the deliberate per-invocation form: an `elevate <user> <program>`
  request the shell forwards to its console's session supervisor (login),
  which **re-authenticates the target account's credentials** and spawns
  the program as that account — the same `CAP_SPAWN_AS_USER` +
  `CAP_USERS_READ` path as a fresh login, one more caller of the existing
  gates, no new capability. The spawned command's set is still
  `its manifest ∩ the target account's ceiling`; the requesting shell gains
  nothing. Every elevation attempt (grant or refusal) is audited.

### 4.5 Manifests: today's registry, tomorrow's bundles

Today a program's manifest is its `EmbeddedProgram` registry row; the signed
`rxe`/`AppInfo` manifest supersedes it when the bundle store lands, with the
same semantics (request, not grant). Because the user ceiling bounds every
session process once CU1 lands, giving the shell (or any tool) a wider
manifest is safe: a manifest request an account's grant does not cover is
simply not in the intersection.

**The binding sizing rule (CU7):** a manifest requests every capability the
program has a code path to exercise — **including optional,
gracefully-degrading features** (`top`'s memory line, `ps -e`) — and nothing
it has no code path for. "Minimal" binds against *unexercised* authority (a
capability no code path uses is unaudited surface and stays out); it never
means "only what every account is guaranteed to hold". The manifest is a
mask describing what the code *can* do; the ceiling is policy describing
what the user *may* do; the intersection does the security work. Sizing a
tool's request down to the session baseline when it has privileged code
paths is a defect: it strips the feature even from accounts whose ceiling
grants it, with no recourse — the very trap this plan exists to prevent. An
above-baseline request must correspond to a feature that degrades gracefully
(`AGENTS.md` §2.24) when a non-entitled account's intersection strips it,
and every session tool's above-baseline subset is pinned as its own audited
set so widening is a reviewed diff.

### 4.6 The desktop

The graphical session changes no rule. The desktop-session service's
manifest requests the graphical class (`CAP_DISPLAY`/`CAP_INPUT_READ`/
`CAP_SHM` — seat ownership and the zero-copy frame region per
`plans/DISPLAY.md`) plus `CAP_PROC_SPAWN` for its taskbar launchers and
program-library popup (`plans/NEW-TASKBAR.md`)
and `CAP_FS_ACCESS` for the trusted picker and the catalog stores (each
sized to an exercised
code path per §4.5), the session baseline carries the same class so the
intersection keeps it for every interactive account, and a desktop app is
spawned like any other process and gets `AppInfo request ∩ user ceiling`;
user-mediated file access beyond the app's own state flows through
picker-issued one-shot descriptors, not class capabilities (`AGENTS.md`
§16.5 — live: the `fd_grant`/`fd_redeem` delegation of `plans/APPWIN.md`
AW5). Nothing in CU1–CU5 needed revisiting.

### 4.7 Resource limits interplay

Capabilities and rlimits stay orthogonal: the ceiling says *what kinds* of
operation a session may attempt; the `ulimit` facility bounds *how much*
(`AGENTS.md` §24.3). Limits inherit and intersect across spawn exactly as
the ceiling does; only `CAP_RLIMIT_RAISE` (administrative set) may raise a
hard bound.

---

## 5. The session, end to end (normative walkthrough)

How the pieces compose once CU1–CU3 land. Each step names the check that
authorises it; anything not listed is denied.

1. **Boot.** The kernel mounts root, verifies `/System/Security/Users` +
   `Groups`, installs the `IdentityTable`, and launches PID 1 `init`
   (system principal, ceiling = manifest: `CAP_CONSOLE_WRITE` +
   `CAP_PROC_SPAWN`). `init` spawns the boot services and one `login` per
   console.
2. **Login.** `login` (system principal; console pair + `CAP_PROC_SPAWN` +
   `CAP_USERS_READ` + `CAP_SPAWN_AS_USER` + `CAP_LOG_EMIT`) prompts on its
   console, verifies the offered password against the delivered record
   (timing-equalised, drops the secret immediately), and on success spawns
   the account's recorded shell **as that user**: the kernel resolves
   `(uid, gids, ceiling)` from the `IdentityTable` and derives the shell's
   set = shell manifest ∩ account ceiling. A locked account or wrong
   password is refused indistinguishably.
3. **Prompt, navigate.** The shell (session baseline) reads and writes its
   inherited console streams (`CAP_CONSOLE_READ`/`WRITE` + the descriptor
   table). `ls`, `cd`, tilde/alias paths: `CAP_FS_ACCESS` admits the
   `fs_*` call, then the secured VFS authorises the specific inode under
   the kernel-attested `(uid, gids)` — the user reads their home, reads
   `/System` where world-readable, and is denied writes to `/System`
   regardless of capability.
4. **Read and write files.** `fs_open`/`fs_read`/`fs_write` under the same
   two-layer check; redirection and pipes are pure descriptor plumbing and
   need no capability beyond the fds the shell already holds.
5. **Run a program.** `CAP_PROC_SPAWN` admits `spawn`; the registry (later
   the bundle store) resolves the path; the child runs as the same user
   with set = its own manifest ∩ the user's ceiling. `ps`/`sysinfo`/`top`
   then answer self-scoped queries for free, and global queries only if
   the *account* ceiling carries `CAP_SYSINFO_GLOBAL` (administrator).
6. **Administer.** An administrator account's shell holds the §4.3 set, so
   `users` (CU4), mount tools, `ulimit` hard-raises, and audit reads work —
   each still audited and each still bounded per object. A non-admin
   invoking the same tools gets `PermissionDenied` from the same gates.
7. **Log out.** The shell exits; its processes' capability records are
   released with them; `login` reaps the session and prompts again. Nothing
   persists except what the users database says.

---

## 6. Staged work

### CU1 — thread the user ceiling through spawn (fixes B1)

**Status: done.**

- `LateIdentity::resolve_credential` returns the target account's
  `capability_grants` ceiling alongside its groups; fail-closed behaviour
  unchanged (`NotImplemented` before install, `PermissionDenied` on an
  unknown uid).
- `SpawnCredential` carries the ceiling as an immutable kernel-side
  snapshot (`Option<CapabilitySet>`; `None` is the §4.1 system-principal
  manifest-as-ceiling shape). The spawn handler populates it: an
  inherit-spawn copies the caller's stored `TaskCapabilities::user_ceiling()`
  (never its effective set); a switch takes the resolved account ceiling.
  `KernelSpawnCtx::admit_process` derives `ceiling ∩ manifest`.
- System programs keep manifest-as-ceiling (§4.1): the task record carries
  a `system_principal` marker (`user_ceiling()` answers `None`), set at
  admit for a ceiling-less credential and on PID 1's boot record, so an
  inherit-spawn *from* a system principal bounds the child by the child's
  own registered manifest (init → login/devmgr) and the shape propagates.
  The boot-path `derive(caps, caps)` call sites (PID 1 in `init.rs`, the
  driver-spawn paths under a system credential) are documented as such.
- Tests (`kernel/core/src/syscalls.rs`): the B1 regression
  (`spawn_as_user_intersects_the_manifest_with_the_account_ceiling`, also
  asserting the audit event's derived count), inherit-spawn intersecting
  against the ceiling not the caller's effective set, the system-principal
  inherit keeping manifest-as-ceiling, and unknown-uid /
  uninstalled-table fail-closed. The `RecordingSpawn` test double forwards
  `program.capability_set()` exactly as the production producers do.
- Docs: `docs/src/security/capabilities.md` (the lifecycle page, §8).

### CU2 — session-baseline manifests (fixes B2)

**Status: done.**

- Every embedded program's manifest-requested capability list is defined
  once in `kernel/tairix-kernel/src/program_manifests.rs` (pure data, host-
  testable) and consumed by the `SPAWN_PROGRAMS` rows and `init_caps` in
  `spawn_layout.rs`. The shell's manifest is `SESSION_BASELINE`
  (`CAP_FS_ACCESS`, `CAP_PROC_SPAWN`, `CAP_CONSOLE_READ`,
  `CAP_CONSOLE_WRITE`) — exactly §4.2.
- Every row was audited against the gated syscalls its program actually
  issues and left at its real need: login keeps the console pair +
  `PROC_SPAWN` + `USERS_READ` + `SPAWN_AS_USER` + `LOG_EMIT` (it reads the
  users db through its own gated syscall, so **no** `CAP_FS_ACCESS`);
  devmgr and sysinfod were sized exactly. CU2's original sizing of `ps`,
  `sysinfo`, and `top` under-requested their privileged optional query
  paths; CU7 corrected those manifests under the §4.5 sizing rule.
- Tests: one exact-`CapabilitySet` pinning test per manifest (plus the
  init set), so a manifest change is a reviewed diff, not an accident.
  The original "every session tool requests within the session baseline"
  invariant codified the under-request and was replaced by CU7's pair of
  invariants (within-admin-ceiling, plus the audited above-baseline
  subset per tool).
- Docs: `docs/src/security/capabilities.md` records the pinned-manifest
  state; the `EmbeddedProgram::caps` and `SPAWN_PROGRAMS` rustdoc name the
  shared lists.

### CU3 — the debug administrator ceiling (fixes B3)

**Status: done.**

- The §4.2 + §4.3 sets are account policy with one home: `lib/users`
  (`grants` module) defines `SESSION_BASELINE`, `ADMINISTRATIVE_SET`, and
  `administrator_ceiling()` beside the record format that stores a grant,
  with exact-membership pinning tests (including the invariant that no
  service-/driver-class capability is ever in a ceiling). This goes
  further than a per-profile named constant: mkimage, the disk fixtures,
  and the kernel shell manifest (`program_manifests::SESSION_BASELINE`,
  now a re-export) all import the one definition, so the CU4 "one
  definition of both sets, shared with `tools/mkimage`" requirement is
  already satisfied for the installer to reuse.
- `tools/mkimage`'s profile-keyed `users_db` seeds the debug root grant
  as `administrator_ceiling()`; its unit test pins the seeded record to
  the exact set. The shared users-root QEMU fixture
  (`tairix_test_arxfs_image`) plants the same ceiling and the account's
  `/Users/root` home directory, so the fixture cannot drift from the
  debug profile.
- End-to-end QEMU acceptance vertical
  (`tests/integration/session_ceiling_qemu_aarch64`, enrolled in the
  `cargo xtask test --qemu` matrix): boots the production aarch64
  pipeline with the encrypted-root disk, unlocks at `Root passphrase: `,
  authenticates `root`/`root`, and drives the spawned shell —
  `cd /Users/root` (CAP_FS_ACCESS, the B3 regression), `pwd`, spawning
  `/System/Commands/ps.app/Run` (CAP_PROC_SPAWN) — then the negative: a `ulimit`
  hard-bound raise is refused with `PermissionDenied` because the
  shell's baseline manifest does not request `CAP_RLIMIT_RAISE` even
  though the ceiling carries it (the intersection binds). PASS keys on
  the audited `rlimit_set` rejection followed by the scripted `exit`.
  The scripted `ls /` / file write-read steps of the original sketch are
  deliberately absent: no `ls`/file tool is in the embedded program
  registry yet (they arrive with the `/Apps` tool enrolment), and `cd`'s
  kernel-authorised `fs_chdir` + `pwd` already witness the filesystem
  grant end to end.

### CU4 — user management under `CAP_USER_ADMIN`

**Status: done** (except the installer first-user flow, which lands with
the installer work — the shared `lib/users` grant sets it needs already
exist from CU3).

- **The surface** is one `CAP_USER_ADMIN`-gated, per-call-audited syscall,
  `users_admin` (no. 69), taking a versioned typed request
  (`lib/abi/src/users_admin.rs`: create/modify/delete account, lock/unlock,
  set grants, set password record, create/delete group, list users/groups).
  No raw-text edit path exists: password records never leave the kernel
  (`CAP_USERS_READ` stays login's alone), the list responses are
  secret-free, and a new password crosses only as a client-built salted
  PBKDF2 record. Wrappers: `tairix_rt::users_admin`,
  `tairix_sys_users_admin`; the decoder is in the shared `lib/abi` fuzz sweep.
- **The engine** (`kernel/core/src/useradmin.rs`, `UserAdminEngine` behind
  the `UsersAdmin` seam and the set-once `LateUsersAdmin` cell) applies one
  operation at a time, whole-or-nothing: never-widen (an added grant must be
  in the *caller's own* effective set — kernel-enforced), the
  last-active-administrator guard (cannot delete/lock/strip the last
  `CAP_USER_ADMIN` holder), full re-validation through `lib/users` and
  `build_identity_table`, serialised-size bounds, persist-then-swap. Audit
  events `USER_ADMIN_APPLIED`/`USER_ADMIN_REJECTED` (4045/4046) carry op,
  target, and attested caller uid.
- **Live state became replaceable through this path alone:** `LateUsersDb`
  and `LateIdentity` are installed set-once at boot and swapped only by the
  engine's commit (`replace` refuses to create a first state), so an edit
  binds at the next spawn/login (§2.6) — exercised by tests: a locked
  account's next authentication is refused indistinguishably while its
  identity row (and any running session) is unaffected.
  `UsersDbSource::text` now serves an owned zero-on-drop snapshot.
- **Persistence** is the `UserAdminBacking` seam: production is
  `RootAdminBacking` (`kernel/tairix-kernel/src/user_admin_backing.rs`)
  over a dedicated read-write `ARXFS` window the unlock's
  `WritableRootSink::publish` opens (the `/System` VFS mount shadows
  `/System/Security`, so the engine writes through a direct window exactly
  as the boot load read). Databases are replaced crash-safely (temp node
  carrying the original's security record, flushed, renamed); `CreateUser`
  provisions `/Users/<name>` owner-only under the new identity
  (idempotent). `finish_install` builds and installs the engine (aarch64
  wired; other ports gain it with their unlock paths via
  `UnlockInstall::admin`).
- **The first holder** is the interactive `users` tool
  (`userland/shell/users`, `/System/Commands/users.app/Run`, registry-enrolled):
  session logic behind host-tested seams, manifest = console pair +
  `CAP_USER_ADMIN` (deliberately above the baseline — armed only for an
  administrator's intersection, no `CAP_FS_ACCESS`), salt from
  `sys:random`, echo-off zeroised password entry. Spawn now carries an
  argument vector (the startup-strings block), so the staged
  `useradd`/`groupadd` argv grammars can become thin frontends over the
  same syscall — planned work, unblocked.
- The installer's first-user flow (admin ceiling for the first account,
  session baseline for subsequent ones) lands with the installer work —
  one definition of both sets, shared with `tools/mkimage`.

### CU5 — per-invocation elevation (`elevate`)

**Status: done.**

- The §4.4 broker path is live. The shell's `elevate <user> <program>`
  builtin (`userland/shell/elsh`: `Elevator` seam, `elevate.rs`, production
  seam in the `Run` binary) prompts for the password echo-off, posts one
  synchronous `ipc_call` to its console's login supervisor, and blocks — a
  foreground elevated command — until the re-authenticated program has run
  as the target account; its exit code becomes `$?`. The requesting
  shell's set is untouched; the elevated child's set is derived kernel-side
  as `its manifest ∩ the target account's ceiling`, exactly as at login.
- The same rendezvous also serves a narrower, verify-only request that
  re-authenticates the **calling principal's own** account and runs
  nothing — the primitive a graphical session's screen lock needs, so no
  second authenticator exists anywhere in the tree. It is strictly weaker
  than the run request (it never spawns a program) and narrower still (the
  account checked is always the caller's own kernel-attested uid, never a
  name the request supplies), so it grants no authority the run request
  did not already carry.
- Wire contract: `lib/abi/src/elevate.rs` — `ElevateRequest` is a
  two-variant enum (`Run { username, password, program }` /
  `Verify { password }`, an opcode byte after the version word) and
  `ElevateReply` is a three-variant enum (`Completed { exit_code }` /
  `Verified` / `Refused(Errno)`, encoded as a result-discriminant word — `0`
  completed, `1` verified, negative `-errno` refused — plus the exit-code
  word); both decode fail-closed (wrong version, unknown opcode/status,
  over-long buffer, a field past the end, non-UTF-8, an empty field,
  trailing bytes). The per-console rendezvous
  `elevate_endpoint(console) = ELEVATE_ENDPOINT_BASE + console` refuses the
  "no console" sentinel. Both ends derive the endpoint from their **own**
  kernel-attested `Origin::console` (never a claim), and the supervisor
  additionally refuses any caller whose attested console is not its own —
  before the request bytes are even parsed; a `Verify` request is decided
  against the caller's kernel-attested `Origin::uid` from that same
  attestation, read off the identical `call_peer_origin` result.
- Kernel extensions (the original "no new kernel primitive" clause was
  unsatisfiable — single-threaded login could not wait on "request posted"
  and "child exited" at once, and one global endpoint id cannot serve N
  consoles): the owner-checked `WaitSourceKind::Child` wait-set member (a
  non-consuming reapable-child peek, PID or `WAITSET_CHILD_ANY`) and the
  kernel-attested console index on `Origin`. Login supervises a session as
  waitset{elevate endpoint, shell child}, serving requests while the shell
  blocks in its call; elevation serialises per console (endpoint capacity
  1, second concurrent post fails closed) and stays concurrent across
  consoles. A login without a bindable rendezvous degrades to broker-less
  sessions and audits `ELEVATE_UNAVAILABLE`.
- Reserved-rendezvous hardening: `tairix_abi::ipc::is_reserved_endpoint`
  is the one definition of the reserved well-known call-endpoint ids
  (driver store, log ingress, mailbox, sysinfo, the elevate range), and
  `CallEndpoint::create` refuses to bind any of them without
  `CAP_IPC_BIND_PRIVILEGED` — even as an open bind — so an unprivileged
  squatter can never receive a service's traffic (an elevation request
  carries an offered password). `LOGIN_MANIFEST` and `SYSINFOD_MANIFEST`
  carry the capability; the kernel-side binders already held it.
- Authentication: `Authenticator::authenticate_uid(uid, password)` is the
  uid-keyed counterpart of `authenticate(credentials)`, sharing the same
  timing-equalised comparison — `lib/users`' `UsersDb::authenticate_uid`
  reuses the identical dummy-derivation burn `authenticate` pays, so an
  attested uid owning no account costs and looks exactly like a wrong
  password on a real one. `UsersAuthenticator` and the fail-closed
  `DenyAll` both implement it; no second credential-verification path
  exists.
- Audit: `ELEVATE_GRANTED` / `ELEVATE_REFUSED` / `ELEVATE_UNAVAILABLE` /
  `VERIFY_GRANTED` / `VERIFY_REFUSED` (login's 10_007–10_009, 10_012–10_013);
  refusal causes (wrong password, unknown or locked account, no attested
  uid) are audited but never disclosed — the requester sees one
  indistinguishable `PermissionDenied`.
- Tests: abi encode/decode round-trip and fail-closed rejects for both
  request variants and all three reply variants; the login broker decision
  table host-tested (grant + audit, foreign console refused before
  parsing, malformed refused without authentication, indistinguishable
  auth refusals, spawn refusal reported verbatim, a verify-only request
  answered without ever invoking the launcher, an attested uid owning no
  account refused indistinguishably from a wrong password, an unattested
  caller refused before authenticating); `lib/users` and the login
  `Authenticator` implementations tested for `authenticate_uid` parity
  with the username path; the shell builtin host-tested over the shared
  fixture (prompt + post + exit-code, refusal reporting, no post after a
  failed secret read, usage fail-closed, fail-closed default seam); kernel
  tests for the reserved-id squat denial / privileged allow and the
  `Child` wait-set member.

### CU6 — desktop session capabilities

**Status: done — the session/ceiling slice landed with `plans/DISPLAY.md`
D7d, and the picker-issued one-shot descriptors landed with
`plans/APPWIN.md` AW5 (its remaining QEMU-vertical stage is tracked
there).**

- Live: the graphical-session class (`CAP_DISPLAY`/`CAP_INPUT_READ`/
  `CAP_SHM`) is part of `SESSION_BASELINE` — a graphical login is an
  ordinary session, and the kernel still owner-gates every seat acquire,
  drain, and present against the live lease and every shm region against
  its owner, so the class capability only admits the syscall. The
  desktop session's `AppInfo` requests exactly that class; login spawns
  it as the authenticated user and its set is `manifest ∩ ceiling`,
  exactly as §4.6 prescribes. The shell's manifest was decoupled from
  the baseline (`SHELL_MANIFEST`: console pair + `CAP_FS_ACCESS` +
  `CAP_PROC_SPAWN`) per the §4.5 sizing rule, so widening the account
  ceiling never widened elsh; login's manifest gained `CAP_FS_ACCESS`
  for its one read-only desktop-bundle probe.
- Live: picker-issued one-shot file descriptors (`AGENTS.md` §16.5,
  `plans/APPWIN.md` AW5). The desktop session is the trusted UI: an app
  asks over the window channel (`PickFile`), the session browses and
  opens the chosen file under **its own** identity (its manifest gained
  `CAP_FS_ACCESS` for exactly this), and the kernel's `fd_grant` mints a
  one-shot, recipient-owner-bound, **read-only** delegation the app
  redeems with the unprivileged `fd_redeem` — every later read is
  re-authorised under the *grantor's* captured uid + effective set, the
  grant is audited, delegation never chains, and an exited recipient's
  pending grants are reclaimed. The `viewer.app` consumer holds no
  filesystem capability at all and reads exactly the one user-chosen
  file: spawn-time narrowing plus user-mediated widening, with no new
  capability added (the §5.2 minimalism rule — `CAP_FS_ACCESS` already
  gates delegating filesystem authority).

### CU7 — manifest entitlement audit (the §4.5 sizing rule)

**Status: done.**

- **The defect this stage fixed:** CU2 sized the session tools' manifests
  to the session baseline, so the `manifest ∩ ceiling` intersection
  stripped `CAP_SYSINFO_KERNEL`/`CAP_SYSINFO_GLOBAL` even from an
  administrator whose ceiling grants them — `top`'s memory line read
  `unavailable (needs CAP_SYSINFO_KERNEL)` and its `a` toggle was refused
  *for every account*, with no recourse. The §5 walkthrough promise
  ("global queries … if the *account* ceiling carries `CAP_SYSINFO_GLOBAL`")
  was unsatisfiable. The fix is doctrine + data, zero new mechanism: no new
  capability, no ceiling change, no runtime raise, and `sysinfod` keeps
  gating on the caller's kernel-attested *effective* set.
- Manifests audited against every gated code path
  (`kernel/tairix-kernel/src/program_manifests.rs` and each tool's
  `AppInfo.toml`, kept in lockstep by the drift pin): `top` +=
  `CAP_SYSINFO_GLOBAL` (the `a` system-wide toggle) + `CAP_SYSINFO_KERNEL`
  (the memory summary line); `ps` += `CAP_SYSINFO_GLOBAL` (`-e`/`-A`);
  `sysinfo` += `CAP_SYSINFO_GLOBAL` + `CAP_SYSINFO_KERNEL` +
  `CAP_SYSINFO_HW` (its global/kernel/hardware-tree queries). All other
  session tools request nothing above the baseline; `users` (console pair +
  `CAP_USER_ADMIN`) was already the correct precedent, as was login's
  `CAP_SYSINFO_KERNEL` request.
- Tests: the exact-set pins cover the widened manifests; the wrong
  baseline invariant was replaced by
  `session_tool_requests_stay_within_the_administrator_ceiling` (a session
  tool never requests a service-/driver-class capability) and
  `session_tool_requests_above_the_baseline_are_the_audited_set` (the
  exact above-baseline subset per tool, empty for every tool without a
  privileged optional feature — widening any tool is a reviewed diff
  naming its feature).
- Docs: `docs/src/security/capabilities.md` states the sizing rule and the
  entitled-tool behaviour; the manifests' rustdoc and `AppInfo.toml`
  comments name each above-baseline request's feature and its degraded
  form.

---

## 7. Deliberately not done (and why)

- **No setuid / setuid-self / capability raise at runtime** — the only
  identity/authority transition is spawn (§1.5).
- **No runtime capability *drop* syscall** — no consumer; sandboxes are
  spawned narrow (§2.5).
- **No live class-capability revocation** — next-spawn semantics (§2.6);
  kill the process if it must stop now.
- **No wheel group / admin flag / uid-0 special case** — admin is a grant
  set (§4.3).
- **No new capabilities in CU1–CU5** — the existing vocabulary already
  expresses every stage; `CAP_USER_ADMIN`'s enforcement point (CU4) is the
  only place a gate is *wired*, and the capability already exists.
- **No per-binary "file capabilities"** — the manifest *is* the per-binary
  request; a second, filesystem-attached grant channel would duplicate it.

## 8. Documentation

`docs/src/security/capabilities.md` (the lifecycle, the baseline and
administrative sets, the elevation model) is written with CU1 and kept
current in the same change as each later stage; the rustdoc of every seam a
stage touches (`TaskCapabilities`, `LateIdentity`, `SPAWN_PROGRAMS`,
`tools/mkimage`'s `users_db`) is updated in that stage.
