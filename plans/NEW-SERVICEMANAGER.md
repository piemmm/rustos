# NEW-SERVICEMANAGER.md — A first-class, capability-scoped service manager

Binding under `AGENTS.md`. This plan makes service lifecycle a single,
first-class subsystem of TAIRiX rather than the embryonic launcher that
lives inside PID 1 today plus a scatter of ad-hoc starts (the desktop /
`login` starting `fontd`). It **evolves the existing `init` model in
place** (§2.2, §2.13) — no parallel manager, no `v2`.

The manager is a vital OS component and a large part of the trusted
computing base, so it holds the *minimum* authority, validates every
request, and fails closed (§4, §5.4).

---

## 1. What exists today (evolve, do not greenfield)

PID 1 (`userland/system/init`) is already an embryonic service manager:

- `service.rs` — `ServiceSpec { name, binary_path, account, dependencies,
  readiness, requires, provides, activation, stop_grace, connect_capability }`
  and the `Spawner` / `Reaper` / `Stopper` seams (pure core, host-tested). The
  spec names the service **account** (a uid), not manifest bytes: the kernel
  derives the grant (SVC-A).
- `manager.rs` (`Init`) — dependency **topological ordering**, `register`,
  `start_all`, readiness-gated admission, `reap`, and audit emission.
  **The kernel is the single capability authority** (SVC-A): the manager
  names only a service's binary and its **service account** (`ServiceSpec`
  carries a `binary_path` + `account` uid, never manifest bytes), and the
  kernel derives `manifest ∩ account-ceiling` from the signed bundle at load
  time — exactly as `drvhost` does for drivers. The `Spawner` seam is
  therefore `spawn(spec) -> Pid` (path + account, no capability set), so no
  init-side capability derivation can drift from the kernel's authoritative
  one. `RunningService` tracks live PIDs.
- `supervisor.rs` — wait-any supervision with a **bounded per-entry
  crash-loop budget** (never `spawn`-in-a-loop, §2.1).
- `startup.rs` — the boot service set as a parsed, fail-closed
  `StartupConfig` (`DEFAULT_CONFIG`: `sysinfod`, `netstack`, `devmgr`,
  `seatmgr`, then `login` as the session), currently **compiled in**. Its
  `MAX_SERVICES` bound is **derived from the floor text** (`DEFAULT_CONFIG`'s
  own `service`-directive count), so it tracks the floor rather than a magic
  cap (SVC-1, done).
- `events.rs` — reserved audit event IDs in `9000..10000`
  (`SERVICE_STARTED/START_FAILED/SKIPPED/EXITED`, `ORPHAN_REAPED`,
  `GRAPH_REJECTED`, `SERVICE_READY/CONDITION_SATISFIED/NOTIFY_REJECTED`,
  `SERVICE_NOT_ENROLLED`).
- `registry.rs` — the fail-closed enrolment registry (SVC-3): `Enrolment`
  (the enabled-service-name set), strict `validate_service_name`, and the
  ceiling-checked `enrol`/`unenrol` record transforms. `service.rs` also
  now owns the one shared `decode_manifest_capabilities` the manager and the
  enrolment ceiling check both use (§2.2).

Two mechanisms already in the tree that this plan reuses rather than
reinvents (§2.2):

- **Discovery by scanning the store** — `enumerate_driver_store` /
  `SystemFileService::list_store` (`kernel/tairix-kernel/src/system_files.rs`)
  already walks `/System/Drivers/` and reads bundles fail-closed. Service
  discovery is the same walk over `/System/Services/`.
- **Pre-unlock `/System/Settings` reads** — `SystemFileService::read_system_config`
  + the closed `SystemConfigFile` ABI enum already read a *whitelisted* set
  of `/System/Settings/**` files off the always-mounted read-only `/System`
  before the encrypted root is unlocked. The service **registration store**
  (below) is read through this same confined, fail-closed path.

TAIRiX has not shipped, so `abi-v1` is **not** frozen: the new `lib/abi`
types below are added now and only freeze on first release (§2.13, §9).

---

## 2. Where service data lives — `/System/Settings`, not `/System/Security`

Per the issue, all service configuration and registration data lives under
**`/System/Settings`** (machine-wide) and **`/Users/<u>/Settings`**
(per-user), never under `/System/Security` (§5.1, §16.2, §16.3). Rationale:
`/System/Security` holds identity/keys/MAC policy; service *enrolment* is
system configuration, and `/System/Settings` is exactly the machine-wide
settings tree (§16.2), already the one writable-by-the-trusted-path,
`nosuid,nodev,noexec` location the kernel can read pre-unlock.

- System service registration store: `/System/Settings/Services/`.
- Per-user service registration store: `/Users/<u>/Settings/Services/`.

