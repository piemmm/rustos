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
| `CPU_INFO`              | none                   | no      |
| `NET_RESOLVER_SERVERS`  | none                   | no      |
| `IRQ_LIST`              | `CAP_SYSINFO_HW`       | yes     |
| `CRASH_RECORD`          | `CAP_SYSINFO_KERNEL`   | yes     |
| `VOLUME_IO_HEALTH`      | `CAP_SYSINFO_KERNEL`   | yes     |
| `RAID_ARRAYS`           | `CAP_SYSINFO_HW`       | yes     |
| `RAID_MEMBERS`          | `CAP_SYSINFO_HW`       | yes     |
| `MEMORY_PRESSURE_BAND`  | none                   | no      |
| `MEMORY_TOTAL`          | none                   | no      |
| `CACHE_LEDGERS`         | `CAP_SYSINFO_KERNEL`   | yes     |
| `CACHE_REPORT`          | none (self-scoped)     | no      |

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
observer. `MEMORY_PRESSURE_BAND` is ungated and unaudited like
`LOAD_AVERAGE`: its `MemoryPressureBand` response is a single band index
into `PRESSURE_BAND_NAMES` and nothing else — no byte figure, no
watermark, no per-task or per-user attribution — and it is *read-only of
the already-published state*, taking no fresh reading, so an unprivileged
caller cannot use it to drive a free-memory sample on demand. It is
strictly coarser than `LOAD_AVERAGE`'s run-queue census, and withholding
it would not protect anything: it would only leave an unprivileged
cooperative reclaimer (`plans/SMARTRAM.md` SMART5) with no way to learn
when to give memory back. The gated, audited `MEMORY_PRESSURE` view below
(free/total bytes, every watermark, the transition history) is unchanged
and is the one query privileged monitoring reads.

`MEMORY_TOTAL` is ungated and unaudited for the same reason, and on even
weaker grounds: its `MemoryTotal` response is one `u64` — the machine's
total usable physical RAM in bytes, the figure printed on the machine's
spec sheet. Installed RAM is a *static hardware fact*, not a runtime
reading: it changes only when RAM is physically added or removed, and it
carries no per-process, per-user, or byte-level state, so it discloses
strictly less than the already-ungated `LOAD_AVERAGE` census (which
varies continuously with what the machine is doing). It exists so a
process can derive a cache budget from the real machine instead of a
hand-picked constant, as the scalability rule requires (`AGENTS.md`
§24.1); withholding it would protect nothing and would only force every
caller back to a constant that a small board outgrows and a large server
wastes.

A zero answer means **unknown and admits nothing**: an unprovisioned
machine (or a kernel that cannot report the census) reports zero, and a
budget scaled from it must come out as "size nothing", never as
"unbounded". The client helper `tairix_procinfo::memory_total_bytes`
passes zero through honestly rather than substituting a default.

The figure is the *same* one the gated `KERNEL_MEMORY_STATS` view reports
as `KernelMemoryStats::total_bytes`: the kernel derives both from one
usable-frame census, so the two can never disagree and there is no second
definition of "how much RAM this machine has". Only the total is exposed
here — the gated, audited `MEMORY_PRESSURE` view (free bytes, watermarks,
the reserve, transition history) is untouched and stays gated.

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
  aggregated across every registered live cache, kernel-measured and
  self-reported alike, with `self_reported_bytes` naming how much of the
  resident total came from the latter. The class ids and the stable names
  in `RECLAIM_CLASS_NAMES` are the shared vocabulary the
  `stats:mem/reclaim/<class>` selectors resolve through.
