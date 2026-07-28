# `tairix-init` — PID 1 service manager

Stage 6 deliverable (`AGENTS.md` §5.2, §16.2). The first user-space
process the kernel starts. It owns the lifecycle of every long-running
system service under `/System/Services`: dependency-ordered, readiness-gated
start; reaping the whole system's zombies; and on-demand activation.
Installed to `/System/Services/init`.

**The kernel is the single capability authority.** init names a service's
binary and its **service account** (a uid); the kernel loads the signed
bundle and grants `manifest ∩ account-ceiling` at load time — the same gate
`drvhost` runs for drivers (`AGENTS.md` §8/§18.6). init never decodes a
manifest or computes a grant on the launch path, so no second
capability-derivation path can drift from the kernel's.

## What this crate is

The **orchestrator** — it decides what runs, in what order, and when. It is
*not* a loader and *not* the capability authority: verifying a service
binary's signature, syscall-table hash, and `rxe` envelope, and granting
`manifest ∩ account-ceiling`, are all the kernel's job at load time (the same
pipeline `drvhost` runs for drivers, `AGENTS.md` §8). `init` names the binary
and the service account; the kernel derives and enforces the grant.

## Bring-up pipeline (`Init::start_all`)

Fails closed at the first problem (`AGENTS.md` §5.4.5):

1. Order the registered services by their declared dependencies (Kahn
   topological sort; ready services in registration order, so the result
   is deterministic — `AGENTS.md` §18.3).
2. Reject the whole graph — starting nothing — if a dependency names an
   unregistered service (`DependencyMissing`) or the graph has a cycle
   (`DependencyCycle`).
3. Per service, in order (once its dependencies are ready and its required
   readiness conditions hold): spawn it as its service account and audit the
   outcome. The kernel derives and enforces its capability grant from the
   signed bundle; init passes no capability set.

When the graph is sound, init starts every service it can; a service that
fails is recorded and its transitive dependents are skipped, without
aborting independent services. The outcome is a `StartReport`
(`started` + `failed`).

## Capability authority (`AGENTS.md` §5.2)

The **kernel** is the single authority over a service's capabilities. When
it loads the service binary it reads the signed bundle manifest and grants
the intersection of the manifest's request with the **service account's**
ceiling — exactly the model `drvhost` uses for drivers (`AGENTS.md` §8,
§18.6). init names only the binary and the account uid, so there is no
init-side derivation to keep in step with the kernel's and no ambient
authority (`AGENTS.md` §4). The enrolment path (`registry::enrol`, the
registered/user tier) still decodes a manifest it is *given* to refuse
enabling a service whose request exceeds the enroller's ceiling, using the
shared `service::decode_manifest_capabilities` decoder (`AGENTS.md` §2.2).

## Reaping (`Init::reap`)

PID 1 reaps the whole system's zombies. A reaped process matching a
running service is logged as a service exit and dropped from the running
set; any other reaped process is an inherited orphan and logged as such.
Neither path panics (`AGENTS.md` §2.9).

## Seams

Injected, mirroring the `drvhost` host configuration:

- `Spawner::spawn(&ServiceSpec) -> Result<Pid, Errno>` — the trusted loader
  that verifies and executes a service binary as the account the spec names;
  the kernel derives its capability grant from the signed bundle (init passes
  no capability set).
- `Stopper::{request_stop, force_terminate}(Pid)` — the two-phase graceful
  stop (request, then force after the grace period).
- `Reaper::collect() -> Option<ReapedChild>` — exited-child notifications.

On a running kernel both are syscall-backed; in tests they are in-memory
fixtures.

## Audit events

Reserved `EventId` range `9000..10000`:

- `9001 SERVICE_STARTED` — a service was launched as its account; the kernel
  granted its capabilities from the signed bundle (Info).
- `9002 SERVICE_START_FAILED` — the kernel's load gate refused the spawn (a
  bad manifest, a capability beyond the account's ceiling, or another load
  failure) (Warn).
