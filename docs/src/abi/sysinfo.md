# System Information API (`sysinfo`, `sysinfo-v1`)

TAIRiX has no `/proc` and no `/sys` (`AGENTS.md` §16.1). Every piece of
live system information that would have lived under those trees is exposed
through one versioned, capability-checked API: the **System Information
API**, whose wire types live in `lib/abi/src/sysinfo.rs`
(`tairix_abi::sysinfo`). The user-space service that answers the queries is
`/System/Services/sysinfod.app/Run` (`userland/system/sysinfod`); the command-line
`sysinfo` tool and the `ps`/`mount` utilities are clients of this API and
never scrape a virtual filesystem.

## Discipline

Each query is a *typed* request returning a *typed* response — there is no
free-form text-scraping interface. Adding a query carries the same
discipline as adding a syscall (`AGENTS.md` §9, §16.6):

- The registry is **versioned** ([`SYSINFO_VERSION_V1`]) and **frozen** on
  release: existing [`SysinfoQueryId`] numbers and [`SysinfoQuerySpec`]
  rows are never re-numbered or removed; a new query takes the next free
  identifier and ships in `sysinfo-v2`.
- The registry has a canonical, **hashable** byte image
  ([`ENCODED_QUERY_TABLE`], borrowed by [`encoded_query_table`]) so a
  service and a client built against different registries produce
  different digests. A golden unit test pins the encoding.
- Every query **declares the capability** it requires; the serving
  service checks it before touching any state and fails closed.

## Queries (`sysinfo-v1`)

| `SysinfoQueryId`        | Capability             | Audited |
|-------------------------|------------------------|:-------:|
| `SELF_PROCESS_LIST`     | none (self-scoped)     | no      |
| `GLOBAL_PROCESS_LIST`   | `CAP_SYSINFO_GLOBAL`   | yes     |
| `KERNEL_MEMORY_STATS`   | `CAP_SYSINFO_KERNEL`   | yes     |
| `HARDWARE_TREE`         | `CAP_SYSINFO_HW`       | yes     |
| `SYSTEM_IDENTITY`       | none                   | no      |
| `UPTIME`                | none                   | no      |
| `MOUNT_LIST`            | none                   | no      |
| `RESOURCE_LIMITS`       | none (self-scoped)     | no      |
| `PROCESS_IDENTITY`      | none (self-scoped)     | no      |
| `LOAD_AVERAGE`          | none                   | no      |
| `USER_DIRECTORY`        | none                   | no      |
| `CPU_TIME_STATS`        | none                   | no      |
| `SEAT_LIST`             | `CAP_SYSINFO_HW`       | yes     |
| `MEMORY_PRESSURE`       | `CAP_SYSINFO_KERNEL`   | yes     |
| `RECLAIM_STATS`         | `CAP_SYSINFO_KERNEL`   | yes     |
| `RAMZIP_STATS`          | `CAP_SYSINFO_KERNEL`   | yes     |
| `CPU_LOAD`              | `CAP_SYSINFO_KERNEL`   | yes     |
| `NET_INTERFACE_FACTS`   | `CAP_SYSINFO_HW`       | yes     |
| `NET_INTERFACE_STATE`   | `CAP_SYSINFO_GLOBAL`   | yes     |
| `NET_INTERFACE_COUNTERS`| `CAP_SYSINFO_GLOBAL`   | yes     |
| `NET_INTERFACE_RATES`   | `CAP_SYSINFO_GLOBAL`   | yes     |
| `NET_SOCKETS`           | `CAP_SYSINFO_GLOBAL`   | yes     |
| `NET_BOND_MEMBERS`      | `CAP_SYSINFO_GLOBAL`   | yes     |
| `NET_RESOLVER_SERVERS`  | none                   | no      |
| `IRQ_LIST`              | `CAP_SYSINFO_HW`       | yes     |
| `VOLUME_IO_HEALTH`      | `CAP_SYSINFO_KERNEL`   | yes     |