The **unit metadata itself** (restart policy, activation mode, linger,
dependencies, required readiness conditions, per-service rlimits) does **not**
live in `/System/Settings`: it lives in the service's **signed `AppInfo`
bundle manifest** (§16.5), so tampering is a load refusal (§9). The
registration store holds only *enrolment records* — a decision "this
discovered bundle is eligible to auto-start / be on-demand-activated" keyed
to the bundle, produced by trusted tooling or an explicit `enable` action.
It is never a hand-maintained second copy of the unit metadata (§2.2,
§18.5) and never a place a user can raise a system service's authority.

---

## 3. Design

### 3.1 Discovery vs registration vs activation (three distinct steps)

A service, unlike a driver, has no natural activation gate (a driver's is
"the hardware is physically present and matched", §18.3). Dropping a signed
bundle on disk must therefore **not** make a live service appear — that is
an ambient-authority-shaped risk. Three separated steps:

1. **Discovery** — what bundles exist: scan `/System/Services/*.app` (and,
   for user services, the user's bundles) and read each signed `AppInfo`.
   No compiled-in list of *which services exist* (§2.2, §18.5); only the
   irreducible boot floor stays compiled in (§18.6).
2. **Registration / enablement** — is a discovered service *eligible* to
   auto-start or be on-demand-activated. An explicit, recorded,
   integrity-protected decision in the registration store (§2), never
   implied by presence. This is the systemd *present-vs-enabled* split with
   the capability framing.
3. **Activation** — actually starting it: at boot (for `boot`-mode enabled
   services whose readiness conditions are met), on-demand (endpoint
   activation, §3.4), or by an explicit control request (§3.7).

Two enrolment channels, mapped onto trust domains:

- **System services** are enrolled by trusted tooling at **image-build /
  install time** (`tools/mkimage`, the §11 installer). Post-install
  additions flow through the **signed update path** (§11/§19.3) under
  `CAP_SYSTEM_UPDATE` — never a user writing the system registration store.
- **User services** are enrolled **manually** by the user via an explicit,
  capability-gated `enable` action into that user's **own** per-user scope,
  bounded by that user's ceiling (§5.1/§5.2). Enrolment records a decision;
  it can never grant authority the enroller lacks.

### 3.2 Two scopes, one engine

The system-vs-user boundary is realised as **one policy engine instantiated
at two authority scopes** (§2.2), never two codebases:

- **One system service manager** — PID 1's role. Holds system authority,
  minimal TCB (§4), runs system-scoped services under their own service
  accounts (`plans/USERS.md`), and remains the **last-resort orphan
  reaper**. (A userland launcher-as-parent breaks reaping per
  `FIX-DESKTOP.md` §2.4, but a service manager *should* parent what it
  supervises, so that objection does not apply here.)
- **One per-user service manager instance per logged-in user** — spawned by
  the system manager at session start, delegated **only that user's
  sub-ceiling** (intersection, never widening, §5.2; no ambient authority,
  §4). It parents/supervises/reaps that user's own services; on logout it
  stops them (reverse-dependency order) and exits, its orphans falling to
  PID 1.

Boundary invariants (a reviewer will hammer these):

- A user's on-demand request can **never** make a system-authority service
  appear, nor touch another user's services.
- A **shared sandboxed service like `fontd`** is **system-scoped** but
  usable by all users *because it is sandboxed to minimum capability*
  (§19.5) and exposes only a data endpoint. A user-triggered activation
  hands the *user* a **connection**, never any of the service's authority.
  The per-user manager does not own `fontd`; it routes the user's activation
  request to the system manager, which brokers the connection.

### 3.3 Service lifecycle + readiness protocol

The current model treats "spawned" as "done" — a correctness gap: a
dependent that needs "network up" cannot honestly start when `netstack` is
merely *spawned*. Add:

- An explicit lifecycle: `inactive → starting → ready → running → stopping →
  stopped | failed`.
- A **readiness notification** protocol (an `sd_notify` analogue) so a
  service declares "I am up" via a versioned `lib/abi` call, and
  dependents / readiness gates release only then.
- **Named readiness conditions / targets** (`network-up`,
  `filesystems-mounted`, `boot-complete`, `display-present`,
  `seat-available`, …). A service declares the conditions it requires; the
  manager releases it only when all are satisfied. This **generalises the
  headless case**: GUI-only services (`fontd`) simply never activate because
  `display-present` / `seat-available` is never satisfied (§17.3), which is
  exactly how the current "`login` starts `fontd`" hack (`FONT-SERVICE.md`
  §3) is **deleted** (§2.14), not reworked. The one-way non-GUI→GUI edge
  stays intact (§17.3/§17.4).

### 3.4 On-demand activation — capability-brokered endpoint activation

On a capability OS the correct on-demand mechanism is **not** Linux
socket-activation; it is capability-brokered endpoint activation:

- A shared service owns a well-known reserved IPC endpoint (the pattern
  `FONT_ENDPOINT` / `lib/abi/src/{window,display,net}_ipc.rs` already use).
- A client asks the service manager (capability-gated, §5.4) to **connect**
  to that endpoint. If the service is not running the manager starts it (as
  its sandboxed service account), **parks the client** until the service is
  *ready* (§2.23 — wake on the readiness event, never busy-poll), then hands
  back the endpoint.
- This one mechanism serves on-demand start, dependency-triggered start, and
  multiuser sharing. Requests that arrive while a service is `starting` are
  **queued and woken** on ready (§2.23), the queue **bounded** and
  fail-closed (§24.3) — never dropped, never spun on.

### 3.5 Idle stop — a defined sink and a tickless linger

- Define **sink** = live connected clients: a refcount on the reserved
  endpoint.
- Last client disconnects (refcount → 0) → arm a **one-shot tickless linger
  timer** (§17.1) → on expiry with refcount still zero, run the graceful
  stop (§3.6). Any new connect before expiry cancels the timer. No polling
  loop (§2.23).
- The linger duration and activation mode (`permanent | on-demand{linger}`)
  are **per-service unit metadata** in the signed `AppInfo`, not hard-coded.
  A web server is `permanent`; `fontd` is `on-demand`.

### 3.6 Stop / shutdown

- **Graceful per-service stop:** stop request → **grace timeout** (`Time64`,
  §21; configurable per-service) → forced terminate only if it has not
  exited. No blind kill.
- **Reverse-dependency ordering:** on stop and on shutdown, tear down in the
  reverse of start order; a service is not stopped before its dependents.
  Same determinism / cycle rejection as start order, applied in reverse.
- **Idle stop is a special case of stop** (§3.5).
- **System shutdown:** per-user managers stopped first (each stops its
  user's services in reverse-dep order), then system services in
  reverse-dep order, then PID 1 exits last.

### 3.7 Restart policy

`restart = never | on-failure | always` with **bounded exponential backoff**
(the crash-loop budget already in `supervisor.rs`), plus an optional
**health-check / watchdog** tied to `plans/WATCHDOG.md`. A **blind periodic
restart** is the §2.1 "retry-until-it-works" hack and is **not** a default;
if offered at all it is opt-in, audited, and documented as a workaround, not
a feature.

### 3.8 Control API + tool + observability

- A versioned, capability-checked `lib/abi` **control surface**
  (`start / stop / enable / disable / status`) — the `systemctl` analogue.
  Status is served through the System Information API (§16.6), **never** a
  `/proc` / `/sys` view (§16.1); no free-form text scraping.
- A `userland/shell/` control tool over that API.
- **Audit:** extend `events.rs` IDs for on-demand start, activation,
  idle-stop, restart, readiness, and denials — every security-relevant
  decision with a stable ID (§5.4/§19.4).

### 3.9 Resource limits and RAM

- Per-service `rlimit`/`ulimit` (§24.3) is **optional** unit metadata in the
  signed `AppInfo`; the default is *unset* (uncapped — an `apache`-class
  service may consume the machine). Raising a hard bound above the inherited
  ceiling needs an explicit capability (`CAP_RLIMIT_RAISE`); enforcement is
  kernel-side and fails closed (§5.4).
- No per-service RAM *cap* default and no compile-time RAM `const` (§24.1).
  A greedy service is bounded by the machine, a **system reserve** (a
  discovered-RAM fraction — a policy, not a scalar — §24.2) that keeps the
  kernel/PID 1/log path alive, and the fairness + reclaim arbiter with
  **per-principal accounting** (§26.2/§26.3), not by an arbitrary number.

### 3.10 The `MAX_SERVICES` fix — bound vs capacity (§24.1/§24.4)

- The **compiled-in boot floor** (`console`, `sysinfod`, `netstack`,
  `devmgr`, `seatmgr`, `login`) is a genuinely fixed, irreducible set
  (§18.6). Its size is a **bound dictated by the floor**, so it may stay
  fixed — but the magic `MAX_SERVICES = 4` that would silently *truncate* a
  fifth floor entry is a latent defect: size the floor parser to the actual
  floor set and **fail closed** (`ConfigError::TooManyServices`) if the
  config exceeds it, never drop entries.
- The **discovered / registered tier** (everything past the floor) is a
  **growable capacity**: sized from what is discovered and grown once the
  `lib/rt` heap lands (`plans/SPAWN.md` SP5b, §25). **No `const` cap there.**

---

## 4. Invariants (must hold)

- **One engine, two scopes** (§3.2); the boundary invariants of §3.2 hold.
- **Discovery ≠ registration ≠ activation** (§3.1); presence never grants
  eligibility; no compiled-in service list beyond the floor (§18.5/§18.6).
- **No ambient authority** (§4): a user activation grants a *connection*,
  never a service's own authority; a user service runs within the user's
  ceiling only.
- **Fail closed everywhere** (§5.4): missing/corrupt registration store →
  the service is simply not eligible, never a guess; bounded queues; every
  denial audited.
- **No busy-poll** (§2.23): clients park on readiness; idle-stop is a
  one-shot tickless timer; supervision waits on child exit.
- **All timers/backoff/linger are `Time64`** (§21).
- **Signed metadata** (§9): unit metadata lives in the signed `AppInfo`;
  tamper = load refusal. The registration store is never a second copy.
- **PID 1 stays the minimal last-resort orphan reaper** (§4).
- **Untrusted parsing** of enrolment/registration input fails closed and, if
  it grows non-trivial, is sandboxed (§19.5) with a fuzz harness (§19.6).

---

## 5. Scope audit (surprises checked in the tree)

- **`MAX_SERVICES`** (`startup.rs`) is a floor-sized fail-closed bound derived
  from `DEFAULT_CONFIG` (not a magic `4`), and `supervisor::MAX_SUPERVISED_SERVICES`
  imports it rather than carrying a second copy; the bound tests assert the
  floor exactly fills it and that a longer config fails closed (SVC-1, done).
- **`login` starts `fontd`** (`FONT-SERVICE.md` §3; `login`'s start path,
  the x86_64/riscv64 compiled-in `FONTD_PATH`/`FONTD_MANIFEST`/
  `SPAWN_PROGRAMS` fallbacks): deleted in favour of readiness-condition
  activation. This is the main deletion (§2.14) and touches `login`, the
  spawn-program registry, and `FONT-SERVICE.md`.
- **Registration-store read**: reuse `SystemFileService::read_system_config`
  + `SystemConfigFile` (add the services store path to the closed set) — no
  new pre-unlock read path.
- **Discovery**: reuse `enumerate_driver_store`/`list_store` over
  `/System/Services/` — no new scanner.
- **Heap dependency**: the growable registered tier depends on the `lib/rt`
  userland heap (`plans/SPAWN.md` SP5b). Until it lands the floor stays
  no-heap and exactly-sized; the growable tier is staged behind the heap.
- **New `lib/abi`**: readiness-notification, control API, and (if needed) a
  service-manager IPC endpoint — versioned/hashed (§9), C view regenerated
  (`cargo xtask c-header --write`), `abi-check` clean.

---

## 6. Stages

Each stage leaves the whole-project §7 gate green before it is reported done.

### SVC-1 — Kill the `MAX_SERVICES` cap; floor-sized fail-closed bound — DONE
- `startup::MAX_SERVICES` is derived from `DEFAULT_CONFIG` by a `const fn`
  service-directive counter, so the floor sizes its own bound and a stale
  magic number can neither silently truncate a floor entry nor drift.
  `supervisor::MAX_SUPERVISED_SERVICES` imports it (one definition, §2.2). A
  config exceeding the floor fails closed (`ConfigError::TooManyServices`);
  no behaviour change for the shipped floor. Tests assert the floor exactly
  fills the bound and that the `const` counter agrees with the runtime parser.
  The growable, discovery-registered tier past the floor is SVC-3/SVC-4 and
  waits on the `lib/rt` heap (§3.10).

### SVC-A — Capability authority + the live PID 1 engine

**Decision (confirmed): the kernel is the single capability authority.** A
service is launched by naming its binary and its **service account** uid; the
kernel loads the signed bundle and grants `manifest ∩ account-ceiling` (the
same gate `drvhost` runs for drivers, §8/§18.6). The manager never decodes a
manifest or computes a grant on the launch path, so there is no second,
divergent capability-derivation path to keep in step with the kernel's. This
resolves the mismatch between the earlier engine design (init decodes the
manifest, intersects with its own authority, and passes an explicit `granted`
set) and the live `spawn_as(path, uid)` model (the kernel derives the grant):
the live model wins, and the engine is reshaped to it in place (§2.13).

- **Engine reshaped to the kernel-authority model — DONE.** `ServiceSpec`
  carries `account: u32` instead of manifest bytes; `Spawner::spawn(spec)`
  drops the `granted` argument; the manager drops `InitConfig.authority` /
  `accepted_abi_version`, `requested_capabilities`, the init-side
  intersection, `StartedService.granted`, and the retired
  `SERVICE_DENIED` (9003) audit id / `StartFailure::{ManifestInvalid,
  CapabilityEscalation}` (a refused load now surfaces as the kernel's own
  `SpawnFailed`). The enrolment ceiling check (`registry::enrol`) keeps
  decoding a manifest it is *given* — that is the registered/user tier, where
  a manifest genuinely exists — via the still-shared
  `service::decode_manifest_capabilities`. Host tests updated; the 3 tests
  that asserted init-side intersection/escalation are deleted (§2.14). The
  live boot is unchanged (the engine is still only reached from tests), so
  the existing boot behaviour and QEMU verticals are untouched by this step.
- **Wire the engine into live PID 1 — TODO (the meat of "Stage A").**
  Replace the no-heap bootstrap `supervise` / `StartupConfig` service path in
  `userland/system/init/src/run.rs` with the heap-backed `Init` engine
  driving the boot-floor services (all `Permanent`/`Immediate`), backing the
  `Spawner`/`Reaper`/`Stopper`/`Sink` seams on the real syscalls
  (`spawn_as(path, 0, account)`, `wait`, `signal`, `lib/log`). Per-console
  **session** supervision (one `login` per console, crash-loop relaunch)
  stays a distinct concern layered over the engine; the single wait loop
  routes each reaped pid to the session table or to the engine
  (`reap` handles service exits + orphans). No parallel manager (§2.2): the
  old flat service handling is deleted in the same change. This step owns
  the 3-arch QEMU boot verticals and the full §7 gate.

### SVC-2 — Lifecycle + readiness protocol (`lib/abi` + engine) — DONE
- `lib/abi/src/service.rs` (versioned/fail-closed, frozen on first release):
  the `ServiceState` lifecycle (`inactive → starting → ready → running →
  stopping → stopped | failed`) with `is_ready`/`is_terminal`; the closed
  `ReadyCondition` set (`network-up`, `filesystems-mounted`, `boot-complete`,
  `display-present`, `seat-available`); `ReadinessKind` (`immediate` default
  vs `notify`); and the `ReadyNotice` (`sd_notify` analogue) carrying a
  `LifecycleSignal` (`ready`/`failed`) and **no identity** — the manager
  binds it to the kernel-attested sender. It is an IPC-protocol module like
  `font_ipc`, so it is outside the generated C header (no `abi-check`/
  `c-header` change).
- `init` engine (`manager.rs`): per-service `ServiceState`+pid; `ServiceSpec`
  gains `readiness`/`requires`/`provides`. Bring-up is a readiness-gated
  admission fixpoint — a dependent is released only when every dependency is
  `is_ready()` and every required condition is satisfied, never on merely
  spawned. `immediate` services reach ready on spawn success; `notify`
  services wait for `Init::notify`. `satisfy_condition` records
  externally/kernel-signalled conditions; a provider satisfies its `provides`
  on readiness. Everything fails closed: a never-ready dependency leaves its
  dependent `inactive`, and `notify` is refused (`NotifyError`) for an
  unknown or non-`starting` service. New audit IDs `SERVICE_READY` (9008),
  `CONDITION_SATISFIED` (9009), `NOTIFY_REJECTED` (9010).
- Because `immediate` is the readiness default, the existing bring-up
  semantics (and their tests) are preserved unchanged; new host tests cover
  the `notify`/condition gating, the never-ready and explicit-failure paths,
  and the fail-closed notify rejections.
- Not yet wired to a live transport: the manager consumes decoded notices
  through its engine seam; binding the readiness/control endpoint and mapping
  a kernel-attested sender to a service is SVC-4/SVC-8 work.

### SVC-3 — Discovery + registration store under `/System/Settings` — DONE
- The enrolment engine is `userland/system/init/src/registry.rs` (pure,
  host-tested, `no_std`+alloc): `Enrolment` is the fail-closed parsed set of
  enabled service names for one scope (`startup.rs`-style line parser: `#`
  comments, blank lines ignored, one name per line). `validate_service_name`
  is a strict lowercase-`[a-z0-9._-]` (alnum-first) identifier check — a
  security control, so a `..`/path-traversal- or case-collision-shaped token
  can never be enrolled. The enabled set is a **growable capacity** (no
  fixed `const` cap on the number of services, §24.1); only a single-name
  length bound (`MAX_SERVICE_NAME_LEN`, a validation bound §24.4) is fixed.
- The store path is the closed-whitelist entry
  `SystemConfigFile::SystemServices` → `/System/Settings/Services/enabled`
  (`lib/abi/src/driver_store.rs`), read through the **existing** confined
  pre-unlock `read_system_config` path — no new read primitive (§2.2). A
  per-user store lives under `/Users/<u>/Settings/Services/`, parsed
  identically. Both a **corrupt** (`parse` → `EnrolError`) and a **missing**
  store resolve to `Enrolment::empty()` — nothing eligible, never a guess.
- `enrol`/`unenrol` are pure record transforms returning the new
  `Enrolment` for the caller to write back through the appropriate
  trusted-path store. `enrol` decodes the service's signed manifest (the
  shared `service::decode_manifest_capabilities`, hoisted out of the
  manager, §2.2) and **refuses** (`CapabilityEscalation`) any request beyond
  the enroller's ceiling, so enrolment can never widen authority; it is
  idempotent. `unenrol` needs no capability (removal only narrows) but fails
  closed on an absent service.
- Activation wiring: `Init::register_enrolled(discovered, &Enrolment)`
  registers a discovered `ServiceSpec` **only** if enrolled; a
  present-but-unenrolled bundle is never registered and its skip audits
  `SERVICE_NOT_ENROLLED` (9011). The kernel still derives the grant
  (`manifest ∩ account-ceiling`) from the signed bundle at start (SVC-A), so
  enrolment records a decision and never grants power.
- Host tests cover: a present-but-unregistered bundle never registers/starts;
  a corrupt store and a missing store both leave nothing eligible; enrolment
  refuses a manifest exceeding the ceiling; strict-name/duplicate rejection;
  the canonical-text round trip; idempotent enrol; fail-closed unenrol.
- The `AppInfo` unit-metadata **parse** is SVC-3b (below): a discovered
  bundle's signed unit metadata decodes into a `ServiceSpec` via
  `ServiceSpec::from_manifest`. Not yet wired to a live boot path: the
  `/System/Services` **scan** itself, and reading the store off `/System`,
  are done by the loader/kernel seam that SVC-4/SVC-5 wire; the boot floor
  still comes from the compiled-in `DEFAULT_CONFIG` until the growable
  registered tier lands on the `lib/rt` heap (§3.10).

### SVC-3b — Service unit-metadata record + discovery parser — DONE
- `lib/abi/src/service.rs` gains the `ServiceManifest`/`ServiceUnit` pair —
  the compact, versioned, fail-closed binary record of a service's unit
  metadata (`SERVICE_MANIFEST_MAGIC` = `"SUM1"`, `SERVICE_VERSION_V1`): the
  service account, readiness kind, activation mode + idle-linger, restart
  policy, stop grace, connect capability, and the dependency names and
  required/provided `ReadyCondition`s. `ServiceUnit` is the allocation-free
  encoder input; `ServiceManifest` is the borrowed decoder view whose
  `from_bytes` validates the *whole* record up front (magic, version,
  reserved bytes, known flag bits, every enum discriminant, every count
  against its bound, every dependency name as bounded UTF-8, an exact overall
  length, and the canonical forms — reserved/connect-cap/linger forced to
  zero unless their flag says otherwise) so every accessor is infallible and
  a malformed byte fails closed. It is an IPC-protocol module like
  `ReadyNotice`, so it is outside the generated C header (no `abi-check`/
  `c-header` change), and its decoder has a `fuzz_decode` arm asserting the
  never-panic + canonical-round-trip contract (§19.6). The metadata is the
  data that lives in the service's **signed** `AppInfo` bundle manifest (§2),
  so tampering is a load refusal upstream.
- `ServiceSpec::from_manifest(name, binary_path, &ServiceManifest)` is the
  bridge from a decoded manifest to the `ServiceSpec` the manager consumes,
  applying the manager's strict **name policy** (`registry::validate_service_name`,
  §2.2 — one authoritative check, not duplicated in the ABI) to the service
  name and every dependency name, so a manifest can never smuggle a
  path-traversal-shaped dependency into the graph. Fails closed on a name
  defect. Host tests cover the full round trip of every field and the
  name-policy rejection of a bad service or dependency name.
