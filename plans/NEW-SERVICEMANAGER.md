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

- `service.rs` — `ServiceSpec { name, binary_path, manifest, dependencies }`
  and the `Spawner` / `Reaper` seams (pure core, host-tested).
- `manager.rs` (`Init`) — dependency **topological ordering**, `register`,
  `start_all`, `reap`, capability **intersection** (`ceiling ∩ manifest`,
  escalation refused), audit emission. `RunningService` tracks live PIDs.
- `supervisor.rs` — wait-any supervision with a **bounded per-entry
  crash-loop budget** (never `spawn`-in-a-loop, §2.1).
- `startup.rs` — the boot service set as a parsed, fail-closed
  `StartupConfig` (`DEFAULT_CONFIG`: `sysinfod`, `netstack`, `devmgr`,
  `seatmgr`, then `login` as the session), currently **compiled in** with a
  `MAX_SERVICES = 4` bound.
- `events.rs` — reserved audit event IDs in `9000..10000`
  (`SERVICE_STARTED/START_FAILED/DENIED/SKIPPED/EXITED`, `ORPHAN_REAPED`,
  `GRAPH_REJECTED`).

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

- **`MAX_SERVICES = 4`** (`startup.rs:65`) and its two tests
  (`more_than_max_services_fails_closed`, `exactly_max_services_is_accepted`)
  must move from a magic cap to a floor-sized fail-closed bound.
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

### SVC-1 — Kill the `MAX_SERVICES` cap; floor-sized fail-closed bound
- Size the floor parser to the actual boot-floor set; a config exceeding the
  floor fails closed. Update the two `startup.rs` tests. No behaviour change
  for the shipped floor.

### SVC-2 — Lifecycle + readiness protocol (`lib/abi` + engine)
- The `inactive → … → failed` state machine in the `init` policy core;
  the readiness-notification `lib/abi` call and named readiness conditions
  (`network-up`, `filesystems-mounted`, `boot-complete`, `display-present`,
  `seat-available`). Dependencies gate on `ready`, not `spawned`.
- Host tests: a dependent starts only after its dependency reports ready; a
  never-ready dependency leaves its dependent inactive (fail closed).

### SVC-3 — Discovery + registration store under `/System/Settings`
- Service discovery over `/System/Services/*.app`; the enrolment-record
  store at `/System/Settings/Services/` read fail-closed through the
  existing whitelisted `/System/Settings` read path; the `enable`/`disable`
  actions (system: build/install/update-path; user: per-user, ceiling-bound).
- Host tests: a present-but-unregistered bundle never auto-starts; a
  corrupt/missing store leaves nothing eligible; enrolment never widens
  authority.

### SVC-4 — On-demand endpoint activation + idle linger
- Capability-brokered connect → start-if-down → park-until-ready → hand back
  endpoint; sink refcount; one-shot tickless linger stop; bounded
  starting-request queue.
- Host + QEMU tests: connect starts a down service and parks until ready;
  last disconnect lingers then stops; a new connect cancels the linger.

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

### SVC-7 — Restart policy + health-check; stop/shutdown ordering
- `restart = never|on-failure|always` + bounded backoff + optional
  health-check (`plans/WATCHDOG.md`); graceful stop with grace timeout;
  reverse-dependency stop/shutdown ordering; system-shutdown sequence.

### SVC-8 — Control API + tool + audit + rlimits + docs/gate
- The versioned capability-checked control `lib/abi` surface, status via
  §16.6, the control tool, the extended `events.rs` IDs, per-service
  `rlimit` unit metadata enforced at spawn. Docs (`docs/src/userland/`,
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
