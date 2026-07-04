# `rustos-sysinfod` — System Information service

Stage 6 deliverable (`AGENTS.md` §16.6). The user-space service that
answers the `sysinfo-v1` API defined in `rustos_abi::sysinfo`. Installed
to `/System/Services/sysinfod.app/Run`.

RustOS has **no `/proc` and no `/sys`**. Every piece of live system
information those trees would have exposed is served here, through typed,
versioned, capability-checked queries — never through a virtual
filesystem (`AGENTS.md` §16.1). `sysinfod` is the only server of the API
and the kernel exposes no path that bypasses it.

## What this crate is

The **dispatcher** — the policy layer. It owns no data of its own. For
each request `serve` performs, in order, failing closed at the first
problem (`AGENTS.md` §5.4):

1. Decode the `SysinfoRequestHeader` and any typed payload.
2. Look the query up in the frozen registry (`SYSINFO_QUERIES`).
3. Enforce the query's declared capability against the caller's
   `CapabilityQuery` view **before** touching any state.
4. Emit a `lib/log` audit record for every invocation of an audited
   query, and for every capability denial.
5. Page and encode the answer supplied by the injected `SysinfoSource`.

The live data is read through the `SysinfoSource` seam, so the
security-relevant dispatch code is independent of any kernel plumbing
and is fully testable against an in-memory fixture.

## Queries served (`sysinfo-v1`)

| Query                 | Capability           | Audited | Response                       |
|-----------------------|----------------------|---------|--------------------------------|
| `SELF_PROCESS_LIST`   | none                 | no      | packed `ProcessRecord`s        |
| `GLOBAL_PROCESS_LIST` | `CAP_SYSINFO_GLOBAL` | yes     | packed `ProcessRecord`s        |
| `KERNEL_MEMORY_STATS` | `CAP_SYSINFO_KERNEL` | yes     | `KernelMemoryStats`            |
| `HARDWARE_TREE`       | `CAP_SYSINFO_HW`     | yes     | encoded hardware tree (opaque) |
| `SYSTEM_IDENTITY`     | none                 | no      | `SystemIdentity`               |
| `UPTIME`              | none                 | no      | `Uptime`                       |
| `MOUNT_LIST`          | none                 | no      | packed `MountRecord`s          |
| `RESOURCE_LIMITS`     | none                 | no      | packed `ResourceLimitRecord`s  |

## Response encoding

There is no response envelope: the typed payload *is* the response.

- Process-list queries pack zero or more `ProcessRecord`s back-to-back.
  The caller pages with the request's `offset`/`limit` and detects the
  end of the list when it receives fewer than `limit` records. Paging
  bounds live in the dispatcher (one place), not in the source.
- Scalar queries return the little-endian wire image of their struct.
- The hardware-tree query passes the source's encoded bytes through
  verbatim: the hardware-tree wire format is owned by `lib/abi`
  (`AGENTS.md` §18.1), not by this service.

## `SysinfoSource` seam

`init` injects a `SysinfoSource` when it starts the service. On a running
kernel this is a thin shim over the kernel's process table, memory
accounting, hardware tree, and identity; in tests it is an in-memory
fixture. The `Caller` passed to every method carries the kernel-provided
`uid` and capability view — never caller-supplied bytes (`AGENTS.md`
§5.4.1). `ProcessScope` (`Caller` vs `Global`) is chosen by the
dispatcher from the query id, so a self-scoped request can never widen
into a global one without the global query's capability gate.

## Audit events

Reserved `EventId` range `8000..9000`:

- `8001 QUERY_SERVED` — an audited query was invoked (Info).
- `8002 QUERY_DENIED` — capability check failed (Warn).
- `8003 REQUEST_MALFORMED` — header/payload decode failed (Warn).
- `8004 QUERY_UNAVAILABLE` — reserved-but-unassigned query id (Warn).

## Layering & safety

`no_std`, depends only on `rustos-abi` and `rustos-log` (both `lib/*`),
so a userland service never links a kernel or driver crate (`AGENTS.md`
§17.4). No `unsafe`, no `unwrap`/`expect`/`panic!` in production paths
(`AGENTS.md` §2.9).

## Test surface

`cargo test -p rustos-sysinfod` (12 unit tests):

- self-scoped list needs no capability and is not audited;
- paging by `offset`/`limit`, and an empty page past the end;
- global list denied without `CAP_SYSINFO_GLOBAL` (with the denial
  audit record);
- an audited query emitting exactly one `QUERY_SERVED` record;
- the hardware-tree pass-through, gated by `CAP_SYSINFO_HW`;
- the ungated scalar queries (`UPTIME`, `SYSTEM_IDENTITY`) round-tripping
  with no audit record;
- malformed header, truncated payload, unassigned query id, and an
  undersized response buffer all failing closed;
- the `EventId` range/uniqueness invariants.