- `CACHE_LEDGERS` — one `CacheLedgerRecord` per *cache* (paged by a
  `CacheLedgerListRequest`): the breakdown behind those class totals. See
  [Cache ledgers](#cache-ledgers-and-the-one-submission) below.
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
- `RAID_ARRAYS` — one `RaidArrayRecord` per array the RAID composer
  serves (paged by a `RaidListRequest`): the array's 128-bit identity,
  its `RaidLevel`, its `ArrayHealth`, the in-flight scrub/resync flags,
  its in-sync and defined member tallies, its logical block size and
  stripe unit, its block count, the block-service endpoint it is served
  on, the hardware-tree node it is published as, the scrub and resync
  cursors, and its metadata generation.
- `RAID_MEMBERS` — one `RaidMemberRecord` per device the composer holds
  (paged by the same `RaidListRequest`): the array it belongs to (all
  zero for an unaffiliated candidate), its `RaidMemberDisposition`, the
  array slot it occupies (`RAID_SLOT_NONE` for none), the hardware-tree
  node it was offered under, its block-service endpoint, its size, and
  the metadata generation its own superblock carries.

The two RAID queries are sourced from the composer, not the kernel: the
broker forwards each read to the composer's reserved control endpoint and
pages the reply. They are gated like `HARDWARE_TREE` — on
`CAP_SYSINFO_HW`, and audited — because an array report says which
storage devices exist and how they are composed, which is the hardware
view itself rather than kernel operational state; the composer enforces
the identical bar on a caller that asks it directly, so the query cannot
be side-stepped. A machine with no running composer fails closed with the
transport's own error, never a fabricated empty table: "no arrays" and
"nothing answered" are different answers.

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

## Cache ledgers, and the one submission

The reclaim model is two-sided. The kernel's block, filesystem, launch,
and transform caches and a desktop process's glyph atlases, decoded icon
artwork, and rasterised window chrome all declare the same
`ReclaimClass`, obey the same pressure bands, and shrink in the same
order (`plans/SMARTRAM.md`). Only the kernel's side can be *measured*
from outside a process: a process's heap is its own, so nothing but that
process can see how many bytes its glyph atlas is holding.

Left there, the class totals lie. `disposable-ui` is documented as
"rasterised assets, glyph atlases" and is the class reclaim *starts*
with, and it would read zero on a desktop holding megabytes of exactly
that. So the API carries the figures both ways:

- **`CACHE_LEDGERS`** reads one `CacheLedgerRecord` per cache: its label,
  its `CacheOwnerKind` and the numeric owner id the kind carries, its
  class, the reporting process's pid, a `CacheLedgerOrigin` saying
  whether the figures were measured or reported, and the same nine
  figures the class record aggregates. Kernel rows come first, then
  reported rows, in a stable order so a paging client never skips or
  repeats one. Summing the rows of a class reproduces that class's
  `ReclaimClassRecord` exactly — there is one fold, `fold_cache_ledgers`,
  and both views go through it.
- **`CACHE_REPORT`** is the one submission in an otherwise read-only API:
  a `CacheReportRequest` header followed by the caller's own rows, at
  most `MAX_CACHE_REPORT_ENTRIES` of them. It is ungated for the same
  reason `SELF_PROCESS_LIST` is — a process describes only itself, grants
  nothing, and reads nothing — and a count of zero withdraws the
  process's rows, which is what a process does as it tears its caches
  down.

### What stops a lying process

A reported figure is a claim, and the design treats it as one.

- **It never enters the kernel.** The registry of reported rows lives in
  `sysinfod`, in user space. The kernel's own `reclaim_class_stats` sums
  only ledgers it measures itself, and that sum gates a real reclaim
  decision (the `ramzip` compress-out handoff waits for clean and
  transform cache residue to drain). Keeping reported rows out of the
  kernel makes it *structurally impossible* for a process to steer
  reclaim by inflating its own numbers, rather than merely forbidden.
- **The identity is the kernel's, not the caller's.** A submitted row
  must carry `CacheLedgerOrigin::Unset` and a zero `reporter_pid`; a row
  that pre-empts either is refused. `sysinfod` stamps
  `CacheLedgerOrigin::SelfReported` and the pid from the caller's
  kernel-attested `Origin`, so no process can attribute its figures to
  another or present them as measured. Nor can it claim to *be* the
  kernel: a submitted row naming `CacheOwnerKind::KernelSubsystem` is
  refused, because no correct reporter can produce one. The other four
  owner kinds stay open to it — a userland filesystem driver's cache is
  genuinely owned by the volume it caches, a per-task cache by its task,
  and a desktop cache by its seat.
- **The footprint is bounded and expires.** Rows are keyed by the
  caller's unforgeable process-instance id and *replace* that process's
  previous rows, so a process cannot grow its share of the registry by
  reporting repeatedly; the registry's reporter count is derived from the
  machine's RAM rather than hand-picked; and a reporter whose instance is
  no longer live is dropped, so a recycled numeric pid can never inherit
  a dead process's rows.
- **It cannot write the audit log.** The submission is ungated, so it
  emits no audit record — otherwise any process could spam the
  hash-chained journal by calling it. The privileged *reads* are audited,
  exactly as their capability gate implies.
- **It is labelled wherever it is shown.** Every row carries its origin,
  and `ReclaimClassRecord::self_reported_bytes` says how much of a class
  total came from reported ledgers, so an operator can see at a glance
  what is attested and what is claimed.

### How a process reports without polling

`tairix_rt::cachereport` holds the process-wide set of caches. Each time
round its event loop a program calls `publish_if_due()`, which samples
its caches and sends a report only when the sample *differs* from the
last one sent and the minimum interval has elapsed — the comparison is
the change detection, so there are no dirty flags to forget to set. When
a change is suppressed by the interval, `wait_deadline_ns()` returns the
remaining nanoseconds and the program passes them as its `waitset_wait`
timeout, so exactly one bounded wait is armed and only while something is
genuinely pending. A process whose caches are unchanged arms nothing at
all and reports nothing: its last report is still true, because an idle
process is not changing what it holds.

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
  mount-policy bitmap (`ro`/`nosuid`/`nodev`/`noexec`), the volume's live
  [`MountAvailability`] (so a surprise-removed volume never reads as
  healthy), its stable 16-byte volume identity ([`MOUNT_VOLUME_ID_LEN`],
  all-zero when the mount publishes none — the identity a `volume_detach`
  request names), the storage `medium` of the block device backing it, and
  the volume's space accounting as a `VolumeStats` usage block (block size
  plus 64-bit total/free/available block and inode counts) — the figures
  `df` renders. Both the flag field and the usage block reuse the
  filesystem-driver ABI's types rather than re-declaring them
  (`AGENTS.md` §2.2); a mount with no live backing volume carries the
  all-zero usage (the honest "no capacity known"), and a decode refuses an
  internally inconsistent usage (available exceeding free, or free
  exceeding total) whole.

  The medium is the [`BlkDeviceClass`] the backing block device declared,
  read through the typed [`MountRecord::medium`] accessor rather than as a
  raw byte, so a consumer such as the file manager can show a
  medium-appropriate drive icon instead of guessing one. It reuses the
  block ABI's own class vocabulary rather than a second one
  (`AGENTS.md` §2.2) and is `None` — *unknown* — for a mount with no block
  backing (a synthetic or view mount), for a device whose class word this
  ABI does not define, and for a wire byte a decoder does not recognise:
  the record never fabricates a medium nobody reported. On the wire the
  medium is one byte after `availability`, `0` meaning unknown and a known
  class its discriminant plus one; the seven bytes that follow it are
  reserved-must-be-zero and a decode refuses a record that sets them.
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
[`MountRecord::medium`]: ../../tairix_abi/sysinfo/struct.MountRecord.html#method.medium
[`MountAvailability`]: ../../tairix_abi/sysinfo/enum.MountAvailability.html
[`MOUNT_SOURCE_MAX`]: ../../tairix_abi/sysinfo/constant.MOUNT_SOURCE_MAX.html
[`MOUNT_TARGET_MAX`]: ../../tairix_abi/sysinfo/constant.MOUNT_TARGET_MAX.html
[`MOUNT_FSTYPE_MAX`]: ../../tairix_abi/sysinfo/constant.MOUNT_FSTYPE_MAX.html
[`MOUNT_VOLUME_ID_LEN`]: ../../tairix_abi/sysinfo/constant.MOUNT_VOLUME_ID_LEN.html
[`MountFlags`]: ../../tairix_abi/driver/filesystem/struct.MountFlags.html
[`BlkDeviceClass`]: ../../tairix_abi/blkio/enum.BlkDeviceClass.html
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
