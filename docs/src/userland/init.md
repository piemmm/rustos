# PID 1 service manager (`userland/system/init`)

`tairix-init` is the first user-space process the kernel starts. It owns
the lifecycle of every long-running system service under
`/System/Services` (`AGENTS.md` §16.2): it brings them up in dependency
order, launches each one as its own **service account**, and reaps the
children that any PID 1 inherits. The **kernel** is the single authority
over a service's capabilities — it grants `manifest ∩ account-ceiling` from
the signed bundle at load time (`AGENTS.md` §5.2, §8); init names the binary
and the account, never a capability set.

> **Planned evolution.** This page describes the PID 1 service manager as
> it stands. The first-class service manager it is growing into is
> specified in `plans/NEW-SERVICEMANAGER.md`, which evolves this model *in
> place* (`AGENTS.md` §2.2) — it is not a parallel manager. The service
> **lifecycle and readiness protocol** (`NEW-SERVICEMANAGER.md` SVC-2) and
> **discovery + the fail-closed enrolment registry** (SVC-3) described
> below have landed, as has the **authority-scope boundary** that confines
> a per-user manager to its own user (SVC-6, *Authority scope* below). The
> engine cores of on-demand endpoint activation, idle linger, and
> stop/shutdown ordering are in place; binding them to a live transport
> (and spawning the per-user manager at session start) is still ahead.

The crate is `no_std` (with `alloc`), has no `unsafe`, and depends only on
the audited `lib/*` crates `tairix-abi`, `tairix-caps`, and `tairix-log`,
so a userland service never links a kernel or driver crate
(`AGENTS.md` §17.4). The installed binary lives at
`/System/Services/init`.

## The orchestrator, not a loader

`init` decides *what* runs, *in what order*, and *when*. It deliberately
does **not** verify a service binary's signature, syscall-table hash, or
`rxe` envelope, and it is **not** the capability authority: verifying the
binary and granting `manifest ∩ account-ceiling` are the loader/kernel's
job, the same pipeline [`drvhost`](../drivers/host.md) runs for drivers
(`AGENTS.md` §8). `init` names the binary and the service account and hands
both to the `Spawner` seam; the kernel derives and enforces the grant.

## Bring-up pipeline

`Init::start_all` runs the registered service set through a fixed pipeline
that **fails closed** (`AGENTS.md` §5.4.5):

1. **Order** the services by their declared dependencies (Kahn
   topological sort, ready services emitted in registration order so the
   result is deterministic — `AGENTS.md` §18.3).
2. **Reject the whole graph** — starting nothing — if it is structurally
   invalid: a dependency names an unregistered service
   (`DependencyMissing`) or the graph contains a cycle
   (`DependencyCycle`). The system never boots a partial, surprising
   configuration.
3. **Admit** each service whose prerequisites are met, in order: **spawn**
   it as its service account and **audit** the outcome. The kernel derives
   and enforces its capability grant from the signed bundle; init passes no
   capability set. Bring-up is an admission engine, not a single
   spawn-everything pass — see *Lifecycle and readiness* below.

When the graph is sound, init brings up every service whose prerequisites
can be met. A single service that fails — the kernel's load gate refuses
the spawn (a bad manifest, a capability beyond the account's ceiling, or
another load failure) — is recorded, and the
services that *transitively depend on it* are skipped; services
independent of the failure still start. Each admission pass returns a
`StartReport` (`started` + `failed`), so a caller can see what came up and
which optional services are absent without the boot aborting.

## Lifecycle and readiness (`NEW-SERVICEMANAGER.md` SVC-2)

A service manager that treats *spawned* as *up* cannot honestly start a
dependent that needs "the network is up" — the dependency may be running
but not yet functional. `init` therefore tracks each service through the
lifecycle `tairix_abi::ServiceState`
(`inactive → starting → ready → running → stopping → stopped | failed`)
and releases a dependent only once every dependency it names is
`ServiceState::is_ready` (`ready` or `running`), never merely spawned.

