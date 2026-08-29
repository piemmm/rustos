# System Information service (`userland/system/sysinfod`)

`tairix-sysinfod` is the user-space service that answers the System
Information API (`AGENTS.md` §16.6). TAIRiX has no `/proc` and no `/sys`;
every piece of live system information those trees would have exposed is
served here, through the typed, versioned `sysinfo-v1` wire surface
defined in `tairix_abi::sysinfo` (see
[System Information API (`sysinfo-v1`)](../abi/sysinfo.md)). `sysinfod`
is the only server of the API and the kernel exposes no path that
bypasses it; the installed binary lives at `/System/Services/sysinfod.app/Run`.

The crate is `no_std`, has no `unsafe`, and depends only on the audited
`lib/*` crates `tairix-abi` and `tairix-log`, so a userland service never
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
4. Emit a `tairix_log` audit record for every invocation of an audited
   query, and for every capability denial.
5. Page and encode the answer supplied by the injected data source.

Because steps 1–3 precede any data access, there is no path that answers
a privileged query without first passing its capability gate.

`CACHE_REPORT` is the one query that *writes*, and it takes the same five
steps: it is admitted by the same decode and the same attested identity,
and what it writes is only the caller's own row in the broker's reported-
ledger registry. See [Reported cache ledgers](#reported-cache-ledgers).

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
| `RAID_ARRAYS`            | `CAP_SYSINFO_HW`     | yes     | packed `RaidArrayRecord`s           |
| `RAID_MEMBERS`           | `CAP_SYSINFO_HW`     | yes     | packed `RaidMemberRecord`s          |
| `MEMORY_PRESSURE_BAND`   | none                 | no      | `MemoryPressureBand`                |
| `MEMORY_TOTAL`           | none                 | no      | `MemoryTotal`                       |
| `CACHE_LEDGERS`          | `CAP_SYSINFO_KERNEL` | yes     | packed `CacheLedgerRecord`s         |
| `CACHE_REPORT`           | none                 | no      | empty (a submission)                |
| `NET_STACK_DEFENCE`      | `CAP_SYSINFO_GLOBAL` | yes     | `NetStackDefenceCounters`           |
| `DESKTOP_FRAME_REPORT`   | none                 | no      | empty (a submission)                |
| `DESKTOP_FRAME_STATS`    | `CAP_SYSINFO_GLOBAL` | yes     | packed `DesktopFrameRecord`s        |

`MEMORY_PRESSURE_BAND` and `MEMORY_TOTAL` are the two ungated, unaudited
self-regulation reads a process makes about its own resource use: the
published pressure band, and the machine's total usable physical RAM in
bytes. Neither carries a per-process or per-user figure, and the total is
a static hardware fact — the number on the machine's spec sheet — so both
disclose strictly less than the already-ungated `LOAD_AVERAGE`. Together
they let a process size its caches against the real machine and give the
memory back when the machine tightens, without holding
`CAP_SYSINFO_KERNEL`. The total is the same figure `KERNEL_MEMORY_STATS`
reports as `KernelMemoryStats::total_bytes` (the kernel derives both from
one usable-frame census, so they cannot disagree), and a zero answer
means *unknown*, which admits nothing. The detailed `MEMORY_PRESSURE`
view — free bytes, watermarks, the reserve, transition history — stays
gated and audited.

`IRQ_LIST` is gated on `CAP_SYSINFO_HW`, not `CAP_SYSINFO_KERNEL`, and
audited: like `HARDWARE_TREE` and `SEAT_LIST` it names which driver task
owns each physical interrupt line — cross-principal surface topology,
not a self-scoped observer. Each `IrqRecord` carries the line id, the
kernel-attested owning task, the monotonic interrupt count since boot
(the classic `/proc/interrupts` per-line total), and a quarantine flag
for a line the kernel's runaway-interrupt safety net has disabled.

`RAID_ARRAYS` and `RAID_MEMBERS` are gated on `CAP_SYSINFO_HW` and
audited, for the same reason `HARDWARE_TREE` is: an array report says
which storage devices exist and how they are composed. `RAID_ARRAYS`
answers one `RaidArrayRecord` per composed array — identity, level,
health, member tallies, geometry, endpoint, published node, scrub/resync
cursors, generation — and `RAID_MEMBERS` one `RaidMemberRecord` per
device the composer holds, including the unaffiliated candidates a new
array can be built from. Both page with a `RaidListRequest`.

## Reported cache ledgers

The reclaim model has two halves and only one of them can be measured
from outside. The kernel's block, filesystem, launch, and transform
caches are visible to the kernel; a desktop process's glyph atlases and
decoded icon artwork live in that process's own heap, where nothing else
can see them. Left there the class totals would lie — `disposable-ui`,
the class reclaim starts with, would read zero on a desktop holding
megabytes of exactly that — so `sysinfod` is where the two halves meet.

The broker owns the registry of **reported** rows. `CACHE_REPORT` takes a
process's own rows, and `CACHE_LEDGERS` serves the kernel's rows followed
by the reported ones; `RECLAIM_STATS` folds that same combined list into
the per-class totals with the single `fold_cache_ledgers`, so the class
table is by construction the sum of the breakdown.

It is the broker and not the kernel deliberately. The kernel's own
per-class sum gates a real reclaim decision, and keeping reported rows on
this side of the syscall boundary makes it *structurally* impossible for
a process to steer that decision by inflating its own figures. It also
bounds a hostile reporter's blast radius to a restartable service.

The registry treats every submitted row as the claim it is:

- A row must arrive with its origin unset and its reporter pid zero, and
  may not claim to be owned by a kernel subsystem — a process describing
  itself is not one. The broker stamps the origin and pid from the
  caller's kernel-attested `Origin`, so a caller can neither present its
  figures as measured nor attribute them to another process.
- Entries are keyed by the caller's unforgeable process-instance id, not
  its numeric pid, so a recycled pid inherits nothing; a report
  *replaces* that process's rows rather than adding to them, so
  reporting repeatedly cannot grow a process's share; and an empty
  report withdraws them.
- The reporter count is derived from the machine's RAM rather than
  hand-picked. When the registry is full, dead reporters are expired
  first and a genuinely new one is then refused — never by evicting a
  live reporter's truthful rows in favour of an unknown one.
- The submission emits no audit record. It is ungated by its spec, and
  auditing it would hand every process a way to write the hash-chained
  journal. The gated *reads* are audited as their capability implies.

## Reported desktop frame accounting

A compositor's pixel counts are the second figure only the process holding
them can see, and they arrive the same way: `DESKTOP_FRAME_REPORT` takes a
session's own `DesktopFrameTotals`, and `DESKTOP_FRAME_STATS` serves the
retained records to a `CAP_SYSINFO_GLOBAL` holder, one per publishing
session, each stamped with the publisher the broker attested it to
(`plans/FIX-DESKTOP-SPEEDUP.md` A.4).

The two submissions share one table type, so there is one keying rule, one
capacity policy, and one liveness sweep rather than a second copy of each:
`SelfReports` holds a `ReportTable` per kind, keyed on the unforgeable
process instance, sized from the machine's RAM, swept of dead reporters
before a new one is admitted, and closed to a kernel-domain principal
(which holds neither a userland cache nor a compositor). A frame submission
carries no identity field at all, so there is nothing in it to refuse; an
all-zero `DesktopFrameTotals` withdraws the publisher, as a zero-row cache
report withdraws its rows.

What the frame submission *is* checked against is arithmetic no composite
pass could produce — the bounds `DesktopFrameTotals::from_bytes` enforces
(`docs/src/abi/sysinfo.md`) — applied before the retained accounting is
touched, so a malformed submission leaves the previous truthful one
standing. The read touches no `SysinfoSource`: the records are the broker's
own retained state, and a reader arriving between a session's exit and the
next submission sees that session's last figures rather than nothing.

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
| 8001 | `QUERY_SERVED`      | Debug | an audited query was invoked              |
| 8002 | `QUERY_DENIED`      | Warn  | capability check failed                   |
| 8003 | `REQUEST_MALFORMED` | Warn  | header or payload decode failed           |
| 8004 | `QUERY_UNAVAILABLE` | Warn  | reserved-but-unassigned query identifier  |

`QUERY_SERVED` is recorded at `Debug` because a polling monitor emits it
continuously: at `Info` it would flood the default console filter.
Lowering the global filter recovers the allow stream for forensics;
denials stay at `Warn` and always surface.

Self-scoped, ungated observers are deliberately not audited, to avoid
drowning the log; the cross-principal, kernel, and hardware queries are.

## Tests

`cargo test -p tairix-sysinfod` drives `serve` against an in-memory
`SysinfoSource` fixture and a recording log sink, covering every query,
paging (`offset`/`limit` and the empty page past the end), the capability
gates and their denial records, the audited-served record, the
hardware-tree pass-through, the ungated mount-table and resource-limit
listings, and the malformed-header / truncated-payload
/ unassigned-query / undersized-buffer fail-closed paths, plus the
`EventId` range and uniqueness invariants.