- Still deferred to the loader/kernel seam (SVC-4/SVC-5): reading the
  `ServiceManifest` bytes out of a discovered `/System/Services` bundle's
  signed `AppInfo` and calling `from_manifest` on the live boot path.

### SVC-4 — On-demand endpoint activation + idle linger — DONE (engine core)
- `lib/abi/src/service.rs` gains `ActivationMode` (`Permanent` |
  `OnDemand { linger: Duration64 }`) — unit metadata carried in the signed
  manifest, IPC-protocol module so no `abi-check`/`c-header` change.
  `ServiceSpec` gains `activation`, `stop_grace` (default `DEFAULT_STOP_GRACE`
  = 5 s), and `connect_capability` builders/accessors.
- New seams in `service.rs`: `ClientId` (kernel-attested connection id) and
  `Stopper` (`request_stop` graceful + `force_terminate`), wired through
  `InitConfig`.
- `Init` engine (`manager.rs`): the one capability-brokered activation entry
  `connect(name, client_caps, client)` — capability check **before** any
  state (fail closed), then connect-now if ready, activate-if-down (start as
  the service account, fail closed when a required readiness condition is
  unmet — the headless case), or park behind a **bounded** per-service queue
  (`MAX_PENDING_PER_SERVICE`, a §24.4 anti-flood security bound → `QueueFull`).
  Parked clients are released into the sink and reported via
  `take_ready_clients` when the service reaches ready (through boot admission,
  a readiness notice, or a satisfied condition) — woken by the event, never
  polled. `disconnect(name, client, now)` refcounts the sink and arms a single
  one-shot idle-linger deadline when the last interest leaves an on-demand
  service; a new `connect` cancels it. `expire_linger`/`expire_grace` are the
  one-shot-timer callbacks the transport arms from `linger_deadline`/
  `grace_deadline`. `pump` now **skips on-demand services** so they are never
  eagerly started at boot.
