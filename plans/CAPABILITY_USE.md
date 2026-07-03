# CAPABILITY_USE.md — The capability lifecycle: how a user actually uses RustOS

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
   or launch anything. The shell now requests the session baseline
   (`kernel/rustos-kernel/src/program_manifests.rs`, CU2).
3. The debug root account's seeded grant (`tools/mkimage`) omits
   `CAP_FS_ACCESS`, so even with the intersection wired the shell would
   still be denied the filesystem.

This plan defines the lifecycle properly, then fixes the defect through it —
never by widening a check or special-casing `uid 0` (`AGENTS.md` §2.17,
§15.9).

## Status legend

`planned` / `in progress` / `done`, per `AGENTS.md` §13.

---

## 1. The model (normative recap)

RustOS authority is layered. Nothing here is new; this section fixes the
vocabulary the rest of the plan uses.

1. **Identity** — every process runs as a kernel-attested
   `(uid, gid, supplementary gids)` credential plus a capability record.
   The kernel supplies identity; a caller never does. `uid 0` carries **no**
   ambient power: powers come from capabilities, not from the uid
   (`AGENTS.md` §5.1).
2. **Class capabilities (`CAP_*`)** — the coarse "may use this subsystem at
   all" gates (`lib/abi/src/capability.rs`): `CAP_FS_ACCESS` admits a caller
   to the filesystem syscalls, `CAP_PROC_SPAWN` to `spawn`, and so on. They
   are checked at dispatch, before any state is touched, and they confer no
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
   identity transition is a `CAP_SPAWN_AS_USER` spawn (login), which
   resolves the target credential from the kernel identity table.
6. **Fail closed, audit the decision** — every denial is a typed
   `PermissionDenied`, never a fallback, and every security-relevant grant
   or refusal is logged with a stable event ID (`AGENTS.md` §5.4, §19.4).

---

## 2. The capability lifecycle (binding)

The life of a capability, from disk to exercise to revocation:

1. **Grant (at rest).** An account's ceiling lives in its
   `/System/Security/Users` record (`users-v1` `capabilities` field). It is
   authored by the image builder (debug), the installer (first user), or a
   `CAP_USER_ADMIN` holder (user management, CU4). The *system* principal
   (PID 1 `init` and the boot services it launches) has no users-db row; its
   ceiling is defined in-kernel per program (§4.1).
2. **Install (at boot).** The kernel reads and verifies the users/groups
   databases while mounting root, builds the immutable `IdentityTable`, and
   installs it exactly once (`LateIdentity`). Before the table is installed
   every identity resolution fails closed.
3. **Delegate (at spawn).** Spawn is the **only** delegation point:
   - *Inherit spawn* (no target uid): the child keeps the caller's
     credential **and the caller's user ceiling**; its effective set is
     `child manifest ∩ that ceiling`. A shell that launches `ps` hands it
     the user's ceiling, and `ps` ends up with just what its own manifest
     requests within it — never the shell's effective set, never more.
   - *Spawn-as-user* (`CAP_SPAWN_AS_USER`, login only): the kernel resolves
     the target account's full credential **and ceiling** from the
     `IdentityTable` and derives `child manifest ∩ target ceiling`. The
     caller chooses *which* account; it fabricates nothing.
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
- Login as the sole `CAP_SPAWN_AS_USER`/`CAP_USERS_READ` holder;
  `resolve_credential` resolving the target's groups **and capability
  ceiling** from the table, fail closed before install / on unknown uid.
- Per-inode owner/mode/ACL enforcement under kernel-attested credentials in
  the secured VFS; capability-gated dispatch on every syscall.
- The user ceiling threaded through spawn (CU1): `SpawnCredential` carries
  the account's `capability_grants` snapshot and
  `KernelSpawnCtx::admit_process` derives `ceiling ∩ manifest` — B1 is
  fixed.
- The session-baseline shell manifest and audited, pinned per-program
  manifests (`kernel/rustos-kernel/src/program_manifests.rs`, CU2) — B2 is
  fixed.

**Broken (the defect this plan fixes):**

- **B3** — the debug root grant (`tools/mkimage::debug_users_db`) omits
  `CAP_FS_ACCESS` (and the observability set §4.3 defines), so the seeded
  administrator cannot use the filesystem even once B1/B2 are fixed.

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

The **debug image's root account** is seeded with exactly the session
baseline plus this administrative set — the "administrative capability
ceiling a bring-up session needs" its rustdoc already promises. Driver-class
(`CAP_MEM_DMA`, `CAP_IRQ_BIND`, `CAP_MMIO_MAP`, `CAP_HW_EMIT`, …) and
service-class (`CAP_SPAWN_AS_USER`, `CAP_USERS_READ`,
`CAP_SYSINFO_INTROSPECT`, `CAP_INPUT_INJECT`, …) capabilities are **never**
part of any account ceiling: they belong to the specific system program
whose manifest requests them. An administrator administers the system; they
do not impersonate its services.

### 4.4 Elevation (running with more than your ceiling)

There is no setuid, no self-elevation, and no "enter admin mode" for a live
process. Elevation is **starting a new process under a more-privileged
account, through the one spawn-as-user holder, after re-authentication**:

- Day 1 (after CU1–CU3): elevation is *logging in as an administrator
  account* — on another console, or by exiting the session. The debug image
  has exactly one account and it is the administrator, so bring-up needs
  nothing further.
