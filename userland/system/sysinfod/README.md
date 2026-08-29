# `tairix-sysinfod` — System Information service

Stage 6 deliverable (`AGENTS.md` §16.6). The user-space service that
answers the `sysinfo-v1` API defined in `tairix_abi::sysinfo`. Installed
to `/System/Services/sysinfod.app/Run`.

TAIRiX has **no `/proc` and no `/sys`**. Every piece of live system
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

| Query                    | Capability           | Audited | Response                            |
|--------------------------|----------------------|---------|-------------------------------------|
| `SELF_PROCESS_LIST`      | none                 | no      | packed `ProcessRecord`s             |
| `GLOBAL_PROCESS_LIST`    | `CAP_SYSINFO_GLOBAL` | yes     | packed `ProcessRecord`s             |
| `KERNEL_MEMORY_STATS`    | `CAP_SYSINFO_KERNEL` | yes     | `KernelMemoryStats`                 |
| `HARDWARE_TREE`          | `CAP_SYSINFO_HW`     | yes     | `HwTreeHeader` + `HwNode` page      |
| `SYSTEM_IDENTITY`        | none                 | no      | `SystemIdentity`                    |
| `UPTIME`                 | none                 | no      | `Uptime`                            |
| `MOUNT_LIST`             | none                 | no      | packed `MountRecord`s               |
| `RESOURCE_LIMITS`        | none                 | no      | packed `ResourceLimitRecord`s       |
| `PROCESS_IDENTITY`       | none                 | no      | the caller's own `Origin`           |
| `LOAD_AVERAGE`           | none                 | no      | `LoadAverage`                       |
| `USER_DIRECTORY`         | none                 | no      | packed `UserDirectoryRecord`s       |
| `CPU_TIME_STATS`         | none                 | no      | packed `CpuTimeRecord`s             |
| `SEAT_LIST`              | `CAP_SYSINFO_HW`     | yes     | packed `SeatRecord`s                |
| `MEMORY_PRESSURE`        | `CAP_SYSINFO_KERNEL` | yes     | `MemoryPressureStats`               |
| `RECLAIM_STATS`          | `CAP_SYSINFO_KERNEL` | yes     | packed `ReclaimClassRecord`s        |
| `RAMZIP_STATS`           | `CAP_SYSINFO_KERNEL` | yes     | `RamzipStats`                       |
| `CPU_LOAD`               | `CAP_SYSINFO_KERNEL` | yes     | packed `CpuLoadRecord`s             |
| `NET_INTERFACE_FACTS`    | `CAP_SYSINFO_HW`     | yes     | packed `NetInterfaceFactsRecord`s   |
| `NET_INTERFACE_STATE`    | `CAP_SYSINFO_GLOBAL` | yes     | packed `NetInterfaceStateRecord`s   |
| `NET_INTERFACE_COUNTERS` | `CAP_SYSINFO_GLOBAL` | yes     | packed `NetInterfaceCountersRecord`s|
| `NET_INTERFACE_RATES`    | `CAP_SYSINFO_GLOBAL` | yes     | packed `NetInterfaceRatesRecord`s   |
| `NET_SOCKETS`            | `CAP_SYSINFO_GLOBAL` | yes     | packed `NetSocketRecord`s           |
| `NET_BOND_MEMBERS`       | `CAP_SYSINFO_GLOBAL` | yes     | packed `NetBondMemberRecord`s       |
| `CPU_INFO`               | none                 | no      | packed `CpuInfoRecord`s             |
| `NET_RESOLVER_SERVERS`   | none                 | no      | packed `NetResolverServer`s         |
| `IRQ_LIST`               | `CAP_SYSINFO_HW`     | yes     | packed `IrqRecord`s                 |
| `CRASH_RECORD`           | `CAP_SYSINFO_KERNEL` | yes     | packed `CrashRecord`s               |
| `VOLUME_IO_HEALTH`       | `CAP_SYSINFO_KERNEL` | yes     | packed `VolumeIoHealthRecord`s      |
| `MEMORY_PRESSURE_BAND`   | none                 | no      | `MemoryPressureBand`                |
| `MEMORY_TOTAL`           | none                 | no      | `MemoryTotal`                       |
| `RAID_ARRAYS`            | `CAP_SYSINFO_HW`     | yes     | packed `RaidArrayRecord`s           |
| `RAID_MEMBERS`           | `CAP_SYSINFO_HW`     | yes     | packed `RaidMemberRecord`s          |
| `CACHE_LEDGERS`          | `CAP_SYSINFO_KERNEL` | yes     | packed `CacheLedgerRecord`s         |
| `CACHE_REPORT`           | none                 | no      | empty (a submission)                |
| `NET_STACK_DEFENCE`      | `CAP_SYSINFO_GLOBAL` | yes     | `NetStackDefenceCounters`           |
| `DESKTOP_FRAME_REPORT`   | none                 | no      | empty (a submission)                |
| `DESKTOP_FRAME_STATS`    | `CAP_SYSINFO_GLOBAL` | yes     | packed `DesktopFrameRecord`s        |

`MEMORY_PRESSURE_BAND` and `MEMORY_TOTAL` are the two ungated,
unaudited self-regulation reads: the published pressure band, and the
machine's total usable physical RAM in bytes. Both are deliberately the
smallest useful answer, so a process can shrink its own caches and size
them against the real machine without holding `CAP_SYSINFO_KERNEL`. The
total is the same figure `KERNEL_MEMORY_STATS` reports as
`KernelMemoryStats::total_bytes` — the kernel derives both from one
usable-frame census — and a zero answer means *unknown*, which admits
nothing. The detailed `MEMORY_PRESSURE` view stays gated and audited.

## The two submissions

Two figures cannot be measured from outside the process that holds them: a
userland cache's ledger, and a compositor's frame accounting. Both are
therefore submitted rather than read — `CACHE_REPORT` and
`DESKTOP_FRAME_REPORT` — and retained here, in `SelfReports`, never in the
kernel: the kernel's own per-class reclaim sum gates a real decision, and
keeping self-reported figures on this side of the syscall boundary makes it
structurally impossible for a process to steer it.

Both are ungated and unaudited, because a process describing itself grants
nothing and reads nothing — and auditing them would hand every process a way
to write the hash-chained journal. Both share one table type, so the keying
(the caller's unforgeable process instance), the RAM-derived capacity, the
liveness sweep before a new reporter is admitted, and the refusal of a
kernel-domain principal are written once. The matching reads —
`CACHE_LEDGERS` and `DESKTOP_FRAME_STATS` — are gated and audited as their
capability implies. See `docs/src/userland/sysinfod.md`.

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

- `8001 QUERY_SERVED` — an audited query was invoked (Debug: a polling
  monitor emits it continuously, so it sits below the default `Info`
  filter; lower the filter to capture it).
- `8002 QUERY_DENIED` — capability check failed (Warn).
- `8003 REQUEST_MALFORMED` — header/payload decode failed (Warn).
- `8004 QUERY_UNAVAILABLE` — reserved-but-unassigned query id (Warn).

## Layering & safety

`no_std`, depends only on `tairix-abi` and `tairix-log` (both `lib/*`),
so a userland service never links a kernel or driver crate (`AGENTS.md`
§17.4). No `unsafe`, no `unwrap`/`expect`/`panic!` in production paths
(`AGENTS.md` §2.9).

## Test surface

`cargo test -p tairix-sysinfod`:

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