- The graceful-stop primitive (request → `Stopping` → grace deadline →
  `force_terminate`, and `reap` mapping a stopping service's exit to
  `Stopped` regardless of code) landed here because idle-stop is a special
  case of it (§2.2/§2.19); SVC-7 builds restart policy and reverse-dependency
  ordering on top of it rather than reinventing it.
- New audit IDs (9012–9017): `SERVICE_ACTIVATED`, `ACTIVATION_QUEUED`,
  `ACTIVATION_DENIED` (unknown/capability/unavailable/queue-full),
  `SERVICE_LINGER_ARMED`, `SERVICE_STOPPING`, `SERVICE_FORCE_TERMINATED`.
- Host tests cover: on-demand not started at boot; connect activates a down
  immediate service and connects now (shared by a second client); a notify
  service parks until it announces ready then wakes the parked client once;
  the full idle → linger → graceful stop → grace → force → reap-to-`Stopped`
  lifecycle; a new connect cancels a pending linger; the capability check runs
  before any state; unknown-service and condition-gated (headless) connects
  fail closed; the pending queue is bounded and fails closed; `add_duration`
  carry/saturation.
- Not yet wired to a live transport: binding the reserved endpoint, mapping a
  kernel-attested connecting principal to a `ClientId`, and arming the real
  one-shot timers off `linger_deadline`/`grace_deadline` is the loader/kernel
  seam SVC-5/SVC-8 wire (the QEMU vertical lands with the live `fontd`
  activation in SVC-5). The growable registered tier past the floor still
  waits on the `lib/rt` heap (§3.10).