`CAP_SYSINFO_GLOBAL`, `CAP_SYSINFO_KERNEL`, and `CAP_SYSINFO_HW` are
[`CapabilityId`] values 13, 14, and 15. Self-scoped observers ("list my
own processes") require no capability; the global view does
(`AGENTS.md` §16.6). The hardware-tree query gates the read-only view of
the detected hardware tree (`AGENTS.md` §18.4). `MOUNT_LIST` is ungated:
the mount table is system-wide and secret-free, so — like `UPTIME` and
`SYSTEM_IDENTITY` — any task may read it; the privileged *act* of
mounting is gated separately by `CAP_FS_MOUNT` (`AGENTS.md` §5.2) and is
not part of this read-only API. `RESOURCE_LIMITS` is self-scoped — it
returns the *caller's own* effective resource limits and live usage
(`AGENTS.md` §24.3) — so, like `SELF_PROCESS_LIST`, it needs no capability;
observing another principal's limits would be a separate, gated query.
`PROCESS_IDENTITY` is likewise self-scoped: it returns the caller's own
kernel-attested `Origin`. `LOAD_AVERAGE` is ungated for the same reason
as `UPTIME`: the `LoadAverage` response — the damped 1/5/15-minute
run-queue averages (fixed-point, `LOAD_FIXED_SHIFT` fractional bits) plus
the runnable/total-task and logged-in-user censuses — is the classic
`uptime(1)` line, system-wide and secret-free. `USER_DIRECTORY` is
ungated for the same reason: each `UserDirectoryRecord` carries only the
`/etc/passwd`-class public uid + username pairing — never credential
material, which stays behind the capability-gated `users_db_read`
syscall — so any task may resolve account names for display.
`CPU_TIME_STATS` is ungated like `LOAD_AVERAGE`: each `CpuTimeRecord`
carries one CPU's cumulative busy nanoseconds (accounted on the
scheduler's dispatch bracket) and the idle remainder of the same
monotonic sample — the `top`-class busy/idle utilisation figure, which
exposes strictly less than the load-average census. A consumer derives
a utilisation percentage from the deltas of two samples; TAIRiX
accounts busy and idle time only, never a fabricated
user/system/nice/iowait split. The list is paged by a
`CpuTimeListRequest` exactly like the mount list. `SEAT_LIST` is gated
like `HARDWARE_TREE` and audited: each `SeatRecord` names which task owns
a physical display — cross-principal surface topology, not a self-scoped
observer.

The kernel-statistics queries (`plans/STRESSTEST.md` ST1; storage health
`plans/FIX-IO.md` IO5) share `KERNEL_MEMORY_STATS`'s security boundary —
gated on `CAP_SYSINFO_KERNEL` and audited — because each exposes
kernel-wide operational state:

- `MEMORY_PRESSURE` — a single `MemoryPressureStats`: the live five-band
  gauge's current band (an index into `PRESSURE_BAND_NAMES`), the
  free/total/reserve readings, the derived per-band enter/exit
  watermarks actually in force (reported, never promised), and the
  per-band transition counters since boot.
- `RECLAIM_STATS` — one `ReclaimClassRecord` per reclaim class (paged by
  a `ReclaimListRequest`): live payload/metadata byte and entry gauges
  plus the monotonic refusal/shrink/teardown/failure counters,
  aggregated across every registered live cache. The class ids and the
  stable names in `RECLAIM_CLASS_NAMES` are the shared vocabulary the
  `stats:mem/reclaim/<class>` selectors resolve through.
- `RAMZIP_STATS` — a single `RamzipStats`: the compressed anonymous-
  memory tier's byte/entry gauges, derived min/soft/hard caps, every
  monotonic event counter, and `pinned_bytes` — the live system-wide
  aggregate of anonymous memory exempted from the tier by process pins
  (`mem_pin`, `plans/STRESSTEST.md` ST2), composed from the per-task
  registry so the exemption is observable whether or not a tier is
  running. Counters only — never page contents or key material; a build
  whose tier is not yet driven truthfully reports an idle tier (all
  zeros) rather than refusing or fabricating.
- `CPU_LOAD` — one `CpuLoadRecord` per online CPU (paged by a
  `CpuLoadRequest`): the run-queue depth sample plus the context-switch
  and preemption counters. The cumulative busy/idle time split stays in
  `CPU_TIME_STATS`, so the same figure is never served twice. The
  run-queue depth and context-switch counters are scheduler internals;
  the preemption counter is the kernel **preemption mechanism**'s own
  per-CPU count of real involuntary preemptions (the return-to-user
  preempt point suspending a running task), not a scheduler-policy tick
  observation — so it moves under load even on the tickless default
  policy (EEVDF), which takes no periodic scheduler tick. All are kernel
  internals, hence the gate the utilisation split does not carry.
- `VOLUME_IO_HEALTH` — one `VolumeIoHealthRecord` per fault-aware
  block-backed volume the kernel serves (paged by a
  `VolumeIoHealthRequest`): the volume's durable id, the serving
  block-service endpoint, its current `MountAvailability` (the same live
  reading the mount table overlays), and the cumulative
  `BlkHealthCounters` the kernel filesystem client folds from every
  completion — the per-status outcome tallies (`ok`, `degraded`,
  `transient`, `timeouts`, `resets`, `medium_errors`, `offline`,
  `faults`) plus the consumer `reissues` count. The per-status buckets
  partition every folded completion exactly once. Monotonic since the
  volume was attached, these tallies are the storage analogue of the
  per-line `IRQ_LIST` counters and the surface a failing or flapping
  disk becomes visible on; they are kernel-wide storage operational
  state, hence the gate the ungated `MOUNT_LIST` does not carry.

`IRQ_LIST` is gated like `SEAT_LIST` and `HARDWARE_TREE` — on
`CAP_SYSINFO_HW`, and audited — because each `IrqRecord` names which
driver task owns a physical interrupt line: cross-principal surface
topology, not a self-scoped observer. The list carries one record per
*bound* line, in ascending line order, paged by an `IrqListRequest`. The
per-line `count` is monotonic since boot (the classic `/proc/interrupts`
total, not reset when a line is re-bound), and `flags` reports the line's
containment state (`IRQ_FLAG_QUARANTINED` for a line the kernel's
runaway-interrupt safety net has disabled). It exposes no per-principal
secret beyond the ownership the hardware view already carries.

`NET_INTERFACE_RATES` shares `NET_INTERFACE_COUNTERS`'s boundary —
`CAP_SYSINFO_GLOBAL` and audited — because it derives from the same
system-wide counters. It is the one query that carries a *decoration*: a
`NetInterfaceRatesRequest` adds a caller-supplied averaging window to the
paging header, and each `NetInterfaceRatesRecord` reports the received /
transmitted packets- and bits-per-second **averaged over the window that
actually elapsed** — which may be shorter than requested when an
interface's history is younger, and is `0` over a zero window when there
is no usable baseline yet. The rates are the surface a traffic flood
becomes visible on; the `stats:net/<iface>/{rx,tx}.{pps,bps}?window=…`
selectors resolve through it (`plans/NETWORK.md` §5).

`NET_BOND_MEMBERS` shares the same boundary — `CAP_SYSINFO_GLOBAL` and
audited — because link aggregation is system-wide topology and its live
failover state. It pages one `NetBondMemberRecord` per (bond, member)
pair, flattened in interface-table then configured-member order: the
owning bond alias, the member alias, whether the member is the bond's
currently-active transmitting member (active-backup only), and its
link/eligibility health. The `info:net/<bond>/members`,
`state:net/<bond>/active-member`, and `state:net/<bond>/member-health`
selectors resolve through it (`plans/NETWORK.md` §5, §6.3).

## Wire framing

A request is a fixed [`SysinfoRequestHeader`] (24 bytes: magic
`SYI1`, version, flags, query id, reserved, payload length, and a
caller-chosen `request_id` echoed in the response) followed by the typed
request payload. All multi-byte fields are little-endian. The decoder
fails closed: bad magic or a non-zero reserved field is
[`Errno::BadMagic`], an unknown version is
[`Errno::AbiVersionUnsupported`], an out-of-range query id is
[`Errno::OutOfRange`], and an over-large payload is
[`Errno::LengthOutOfRange`].

## Reply framing

The synchronous call transport (`ipc_call`) always succeeds at the
transport level, so it cannot carry a per-query refusal (a missing
`CAP_SYSINFO_GLOBAL`, say). `sysinfod` therefore frames every reply with a
four-byte little-endian status word ([`SYSINFO_REPLY_STATUS_LEN`]): zero
followed by the typed payload on success ([`encode_reply_ok`]), or the
non-zero [`Errno`] code alone on a refusal ([`encode_reply_err`]). A client
decodes it with [`decode_reply`], which returns the payload slice on
success, the server's reported `Errno` on a named refusal, and fails
closed with [`Errno::OutOfRange`] on a status word that is neither zero nor
a defined code. The reply frame is untrusted server output, so its decoder
— together with the kernel primitive's closed [`IntrospectDomain`] selector
(`IntrospectDomain::from_u32`) — is exercised by the `lib/abi` fuzz harness
(`AGENTS.md` §19.6) alongside the request decoders.

## Endpoint, message bounds, and the client transport

`sysinfod` binds the well-known unrestricted-sender call endpoint
[`SYSINFO_ENDPOINT`]; any process may post a request, and per-query scope is
enforced by the service against the caller's kernel-attested origin, not by
the transport. The id itself is a **reserved rendezvous**
(`tairix_abi::ipc::is_reserved_endpoint`): binding it requires
`CAP_IPC_BIND_PRIVILEGED` (carried by `sysinfod`'s manifest), so an
unprivileged squatter can never claim the endpoint and serve forged system
state. The endpoint's message sizes are one shared contract:
[`SYSINFO_MAX_REQUEST`] bounds the request the server accepts, and
[`SYSINFO_MAX_REPLY`] bounds the framed reply it delivers (one page of
records past the status word). The server sizes its endpoint by these
constants and every client sizes its buffers by them, so neither keeps a
private copy that could drift; a list longer than one page is paged across
successive requests (a client advancing `offset`/shrinking `limit`). The
hardware tree pages the same way: each `HARDWARE_TREE` reply is the
snapshot's `HwTreeHeader` (its total node count and generation) followed
by one page of whole `HwNode` records, so a client can page a tree of any
size and detect a snapshot that changed under its walk.

First-party programs do not hand-roll this call: the `program` feature of
`lib/procinfo` provides `IpcTransport`, the production `Transport` that posts
a framed request over [`SYSINFO_ENDPOINT`] with `ipc_call` and unwraps the
reply frame with [`decode_reply`]. It is what the `sysinfo` and `ps` `Run`
binaries link (registered at `/System/Apps/sysinfo.app/Run` and `/System/Apps/ps.app/Run`),
so a spawned tool queries the live service; the request/render libraries stay
testable against in-memory fixtures.

## Typed payloads

- [`ProcessListRequest`] — `offset`/`limit` pagination for the two
  process-list queries, so a fixed-size transport buffer never has to
  hold every process at once.
- [`ProcessRecord`] — one process entry. Identity is carried on two axes:
  the numeric `pid`/`parent_pid` (the scheduler task ids, familiar for a
  `ps`-style display but *reused* across process lifetimes) and the
  kernel-attested, never-reused `proc_id`/`parent_proc_id`
  ([`ProcId`], 16 bytes each) — a consumer that must correlate a process
  across time, or distinguish two lifetimes that reused a numeric id, keys
  on the `proc_id` pair. The record also carries `uid`, `gid`,
  [`ProcessState`], the CPU it is currently running on (or
  [`PROCESS_CPU_NONE`] when it is not presently scheduled), and an inline
  (allocation-free) name buffer bounded by [`PROCESS_NAME_MAX`].
- [`KernelMemoryStats`] — total/free/kernel-heap/user-resident bytes and
  the architecture page size.
- [`Uptime`] — the monotonic span since boot as a [`Duration64`] and the
  wall-clock boot instant as a [`Time64`]; absolute time is carried with
  the 64-bit-native time types, never a seconds-only scalar
  (`AGENTS.md` §21).
- [`SystemIdentity`] — the per-installation machine id
  ([`MACHINE_ID_LEN`] bytes), the OS version triple, and an inline
  hostname bounded by [`HOSTNAME_MAX`].
- [`MountListRequest`] — `offset`/`limit` pagination for the mount-list
  query, structurally parallel to [`ProcessListRequest`] but a distinct
  frozen payload (each query owns its argument type, `AGENTS.md` §9).
- [`MountRecord`] — one mount-table entry: the backing `source` (bounded
  by [`MOUNT_SOURCE_MAX`]), the `target` mount point ([`MOUNT_TARGET_MAX`]),
  the driver `fstype` ([`MOUNT_FSTYPE_MAX`]), the [`MountFlags`]
  mount-policy bitmap (`ro`/`nosuid`/`nodev`/`noexec`), and the volume's
  space accounting as a `VolumeStats` usage block (block size plus 64-bit
  total/free/available block and inode counts) — the figures `df` renders.
  Both the flag field and the usage block reuse the filesystem-driver
  ABI's types rather than re-declaring them (`AGENTS.md` §2.2); a mount
  with no live backing volume carries the all-zero usage (the honest "no
  capacity known"), and a decode refuses an internally inconsistent usage
  (available exceeding free, or free exceeding total) whole.
- [`ResourceLimitRecord`] — one row of the `RESOURCE_LIMITS` response: a
  resource's `kind` ([`LimitKind`]), its effective [`ResourceLimit`]
  (soft/hard), and the caller's current live `usage`. The query takes no
  request payload; its response is exactly `LimitKind::COUNT` records in
  discriminant order ([`RESOURCE_LIMITS_REPORT_LEN`] bytes), read
  positionally. See [Resource limits and scalability](../architecture/resource-limits.md).
- [`IrqListRequest`] — `offset`/`limit` pagination for the `IRQ_LIST`
  query (a reserved `flags` field, zero in `sysinfo-v1`), structurally
  parallel to [`MountListRequest`].
- [`IrqRecord`] — one bound interrupt line: the architecture-defined
  `line` id, the kernel-attested `owner` task, the monotonic `count` of
  interrupts delivered since boot, and a `flags` bitmap
  ([`IRQ_FLAG_QUARANTINED`]). A `from_bytes` fails closed on an undefined
  `flags` bit, so an unknown record shape is refused whole rather than
  half-interpreted.

Every payload is `#[repr(C)]`, allocation-free, and exposes a
`to_le_bytes`/`from_bytes` pair; every `from_bytes` is exercised by the
`lib/abi` fuzz harness (`AGENTS.md` §19.6).

[`SYSINFO_VERSION_V1`]: ../../tairix_abi/sysinfo/constant.SYSINFO_VERSION_V1.html
[`SysinfoQueryId`]: ../../tairix_abi/sysinfo/struct.SysinfoQueryId.html
[`SysinfoQuerySpec`]: ../../tairix_abi/sysinfo/struct.SysinfoQuerySpec.html
[`ENCODED_QUERY_TABLE`]: ../../tairix_abi/sysinfo/constant.ENCODED_QUERY_TABLE.html
[`encoded_query_table`]: ../../tairix_abi/sysinfo/fn.encoded_query_table.html
[`SysinfoRequestHeader`]: ../../tairix_abi/sysinfo/struct.SysinfoRequestHeader.html
[`ProcessListRequest`]: ../../tairix_abi/sysinfo/struct.ProcessListRequest.html
[`ProcessRecord`]: ../../tairix_abi/sysinfo/struct.ProcessRecord.html
[`ProcessState`]: ../../tairix_abi/sysinfo/enum.ProcessState.html
[`PROCESS_NAME_MAX`]: ../../tairix_abi/sysinfo/constant.PROCESS_NAME_MAX.html
[`PROCESS_CPU_NONE`]: ../../tairix_abi/sysinfo/constant.PROCESS_CPU_NONE.html
[`ProcId`]: ../../tairix_abi/origin/struct.ProcId.html
[`KernelMemoryStats`]: ../../tairix_abi/sysinfo/struct.KernelMemoryStats.html
[`Uptime`]: ../../tairix_abi/sysinfo/struct.Uptime.html
[`Time64`]: ../../tairix_abi/time/struct.Time64.html
[`Duration64`]: ../../tairix_abi/time/struct.Duration64.html
[`SystemIdentity`]: ../../tairix_abi/sysinfo/struct.SystemIdentity.html
[`MACHINE_ID_LEN`]: ../../tairix_abi/sysinfo/constant.MACHINE_ID_LEN.html
[`HOSTNAME_MAX`]: ../../tairix_abi/sysinfo/constant.HOSTNAME_MAX.html
[`MountListRequest`]: ../../tairix_abi/sysinfo/struct.MountListRequest.html
[`MountRecord`]: ../../tairix_abi/sysinfo/struct.MountRecord.html
[`MOUNT_SOURCE_MAX`]: ../../tairix_abi/sysinfo/constant.MOUNT_SOURCE_MAX.html
[`MOUNT_TARGET_MAX`]: ../../tairix_abi/sysinfo/constant.MOUNT_TARGET_MAX.html
[`MOUNT_FSTYPE_MAX`]: ../../tairix_abi/sysinfo/constant.MOUNT_FSTYPE_MAX.html
[`MountFlags`]: ../../tairix_abi/driver/filesystem/struct.MountFlags.html
[`ResourceLimitRecord`]: ../../tairix_abi/sysinfo/struct.ResourceLimitRecord.html
[`RESOURCE_LIMITS_REPORT_LEN`]: ../../tairix_abi/sysinfo/constant.RESOURCE_LIMITS_REPORT_LEN.html
[`LimitKind`]: ../../tairix_abi/rlimit/enum.LimitKind.html
[`ResourceLimit`]: ../../tairix_abi/rlimit/struct.ResourceLimit.html
[`CapabilityId`]: ../../tairix_abi/capability/struct.CapabilityId.html
[`Errno::BadMagic`]: ../../tairix_abi/error/enum.Errno.html
[`Errno::AbiVersionUnsupported`]: ../../tairix_abi/error/enum.Errno.html
[`Errno::OutOfRange`]: ../../tairix_abi/error/enum.Errno.html
[`Errno::LengthOutOfRange`]: ../../tairix_abi/error/enum.Errno.html
[`Errno`]: ../../tairix_abi/error/enum.Errno.html
[`SYSINFO_ENDPOINT`]: ../../tairix_abi/sysinfo/constant.SYSINFO_ENDPOINT.html
[`SYSINFO_MAX_REQUEST`]: ../../tairix_abi/sysinfo/constant.SYSINFO_MAX_REQUEST.html
[`SYSINFO_MAX_REPLY`]: ../../tairix_abi/sysinfo/constant.SYSINFO_MAX_REPLY.html
[`SYSINFO_REPLY_STATUS_LEN`]: ../../tairix_abi/sysinfo/constant.SYSINFO_REPLY_STATUS_LEN.html
[`encode_reply_ok`]: ../../tairix_abi/sysinfo/fn.encode_reply_ok.html
[`encode_reply_err`]: ../../tairix_abi/sysinfo/fn.encode_reply_err.html
[`decode_reply`]: ../../tairix_abi/sysinfo/fn.decode_reply.html
[`IntrospectDomain`]: ../../tairix_abi/sysinfo/enum.IntrospectDomain.html
[`IrqListRequest`]: ../../tairix_abi/sysinfo/struct.IrqListRequest.html
[`IrqRecord`]: ../../tairix_abi/sysinfo/struct.IrqRecord.html
[`IRQ_FLAG_QUARANTINED`]: ../../tairix_abi/sysinfo/constant.IRQ_FLAG_QUARANTINED.html