- **Readiness kind.** A service declares how it reaches readiness
  (`tairix_abi::ReadinessKind`, unit metadata read from its manifest). An
  `immediate` service (the default, the analogue of systemd `Type=simple`)
  is `ready` the instant its spawn succeeds; a `notify` service
  (`Type=notify`) stays `starting` until it announces itself up.
- **Readiness notification.** A `notify` service sends a
  `tairix_abi::ReadyNotice` — an `sd_notify` analogue carrying a
  `LifecycleSignal` (`ready` or `failed`) and **no identity**: the manager
  attributes the notice to the kernel-attested sender, never a
  caller-supplied name (`AGENTS.md` §5.4). `Init::notify` applies it —
  `ready` releases the service's dependents, `failed` marks it failed and
  skips the dependents blocked on it — and fails closed (`NotifyError`) on
  an unknown service or a notice for one that is not `starting`.
- **Named readiness conditions.** A service may `require` and `provide`
  named conditions (`tairix_abi::ReadyCondition`: `network-up`,
  `filesystems-mounted`, `boot-complete`, `display-present`,
  `seat-available`). The manager admits a service only once every
  condition it requires is satisfied; a condition is satisfied when a
  providing service becomes ready or when the manager/kernel signals it
  through `Init::satisfy_condition`. Conditions decouple readiness from a
  service name — a client requires `network-up` without naming
  `netstack` — and generalise the headless case: a GUI-only service that
  requires `display-present` simply never activates on a headless boot,
  because nothing ever satisfies that condition (`AGENTS.md` §17.3).

Bring-up runs this as an admission fixpoint: it repeatedly admits every
service whose dependencies are ready and whose required conditions are
satisfied, and skips every service a failed dependency blocks, until
nothing changes. A chain of `immediate` services comes up in one pass; a
`notify` service pauses the chain until it announces readiness, and a
dependency that never reports ready simply leaves its dependents
`inactive` — fail closed, never a guess (`AGENTS.md` §5.4).

## Discovery and the enrolment registry (`NEW-SERVICEMANAGER.md` SVC-3)

Unlike a driver, a service has no natural activation gate — a driver's is
"the hardware is physically present and matched" (`AGENTS.md` §18.3), but
merely dropping a signed service bundle on disk must **not** make a live
service appear (that would be an ambient-authority-shaped risk). The
lifecycle is therefore split into three distinct steps (the systemd
*present-vs-enabled* split, given the capability framing):

1. **Discovery** — *what bundles exist*: a scan of `/System/Services/*.app`
   reading each signed `AppInfo`, reusing the same store walk drivers use
   (`AGENTS.md` §18.5/§18.6) — no compiled-in list of which services exist
   beyond the boot floor.
2. **Registration / enablement** — *is a discovered bundle eligible* to be
   brought up. This is an explicit, recorded decision in the **enrolment
   registry** (`tairix_init::registry`), never implied by presence.
3. **Activation** — actually starting an eligible service, through the
   bring-up engine above.

The enrolment registry (`registry::Enrolment`) is the parsed set of
enabled service names for one scope. The **system** store is read from
`/System/Settings/Services/enabled`
(`tairix_abi::driver_store::SystemConfigFile::SystemServices`) off the
always-mounted read-only `/System` through the same confined, fail-closed
pre-unlock read path the device manager already uses for its
configuration — no new read primitive (`AGENTS.md` §16.2). A **per-user**
store lives under the user's own `/Users/<u>/Settings/Services/` and parses
identically.

- **The store holds only enrolment records — not unit metadata.** A
  service's restart policy, activation mode, linger, dependencies,
  readiness conditions, and rlimits live in its **signed `AppInfo`
  manifest** (`AGENTS.md` §16.5), so tampering is a load refusal rather
  than a silent behaviour change. The store records only the decision
  "this bundle is enabled", keyed to the bundle; duplicating unit metadata
  into a separately-writable store would be both the duplication
  `AGENTS.md` §2.2 forbids and a place authority could be raised.
- **`Init::register_enrolled` registers only enrolled bundles.** Given the
  discovered set (each parsed into a `ServiceSpec` by the loader seam) and
  the enrolment record, it registers a bundle for bring-up **only** if it
  is enabled; a present-but-unenrolled bundle is never registered and its
  skip is audited (`SERVICE_NOT_ENROLLED`). Presence on disk grants no
  eligibility (`AGENTS.md` §4).