### SVC-5 — Delete the `login`-starts-`fontd` hack
- `fontd` becomes an on-demand, `display-present`-gated service; remove the
  `login` start path and the x86_64/riscv64 compiled-in fallbacks. Update
  `FONT-SERVICE.md` §3 in the same change (§2.14).
- QEMU: graphical login/`desktop` still gets fonts (via activation); a
  headless boot never activates `fontd`.

### SVC-6 — Per-user manager scope
- The per-user manager instance spawned at session start with the user's
  sub-ceiling; parents/supervises/reaps the user's services; logout stops
  them in reverse-dep order. Boundary invariants (§3.2) tested.

### SVC-7 — Restart policy + reverse-dependency stop/shutdown ordering — DONE (engine core)
- `lib/abi/src/service.rs` gains `RestartPolicy` (`never` | `on-failure` |
  `always`, default `never`) — unit metadata carried in the signed manifest,
  IPC-protocol module so no `abi-check`/`c-header` change. `never` is the
  default (a service is brought back only when its manifest asks), `on-failure`
  restarts only after a non-zero/crash exit, `always` after any exit;
  `should_restart(exit_code)` is the one decision point. `ServiceSpec` gains
  `restart`/`with_restart`/`restart()`.
- `Init` engine (`manager.rs`): `reap` now takes the monotonic `now` and, for
  an exit the manager did **not** itself initiate (a graceful idle-stop or
  shutdown is honoured, never fought), schedules a policy-driven restart via
  `schedule_restart`: it arms a single one-shot `restart_deadline = now +
  restart_backoff(attempts)` and audits `SERVICE_RESTART_SCHEDULED` (9018).
  Backoff is exponential (100 ms base, ×2, clamped to a 30 s cap) computed in
  `u128` ns with a shift-back overflow check. The crash-loop budget
  (`MAX_RESTART_ATTEMPTS` = 5) bounds a *tight* loop and fails closed
  (`SERVICE_RESTART_EXHAUSTED`, 9019) rather than relaunching forever (§2.1);
  it resets once a relaunched service has run past `RESTART_STABLE_WINDOW`
  (30 s), tracked by `relaunched_at`, so a long-lived daemon that crashes once
  after hours restarts with a full budget. `expire_restart_backoff(name, now)`
  is the one-shot-timer callback the transport arms from `restart_deadline`; it
  returns the service to `Inactive` and re-drives the admission `pump` (woken by
  the event, never polled). `dependency_failed` ignores a `Failed` dependency
  that has a live restart deadline, so a restarting dependency does not
  permanently skip its dependents.
