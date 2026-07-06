# System Information API (`sysinfo`, `sysinfo-v1`)

RustOS has no `/proc` and no `/sys` (`AGENTS.md` §16.1). Every piece of
live system information that would have lived under those trees is exposed
through one versioned, capability-checked API: the **System Information
API**, whose wire types live in `lib/abi/src/sysinfo.rs`
(`rustos_abi::sysinfo`). The user-space service that answers the queries is
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
a utilisation percentage from the deltas of two samples; RustOS
accounts busy and idle time only, never a fabricated
user/system/nice/iowait split. The list is paged by a
`CpuTimeListRequest` exactly like the mount list.

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
(`rustos_abi::ipc::is_reserved_endpoint`): binding it requires
`CAP_IPC_BIND_PRIVILEGED` (carried by `sysinfod`'s manifest), so an
unprivileged squatter can never claim the endpoint and serve forged system
state. The endpoint's message sizes are one shared contract:
[`SYSINFO_MAX_REQUEST`] bounds the request the server accepts, and
[`SYSINFO_MAX_REPLY`] bounds the framed reply it delivers (one page of
records past the status word). The server sizes its endpoint by these
constants and every client sizes its buffers by them, so neither keeps a
private copy that could drift; a list longer than one page is paged across
successive requests (a client advancing `offset`/shrinking `limit`).

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
  the driver `fstype` ([`MOUNT_FSTYPE_MAX`]), and the [`MountFlags`]
  mount-policy bitmap (`ro`/`nosuid`/`nodev`/`noexec`). The flag field
  reuses the filesystem-driver ABI's `MountFlags` rather than re-declaring
  the flag algebra (`AGENTS.md` §2.2).
- [`ResourceLimitRecord`] — one row of the `RESOURCE_LIMITS` response: a
  resource's `kind` ([`LimitKind`]), its effective [`ResourceLimit`]
  (soft/hard), and the caller's current live `usage`. The query takes no
  request payload; its response is exactly `LimitKind::COUNT` records in
  discriminant order ([`RESOURCE_LIMITS_REPORT_LEN`] bytes), read
  positionally. See [Resource limits and scalability](../architecture/resource-limits.md).

Every payload is `#[repr(C)]`, allocation-free, and exposes a
`to_le_bytes`/`from_bytes` pair; every `from_bytes` is exercised by the
`lib/abi` fuzz harness (`AGENTS.md` §19.6).

[`SYSINFO_VERSION_V1`]: ../../rustos_abi/sysinfo/constant.SYSINFO_VERSION_V1.html
[`SysinfoQueryId`]: ../../rustos_abi/sysinfo/struct.SysinfoQueryId.html
[`SysinfoQuerySpec`]: ../../rustos_abi/sysinfo/struct.SysinfoQuerySpec.html
[`ENCODED_QUERY_TABLE`]: ../../rustos_abi/sysinfo/constant.ENCODED_QUERY_TABLE.html
[`encoded_query_table`]: ../../rustos_abi/sysinfo/fn.encoded_query_table.html
[`SysinfoRequestHeader`]: ../../rustos_abi/sysinfo/struct.SysinfoRequestHeader.html
[`ProcessListRequest`]: ../../rustos_abi/sysinfo/struct.ProcessListRequest.html
[`ProcessRecord`]: ../../rustos_abi/sysinfo/struct.ProcessRecord.html
[`ProcessState`]: ../../rustos_abi/sysinfo/enum.ProcessState.html
[`PROCESS_NAME_MAX`]: ../../rustos_abi/sysinfo/constant.PROCESS_NAME_MAX.html
[`PROCESS_CPU_NONE`]: ../../rustos_abi/sysinfo/constant.PROCESS_CPU_NONE.html
[`ProcId`]: ../../rustos_abi/origin/struct.ProcId.html
[`KernelMemoryStats`]: ../../rustos_abi/sysinfo/struct.KernelMemoryStats.html
[`Uptime`]: ../../rustos_abi/sysinfo/struct.Uptime.html
[`Time64`]: ../../rustos_abi/time/struct.Time64.html
[`Duration64`]: ../../rustos_abi/time/struct.Duration64.html
[`SystemIdentity`]: ../../rustos_abi/sysinfo/struct.SystemIdentity.html
[`MACHINE_ID_LEN`]: ../../rustos_abi/sysinfo/constant.MACHINE_ID_LEN.html
[`HOSTNAME_MAX`]: ../../rustos_abi/sysinfo/constant.HOSTNAME_MAX.html
[`MountListRequest`]: ../../rustos_abi/sysinfo/struct.MountListRequest.html
[`MountRecord`]: ../../rustos_abi/sysinfo/struct.MountRecord.html
[`MOUNT_SOURCE_MAX`]: ../../rustos_abi/sysinfo/constant.MOUNT_SOURCE_MAX.html
[`MOUNT_TARGET_MAX`]: ../../rustos_abi/sysinfo/constant.MOUNT_TARGET_MAX.html
[`MOUNT_FSTYPE_MAX`]: ../../rustos_abi/sysinfo/constant.MOUNT_FSTYPE_MAX.html
[`MountFlags`]: ../../rustos_abi/driver/filesystem/struct.MountFlags.html
[`ResourceLimitRecord`]: ../../rustos_abi/sysinfo/struct.ResourceLimitRecord.html
[`RESOURCE_LIMITS_REPORT_LEN`]: ../../rustos_abi/sysinfo/constant.RESOURCE_LIMITS_REPORT_LEN.html
[`LimitKind`]: ../../rustos_abi/rlimit/enum.LimitKind.html
[`ResourceLimit`]: ../../rustos_abi/rlimit/struct.ResourceLimit.html
[`CapabilityId`]: ../../rustos_abi/capability/struct.CapabilityId.html
[`Errno::BadMagic`]: ../../rustos_abi/error/enum.Errno.html
[`Errno::AbiVersionUnsupported`]: ../../rustos_abi/error/enum.Errno.html
[`Errno::OutOfRange`]: ../../rustos_abi/error/enum.Errno.html
[`Errno::LengthOutOfRange`]: ../../rustos_abi/error/enum.Errno.html
[`Errno`]: ../../rustos_abi/error/enum.Errno.html
[`SYSINFO_ENDPOINT`]: ../../rustos_abi/sysinfo/constant.SYSINFO_ENDPOINT.html
[`SYSINFO_MAX_REQUEST`]: ../../rustos_abi/sysinfo/constant.SYSINFO_MAX_REQUEST.html
[`SYSINFO_MAX_REPLY`]: ../../rustos_abi/sysinfo/constant.SYSINFO_MAX_REPLY.html
[`SYSINFO_REPLY_STATUS_LEN`]: ../../rustos_abi/sysinfo/constant.SYSINFO_REPLY_STATUS_LEN.html
[`encode_reply_ok`]: ../../rustos_abi/sysinfo/fn.encode_reply_ok.html
[`encode_reply_err`]: ../../rustos_abi/sysinfo/fn.encode_reply_err.html
[`decode_reply`]: ../../rustos_abi/sysinfo/fn.decode_reply.html
[`IntrospectDomain`]: ../../rustos_abi/sysinfo/enum.IntrospectDomain.html