- **Fail closed.** The store text is untrusted input: parsing rejects a
  malformed name (a strict lowercase-`[a-z0-9._-]` bundle identifier, so a
  `..`- or path-traversal-shaped token can never be enrolled) or a
  duplicate, and the caller resolves both a **corrupt** and a **missing**
  store to the empty enrolment — nothing is eligible, never a guess
  (`AGENTS.md` §5.4, §2.9).
- **`enable` / `disable` never widen authority.** `registry::enrol` takes
  the enroller's capability ceiling and the service's signed manifest and
  **refuses** (`CapabilityEscalation`) to enable a service whose manifest
  requests authority beyond that ceiling, so a user enabling a service in
  their own scope can never make it eligible to run with more authority
  than they hold (`AGENTS.md` §5.2). `disable` (`registry::unenrol`) needs
  no capability — removing eligibility only narrows authority — but fails
  closed if the service was not enrolled. Both are pure transforms that
  return the new record for the caller to write back through the
  appropriate trusted-path store; the kernel still derives the grant
  (`manifest ∩ account-ceiling`) from the signed bundle at start regardless
  (below).

## Capability authority (`AGENTS.md` §5.2)

The **kernel** is the single authority over a service's capabilities. When
it loads the service binary it reads the signed bundle manifest and grants
the intersection of the manifest's request with the **service account's**
ceiling — the very model [`drvhost`](../drivers/host.md) uses for drivers
(`AGENTS.md` §8, §18.6). `init` names only the binary and the account uid;
it never decodes a manifest or computes a grant on the launch path, so no
second capability-derivation path can drift from the kernel's and there is
no ambient authority (`AGENTS.md` §4). A load the kernel refuses (a bad
manifest, or a request beyond the account's ceiling) surfaces to init as a
`SpawnFailed`, exactly like any other refused load.

The one place init decodes a manifest is the **enrolment** path
(`registry::enrol`, the registered/user tier): it refuses to *enable* a
service whose request exceeds the enroller's ceiling, using the single
shared decoder `tairix_abi::decode_capability_ids` (the same decoder
`drvhost` uses — one implementation of the manifest-body format,
`AGENTS.md` §2.2). That is a decision about *eligibility*, recorded ahead
of time; the authoritative grant is still the kernel's at load.

## Authority scope (`NEW-SERVICEMANAGER.md` SVC-6)

TAIRiX runs **one policy engine at two authority scopes**, never two
codebases (`AGENTS.md` §2.2): the single **system** service manager (PID 1's
role) and one **per-user** manager instance per logged-in user, spawned by
the system manager at session start and delegated only that user's
authority. Each `Init` instance carries its `tairix_init::AuthorityScope`
(`System`, or `User { uid }`) — chosen once at construction and never
changed — and it is the fixed security boundary between the two roles.

Because a service is always launched **as a service account** (a uid) and
the kernel derives its grant from that account's ceiling (above), a manager
confined to one user can only be permitted to manage services that run **as
that user**. `Init::register` enforces this before it touches any state
(capability check before state, `AGENTS.md` §5.4): a `User { uid }` manager
registers a service only if the spec's account equals `uid`, and the system
manager permits any account. A spec naming a different account — a system
service account, or another user's uid — is refused (`ScopeViolation`) and
audited (`SERVICE_SCOPE_REJECTED`), so a per-user manager can neither raise
a service to system authority nor reach into another user's services
(`AGENTS.md` §4, §5.2). `register_enrolled` inherits the same check, so even
a positively-enrolled bundle whose account is out of scope fails closed
before any service starts.

This is deliberately an **identity** check on the account the spec already
names, not a capability computation: the engine never decodes a manifest or
derives a grant on the launch path, so there is no second
capability-derivation path to drift from the kernel's authoritative one (the
enrolment-ceiling check above is the only place init reads a manifest, and
it governs eligibility, not the grant).

## Control surface (`NEW-SERVICEMANAGER.md` SVC-8)