- Reverse-dependency teardown: `stop(name, now)` gracefully stops `name` and its
  transitive dependents dependents-first (`dependent_closure` + reversed
  topological `reverse_stop_order`), and `shutdown(now)` tears the whole set
  down the same way — the system-shutdown sequence (per-user managers stop their
  users' services and exit first, then the system manager `shutdown`s the system
  services). Both cancel any pending restart first (a deliberate stop is never
  fought with a relaunch), and both build on the SVC-4 graceful-stop primitive
  (`begin_stop`, `Stopping`, grace deadline → `expire_grace` → force) — not a
  reinvention (§2.2). `stop` fails closed on an unknown name.
- Host tests cover: every policy (never leaves a crash down; on-failure restarts
  an abnormal exit but honours a clean one; always restarts even a clean exit);
  the backoff doubling+cap and `duration_since` carry/clamp; the crash-loop
  budget bounding a tight loop and failing closed; the stable-window budget
  reset; reverse-dependency `shutdown` and `stop` ordering (dependents first,
  independents untouched); fail-closed unknown-service stop; and shutdown
  cancelling a pending restart.
- Health-check/watchdog restart (`plans/WATCHDOG.md`) and the capability-gated
  control surface that gates *who* may `stop`/restart a service are SVC-8; the
  live one-shot-timer wiring off `restart_deadline` rides with the loader/kernel
  transport seam SVC-5/SVC-8 wire, on the `lib/rt` heap for the growable tier
  (§3.10). A blind periodic restart is not offered (§2.1, §3.7).

