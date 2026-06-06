# `rustos-init` — PID 1 service manager

Stage 6 deliverable (`AGENTS.md` §5.2, §16.2). The first user-space
process the kernel starts. It owns the lifecycle of every long-running
system service under `/System/Services`: dependency-ordered start,
capability granting from each service's signed manifest, and reaping.
Installed to `/System/Services/init`.

## What this crate is

The **orchestrator** — it decides what runs, in what order, and with what
authority. It is *not* a loader: verifying a service binary's signature,
syscall-table hash, and `rxe` envelope is the `Spawner`'s job (the same
pipeline `drvhost` runs for drivers, `AGENTS.md` §8). `init` computes the
capability ceiling and hands it, with the binary, to the `Spawner`.

## Bring-up pipeline (`Init::start_all`)

Fails closed at the first problem (`AGENTS.md` §5.4.5):

1. Order the registered services by their declared dependencies (Kahn
   topological sort; ready services in registration order, so the result
   is deterministic — `AGENTS.md` §18.3).
2. Reject the whole graph — starting nothing — if a dependency names an
   unregistered service (`DependencyMissing`) or the graph has a cycle
   (`DependencyCycle`).
3. Per service, in order: decode its manifest into a requested capability
   set, gate the request against init's authority, spawn it, audit the
   outcome.

When the graph is sound, init starts every service it can; a service that
fails is recorded and its transitive dependents are skipped, without
aborting independent services. The outcome is a `StartReport`
(`started` + `failed`).

## Capability granting (`AGENTS.md` §5.2)

A service's grant is the intersection of its manifest's requested
capability set with the authority init itself holds. The request is
decoded with the single shared decoder `rustos_abi::decode_capability_ids`
(the same decoder `drvhost` uses — one implementation of the manifest-body
format, `AGENTS.md` §2.2). A service whose request is not a subset of the
authority is refused (`CapabilityEscalation`), never silently narrowed.
The `Spawner` receives the computed ceiling and may never add to it
(`AGENTS.md` §4 — no ambient authority).

## Reaping (`Init::reap`)

PID 1 reaps the whole system's zombies. A reaped process matching a
running service is logged as a service exit and dropped from the running
set; any other reaped process is an inherited orphan and logged as such.
Neither path panics (`AGENTS.md` §2.9).

## Seams

Injected, mirroring the `drvhost` host configuration:

- `Spawner::spawn(&ServiceSpec, &CapabilitySet) -> Result<Pid, Errno>` —
  the trusted loader that verifies and executes a service binary with at
  most the granted capability set.
- `Reaper::collect() -> Option<ReapedChild>` — exited-child
  notifications.

On a running kernel both are syscall-backed; in tests they are in-memory
fixtures.

## Audit events

Reserved `EventId` range `9000..10000`:

- `9001 SERVICE_STARTED` — a service was launched with its grant (Info).
- `9002 SERVICE_START_FAILED` — manifest decode failed or spawn refused (Warn).
- `9003 SERVICE_DENIED` — manifest over-requested authority (Warn).
- `9004 SERVICE_SKIPPED` — a dependency failed, so the service was skipped (Warn).
- `9005 SERVICE_EXITED` — a registered service exited and was reaped (Info).
- `9006 ORPHAN_REAPED` — an inherited orphan was reaped (Info).
- `9007 GRAPH_REJECTED` — the service graph was structurally invalid (Error).

## The `Run` entry-point binary (`plans/PI.md` P6b)

The package also builds the `init` application bundle's `Run` entry-point
binary (`src/run.rs`, `AGENTS.md` §16.5) — the program the kernel spawns
as PID 1 when it reaches user mode (`plans/PI.md` P6c). It is a
freestanding C-ABI program whose entry is crt0's `_start`
(`rustos-crt0`); `main` parses a compiled-in **startup config**
(`src/startup.rs`) and writes its first banner line through the `abi-v1`
`console_write` syscall (`rustos-abi-sys`, the P6a `ros_sys_console_write`
stub).

It links **only** the curated *System runtime / C ABI* class (crt0 + the
syscall stubs, `AGENTS.md` §16.4) — never the orchestrator library above,
whose `alloc` + crypto dependency chain has no place in a
banner-printing program (`AGENTS.md` §2.3). The parser therefore lives
alongside the binary rather than in the library, and the shipped program
contains no crypto code. On the host the binary is an inert stub so
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
`rustos-abi`, `rustos-caps`, and `rustos-log` (all `lib/*`), so a userland
service never links a kernel or driver crate (`AGENTS.md` §17.4). No
`unsafe`, no `unwrap`/`expect`/`panic!` in production paths
(`AGENTS.md` §2.9). The `Run` binary's only `unsafe` is the `extern "C"`
`main` crt0 calls and the panic-free console-write cast, both documented
in `src/run.rs`.

## Test surface

`cargo test -p rustos-init` (27 unit tests): the 17 orchestrator tests —
dependency-ordered start; fail-closed missing-dependency and cycle paths;
duplicate registration; the grant as `request ∩ authority`; an escalation
denial; a spawn failure cascading to transitive dependents; an invalid
manifest; the reaper distinguishing a service exit from an inherited
orphan; plus the `EventId` range/uniqueness invariants and the numeric
audit-field formatter — and 10 startup-config tests covering the default
config, comments/blank/inline-comment handling, surrounding whitespace,
and every fail-closed rejection (unknown/duplicate directive, console with
an argument, a missing or non-absolute `session` path, a missing required
directive, and an over-long config).