Beyond boot bring-up, a running service is driven through a
capability-gated **control surface** — the `systemctl` analogue. Its wire
contract is the reserved synchronous call endpoint
`tairix_abi::service_control::SERVICE_CONTROL_ENDPOINT` (a fixed-size,
bounds-checked `ServiceControlRequest` — a `ServiceControlOp` (`start` /
`stop`) plus a bounded, UTF-8-validated service name — and a status-framed
reply carrying the resulting `ServiceState`). Persistent enablement
(`enable`/`disable`, which mutates the enrolment registry) and observability
(`status`, served through the System Information API, `AGENTS.md` §16.6) are
separate concerns and are **not** carried on this endpoint.

The engine side is `Init::control`, which dispatches a decoded request to
`Init::start_service` (`start`) or `Init::stop` (`stop`). **Authorization is
the endpoint's, not the dispatch's** (`AGENTS.md` §5.2): the kernel gates
*reaching* a manager's control endpoint on the send capability the manager
binds it with, so the receiver does not re-check a caller capability — it
validates the request against the strict service-name policy
(`registry::validate_service_name`, so a path-traversal- or
case-collision-shaped name never matches a service) and applies it, failing
closed and auditing every refusal (`ControlError`):

- `start` brings a down (`inactive`/`stopped`/terminally `failed`) service
  up now, exactly like a boot admission — spawned as its own account,
  marked ready if `immediate`, its order-dependents released — but **only**
  when every readiness condition it requires is satisfied, so a
  `display-present`-gated GUI service fails closed on a headless system
  (`ControlError::Unavailable`) rather than being guessed into life
  (`AGENTS.md` §17.3). It is idempotent for a service already coming up or
  up (returns the current state, no respawn), cancels a pending restart
  backoff first (an explicit start supersedes a queued relaunch), and
  reports a kernel-refused spawn as `ControlError::NotStartable`.
- `stop` tears the service and its transitive dependents down gracefully in
  reverse-dependency order (reusing `Init::stop`; a stop is never fought
  with a relaunch).

An unknown or policy-invalid name fails closed as
`ControlError::UnknownService`. This is the **engine core**; the live
transport — a per-manager wait-set reactor that serves the endpoint
alongside child reaping and one-shot timers, the `servicectl` control tool
that holds the send capability, and the `CAP_SERVICE_CONTROL` grant that
gates it — lands with the loader/kernel transport seam (SVC-5/SVC-8), the
same staging the readiness (`notify`), activation (`connect`), and restart
paths follow.

## Liveness watchdog

A crashed service is one that *exited*; a **wedged** one is still present but
no longer making progress (a deadlocked driver, a serve loop stuck on a
hardware access that never returns). The manager recovers a wedge into the
same restart path a crash uses, so `plans/FIX-IO.md`'s goal — "the driver,
not the disk, is the problem" must never lock up the system — is met without
a second restart engine (`AGENTS.md` §2.2). This is the analogue of systemd's
`WatchdogSec`.

A service opts in through a non-zero `watchdog` interval in its signed unit
metadata (`tairix_abi::ServiceUnit::watchdog`; `Duration64::ZERO`, the
default, opts out; a negative interval fails the manifest closed). Once such a
service is running, `Init::arm_watchdogs(now)` arms a single one-shot deadline
`now + interval`; the service must renew it at least that often by calling
`Init::heartbeat(name, now)` ("I am still making progress"). A heartbeat is a
high-frequency steady-state signal and is deliberately **not** audited. If the
deadline elapses with no heartbeat, `Init::expire_watchdog` force-terminates
the wedged process and marks it as a watchdog kill, so `Init::reap` classifies
the resulting exit as an **abnormal failure** — regardless of the exit code
the forced termination reports — and feeds it to the *existing* restart
policy, backoff, and crash-loop budget. A wedged `on-failure`/`always` service
is relaunched (bounded by the same crash-loop guard, so a process that wedges
the instant it starts is eventually left down, never relaunched forever); a
`never` service is killed and left down, loudly. A deliberate `stop` disarms
the watchdog first, so a graceful teardown is never mistaken for a wedge.