- CU5 adds the deliberate per-invocation form: an `elevate <cmd>`-style
  request the shell forwards to the session service (login), which
  **re-authenticates the target account's credentials** and spawns `<cmd>`
  as that account — the same `CAP_SPAWN_AS_USER` + `CAP_USERS_READ` path as
  a fresh login, one more caller of the existing gates, no new capability
  and no new kernel surface. The spawned command's set is still
  `its manifest ∩ the admin ceiling`; the requesting shell gains nothing.
  Every elevation attempt (grant or refusal) is audited.

### 4.5 Manifests: today's registry, tomorrow's bundles

Today a program's manifest is its `EmbeddedProgram` registry row; the signed
`rxe`/`AppInfo` manifest supersedes it when the bundle store lands, with the
same semantics (request, not grant). Because the user ceiling bounds every
session process once CU1 lands, giving the shell (or any tool) a wider
manifest is safe: a manifest request an account's grant does not cover is
simply not in the intersection. The registry rows are therefore sized to
what each program *does* (§6 CU2), not to a least-common-denominator.

### 4.6 The desktop (not yet implemented — direction only)

The graphical session changes no rule. The WM/session service's manifest
requests `CAP_DISPLAY`/`CAP_INPUT_READ` (seat ownership per
`plans/DISPLAY.md`); a desktop app is spawned like any other process and
gets `AppInfo request ∩ user ceiling`; user-mediated file access beyond the
app's own state flows through picker-issued one-shot descriptors, not class
capabilities (`AGENTS.md` §16.5). Nothing in CU1–CU5 needs revisiting for
the desktop; CU6 is the placeholder that binds it when `userland/gui/*`
work starts.

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
  once in `kernel/rustos-kernel/src/program_manifests.rs` (pure data, host-
  testable) and consumed by the `SPAWN_PROGRAMS` rows and `init_caps` in
  `spawn_layout.rs`. The shell's manifest is `SESSION_BASELINE`
  (`CAP_FS_ACCESS`, `CAP_PROC_SPAWN`, `CAP_CONSOLE_READ`,
  `CAP_CONSOLE_WRITE`) — exactly §4.2.
- Every row was audited against the gated syscalls its program actually
  issues and left at its real need: login keeps the console pair +
  `PROC_SPAWN` + `USERS_READ` + `SPAWN_AS_USER` + `LOG_EMIT` (it reads the
  users db through its own gated syscall, so **no** `CAP_FS_ACCESS`);
  devmgr, sysinfod, `ps`, `sysinfo`, and `top` were already sized exactly
  and are unchanged.
- Tests: one exact-`CapabilitySet` pinning test per manifest (plus the
  init set), and the invariant that every session tool's request is within
  the session baseline, so a manifest change is a reviewed diff, not an
  accident.
- Docs: `docs/src/security/capabilities.md` records the pinned-manifest
  state; the `EmbeddedProgram::caps` and `SPAWN_PROGRAMS` rustdoc name the
  shared lists.

### CU3 — the debug administrator ceiling (fixes B3)

**Status: planned.**

- `tools/mkimage::debug_users_db`: seed the root grant as session baseline +
  administrative set (§4.2 + §4.3), defined once as a named constant beside
  the profile, not an inline list.
- End-to-end QEMU vertical (the acceptance test for the whole defect):
  boot the debug image, authenticate `root`/`root`, then in the spawned
  shell `ls /`, `cd /Users/root`, write and read back a file, and spawn
  `ps` — all succeeding; then assert a *negative*: an operation outside the
  ceiling (e.g. a users-db read from the shell) still fails closed.

### CU4 — user management under `CAP_USER_ADMIN`

**Status: planned.**

- The write path to `/System/Security/Users`/`Groups` for a running system:
  a `CAP_USER_ADMIN`-gated kernel/service surface (create, modify, delete,
  lock/unlock, grant editing), designed with — not ahead of — its first
  holder, a `users` administration tool in `userland/shell/`.
- **Never widen beyond your own ceiling:** a grant editor can grant at most
  the capabilities in its *own* effective set — an administrator without
  `CAP_TIME_SET` cannot mint an account that has it (delegation narrows,
  §2.3; the kernel enforces this, not the tool).
- Deleting/locking an account, and the next-spawn revocation semantics of
  §2.6, are exercised by tests (a locked account's running shell keeps
  working; its next login is refused indistinguishably from a bad
  password).
- The installer's first-user flow (admin ceiling for the first account,
  session baseline for subsequent ones) lands here or with the installer
  work, whichever comes first — one definition of both sets, shared with
  `tools/mkimage` (`AGENTS.md` §2.2).

### CU5 — per-invocation elevation (`elevate`)

**Status: planned.**

- The §4.4 broker path: shell → session service IPC → re-authentication →
  `CAP_SPAWN_AS_USER` spawn of the requested command under the target
  account, audited both ways. No new capability, no new kernel primitive;
  the IPC endpoint is the new surface and is introduced with both ends
  live (`AGENTS.md` §5.2).
- Tests: correct password elevates and the child's set is
  `manifest ∩ admin ceiling`; wrong password / locked account refused
  indistinguishably; the requesting shell's own set is unchanged; audit
  entries for both outcomes.

### CU6 — desktop session capabilities

**Status: planned (blocked on `userland/gui/*` / `plans/DISPLAY.md`).**

- Binds §4.6 when the desktop lands: WM/session manifests, `AppInfo`
  intersection, picker-issued one-shot descriptors. No design work is done
  here ahead of a live consumer.

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
`debug_users_db`) is updated in that stage.