- `9004 SERVICE_SKIPPED` — a dependency failed, so the service was skipped (Warn).
- `9005 SERVICE_EXITED` — a registered service exited and was reaped (Info).
- `9006 ORPHAN_REAPED` — an inherited orphan was reaped (Info).
- `9007 GRAPH_REJECTED` — the service graph was structurally invalid (Error).

(Readiness, on-demand-activation, and stop/linger events `9008`–`9017` are
documented in `src/events.rs`. `9003` is retired: init no longer decides
capability grants, so there is no separate init-side capability-denial
event — the kernel records its own denial. The number is left a gap, never
reused.)

## The `Run` entry-point binary (`plans/PI.md` P6b)

The package also builds the `init` application bundle's `Run` entry-point
binary (`src/run.rs`, `AGENTS.md` §16.5) — the program the kernel spawns
as PID 1 when it reaches user mode (`plans/PI.md` P6c). It is a **pure-Rust**
program: TAIRiX is Rust-only (`AGENTS.md` §1), so it links the pure-Rust
userland runtime `tairix-rt` — never the C ABI (`crt0` + `abi-sys`), which
exists solely for programs **not** written in Rust (`AGENTS.md` §16.4).
`tairix-rt` provides `_start`, the §19.2 stack canary, the panic handler, and
the syscall wrappers; `tairix_rt::entry!` names the program's `main`. `main`
parses a compiled-in **startup config** (`src/startup.rs`) and writes its
first banner line through the `abi-v1` `console_write` syscall
(`tairix_rt::console_write`, the P6a syscall).

It links **only** the runtime and its own startup-config parser — never the
orchestrator library above, whose `alloc` + crypto dependency chain has no
place in a banner-printing program (`AGENTS.md` §2.3). The parser therefore
lives alongside the binary rather than in the library, and the shipped
program contains no crypto code. On the host the binary is an inert stub so
`cargo build --workspace`, clippy, and fmt cover it; the parser's logic is
host-tested directly.

### Startup config (`src/startup.rs`)

A tiny, allocation-free, fail-closed text format the program reads at
user-mode entry. Lines are directives; `#` begins a comment; blank and
comment-only lines are ignored. Two directives are defined and each is
required exactly once:

- `console` — open the system console (no argument).
- `session <path>` — the absolute path of the program `init` launches as
  the user's session (the shell). Launching it needs the process-spawn
  syscall (`plans/PI.md` P6d) and a shell (P6e); until then the path is
  validated as parsed, not launched.

The parser refuses anything it does not fully understand — an unknown or
duplicated directive, a directive given the wrong argument, a non-absolute
`session` path, an over-long config, or an omitted required directive —
returning a `ConfigError` and starting nothing (`AGENTS.md` §2.9, §5.4.5).

## Layering & safety

The orchestrator library is `no_std` (with `alloc`) and depends only on
`tairix-abi`, `tairix-caps`, and `tairix-log` (all `lib/*`), so a userland
service never links a kernel or driver crate (`AGENTS.md` §17.4). No
`unsafe`, no `unwrap`/`expect`/`panic!` in production paths
(`AGENTS.md` §2.9). The `Run` binary is pure safe Rust — it contains **no**
`unsafe`; the `_start`/trap plumbing lives behind `tairix-rt`'s safe API.

## Test surface

`cargo test -p tairix-init` covers the orchestrator engine —
dependency-ordered start; fail-closed missing-dependency and cycle paths;
duplicate registration; enrolment filtering (only enrolled bundles start);
a spawn failure cascading to transitive dependents; readiness gating
(`notify` dependency, required/provided conditions, explicit-failure skip,
fail-closed notify rejection); on-demand endpoint activation and the idle
linger → graceful stop → force → reap lifecycle; and the reaper
distinguishing a service exit from an inherited orphan — plus the `EventId`
range/uniqueness invariants and the numeric audit-field formatter. Because
the kernel is the capability authority, there are no init-side
capability-intersection or escalation-denial tests (that logic is the
kernel's). The `Run` binary's startup-config and session-supervisor logic
are host-tested in `src/startup.rs` and `src/supervisor.rs` (the default
config, comment/whitespace handling, every fail-closed config rejection,
and per-console session launch/relaunch/exhaustion).