This is the **engine core**. The live heartbeat transport — a supervised
driver/daemon renewing its heartbeat to its manager, and the reactor arming
the real one-shot off `Init::watchdog_deadline` and calling
`Init::expire_watchdog` when it fires — lands with the same SVC-5/SVC-8
control transport as the control surface above; the engine is proven
host-side first, exactly as the readiness, activation, and restart paths were.

## Reaping

A PID 1 must reap the zombies of the whole system — both the services it
started and the orphans it inherits when their parent dies. `Init::reap`
drains the `Reaper` seam: a reaped process that matches a running service
is logged as a service exit and dropped from the running set; any other
reaped process is an inherited orphan and is logged as such. Neither path
panics (`AGENTS.md` §2.9).

## The seams

The two operations that touch the outside world are injected, mirroring
the `drvhost` host configuration:

- `Spawner::spawn(&ServiceSpec) -> Result<Pid, Errno>` — the trusted loader
  that verifies and executes a service binary as the account the spec
  names; the kernel derives its capability grant from the signed bundle
  (init passes no capability set).
- `Stopper::{request_stop, force_terminate}(Pid)` — the two-phase graceful
  stop: ask the service to exit, then force it down after its grace period.
- `Reaper::collect() -> Option<ReapedChild>` — the source of
  exited-child notifications, draining the kernel wait queue on a running
  system.

On a running kernel these are backed by syscalls; in tests they are
in-memory fixtures. Splitting the seams from the manager keeps the
security-relevant ordering and capability code independent of kernel
plumbing and exhaustively testable.

## Audit events

`init` owns the reserved `EventId` range `9000..10000`
(`AGENTS.md` §2.5, §19.4):

| Id   | Constant               | Level | Meaning                                       |
|------|------------------------|-------|-----------------------------------------------|
| 9001 | `SERVICE_STARTED`      | Info  | a service was launched as its account (the kernel granted its caps) |
| 9002 | `SERVICE_START_FAILED` | Warn  | the kernel's load gate refused the spawn      |
| 9004 | `SERVICE_SKIPPED`      | Warn  | a dependency failed, so the service was skipped |
| 9005 | `SERVICE_EXITED`       | Info  | a registered service exited and was reaped    |
| 9006 | `ORPHAN_REAPED`        | Info  | an inherited orphan was reaped                |
| 9007 | `GRAPH_REJECTED`       | Error | the service graph was structurally invalid    |
| 9008 | `SERVICE_READY`        | Info  | a service reached readiness, releasing dependents |
| 9009 | `CONDITION_SATISFIED`  | Info  | a named readiness condition became satisfied  |
| 9010 | `NOTIFY_REJECTED`      | Warn  | a readiness notice named an unknown or non-starting service |
| 9011 | `SERVICE_NOT_ENROLLED` | Info  | a discovered bundle was skipped because it is not enrolled |

(On-demand-activation and stop/linger events `9012`–`9017`, the restart and
scope events `9018`–`9020`, the control-surface events `9021`
(`SERVICE_CONTROL_STARTED`), `9022` (`SERVICE_CONTROL_STOPPED`), and `9023`
(`SERVICE_CONTROL_DENIED`), and the liveness-watchdog events `9024`
(`SERVICE_WATCHDOG_ARMED`, Info) and `9025` (`SERVICE_WATCHDOG_TIMEOUT`, Warn —
a wedged process was force-killed and fed to its restart policy) are defined in
`src/events.rs`. `9003` is retired: init no longer decides capability
grants — the kernel is the single authority and records its own denial — so
there is no init-side capability-denial event; the number is left a gap,
never reused.)

## The `Run` entry-point binary and startup config (`plans/PI.md` P6b)

