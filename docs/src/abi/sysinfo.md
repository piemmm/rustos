# System Information API (`sysinfo`, `sysinfo-v1`)

RustOS has no `/proc` and no `/sys` (`AGENTS.md` §16.1). Every piece of
live system information that would have lived under those trees is exposed
through one versioned, capability-checked API: the **System Information
API**, whose wire types live in `lib/abi/src/sysinfo.rs`
(`rustos_abi::sysinfo`). The user-space service that answers the queries is
`/System/Services/sysinfod` (`userland/system/sysinfod`); the command-line
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

`CAP_SYSINFO_GLOBAL`, `CAP_SYSINFO_KERNEL`, and `CAP_SYSINFO_HW` are
[`CapabilityId`] values 13, 14, and 15. Self-scoped observers ("list my
own processes") require no capability; the global view does
(`AGENTS.md` §16.6). The hardware-tree query gates the read-only view of
the detected hardware tree (`AGENTS.md` §18.4).

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

## Typed payloads

- [`ProcessListRequest`] — `offset`/`limit` pagination for the two
  process-list queries, so a fixed-size transport buffer never has to
  hold every process at once.
- [`ProcessRecord`] — one process entry: `pid`, `parent_pid`, `uid`,
  `gid`, [`ProcessState`], last CPU, and an inline (allocation-free)
  name buffer bounded by [`PROCESS_NAME_MAX`].
- [`KernelMemoryStats`] — total/free/kernel-heap/user-resident bytes and
  the architecture page size.
- [`Uptime`] — the monotonic span since boot as a [`Duration64`] and the
  wall-clock boot instant as a [`Time64`]; absolute time is carried with
  the 64-bit-native time types, never a seconds-only scalar
  (`AGENTS.md` §21).
- [`SystemIdentity`] — the per-installation machine id
  ([`MACHINE_ID_LEN`] bytes), the OS version triple, and an inline
  hostname bounded by [`HOSTNAME_MAX`].

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
[`KernelMemoryStats`]: ../../rustos_abi/sysinfo/struct.KernelMemoryStats.html
[`Uptime`]: ../../rustos_abi/sysinfo/struct.Uptime.html
[`Time64`]: ../../rustos_abi/time/struct.Time64.html
[`Duration64`]: ../../rustos_abi/time/struct.Duration64.html
[`SystemIdentity`]: ../../rustos_abi/sysinfo/struct.SystemIdentity.html
[`MACHINE_ID_LEN`]: ../../rustos_abi/sysinfo/constant.MACHINE_ID_LEN.html
[`HOSTNAME_MAX`]: ../../rustos_abi/sysinfo/constant.HOSTNAME_MAX.html
[`CapabilityId`]: ../../rustos_abi/capability/struct.CapabilityId.html
[`Errno::BadMagic`]: ../../rustos_abi/error/enum.Errno.html
[`Errno::AbiVersionUnsupported`]: ../../rustos_abi/error/enum.Errno.html
[`Errno::OutOfRange`]: ../../rustos_abi/error/enum.Errno.html
[`Errno::LengthOutOfRange`]: ../../rustos_abi/error/enum.Errno.html