### SVC-8 — Control API + tool + audit + rlimits + docs/gate
- **Per-service `rlimit` unit metadata — DONE (ABI + engine core).** The
  `ServiceManifest`/`ServiceUnit` SUM1 record carries an optional
  per-service resource-limit section: a `limits_count u16` at prefix offset
  48 (prefix grown 48→50, `reserved0` kept reserved) and a body of
  `ServiceLimit { kind: LimitKind, limit: ResourceLimit }` entries encoded as
  `(u32 kind ‖ u64 soft ‖ u64 hard)`. The section is **canonical** — strictly
  ascending by `LimitKind` discriminant, so a duplicate or descending kind, a
  malformed (`soft > hard`) bound, an unknown discriminant, or more than
  `SERVICE_MANIFEST_MAX_LIMITS` (= `LimitKind::COUNT`) entries all fail the
  record closed — reusing the existing `lib/abi::rlimit` types (no second
  limit model, §2.2) and the existing `CAP_RLIMIT_RAISE` gate (no new
  capability). It stays an IPC-protocol module outside the generated C header
  (no `abi-check`/`c-header` change) and its decoder is covered by the
  `fuzz_decode` never-panic/canonical-round-trip arm (§19.6). `ServiceSpec`
  gains `limits`/`with_limits`/`limits()`, and `ServiceSpec::from_manifest`
  threads the decoded limits through so a discovered bundle's declared
  limits reach the manager. **Kernel enforcement at spawn** (threading
  `spec.limits()` into the `spawn_as` path) rides with the loader/kernel
  transport seam (SVC-5) — the metadata is carried and validated now; the
  live enforcement wiring is not yet in the boot path.