Everything above describes the orchestrator *library*. The same package
also builds the `init` application bundle's `Run` entry-point binary
(`src/run.rs`, `AGENTS.md` §16.5) — the program the kernel spawns as PID 1
the moment it reaches user mode (`plans/PI.md` P6c, the "boot into user
mode" milestone).

That binary is a **pure-Rust freestanding program**. TAIRiX is Rust-only
(`AGENTS.md` §1), so it links the pure-Rust userland runtime `tairix-rt` —
never the C ABI (`crt0` + `abi-sys`), which exists solely for programs
**not** written in Rust (`AGENTS.md` §16.4). `tairix-rt` provides the
program's `_start`, the §19.2 stack canary, the panic handler, and
idiomatic syscall wrappers; `tairix_rt::entry!` names the program's
`main`. `main` renders the startup banner from the kernel-attested
`boot_facts_get` machine summary — `TAIRiX <version>: <installed memory>`
(whole MiB rounded to nearest, whole GiB above 100 GiB), a blank line, then
`<CPU name>, <n> core(s)` (e.g. `ARM Cortex-A72, 4 cores`), falling back to
`Unknown <arch> processor, <n> core(s)` when the kernel discovered no CPU
model; a kernel that installed no facts
degrades the banner to the version line with the reason on fd 2, never a
fabricated machine shape — and writes it to its inherited standard
output (fd 1) through `tairix_rt::stdout` — the `abi-v1` `stream_write`
syscall (`AGENTS.md` §20; `init` binds to the inherited stream, never an
ambient device) — then **supervises** the user's session (see below). The
runtime routes `main`'s return value through the `exit` syscall. (Both the
Rust runtime and the C ABI reach the kernel through the one shared trap,
`tairix-abi-trap`, so the trap assembly is not duplicated — `AGENTS.md`
§2.2.) It links the pure-Rust orchestrator library above and drives it live
over the `lib/rt` userland heap (`plans/NEW-SERVICEMANAGER.md` SVC-A): the
`Run` binary builds an `Init` engine over the real syscall seams — `spawn_as`
for launching a service as its account, `signal` for the graceful-then-forced
stop, `log_emit` (via `tairix_rt::LogSink`) for the audit sink, and a small
`LoopReaper` mailbox the wait loop fills so `Init::reap` drains an exited
child without a second `wait` — never a second, parallel service manager
(`AGENTS.md` §2.2). The tiny startup-config parser still lives alongside the
binary (`src/startup.rs`) rather than in the library.

What `init` should do at user-mode entry is **data, not control flow**: a
small, fail-closed startup config (`src/startup.rs`). The config is
line-oriented; `#` begins a comment, and blank or comment-only lines are
ignored. Three directives are defined; each names the compiled-in system
account its program runs as, resolved to a uid at parse time:

- `console` — open the system console so the banner (and later output)
  has somewhere to go. Takes no argument. Required once.
- `session <path> <account>` — the absolute path of the program `init`
  launches as the user's session (the login service
  `/System/Services/login.app/Run`, `plans/PI.md` P11, which
  authenticates the user and spawns their shell of choice) and the
  account it runs as. Required once. `init` launches it through the
  process-spawn syscall (`plans/PI.md` P6d) and supervises it (below).
- `service <path> <account>` — a long-running system service `init`
  launches at startup and supervises for the life of the system, and the
  account it runs as. Optional and repeatable; the declaration order is
  the launch order. This is the compiled-in **boot floor**
  (`AGENTS.md` §18.6); services past the floor are discovered and
  registered rather than named here (`plans/NEW-SERVICEMANAGER.md`).

Because the config is the first thing a freshly spawned program reads, the
parser treats it as untrusted input (`AGENTS.md` §19.5): it is
allocation-free, borrows from its source text, and **fails closed** with a
`ConfigError` — refusing an unknown or duplicated directive, a directive
given the wrong argument, a non-absolute `session` path, an over-long
config, or an omitted required directive — rather than guess at a
malformed intent (`AGENTS.md` §2.9, §5.4.5).

### Boot-floor services and session supervision (`plans/PI.md` P6e-3b-ii, `plans/NEW-SERVICEMANAGER.md` SVC-A)

Once the banner has landed, the `Run` binary brings the **boot-floor
services** up through the `Init` engine and then **supervises** the login
sessions for the lifetime of PID 1, owning both rather than spawning and
forgetting them.

