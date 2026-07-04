# System Information service (`userland/system/sysinfod`)

`rustos-sysinfod` is the user-space service that answers the System
Information API (`AGENTS.md` §16.6). RustOS has no `/proc` and no `/sys`;
every piece of live system information those trees would have exposed is
served here, through the typed, versioned `sysinfo-v1` wire surface
defined in `rustos_abi::sysinfo` (see
[System Information API (`sysinfo-v1`)](../abi/sysinfo.md)). `sysinfod`
is the only server of the API and the kernel exposes no path that
bypasses it; the installed binary lives at `/System/Services/sysinfod.app/Run`.

The crate is `no_std`, has no `unsafe`, and depends only on the audited
`lib/*` crates `rustos-abi` and `rustos-log`, so a userland service never
links a kernel or driver crate (`AGENTS.md` §17.4).

## The dispatcher

This crate is the **policy layer** and owns no data of its own. The
single entry point, `serve`, runs one request through a fixed pipeline,
failing closed at the first problem (`AGENTS.md` §5.4):

1. Decode the `SysinfoRequestHeader` and any typed payload; a malformed
   header, an unsupported version, or a truncated payload is rejected.
2. Look the query up in the frozen registry `SYSINFO_QUERIES`; a
   reserved-but-unassigned identifier is rejected.
3. Enforce the query's declared capability against the caller's
   `CapabilityQuery` view **before** touching any state.
4. Emit a `rustos_log` audit record for every invocation of an audited
   query, and for every capability denial.
5. Page and encode the answer supplied by the injected data source.

Because steps 1–3 precede any data access, there is no path that answers
a privileged query without first passing its capability gate.

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

- The list queries pack zero or more fixed-size records back-to-back —
  `ProcessRecord`s for the process lists, `MountRecord`s for the mount
  list. The caller pages with the request's `offset`/`limit` and detects
  the end of the list when it receives fewer than `limit` records. The
  paging bounds live in one shared helper in the dispatcher, not in the
  data source.
- `RESOURCE_LIMITS` packs exactly one `ResourceLimitRecord` per
  `LimitKind` in discriminant order (a small, closed set, so it is not
  paged): each record carries the resource's effective `ResourceLimit`
  and the caller's current live usage. It is self-scoped (the caller's
  own task), so it needs no capability (`AGENTS.md` §24.3, §16.6).
- The scalar queries return the little-endian wire image of their
  response struct.
- The hardware-tree query passes the source's encoded bytes through
  verbatim: the hardware-tree wire format is owned by `lib/abi`
  (`AGENTS.md` §18.1), not by this service, so `sysinfod` frames the
  bytes without interpreting them.

## The data seam

The live data — the process table, memory accounting, the hardware
tree, machine identity, uptime, mount table, per-task resource limits and
usage — is read through the `SysinfoSource`
trait, injected by `init` when it starts the service. On a running
kernel this is a thin shim over the kernel's bookkeeping; in tests it is
an in-memory fixture. Splitting policy from data keeps the
security-relevant dispatch code independent of any particular kernel
plumbing and exhaustively testable.

The `Caller` handed to every source method carries the kernel-provided
`uid` and capability view, never caller-supplied bytes (`AGENTS.md`
§5.4.1). The `ProcessScope` (`Caller` vs `Global`) is decided by the
dispatcher from the query identifier, so a self-scoped request can never
widen into a global one without the `CAP_SYSINFO_GLOBAL` gate the global
query carries.

## Audit events

`sysinfod` owns the reserved `EventId` range `8000..9000`
(`AGENTS.md` §2.5, §19.4):

| Id   | Constant            | Level | Meaning                                   |
|------|---------------------|-------|-------------------------------------------|
| 8001 | `QUERY_SERVED`      | Info  | an audited query was invoked              |
| 8002 | `QUERY_DENIED`      | Warn  | capability check failed                   |
| 8003 | `REQUEST_MALFORMED` | Warn  | header or payload decode failed           |
| 8004 | `QUERY_UNAVAILABLE` | Warn  | reserved-but-unassigned query identifier  |

Self-scoped, ungated observers are deliberately not audited, to avoid
drowning the log; the cross-principal, kernel, and hardware queries are.

## Tests

`cargo test -p rustos-sysinfod` drives `serve` against an in-memory
`SysinfoSource` fixture and a recording log sink, covering every query,
paging (`offset`/`limit` and the empty page past the end), the capability
gates and their denial records, the audited-served record, the
hardware-tree pass-through, the ungated mount-table and resource-limit
listings, and the malformed-header / truncated-payload
/ unassigned-query / undersized-buffer fail-closed paths, plus the
`EventId` range and uniqueness invariants.