- **Remaining (TODO).** The versioned capability-checked control `lib/abi`
  surface (`start/stop/enable/disable/status`) and its service-control
  capability (added *with* its enforcement point + a live holder, §5.2),
  status via §16.6, the control tool, the extended `events.rs` IDs, and the
  live rlimit enforcement at spawn. Docs (`docs/src/userland/`,
  `docs/src/architecture/`, `docs/src/abi/`), README matrix, and this plan
  collapsed to done-state. Full §7 gate green, output quoted.

---

## 7. Cross-references

- `plans/SPAWN.md` — the `SPAWN` syscall, admit/parent-child wait link, and
  the `lib/rt` heap (SP5b) the growable registered tier depends on.
- `plans/FONT-SERVICE.md` — `fontd`, the `FONT_ENDPOINT` protocol, and the
  `login`-starts-`fontd` hack this plan deletes (§3/SVC-5).
- `plans/FIX-DESKTOP.md` §2.4 — why a launcher-as-parent breaks reaping (and
  why a service manager legitimately parents what it supervises).
- `plans/USERS.md` — the service accounts system services run as.
- `plans/WATCHDOG.md` — the health-check/liveness source for restart policy.
- `plans/DISPLAY.md` — seats / `display-present` readiness conditions.
- `plans/NETWORK.md` — `netstack` and the `network-up` readiness condition.
- `kernel/tairix-kernel/src/system_files.rs`, `lib/abi` `SystemConfigFile` —
  the whitelisted `/System/Settings` read path the registration store reuses;
  `enumerate_driver_store` — the discovery walk reused for `/System/Services`.
- `userland/system/init` (`service.rs`, `manager.rs`, `supervisor.rs`,
  `startup.rs`, `events.rs`) — the model this plan evolves in place.
- `AGENTS.md` §2.1, §2.2, §2.13, §2.14, §2.23, §4, §5.1, §5.2, §5.4, §9,
  §16.2, §16.3, §16.5, §16.6, §17.1, §17.3, §18.3, §18.5, §18.6, §19.4,
  §19.5, §21, §24.1, §24.2, §24.3, §24.4, §25, §26.2, §26.3.