First it registers each `service` directive with the engine — named by its
`.app` bundle stem, run as the account the directive resolved — and calls
`start_all`, which brings them up in dependency order through the
readiness-gated admission engine above (the floor declares no dependencies,
so all start immediately). PID 1 names only each service's binary and its
account; the kernel — the single capability authority — verifies the signed
bundle and grants `manifest ∩ ceiling` at load time. A service the kernel
refuses to launch is reported on `stderr` and skipped, and the boot
continues with the rest — one dead service never takes down the device
manager, the other services, or the login sessions (`AGENTS.md` §2.24).

Then it runs one wait-any supervision loop that owns the per-console login
sessions directly and routes everything else to the engine:

1. it **launches** one `session` (login) process per installed text console
   with the `spawn` syscall — a separate, hardware-isolated process (a true
   `spawn`, not an `exec`-style hand-off, `AGENTS.md` §4). A refused launch
   is written to `stderr` (`Sessions::report_launch_failure`) and only that
   console's slot is abandoned; the other consoles' sessions keep running;
2. it **blocks** on any child with the `wait` syscall (`plans/SPAWN.md`
   SP6). A reaped pid that is a live session slot is relaunched on **its
   own** console, up to a small `SESSION_SPAWN_BUDGET`; every other reaped
   pid — a service the engine started, or an inherited orphan — is handed to
   the engine (`Init::reap`), which moves a known service to a terminal
   state (scheduling any policy-driven restart) or logs the orphan. A
   negative `wait` — the supervisor cannot reap its own child — is surfaced
   as `EXIT_WAIT_FAILED` rather than continuing blindly.

The per-console budget is a **crash-loop guard**, not a fixed restart count:
a session that blocks on input runs for PID 1's whole life and never
approaches it; a session that exits the instant it starts would otherwise
make the loop a busy `spawn` spin, which `AGENTS.md` §2.1 forbids, so after
its budget is spent that console is abandoned. `init` declares
`EXIT_SESSION_EXHAUSTED` only when **no** session is alive **and** the
engine holds no running service — so a perpetual service (e.g. `devmgr`,
parked in `hw_tree_wait`) keeps PID 1 up for the life of the system even
after every console's session has been abandoned (fail closed, `AGENTS.md`
§2.9). A service's exit is reaped through its manifest restart policy; a
session's exit code is not yet acted on (a clean-logout-vs-crash policy
awaits a session-state ABI).

## Tests

`cargo test -p tairix-init` drives the manager against an in-memory
`Spawner`/`Reaper` and a recording log sink, covering dependency-ordered
start, the fail-closed missing-dependency and cycle paths, duplicate
registration, a spawn failure (the kernel's refused load) cascading to its
transitive dependents, and the reaper distinguishing a service
exit from an inherited orphan, and `register_enrolled` registering only
enrolled bundles while auditing (and never starting) a present-but-
unenrolled one. The enrolment registry has its own unit tests: fail-closed
parsing of a corrupt store, the empty (missing-store) case, strict name
validation rejecting path-traversal and case-collision shapes, the
canonical-text round trip, idempotent `enrol`, `enrol` refusing a request
that exceeds the enroller's ceiling, and `unenrol` failing closed on an
absent service. The readiness protocol is covered too: a
`notify` dependency gates its dependent until it reports ready, a
never-ready dependency leaves its dependent `inactive`, a required named
condition gates a start until satisfied, a provided condition is satisfied
when its provider becomes ready, an explicit `failed` signal skips
dependents, and a notice for an unknown or non-starting service fails
closed. These sit alongside the `EventId` range and uniqueness
invariants and the numeric audit-field formatter. The same run also
covers the `Run` binary's startup-config parser: the default config, the
comment/blank-line/inline-comment and whitespace handling, and every
fail-closed rejection path.

The `Run` binary's freestanding entry is exercised end to end under QEMU
when the production boot path spawns it into EL0: the
`spawn_init_qemu_aarch64` `-M virt` vertical proves the EL0 transition and
the banner (`plans/PI.md` P6c-3), and the `spawn_session_qemu_aarch64`
vertical proves the session supervision — PID 1 launches the session,
`wait`s on and reaps it when it exits, and relaunches it (`plans/PI.md`
P6e-3b-ii).
