# PID 1 service manager (`userland/system/init`)

`rustos-init` is the first user-space process the kernel starts. It owns
the lifecycle of every long-running system service under
`/System/Services` (`AGENTS.md` §16.2): it brings them up in dependency
order, grants each one the capability set its signed manifest requests
intersected with init's own authority (`AGENTS.md` §5.2), and reaps the
children that any PID 1 inherits.

The crate is `no_std` (with `alloc`), has no `unsafe`, and depends only on
the audited `lib/*` crates `rustos-abi`, `rustos-caps`, and `rustos-log`,
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
3. For each service, in order: **decode** its manifest into a requested
   capability set, **gate** that request against init's authority,
   **spawn** it, and **audit** the outcome.

When the graph is sound, init brings up every service it can. A single
service that fails — its manifest will not decode, it over-requests
authority, or the `Spawner` refuses it — is recorded, and the services
that *transitively depend on it* are skipped; services independent of the
failure still start. The full outcome is returned as a `StartReport`
(`started` + `failed`), so a caller can see which optional services are
absent without the boot aborting.

## Capability granting (`AGENTS.md` §5.2)

A service's grant is the intersection of the capability set its signed
manifest requests with the authority init itself holds. init decodes the
manifest request with the single shared decoder
`rustos_abi::decode_capability_ids` (the same decoder `drvhost` uses, so
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

## Tests

`cargo test -p rustos-init` drives the manager against an in-memory
`Spawner`/`Reaper` and a recording log sink, covering dependency-ordered
start, the fail-closed missing-dependency and cycle paths, duplicate
registration, the capability grant as `request ∩ authority`, an
escalation denial, a spawn failure cascading to its transitive
dependents, an invalid manifest, and the reaper distinguishing a service
exit from an inherited orphan — plus the `EventId` range and uniqueness
invariants and the numeric audit-field formatter.
