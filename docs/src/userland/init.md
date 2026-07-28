# PID 1 service manager (`userland/system/init`)

`tairix-init` is the first user-space process the kernel starts. It owns
the lifecycle of every long-running system service under
`/System/Services` (`AGENTS.md` §16.2): it brings them up in dependency
order, grants each one the capability set its signed manifest requests
intersected with init's own authority (`AGENTS.md` §5.2), and reaps the
children that any PID 1 inherits.

> **Planned evolution.** This page describes the PID 1 service manager as
> it stands. The first-class service manager it is growing into is
> specified in `plans/NEW-SERVICEMANAGER.md`, which evolves this model *in
> place* (`AGENTS.md` §2.2) — it is not a parallel manager. The service
> **lifecycle and readiness protocol** described below has landed
> (`NEW-SERVICEMANAGER.md` SVC-2); discovery-vs-registration vs on-demand
> endpoint activation, system- and per-user-scope managers, idle linger,
> and stop/shutdown ordering are still ahead.

The crate is `no_std` (with `alloc`), has no `unsafe`, and depends only on
the audited `lib/*` crates `tairix-abi`, `tairix-caps`, and `tairix-log`,
so a userland service never links a kernel or driver crate
(`AGENTS.md` §17.4). The installed binary lives at
`/System/Services/init`.

## The orchestrator, not a loader

`init` decides *what* runs, *in what order*, and *with what authority*. It
deliberately does **not** verify a service binary's signature, syscall-table
hash, or `rxe` envelope — that is the loader's job, the same pipeline
[`drvhost`](../drivers/host.md) runs for drivers (`AGENTS.md` §8). `init`
computes the capability ceiling and hands it, with the binary, to the
`Spawner` seam, which performs that verification before it executes
anything.

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
3. **Admit** each service whose prerequisites are met, in order:
   **decode** its manifest into a requested capability set, **gate** that
   request against init's authority, **spawn** it, and **audit** the
   outcome. Bring-up is an admission engine, not a single
   spawn-everything pass — see *Lifecycle and readiness* below.

When the graph is sound, init brings up every service whose prerequisites
can be met. A single service that fails — its manifest will not decode, it
over-requests authority, or the `Spawner` refuses it — is recorded, and the
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

## Capability granting (`AGENTS.md` §5.2)

A service's grant is the intersection of the capability set its signed
manifest requests with the authority init itself holds. init decodes the
manifest request with the single shared decoder
`tairix_abi::decode_capability_ids` (the same decoder `drvhost` uses, so
the manifest-body format has exactly one implementation — `AGENTS.md`
§2.2) and refuses any service whose request is **not a subset** of its
authority. Granting it would widen authority, so the service is denied
rather than narrowed silently (`AGENTS.md` §5.4.5). There is no ambient
authority: the `Spawner` receives the computed ceiling and may never add
to it (`AGENTS.md` §4).

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

- `Spawner::spawn(&ServiceSpec, &CapabilitySet) -> Result<Pid, Errno>` —
  the trusted loader that verifies and executes a service binary with at
  most the granted capability set.
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
| 9001 | `SERVICE_STARTED`      | Info  | a service was launched with its grant         |
| 9002 | `SERVICE_START_FAILED` | Warn  | manifest decode failed, or the spawn was refused |
| 9003 | `SERVICE_DENIED`       | Warn  | manifest over-requested authority             |
| 9004 | `SERVICE_SKIPPED`      | Warn  | a dependency failed, so the service was skipped |
| 9005 | `SERVICE_EXITED`       | Info  | a registered service exited and was reaped    |
| 9006 | `ORPHAN_REAPED`        | Info  | an inherited orphan was reaped                |
| 9007 | `GRAPH_REJECTED`       | Error | the service graph was structurally invalid    |
| 9008 | `SERVICE_READY`        | Info  | a service reached readiness, releasing dependents |
| 9009 | `CONDITION_SATISFIED`  | Info  | a named readiness condition became satisfied  |
| 9010 | `NOTIFY_REJECTED`      | Warn  | a readiness notice named an unknown or non-starting service |

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
§2.2.) It links **only** the runtime and its own startup-config parser,
never the orchestrator library above: dragging that crate's `alloc` +
crypto dependency chain into a banner-printing program would be the bloat
`AGENTS.md` §2.3 forbids, so the shipped program contains no crypto code at
all and no `unsafe`.

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

### Session supervision (`plans/PI.md` P6e-3b-ii)

Once the banner has landed, the `Run` binary does not exit — it
**supervises** the `session` program for the lifetime of PID 1, owning its
lifecycle rather than spawning it and forgetting it. Each cycle of the
supervise loop:

1. **launches** the session with the `spawn` syscall — a separate,
   hardware-isolated process (a true `spawn`, not an `exec`-style hand-off,
   `AGENTS.md` §4), so PID 1 keeps running. A negative result is fail-loud
   but never fatal to the boot (`AGENTS.md` §2.24): the refusal is written
   to `stderr` (`Sessions::report_launch_failure`) and only that entry's
   slot is abandoned — the remaining services and sessions keep running,
   so one refused bundle cannot take down the device manager or every
   login session with it;
2. **blocks** on exactly that child with the `wait` syscall
   (`plans/SPAWN.md` SP6), reaping it when it exits so it never lingers as a
   zombie. A negative `wait` — the supervisor cannot reap its own child — is
   surfaced as `EXIT_WAIT_FAILED` rather than continuing blindly;
3. **relaunches** it, up to a small `SESSION_SPAWN_BUDGET` of launches.

That bound is a **crash-loop guard**, not a fixed restart count: a session
that blocks on input runs for PID 1's whole life and never approaches it
(the supervisor blocks in `wait`); a session that exits the instant it
starts — e.g. no input backing is attached — would otherwise make the loop
a busy `spawn` spin, which `AGENTS.md` §2.1 forbids, so after the budget is
spent `init` stops and exits `EXIT_SESSION_EXHAUSTED` (a session that
cannot stay up means the system cannot come up — fail closed, `AGENTS.md`
§2.9). The reaped child's exit code is read but not yet acted on; a policy
that tells a clean logout from a crash (and resets the budget on a session
that ran long enough) awaits a clock/session-state ABI.

## Tests

`cargo test -p tairix-init` drives the manager against an in-memory
`Spawner`/`Reaper` and a recording log sink, covering dependency-ordered
start, the fail-closed missing-dependency and cycle paths, duplicate
registration, the capability grant as `request ∩ authority`, an
escalation denial, a spawn failure cascading to its transitive
dependents, an invalid manifest, and the reaper distinguishing a service
exit from an inherited orphan. The readiness protocol is covered too: a
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
