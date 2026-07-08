# Kernel syscall subsystem

This page documents the architecture-neutral half of the RustOS syscall
ABI delivered by Stage 2.7 of `PLAN.md`: the frozen `abi-v1` table in
`rustos_abi::syscalls` and the generated kernel dispatcher in
`rustos_kernel_syscall::table`. The full rustdoc for those modules is
published alongside this book; refer to the `cargo doc --no-deps` output
in `target/doc/` for the per-item documentation.

Per-architecture entry stubs that marshal real syscall registers into a
`RawArgs` tuple are delivered separately by Stage 3 and are *out of
scope* for this page.

## Cross-checked source of truth

The user/kernel syscall contract is split across two files that
`cargo xtask abi-check` keeps in lock-step:

| Half       | File                                | Owner                        |
| ---------- | ----------------------------------- | ---------------------------- |
| Source     | `lib/abi/src/syscalls.rs`           | Frozen `abi-v1` declaration. |
| Generated  | `kernel/syscall/src/table.rs`       | Dispatcher + table hash.     |

Both halves must ship together. Either half existing without the other
is a hard error; `cargo xtask abi-check` fails the build at that point.

The source half exposes a `&'static [SyscallSpec]` table and a
deterministic byte encoding `ENCODED_TABLE`. The kernel half exposes the
SHA-256 fingerprint of that encoding as `SYSCALL_TABLE_HASH` — but there
is **no hand-maintained literal**: `kernel/syscall/build.rs` derives the
value from `ENCODED_TABLE` at build time and `table.rs` `include!`s it
(`AGENTS.md` §2.2 — one definition, nothing to edit or to let drift). A
change to the table re-derives the fingerprint on the next build.

The kernel re-checks the value at boot (`verify_table_hash`), and
`cargo xtask abi-check` recomputes the SHA-256 of `ENCODED_TABLE` and
demands that the linked `rustos_kernel_syscall::SYSCALL_TABLE_HASH`
matches it (catching stale `target/` caches or a mismatched
`rustos-abi`). A test in `tools/xtask/src/commands/abi_check.rs`
asserts the linked, build-derived constant equals a freshly computed
digest.

## `abi-v1` syscall table

The table **grows by appending**: existing entries are never re-numbered,
removed, or re-typed, and a new syscall takes the next free number. While
`abi-v1` is unfrozen (RustOS has not shipped a release, `AGENTS.md` §9 /
§2.13) a new row also requires regenerating the C header
(`cargo xtask c-header --write`); `SYSCALL_TABLE_HASH` needs no manual
step — it is re-derived from `ENCODED_TABLE` by the build script. The
`abi-check` and `c-header` drift guards enforce both. From the first
release onward the table is frozen and new behaviour ships as `abi-v2`.

| No. | Name           | Args                                    | Returns | Required capability     | Audited |
| ---:| -------------- | --------------------------------------- | ------- | ----------------------- | ------- |
|   0 | `yield`        | —                                       | `unit`  | —                       | no      |
|   1 | `exit`         | `i32 code`                              | `unit`  | —                       | yes     |
|   2 | `ipc_send`     | `endpoint`, `user_ptr`, `len`           | `errno` | —                       | yes     |
|   3 | `ipc_recv`     | `endpoint`, `user_ptr`, `len`           | `errno` | —                       | no      |
|   4 | `cap_query`    | `cap`                                   | `u32`   | —                       | no      |
|   5 | `cap_delegate` | `target_handle`, `user_ptr`             | `errno` | —                       | yes     |
|   6 | `cap_revoke`   | `target_handle`, `cap`                  | `errno` | `CAP_USER_ADMIN`        | yes     |
|   7 | `clock_get`    | —                                       | `u64`   | —                       | no      |
|   8 | `irq_bind`     | `u32 line`                              | `IrqHandle` | `CAP_IRQ_BIND`      | yes     |
|   9 | `irq_wait`     | `IrqHandle handle`, `u64 timeout_ns`    | `errno` | `CAP_IRQ_BIND`          | no      |
|  10 | `random_get`   | `user_ptr`, `len`, `u32 flags`          | `u64`   | —                       | no      |
|  11 | `stream_write` | `u32 fd`, `user_ptr`, `len`             | `u64`   | `CAP_CONSOLE_WRITE`     | no      |
|  12 | `spawn`        | `user_ptr` (path), `len`, `u64 console` | `u64` (pid) | `CAP_PROC_SPAWN`    | yes     |
|  13 | `stream_read`  | `u32 fd`, `user_ptr`, `len`             | `u64`   | `CAP_CONSOLE_READ`      | no      |
|  14 | `mem_map`      | `len`, `u32 flags`, `u64 addr_hint`     | `u64` (base) | —                  | no      |
|  15 | `mem_unmap`    | `u64 base`, `len`                       | `errno` | —                       | no      |
|  16 | `wait`         | `i32 pid`, `user_ptr` (status), `u32 flags` | `u64` (pid) | —               | yes     |
|  17 | `rlimit_get`   | `u32 kind`, `user_ptr` (out)            | `errno` | —                       | no      |
|  18 | `rlimit_set`   | `u32 kind`, `user_ptr` (value)          | `errno` | —                       | yes     |
|  19 | `users_db_read`| `user_ptr` (buf), `len`                 | `u64` (bytes) | `CAP_USERS_READ`  | yes     |
|  20 | `console_count`| —                                       | `u64` (count) | `CAP_CONSOLE_WRITE` | no    |
|  21 | `stream_input_mode` | `u32 fd`, `u32 mode`               | `errno` | `CAP_CONSOLE_READ`      | no      |
|  22 | `key_inject`   | `user_ptr` (record), `len`              | `u64` (bytes) | `CAP_INPUT_INJECT` | no    |
|  23 | `display_acquire` | —                                    | `u64` (lease generation) | `CAP_DISPLAY` | yes |
|  24 | `display_release` | —                                    | `errno` | `CAP_DISPLAY`           | yes     |
|  25 | `keyboard_read`| `user_ptr` (buf), `len`                 | `u64` (bytes) | `CAP_INPUT_READ`  | no      |
|  26 | `mmio_map`     | `Handle handle`, `len offset`, `len`    | `u64` (base vaddr) | `CAP_MMIO_MAP` | yes  |
|  27 | `dma_alloc`    | `Handle handle`, `len`, `user_ptr` (device_out) | `u64` (base vaddr) | `CAP_MEM_DMA` | yes |
|  28 | `resource_grants` | `user_ptr` (buf), `len`              | `u64` (bytes) | —                 | no      |
|  29 | `hw_tree_read` | `user_ptr` (buf), `len`                 | `u64` (bytes) | `CAP_SYSINFO_HW`  | no      |
|  30 | `hw_tree_wait` | `u64 last_generation`, `u64 timeout_ns` | `errno` | `CAP_SYSINFO_HW`        | no      |
|  31 | `ipc_call`     | `IpcEndpoint`, `user_ptr` (req), `len`, `user_ptr` (reply), `len` | `u64` (bytes) | — | yes |
|  32 | `call_create`  | `IpcEndpoint`, `user_ptr` (send caps), `user_ptr` (recv caps), `len`, `len`, `len` | `errno` | — | yes |
|  33 | `call_recv`    | `IpcEndpoint`, `user_ptr` (buf), `len`, `user_ptr` (ticket out) | `u64` (bytes) | — | no |
|  34 | `call_reply`   | `IpcEndpoint`, `Handle` (ticket), `user_ptr` (reply), `len`      | `errno` | — | no |
|  35 | `users_db_wait`| `u64 timeout_ns`                        | `errno` | `CAP_USERS_READ`  | no      |
|  36 | `log_emit`     | `user_ptr` (record), `len`              | `errno` | `CAP_LOG_EMIT`    | no      |
|  37 | `hw_emit_node` | `user_ptr` (node), `len`                | `errno` | `CAP_HW_EMIT`     | yes     |
|  38 | `hw_remove_node` | `u64 node_id`                         | `errno` | `CAP_HW_EMIT`     | yes     |
|  46 | `fs_open`      | `user_ptr` (path), `len`, `u32 flags`   | `u64` (fd)    | `CAP_FS_ACCESS` | yes   |
|  47 | `fs_close`     | `u32 fd`                                | `errno`       | — (backing)     | no    |
|  48 | `fs_read`      | `u32 fd`, `u64 offset`, `user_ptr`, `len` | `u64` (bytes) | — (backing)     | no  |
|  49 | `fs_write`     | `u32 fd`, `u64 offset`, `user_ptr`, `len` | `u64` (bytes) | — (backing)     | yes |
|  50 | `fs_readdir`   | `u32 fd`, `user_ptr` (buf), `len`       | `u64` (bytes) | `CAP_FS_ACCESS` | no    |
|  51 | `fs_stat`      | `u32 fd`, `user_ptr` (out), `len`       | `u64` (bytes) | `CAP_FS_ACCESS` | no    |
|  52 | `fs_truncate`  | `u32 fd`, `u64 size`                    | `errno`       | `CAP_FS_ACCESS` | yes   |
|  53 | `fs_sync`      | `u32 fd`                                | `errno`       | `CAP_FS_ACCESS` | no    |
|  54 | `fs_mkdir`     | `user_ptr` (path), `len`                | `errno`       | `CAP_FS_ACCESS` | yes   |
|  55 | `fs_unlink`    | `user_ptr` (path), `len`, `u32 flags`   | `errno`       | `CAP_FS_ACCESS` | yes   |
|  56 | `dma_free`     | `Handle handle`, `u64 cpu_va`           | `errno`       | `CAP_MEM_DMA`   | yes   |
|  57 | `fs_rename`    | `user_ptr` (src), `len`, `user_ptr` (dst), `len` | `errno` | `CAP_FS_ACCESS` | yes |
|  58 | `call_peer_origin` | `IpcEndpoint`, `Handle` (ticket), `user_ptr` (origin out), `len` | `u64` (bytes) | — | no |
|  59 | `wall_time_get` | `user_ptr` (out), `len`                | `u64` (bytes) | — | no |
|  60 | `wall_time_set` | `user_ptr` (time), `len`, `u32 state`  | `errno` | `CAP_TIME_SET` | yes |
|  61 | `boot_id_get`  | `user_ptr` (out), `len`                | `u64` (bytes) | — | no |
|  62 | `sysinfo_introspect` | `u32 domain`, `u64 arg`, `user_ptr` (out), `len` | `u64` (bytes) | `CAP_SYSINFO_INTROSPECT` | no |
|  63 | `terminal_size` | `u32 fd`, `user_ptr` (out), `len`      | `u64` (bytes) | — | no |
|  64 | `signal`       | `i32 pid`, `u32 signal`                 | `errno`       | —               | yes   |
|  65 | `fs_chdir`     | `user_ptr` (path), `len`                | `errno`       | `CAP_FS_ACCESS` | yes   |
|  66 | `fs_getcwd`    | `user_ptr` (buf), `len`                 | `u64` (bytes) | —               | no    |
|  67 | `resource_open` | `user_ptr` (ref), `len`, `u32 flags`   | `u64` (fd)    | —               | yes   |
|  68 | `self_origin`  | `user_ptr` (out), `len`                | `u64` (bytes) | —               | no    |
|  69 | `users_admin`  | `user_ptr` (req), `len`, `user_ptr` (out), `len` | `u64` (bytes) | `CAP_USER_ADMIN` | yes |
|  70 | `seat_switch`  | `u64 seat`, `u32 console`               | `errno`       | `CAP_SEAT_ADMIN` | yes |
|  71 | `seat_revoke`  | `u64 seat`                              | `errno`       | `CAP_SEAT_ADMIN` | yes |
|  72 | `console_foreground` | `u32 fd`, `i32 pid`               | `errno`       | `CAP_CONSOLE_READ` | yes |

(Syscall numbers 39–45 — `msi_alloc`, `shm_create`/`shm_map`/`shm_unmap`,
`waitset_create`/`waitset_ctl`/`waitset_wait` — are defined in
`lib/abi/src/syscall.rs`; their rows are not yet transcribed into this table.)

`fs_chdir` (no. 65) and `fs_getcwd` (no. 66) give each process a working
directory. A path handed to any path-taking filesystem call (`fs_open`,
`fs_mkdir`, `fs_unlink`, `fs_rename`, and `fs_chdir` itself) is resolved at
the single kernel entry point (`copy_path_in`): an absolute `/`-view path is
normalised through the shared path parser (`lib/path`), and a relative path
is first joined onto the caller's current working directory, so `.`/`..` are
collapsed and `..` can never escape the root. `fs_chdir` re-authorises its
resolved target as a *searchable directory* through the secured VFS (the same
resolve-only, `DIRECTORY`-flag check `fs_open` performs) under the caller's
real credentials and only then records it as the new working directory — a
refused change leaves the directory untouched (fail closed). A child inherits
its spawner's working directory. `fs_getcwd` copies the stored directory out
and needs no capability (reading one's own directory grants no authority).
The per-process directory lives beside the task's streams and limits in
`kernel/core::aspace::AddressSpaceRegistry` and is dropped when the task
exits. An alias spelling (`Alias:/…` or the expanded `alias::Name/…`) names a
first-class storage root: a *machine alias* (`System:`, `Users:`, `Apps:`,
`Storage:`) is the canonical root the `/` view projects as `/<Name>`, so
`System:/Logs/a` resolves to the same object as `/System/Logs/a` and is then
subject to the identical inode/mount-flag authorisation. `lib/path` already
refuses any `..` that would escape the alias root. A name that is not a
published root fails closed with `NotFound` before the VFS is touched; session
and volume aliases are published by their owning services when those land.

`resource_open` (no. 67) is the resource-reference analogue of `fs_open`
(`plans/ALIAS.md`, `.junie/PREREQUISITES2.md` P5). A resource reference
(`sys:random`, `sys:null`, …) names a typed *non-filesystem* resource — there
is no `/dev`, `/proc`, or `/sys` — so the call copies the reference in, parses
it with the single shared reference parser (`lib/resref`, never a second
parser), and resolves it through the capability-checked namespace resolver in
`kernel/core::resource`. Authorisation is per namespace and selector, so the
call carries **no** blanket dispatcher capability: an unprivileged resource
(`sys:random`, `sys:null`) needs none, while a privileged namespace is checked
against the kernel-attested caller inside the resolver and fails closed. Only
the `sys:` namespace's unprivileged members are served today; every other
namespace has no resolver wired yet and fails closed (`NotImplemented`) rather
than fabricating a resource — resolvers are added in place as their consumers
land. On success the call records a **resource-backed** descriptor in the
caller's per-process table, drawn from the *same* number space as `fs_open` so
a resource fd can never collide with a file fd. That descriptor is read and
written with `fs_read` / `fs_write` and released with `fs_close`, exactly as a
file handle is; the read/write handler dispatches on the descriptor's backing
(a path routes through the secured VFS and still requires `CAP_FS_ACCESS`; a
resource routes to its subsystem — `sys:random` streams the CSPRNG reserve
`random_get` draws from, `sys:null` reads as end of stream and discards
writes). `fs_readdir` / `fs_stat` / `fs_truncate` / `fs_sync` on a
resource-backed descriptor fail closed (`OutOfRange`) — those are filesystem
operations with no meaning for a resource. Because `fs_read` / `fs_write` /
`fs_close` must serve either backing, their capability check moved out of the
dispatcher into the handler (a path-backed descriptor still requires
`CAP_FS_ACCESS`), so reading `sys:random` never demands filesystem access.

### Capability matrix

The dispatcher consults `kernel/sec`'s `TaskCapabilities::has` against
the syscall's `required_capability` before any handler runs. The matrix
is exhaustive — anything not listed below is ungated:

| Capability         | Syscalls gated by it       |
| ------------------ | -------------------------- |
| `CAP_USER_ADMIN`   | `cap_revoke`, `users_admin` |
| `CAP_IRQ_BIND`     | `irq_bind`, `irq_wait`     |
| `CAP_CONSOLE_WRITE`| `stream_write`, `console_count` |
| `CAP_PROC_SPAWN`   | `spawn`                    |
| `CAP_CONSOLE_READ` | `stream_read`, `stream_input_mode`, `console_foreground` |
| `CAP_USERS_READ`   | `users_db_read`, `users_db_wait` |
| `CAP_INPUT_INJECT` | `key_inject`               |
| `CAP_DISPLAY`      | `display_acquire`, `display_release` |
| `CAP_INPUT_READ`   | `keyboard_read`            |
| `CAP_MMIO_MAP`     | `mmio_map`                 |
| `CAP_MEM_DMA`      | `dma_alloc`, `dma_free`    |
| `CAP_SYSINFO_HW`   | `hw_tree_read`, `hw_tree_wait` |
| `CAP_SYSINFO_INTROSPECT` | `sysinfo_introspect` |
| `CAP_LOG_EMIT`     | `log_emit`                 |
| `CAP_HW_EMIT`      | `hw_emit_node`, `hw_remove_node` |
| `CAP_FS_ACCESS`    | `fs_open`, `fs_readdir`, `fs_stat`, `fs_truncate`, `fs_sync`, `fs_mkdir`, `fs_unlink`, `fs_rename`, `fs_chdir` (the path-taking calls), and — enforced *in the handler* on a path-backed descriptor — `fs_read`, `fs_write`, `fs_close` |
| `CAP_TIME_SET`     | `wall_time_set`            |

The `CAP_IRQ_BIND` rationale, the wake-up contract, and the failure
modes are documented in
[`security/irq.md`](../security/irq.md).

A future syscall that needs e.g. `CAP_DRV_LOAD` lands as a new entry in
the table and a new row here; existing rows never move.

`mmio_map` (no. 26) maps a **granted** device MMIO register window into the
calling driver's own address space (`plans/PI.md` P10 chunk 5d-0 — the
`DriverHost` MMIO/DMA surface reachable over IPC). A user-space driver does
not pass a raw physical address: its `handle` argument is an unforgeable,
kernel-issued device-resource grant it received for the hardware-tree node
it binds (one grant per `rustos_abi::hwtree::HwResource` the node requested,
`AGENTS.md` §18.3), and its `offset` / `len` arguments name the sub-region
*within* that grant to map. The handler resolves the handle **against the
calling task** through the per-task device-resource grant table that lives in
`kernel/core::aspace::AddressSpaceRegistry` (minted at driver admission via
`AddressSpaceRegistry::mint_grant`, resolved by `AddressSpaceRegistry::grant`,
and reclaimed when the task is withdrawn on exit — the same per-process
lifecycle as the task's streams and limits) — a handle minted for another
task, or an unknown handle, resolves to nothing and is refused with
`NotFound`, exactly the forgery defence `irq_wait` applies to its binding
(`AGENTS.md` §5.4) — confirms the grant names a memory window
(`HwResourceKind::Mmio` / `BusWindow`, else `OutOfRange`), confirms
`[offset, offset + len)` lies wholly inside that window
(`kernel/core::devres::mappable_subwindow`, else `OutOfRange`), and maps
**only** that sub-region — caching disabled — through the architecture
`kernel/core::devres::MmioMapFacility` producer, returning its base user
virtual address. Mapping a bounded sub-region rather than the whole grant is
what lets a driver granted a large outbound bus aperture (the BCM2711 PCIe
1 GiB outbound window) map just the single BAR it enumerated, instead of the
whole window — which would exhaust the per-task MMIO virtual window and fail
closed with `OutOfMemory` (`AGENTS.md` §24.1). A driver therefore never
reaches physical memory the kernel did not grant it (`AGENTS.md` §4 — no
ambient authority). It is
gated on `CAP_MMIO_MAP` and **audited** (a low-volume, security-relevant
grant of direct hardware access). A task with no minted grant resolves to
nothing (`NotFound`), and the mapping mechanism defaults to a fail-closed
NULL producer (`NULL_MMIO_MAP_FACILITY` → `NotImplemented`), so a kernel
that installs neither the grant-minting driver-spawn path nor the
`kernel/mem` live-mapping producer denies every `mmio_map` rather than
mapping (`AGENTS.md` §2.9). Both are now landed: the live-mapping producer
(`LiveMmioMap`) and the driver-spawn grant minter (the privileged
`KernelSpawnCtx` mints one grant per the matched node's requested
`HwResource` at admission — `plans/PI.md` P10 chunk 5d-2-ii).

`dma_alloc` (no. 27) carves a **coherent DMA buffer** into the calling
driver's own address space, bounded by a granted device DMA constraint
(`plans/PI.md` P10 chunk 5d-0). Like `mmio_map` it takes an unforgeable,
kernel-issued device-resource grant `handle` (here a
`HwResourceKind::Dma` constraint) and resolves it **owner-checked against
the calling task** through the same per-task grant table (a forged or
foreign handle → `NotFound`, `AGENTS.md` §5.4). It then validates the
constraint (`kernel/core::devres::dma_constraint`), refuses a zero-length
or over-the-grant-maximum request (`LengthOutOfRange` / `OutOfRange`), and
carves a physically-contiguous, zeroed, coherent block — mapped `RW`,
non-executable, guard-bracketed — into the caller's own live address space
through the architecture `kernel/core::devres::DmaAllocFacility` producer,
bounded so the block lies wholly below the grant's CPU-side addressing
limit (`AGENTS.md` §4 / §18.3). It returns the buffer's base user virtual
address and writes the **device-visible** base to the `device_out` user
pointer through the validated copy-out boundary, exactly as `wait` writes
its status. The device-visible base is resolved by
`kernel/core::devres::translate_device_addr`: for a coherent (untranslated)
constraint it is the CPU-physical base itself (the QEMU `virt` /
coherent-bus case); for a **translating inbound viewport**
(`HwResource::dma_translated`, e.g. the Pi 4 PCIe root complex's
`IB MEM 0x0..0x1ffffffff -> 0x4_0000_0000` `dma-ranges`) the CPU-physical
base is re-based onto the far side of the viewport — checked, never wrapped
(`OutOfRange` if it escapes the aperture, `AGENTS.md` §18.1 / §2.9) — so the
device issues the bus address the bridge translates back to the carved RAM.
The backing frames are zeroed and returned to the allocator when the task's
live space is dropped on exit (`LiveSpace::drop` — zero-on-free, §4). It is
gated on **`CAP_MEM_DMA`** and **audited** (a low-volume, security-relevant
grant of hardware-reachable memory); the carve mechanism defaults to a
fail-closed NULL producer (`NULL_DMA_ALLOC_FACILITY` → `NotImplemented`),
so a kernel without the `kernel/mem` live producer denies rather than
carving (`AGENTS.md` §2.9). The first-party Rust wrapper is
`rustos_rt::dma_alloc`.

`dma_free` (no. 56) is the **symmetric free** for `dma_alloc`: a driver that
issues many transfers must reclaim each request's bounce buffers, or it leaks
DMA frames until it exits — an OS expected to run for years cannot leak per
I/O (`AGENTS.md` §26). It takes the same unforgeable DMA-constraint grant
`handle` and the buffer's `cpu_va` (the base virtual address `dma_alloc`
returned), resolves the handle owner-checked against the calling task (a
forged or foreign handle → `NotFound`), validates the constraint, then
releases the buffer through the same `DmaAllocFacility` (`free`), which
zeroes every backing byte (zero-on-free, §4) before its frames return to the
allocator, and re-freezes the caller's address-space snapshot so the
released window leaves the copy path's view. Only `cpu_va` crosses the trap;
the buffer's extent is the allocator's authoritative per-task record, so a
`cpu_va` that is not the base of a live carve in *this task's* DMA window
fails closed (covering a stale, double, or cross-task free) without releasing
anything (§5.4 — fail closed). Like `dma_alloc` it is gated on
**`CAP_MEM_DMA`** and audited, and the mechanism defaults to the fail-closed
NULL producer (`NotImplemented`). The first-party Rust wrapper is
`rustos_rt::dma_free`; the user-space driver host (`rustos_drvrt`) mints each
carve's `DmaSlab` so its `Drop` issues `dma_free` automatically — a driver's
per-request slabs reclaim themselves at scope end, never leaking. (Frames a
driver never frees are still reclaimed wholesale when its live space is
dropped on exit, `LiveSpace::drop`; `dma_free` is what keeps a *running*
driver's footprint bounded.)

`resource_grants` (no. 28) enumerates the device-resource grants the kernel
minted for the calling driver task, delivering the unforgeable handles it
passes to `mmio_map` / `dma_alloc` (`plans/PI.md` P10 chunk 5d-2 — handing a
spawned driver process the handles for its matched node). The handler
serialises the **calling task's** grant set (`caller.task_id` is
kernel-trusted, §5.4) from the same per-task `AddressSpaceRegistry` grant
table as consecutive `rustos_abi::hwtree::GrantedResource` records (handle +
`HwResource`, `GrantedResource::WIRE_LEN` = 40 bytes each, in ascending
handle order), copies them out through the validated boundary, and returns
the total byte count — `0` for a task with no grants (an unbound driver is
normal, §18.4). A buffer too small for the whole set is refused whole with
`BufferTooSmall` rather than delivering a partial list (`AGENTS.md` §2.9); a
driver sizes its buffer for the matched node's resource count. It is
deliberately **ungated** (no row's capability): a task reads only its *own*
grants, which confers no authority — the handles are useless without the
`CAP_MMIO_MAP` / `CAP_MEM_DMA` the driver also holds, and the kernel
re-checks ownership when they are presented (the §16.6 / §24.3 own-process
baseline). It is unaudited per call — the device manager's one-time driver
load is the audited security decision (§5.4.4 / §18.3). The first-party Rust
wrapper is `rustos_rt::resource_grants` (the user-space driver host
`rustos_drvrt::RtDriverHost::from_grants_query` builds its grant table from
it); the C stub is `ros_sys_resource_grants`.

`hw_tree_read` (no. 29) and `hw_tree_wait` (no. 30) expose the discovered
hardware tree the kernel built at boot (`AGENTS.md` §16.6 / §18.1 / §18.4) —
the read side of the user-space device manager. `hw_tree_read` copies the
current snapshot into the caller's `(buf, len)` buffer: a
`rustos_abi::HwTreeHeader` (the store's current **generation** and node count)
followed by that many `rustos_abi::HwNode` records, returning the byte count.
The whole inventory is copied or none — an undersized buffer is refused with
`BufferTooSmall`, never truncated, so the caller grows its buffer and retries
(the node count is a discovered capacity, not a fixed ceiling, §24.1).
`hw_tree_wait` blocks until the store's generation advances past
`last_generation` (the value from the last header), returning `0` once the
tree has changed or `TimedOut` when `timeout_ns` elapses first — the reactive
re-match / hotplug signal (§18.4). Both are gated on **`CAP_SYSINFO_HW`**, the
privileged *global* hardware view (never the ambient own-process baseline),
and both are **unaudited per call** — they are the high-volume reactive
device-manager path, and the audited security decision is the subsequent
driver load (§5.4.4 / §18.3). Both serve the `kernel/core` `HwTreeSource` seam
the boot path installs through `BootInfo::with_hw_tree` (the
`hwtree_store::HW_TREE` store); until one is installed they fail closed with
`NotImplemented` through `NULL_HW_TREE` (`AGENTS.md` §2.9). The first-party
Rust wrappers are `rustos_rt::hw_tree_read` / `hw_tree_wait`; the C stubs are
`ros_sys_hw_tree_read` / `ros_sys_hw_tree_wait`. The device manager
(`userland/system/devmgr`) reads the tree, then waits and re-reads on every
change — the reactive observe loop behind the `rustos_devmgr::HwTreeService`
seam.

`hw_emit_node` (no. 37) is the **write** side of the same hardware tree:
recursive, user-space hardware discovery (`AGENTS.md` §18.1 / §18.3). A
user-space **bus** driver (a PCIe root complex, a USB host) enumerates the
devices behind it and calls this once per device to publish a discovered
child `rustos_abi::HwNode`, so the device manager autoloads the matching
driver in turn — discovery is data-driven, never a compiled-in list (§18).
The handler copies the encoded node in (rejecting any `len` that is not
exactly `HwNode::WIRE_LEN` before copying, so a hostile length drives no
large copy), decodes it fail-closed, and then enforces the keystone security
rule: it admits the node **only** when every `rustos_abi::hwtree::HwResource`
the node requests is wholly covered by one of the **calling task's** own
minted device-resource grants (`HwResource::covers`, checked against the same
per-task `AddressSpaceRegistry` grant table `resource_grants` reads). A bus
driver therefore can never mint a child more authority than it holds itself —
a resource outside its grants fails the whole publish closed with
`PermissionDenied`, never partially applied (`AGENTS.md` §4 — no ambient
authority; §2.9). The kernel also **owns the published node's identity**: it
resolves the caller's *own* matched node (the kernel-side task→node record
made when the driver was loaded) as the child's parent — a caller with no
matched node may publish nothing and fails closed with `PermissionDenied` —
and the store assigns the node a fresh id one past the largest live node id,
so an emitter-chosen id can never collide with an existing node. This is
load-bearing, not cosmetic: the driver-store load path resolves a matched node
by its id, so a collision would mint the wrong driver's grants (`AGENTS.md`
§4 / §5.4 — identity is kernel-provided, never caller-supplied). On success
the node is appended to the live tree under that parent, bumping the
generation that wakes every parked `hw_tree_wait` caller (the reactive
autoload above). It is gated on **`CAP_HW_EMIT`** — held only by an
autoloaded bus driver, never an ordinary task — and **audited** per call
(admitting a node that drives an autoload and carries resource grants is a
low-volume, security-relevant event, §5.4.4 / §18.6). It serves the same
`kernel/core` `HwTreeSource` seam (`HwTreeSource::publish`); until a store is
installed it fails closed with `NotImplemented` through `NULL_HW_TREE`. The
first-party Rust wrapper is `rustos_rt::hw_emit_node` (the user-space driver
host `rustos_drvrt::RtDriverHost` forwards `DriverHost::emit_node` to it); the
C stub is `ros_sys_hw_emit_node`.

`hw_remove_node` (no. 38) is the exact **mirror** of `hw_emit_node`: hotplug
removal (`AGENTS.md` §18.4). When a device a bus driver published goes away
(a USB port-down, a PCIe hot-remove) the driver calls this with the
`HwNode::id` it wants retired, so the device manager unloads the driver bound
to the vanished node. It is gated on the **same** `CAP_HW_EMIT`, and the
kernel bounds it exactly like publication (`AGENTS.md` §4 — no ambient
authority): it resolves the caller's *own* matched node (the same kernel-side
task→node record `hw_emit_node` uses) and removes the target **only** when
its parent is that node — a child the caller itself published — together
with its whole subtree, so a driver can never retire a node it does not own
and no stale descendant outlives its parent. An unknown id, or a node the
caller does not own, fails closed (`NotFound` / `PermissionDenied`,
indistinguishable so the failure leaks nothing about the rest of the tree,
§5.4). On success the node set shrinks and the generation bumps, waking every
parked `hw_tree_wait` caller; like `hw_emit_node` it adds/removes the node
and leaves the driver *load*/*unload* to the device manager (the microkernel
policy/mechanism split, §4). It is **audited** per call (a low-volume,
security-relevant event that drives an unload). It serves the
`HwTreeSource::remove` seam; until a store is installed it fails closed with
`NotImplemented` through `NULL_HW_TREE`. The first-party Rust wrapper is
`rustos_rt::hw_remove_node`; the C stub is `ros_sys_hw_remove_node`.

`ipc_call` (no. 31), `call_create` (no. 32), `call_recv` (no. 33), and
`call_reply` (no. 34) are the two halves of the **synchronous** request/reply
IPC primitive (`AGENTS.md` §5.2 / §5.4) — a first-class call/reply endpoint,
not a convention layered over two async `ipc_send`/`ipc_recv` ports. A caller
posts a request and blocks for exactly one matching reply with `ipc_call`; a
server task owns the answering endpoint. `call_create` builds and registers a
`kernel/ipc::CallEndpoint` under a well-known id, with the calling task as its
owner and two `CapabilitySet` wire images naming the capability a caller must
hold to post (`send_caps`) and the capability the server must hold to serve
(`recv_caps`); binding a restricted-sender endpoint (non-empty `send_caps`)
requires `CAP_IPC_BIND_PRIVILEGED`, and an id already bound fails closed with
`AlreadyExists` (the kernel never re-points a live endpoint). `call_recv`
blocks until a request is posted, copies it into the server's buffer (a
request larger than the buffer is left queued and refused `BufferTooSmall`,
never lost), and writes the per-call ticket; `call_reply` completes that
ticket and wakes the blocked caller. Both server calls resolve the endpoint
and gate the caller against its `recv_caps` **and** owner identity before
touching state (`AGENTS.md` §5.4); a server that exits has its endpoints torn
down so blocked callers abandon fail-closed rather than hang (`AGENTS.md`
§2.9). The four are dispatcher-**ungated** (the per-call authority is the
endpoint's own send/recv capability check, like `ipc_send` over a port);
`ipc_call`/`call_create` are audited (a synchronous system call / a service
bind), `call_recv`/`call_reply` are not (a server's high-volume serve loop).
The kernel-resident driver-store file service is one `ipc_call` callee
(`lib/abi::driver_store`); the server trio lets an ordinary user-space service
be the callee — the autoloaded `vcmailbox` mailbox service (Design D D3) is
its production consumer. The first-party Rust wrappers are
`rustos_rt::{ipc_call, call_create, call_recv, call_reply}`; the C stubs are
`ros_sys_ipc_call` / `ros_sys_call_create` / `ros_sys_call_recv` /
`ros_sys_call_reply`.

`call_peer_origin` (no. 58) lets a server read the **kernel-attested
identity** of the caller whose in-service call it is handling (P-C). After a
`call_recv` hands the server a ticket, `call_peer_origin` returns the
`rustos_abi::Origin` the kernel captured from the *posting* task's own state
at `ipc_call` time — its trust domain, uid, reusable pid, the unforgeable
`ProcId` that distinguishes process instances across PID reuse, and a
non-secret capability *summary* (a membership bitmap, never any capability
token). The origin is filled entirely kernel-side, so a caller can neither
forge another principal's identity nor inflate its own, and it is read from
the call's own snapshot rather than re-resolving the task — immune to later
capability changes or PID reuse. Like `call_recv`/`call_reply` it is
dispatcher-ungated but resolves the endpoint and checks the reader's
`recv_caps` **and** owner identity before exposing anything; a foreign
endpoint, an unknown or not-in-service ticket, or a buffer shorter than
`rustos_abi::ORIGIN_WIRE_LEN` fails closed, and it is unaudited (a server's
high-volume serve path; refusals are audited by the dispatcher regardless).
It is the foundation a capability-gated user-space service builds on to learn
who called it — its first consumer is `sysinfod`'s self-scoped
`PROCESS_IDENTITY` query (`AGENTS.md` §16.6). The first-party Rust wrapper is
`rustos_rt::call_peer_origin`; the C stub is `ros_sys_call_peer_origin`.

`wall_time_get` (no. 59) and `wall_time_set` (no. 60) are the wall-clock
pair (`PREREQUISITES.md` P-D). The kernel keeps an absolute wall-clock time
beside the per-CPU monotonic clock: `wall_time_get` returns a
`rustos_abi::WallClockReading` — a `Time64` instant plus a
`rustos_abi::WallTimeState` byte (`Unset` / `Firmware` / `Trusted` /
`Adjusted`) saying how trustworthy that time is. It is **ungated** and
unaudited, the same unprivileged observer baseline as `clock_get`; before a
trusted source sets the clock the reading is the Unix epoch tagged `Unset`.
Event **ordering** never rests on this value — the monotonic `clock_get` and
sequence numbers remain the ordering authority; the wall time is provenance
metadata for stamping records. `wall_time_set` records a new wall instant
and its provenance `state`, capturing the monotonic reading at that moment so
a later `wall_time_get` projects the instant forward by the elapsed monotonic
time (the monotonic clock itself is never touched). It is gated on
**`CAP_TIME_SET`** — driving the system clock is privileged and security-
relevant — and **audited**; a malformed instant, a short buffer, or a
non-settable `state` (`Unset`, or any undefined discriminant) fails closed,
and the kernel attests the state itself so a caller cannot mislabel it. The
clock boots `Unset`; until a trusted time source drives it, `wall_time_get`
reports the epoch. The first-party Rust wrappers are `rustos_rt::wall_time` /
`rustos_rt::wall_time_set`; the C stubs are `ros_sys_wall_time_get` /
`ros_sys_wall_time_set`.

`boot_id_get` (no. 61) copies the kernel's per-boot identifier — a 128-bit
`rustos_abi::BootId` — out to the caller's `(out, len)` buffer and returns
its byte count (`PREREQUISITES.md` P-E). The boot id is a public per-boot
nonce: it is stable for the lifetime of a boot, fresh across boots, and user
space can neither supply nor influence it. It is **ungated** and unaudited,
the same unprivileged observer baseline as `clock_get` / `wall_time_get`,
because it is not a secret — boot-scoped state (the system log's
stream-genesis, `plans/SYSLOG.md` §7.1) binds itself to it so a record cannot
be silently replayed from a different boot. The kernel mints it once at boot
from the single CSPRNG output reserve (§22), immediately after that reserve is
seeded; a buffer shorter than `BOOT_ID_LEN` (16) fails closed with
`BufferTooSmall`, and a boot whose random subsystem could not be seeded in
time has no id — the call fails closed with `EntropyNotReady` rather than
return the all-zero `BootId::UNSET` sentinel as if it were real. The
first-party Rust wrapper is `rustos_rt::boot_id`; the C stub is
`ros_sys_boot_id_get`.

`self_origin` (no. 68) is the self-directed twin of `call_peer_origin` (no.
58): where that lets a server read the kernel-attested identity of the *peer*
it is servicing, `self_origin` lets a task read its *own*. The kernel builds
the caller's `Origin` — trust domain, owning uid/gid, task id,
process-instance `ProcId`, and the non-secret effective-capability summary
(the membership bitmap, no capability tokens) — entirely from the caller's own
kernel-held task record (`TaskCapabilities::attest_origin`), never a
caller-supplied value, so a task can neither forge another principal's
identity nor inflate its own. It is unprivileged (a task may always learn its
own identity, like `boot_id_get`) and not audited, and fails closed
(`BufferTooSmall`) on a buffer shorter than `ORIGIN_WIRE_LEN`. The journal
service (`journald`) uses it to stamp the trusted records it authors itself
(the segment self-events and the `security` spoof-notes) with its own attested
origin rather than a fabricated one. The Rust wrapper is
`rustos_rt::self_origin`; the C stub is `ros_sys_self_origin`.

`terminal_size` (no. 63) reports the character-cell grid of the text console
backing a caller's standard stream — `fd` (typically `STDOUT`), then the
`(out, len)` buffer the encoded `rustos_abi::TerminalSize` (rows, then columns,
two little-endian `u16`s) is written to — so a full-screen terminal program
(`top`) draws to the real display extents (`PREREQUISITES.md` P-C). It is
**ungated** and unaudited, the same unprivileged observer baseline as
`clock_get` / `wall_time_get`: asking how big one's own terminal is grants no
authority. The handler resolves `fd` against the caller's descriptor table
(a non-open descriptor → `NotFound`), resolves its backing console, and
reports a size **only** for a console whose geometry the kernel actually
knows — a framebuffer text console, whose grid is a function of the panel
resolution and the font (`VideoConsole::geometry` → the live
`video::text_grid`). For a byte-stream console (a UART) the true size of the
remote terminal is a property of the far-end emulator, unknowable to the
kernel: the call fails closed with `NotImplemented` and the client terminal
library applies the conventional 80×24 fallback — the size policy lives in the
client, and the kernel never fabricates a size (`AGENTS.md` §5.4). A buffer
shorter than the wire length fails closed with `BufferTooSmall`. The
first-party Rust wrapper is `rustos_rt::terminal_size`; the C stub is
`ros_sys_terminal_size`.

`mem_map` / `mem_unmap` are deliberately **ungated** (no row above). They
grow and shrink the caller's *own* hardware-isolated address space with
anonymous `RW` memory, which grants no authority over anything else — the
same unprivileged baseline as "list my own processes" (`AGENTS.md` §16.6).
There is no global user heap and no cross-process mapping; shared memory
stays the capability-checked IPC object (`AGENTS.md` §4).

`wait` (no. 16) is likewise **ungated**: a process may only wait on its
*own* children, so reaping one grants no authority over any other
principal (the same §16.6 baseline). It is, however, *audited* — reaping a
child is a process-lifecycle state change (a principal disappears), exactly
as `spawn` and `exit` are audited (`AGENTS.md` §5.4.4). `pid` is either
a specific child's PID or `rustos_abi::WAIT_PID_ANY` (`-1`, wait for any child);
`status` is a non-null user pointer the kernel writes the typed
`rustos_abi::WaitStatusRecord` to (`kind` exited or stopped plus the exit
code or stopping signal — decoded fail-closed by
`rustos_abi::WaitStatusRecord::decode`, never a bit-packed POSIX status
word); `flags` is a `rustos_abi::WaitFlags` set. The handler reaches
the scheduler-side reaper through the
`kernel/core::procwait::ProcessWait` seam, which is installed at boot like
the `spawn` / `mem_map` producers. The boot path installs the real
`KernelProcessWait` producer (`plans/SPAWN.md` SP6b): it owns the
parent/child + exit-status bookkeeping (`ProcessTable`) — a child is
recorded against its parent at `spawn` admit, its exit code is captured by
the `exit` handler, and the parent's `wait` cooperatively parks (via the
scheduler reschedule path) until a matching child is reapable, then reaps
it. A `wait` issued before that install (or by a non-parkable task) fails
closed with `NotImplemented` through the default `NULL_PROCESS_WAIT`
(`AGENTS.md` §2.9). The first-party Rust wrapper is `rustos_rt::wait`.

With `WaitFlags::NONBLOCK` set the call **polls** instead of blocking — the
reap the shell's job control performs to report finished background jobs
before the next prompt, and PID 1 `init` uses to reap the session without
parking. It reaps an already-exited child (returning its PID and copying
the exit code out, exactly as the blocking form does), or — when a matching
child is still running — returns `WouldBlock` (the `abi-v1` "nothing yet,
retry" signal) without parking the caller, leaving `status` untouched. The
producer serves the poll through the same single `ProcessTable::reap`
primitive the blocking loop uses, so the two can never diverge, and a
poll that finds nothing reapable is audited as the benign
`SYSCALL_HANDLER_WOULD_BLOCK` (Debug), not an ERROR — so a polling
job-control loop never floods the log (`AGENTS.md` §2.1 / §19.4). The
first-party Rust wrapper is `rustos_rt::try_wait`; the C stub `ros_sys_wait`
takes the flags argument and the header defines `ROS_WAIT_FLAG_NONBLOCK`,
`ROS_WAIT_FLAG_STOPPED`, and the `ros_wait_status_t` record.

With `WaitFlags::STOPPED` set the call also reports a child freshly
**stopped** by `Signal::Stop` (`plans/SPAWN.md` SP9 — the `WUNTRACED`
analogue the shell's job control uses): it returns the child's PID and
writes a *stopped* record — **without reaping the child**, which stays
tracked and resumable through `Signal::Continue`. Each stop is reported
exactly once (edge-triggered; a `Continue` clears an unobserved stop so a
stale report never follows a resume, and an exit supersedes one). With the
bit clear a stopped child is invisible to `wait`, exactly as before. The
simple wrapper for a parent with no job control is `rustos_rt::wait_exit`.

`signal` (no. 64) delivers a control signal to a child of the calling
process (`plans/SPAWN.md` SP7) — the job-control primitive the shell's
`fg`/`bg`/kill drive. Like `wait` it is **ungated**: a process may signal
only its *own* children, so the parent/child relationship is the authority
and no capability is required (the §16.6 own-process baseline). It **is**
audited — delivering a signal is a process-lifecycle decision, exactly as
`spawn`/`wait`/`exit` are audited (`AGENTS.md` §5.4.4). `pid` names a child
the caller spawned; `signal` is a closed `rustos_abi::Signal` discriminant
(`Continue` = 1, `Terminate` = 2, `Kill` = 3, `Interrupt` = 4, `Stop` = 5),
and the reserved `0` or any
other value fails closed with `OutOfRange` before dispatch (validate every
input). The handler reaches the scheduler-side deliverer through the
`kernel/core::procsignal::ProcessSignal` seam, installed at boot like the
`wait` producer; a `pid` that is not a live child of the caller fails closed
with `NotFound`, and a `signal` issued before the producer is installed fails
closed with `NotImplemented` through the default `NULL_PROCESS_SIGNAL`,
never pretending a signal was delivered (`AGENTS.md` §2.9). The concrete
deliverer is `kernel/core::procsignal::KernelProcessSignal` (`plans/SPAWN.md`
SP7b): it composes over the `KernelProcessWait` producer — the one owner of
the parent/child + exit-status bookkeeping, so authorisation and the reaped
status share a single definition — and the live scheduler, and delivers by
driving it: `Continue` resumes a stopped child (`SchedulerPolicy::unpark`, a
no-op for a running one, also clearing the stop overlay and any unobserved
stop), `Terminate` / `Kill` / `Interrupt` terminate the child
(`SchedulerPolicy::exit`) and record the signal's POSIX-familiar
termination status (`Signal::termination_status`: `Interrupt` → 130,
`Kill` → 137, `Terminate` → 143 — the `128 + n` codes a shell user already
scripts against, deliberately not our wire discriminants) so the parent's
`wait` reaps it — distinguishable from a self-`exit` — and `Stop` parks the
child (`SchedulerPolicy::park`), marks it in the kernel's stop overlay (a
broadcast waitq wake can otherwise make a parked task runnable; the kthread
dispatch shim re-parks an overlay-held task, so only `Continue` genuinely
resumes it) and records the stop for a `WaitFlags::STOPPED` wait. The
first-party Rust wrapper is
`rustos_rt::signal`; the C stub is `ros_sys_signal` and the header defines
`ROS_SIGNAL_CONTINUE` / `ROS_SIGNAL_TERMINATE` / `ROS_SIGNAL_KILL` /
`ROS_SIGNAL_INTERRUPT` / `ROS_SIGNAL_STOP`.

`console_foreground` (no. 72) grants (or releases, `pid = 0`) the
**controlling ownership** of the console behind readable descriptor `fd`
— the `tcsetpgrp` analogue (`plans/SPAWN.md` SP9, `plans/DISPLAY.md` D5).
The foreground owner is a kernel-tracked task id with two enforced
consequences. First, **only the owner drains the console's input queue or
changes its line discipline**: while an owner is recorded, any other
task's `stream_read` / `stream_input_mode` on that console is refused
with the typed `NotForeground` (errno 27) *before any input is consumed*
— a background reader fails closed instead of being stopped by a racy
`SIGTTIN`-style asynchronous signal; an unowned console reads openly (the
shell at its prompt). Second, the console's **cooked-mode** line
discipline consumes `^C`/`^Z` at arrival time (every input producer — the
UART RX interrupt handler, the seat registry's keyboard sink — pushes
through the console device's input filter) and queues
`Signal::Interrupt`/`Signal::Stop` for the owner; the queueing is a
single atomic store (interrupt-safe) and the scheduler-driving delivery
runs at the next dispatcher-context drain, through the same
`KernelProcessSignal` engine the `signal` syscall uses (installed as the
`ForegroundSignal` hook at boot). Raw/secret modes and an unowned console
pass every byte through unchanged, so a full-screen program still
receives literal control bytes. It is gated on `CAP_CONSOLE_READ` (the
`stream_input_mode` terminal-control gate) and audited; the authority is
layered and capability-minimal: a non-zero `pid` must be a **live child
of the caller** (the same `ProcessWait::authorise_child` bookkeeping
`wait`/`signal` use — the drain right only ever moves down the spawn
chain, inherited and intersected, never widened), and the slot transition
itself is checked on the device — a grant is honoured only from an
unowned console, the recorded **granter** (re-targeting between its own
children), or the current owner (delegating onward to its own child), and
a release only from the granter or the owner. Anything else is refused
with `NotForeground`, so a bystander can neither take the drain right nor
open the console by clearing the slot; a bad `pid` shape fails closed
with `NotFound`. A vanished owner never wedges its console: the `exit`
path releases the ownership immediately, and the read gate clears a
recorded owner the process bookkeeping proves dead (task ids are never
reused). The shell (`elsh`) marks its foreground child around every
blocking `wait` and releases the slot at its prompt. The first-party Rust
wrapper is `rustos_rt::console_foreground`; the C stub is
`ros_sys_console_foreground`.

`rlimit_get` (no. 17) and `rlimit_set` (no. 18) are the settable
`ulimit`/`rlimit`-equivalent (`AGENTS.md` §24.3). Both name a closed
`rustos_abi::LimitKind` resource via a `u32 kind` and carry a
`rustos_abi::ResourceLimit` (`{ soft, hard }`, `RLIMIT_INFINITY` =
"no limit") through a 16-byte user buffer. Both are **ungated at the
dispatcher**: reading one's own limit and *lowering* a bound need no
capability (the §16.6 own-process baseline). `rlimit_set` performs the
finer check **handler-side** — a request that *raises* a hard bound above
the inherited ceiling is refused with `PermissionDenied` unless the caller
holds `CAP_RLIMIT_RAISE`, mirroring the §5.2 "never widen on delegation"
rule. `rlimit_get` is unaudited (a pure observer); `rlimit_set` **is**
audited — it changes enforced policy (`AGENTS.md` §5.4.4). The first-party
Rust wrappers are `rustos_rt::rlimit_get` / `rlimit_set`; the §24 policy,
the discovered-hardware defaults, and the kernel enforcement are detailed
in [`resource-limits.md`](./resource-limits.md).

`users_db_read` (no. 19) copies the system user database
(`/System/Security/Users`, `AGENTS.md` §5.1) the kernel loaded off the
mounted root volume at boot out to the caller's `(buf, len)` buffer and
returns the byte count — the exact `users-v1` text, which the caller
re-parses with the same fail-closed `rustos-users` parser the kernel used
(`plans/PI.md` P11). It is gated on **`CAP_USERS_READ`**: the text carries
every account's salted password record, so only the authentication
principal (login) holds the capability, and every call is **audited**
(low-volume, security-relevant). The handler serves the
`kernel/core::users::UsersDbSource` seam, installed by a boot path that
mounted the root volume and ran the audited `load_users_db` read
(`KernelSyscallHandlers::with_users_db` / the dispatch hook's mirror);
until one is installed it fails closed with `NotImplemented`, and a wired
holder with no database fails closed with `NotFound` — a system without
accounts refuses every login rather than inventing one (`AGENTS.md`
§5.4.5). The `LateUsersDb` holder (the in-kernel-unlock boot path,
`plans/PI.md` P11) adds one more state: while the encrypted root is still
being unlocked the read returns **`WouldBlock`** — the live-but-not-ready
signal — so `login` *waits without prompting* and leaves the console to
the concurrent `Root filesystem passphrase:` prompt; once the unlock resolves the
read returns the installed database, or `NotImplemented` if the unlock
produced none (the deny-all prompt then runs). An undersized buffer is refused whole with `BufferTooSmall` (a
credential database is never truncated, `AGENTS.md` §2.9); a buffer sized
at the format's 64 KiB maximum (`rustos-users` `MAX_DB_LEN`) always
suffices. The first-party Rust wrapper is `rustos_rt::users_db_read`; the
C stub is `ros_sys_users_db_read`.

`users_db_wait` (no. 35) is the **blocking** companion to `users_db_read`:
it parks the caller while the database is in that `WouldBlock` *pending*
state and returns `0` the instant the unlock reaches a terminal outcome —
a database is installed, or the unlock gives up — or `TimedOut` if
`timeout_ns` elapses first. It replaces `login` busy-re-reading
`users_db_read` in a yield loop while pending; `login` now parks on this
wait between reads (one advisory re-read per wake). Either way a
`users_db_read` that returns the expected `WouldBlock` (pending) is
audited as the benign `SYSCALL_HANDLER_WOULD_BLOCK` (Debug, id 5005), not
the ERROR-level `SYSCALL_HANDLER_REJECTED` (id 5004) a genuine refusal
gets — so a poll-while-pending never floods the boot log with errors
(`AGENTS.md` §2.1 / §19.4).
The handler parks on the `kernel/core` `USERS_DB_WAITQ` and is woken by
`LateUsersDb::install` / `resolve` (the terminal unlock transitions), the
same park/wake shape as `hw_tree_wait` (`AGENTS.md` §2.1 / §2.2). It is
gated on the same **`CAP_USERS_READ`** as the read but is **unaudited** —
it is a blocking wait, not a state change, and the capability denial is
audited by the dispatcher regardless. A build with no users-database
service wired is never pending, so the wait returns `0` immediately and the
following read fails closed (`AGENTS.md` §2.9). The first-party Rust wrapper
is `rustos_rt::users_db_wait`; the C stub is `ros_sys_users_db_wait`.

`console_count` (no. 20) reports how many system text consoles the boot
path installed (`AGENTS.md` §20, `plans/PI.md` P11) — the index space
`spawn`'s `console` argument selects from. Each entry is an independent
console with its own session context: with the framebuffer boot console
active the aarch64 list is `[video, uart]`, otherwise `[uart]`. Gated on
`CAP_CONSOLE_WRITE` (console topology belongs to the principals that
drive consoles) and unaudited (a pure observer). PID 1 `init` uses it to
start one login session per discovered console. The first-party Rust
wrapper is `rustos_rt::console_count`; the C stub is
`ros_sys_console_count`.

`stream_input_mode` (no. 21) sets the read line discipline of one of the
caller's inherited input streams (`AGENTS.md` §20, `plans/PI.md` P11).
`fd` is the input descriptor (normally fd 0) and `mode` is an
`InputMode` discriminant: `1` — **cooked**, the interactive default,
echoing what the user types; `2` — **secret**, a password read (echo
suppressed, the activity indicator shown instead); `3` — **raw**, a
full-screen program's read (echo suppressed, nothing drawn — the program
paints its own display, so even the indicator would corrupt it). The
reserved `0` and every unknown value fail closed with `OutOfRange`.
Login selects secret around the password read so the credential is never
rendered, then restores cooked (`AGENTS.md` §5.4 — never echo a
credential); `top` and `man` select raw for their keystroke commands.
The echo is the kernel's read line-discipline behaviour: `stream_read`
writes the consumed bytes back to the resolved console's write half (a bare
CR/LF is rendered as CR-LF so the cursor advances a line), so it needs no
separate `CAP_CONSOLE_WRITE` — `stream_input_mode` shares `stream_read`'s
`CAP_CONSOLE_READ` gate and, as low-volume terminal configuration, is
unaudited. The line discipline also handles **erase** (rub-out): a
Backspace or Delete — the single-byte controls (the one `lib/vt`
`control::is_line_erase` definition, §2.2) or the Delete key's `CSI 3 ~`
escape sequence (the shared `rustos_vt::line::EraseSeq` recogniser, held
across split reads) — is not echoed as stray control glyphs but rubs out
the previous character with a `BS SP BS` sequence, bounded by a
per-console column so an erase at the start of the input line never walks
back over the prompt. The reader's line buffer applies the matching erase
to the bytes it keeps (`rustos_vt::line::LineEditor`), so screen and
buffer stay in step.

The **secret** mode also arms the console's secret-entry feedback
(`rustos_vt::secret`, hosted as the kernel `SecretFeedback`): after the
first typed character of a password read the console
shows the `[input active...]` marker, its dots cycling `.` → `..` → `...`
on a one-second cadence. The animation is **bounded**: it runs for at
least three seconds after the most recent keystroke and then freezes (the
marker stays on screen but the dots stop moving), and a later keystroke
restarts it. On Enter the marker is replaced in place with `[input
complete]`; when the input is erased back to empty the marker is removed
entirely. The console's blocking reader drives the animation with a
one-shot wait deadline armed only while the dots are moving (tickless — a
prompt with nothing typed takes no timer wake-ups, and the animation's
wake-ups span only the bounded window from a keystroke to three seconds
after the last one), and only the *count* of typed characters is tracked:
no secret byte is stored or rendered. Selecting any other mode disarms
the feedback and removes an in-progress marker an aborted read left,
while a completed `[input complete]` marker is deliberate final feedback
and is left in place. The **raw** mode never arms the feedback: a
full-screen program's keystrokes draw neither an echo nor a marker.
The in-kernel root-unlock passphrase prompt arms the same feedback
directly, so every text/terminal password prompt shows one marker. The
line discipline is terminal control, so it belongs to the console's
controlling (foreground) owner exactly as the input drain does
(`plans/DISPLAY.md` D5): while a foreground owner is recorded on the
console (`console_foreground`, no. 72), any other task's
`stream_input_mode` — like its `stream_read` — is refused with the typed
`NotForeground`, so a background task cannot flip the foreground
program's echo or raw mode under it. An
`fd` that is not a readable inherited stream fails closed with `NotFound`;
a console-less build fails closed with `NotImplemented`. The first-party
Rust wrapper is `rustos_rt::set_input_mode`; the C stub is
`ros_sys_stream_input_mode`.

`console_input` (no. 22) injects decoded keystroke bytes into an
installed console's kernel-side input queue — the producer counterpart of
`stream_read`, the path that gives the video console keyboard input
(`AGENTS.md` §20, `plans/PI.md` P11). `console` names an installed-console
index directly (not an inherited descriptor: the producer is a driver, not
a stream owner), and `(buf, len)` is the decoded byte run. A
keyboard-input driver that has decoded a directly attached keyboard
(USB-HID / PS-2) pushes the bytes here; the kernel copies them in and
enqueues them on that console's `ConsoleInputQueue`, which a `stream_read`
from the console's login then drains — so the video console reads its own
keyboard, never the serial line (with a display active the UART carries
only the debug log and is not installed as a console). It is gated on
**`CAP_INPUT_INJECT`**: feeding the system console's input is privileged,
never ambient (`AGENTS.md` §4), so only the keyboard-input driver the
device manager loaded holds it; like the other per-byte stream operations
it is unaudited (the device manager's one-time driver load is the audited
decision). A short push (the bounded type-ahead queue is near full)
reports fewer bytes and the driver retries (`AGENTS.md` §2.1, never
blocks); a `console` index with no installed console, or one whose backing
accepts no injected input (a UART reading its own hardware FIFO), fails
closed with `NotImplemented` (`AGENTS.md` §2.9). The queue zeroes each
byte as the consumer drains it — a typed password transits it, so the
buffer retains no cleartext (`AGENTS.md` §4 / §23.1). The first-party Rust
wrapper is `rustos_rt::console_input`; the C stub is
`ros_sys_console_input`.

## Standard streams (fd 0/1/2/3)

A program performs **all** of its text I/O over the four inherited
standard descriptors, never over a kernel-discovered device (`AGENTS.md`
§20): fd 0 `stdin`, fd 1 `stdout`, fd 2 `stderr`, fd 3 `stdinfo`. The
`stream_write` / `stream_read` syscalls take that descriptor as their
`fd` argument; the program names only the fd number, so the same binary
works whatever the spawner backed the stream with.

The per-process **descriptor table** is part of the process model
(`rustos_abi::DescriptorTable`, `lib/abi/src/process.rs`): a fixed table
of four entries, one per standard descriptor, each recording its
`StreamMode` (`Closed` / `Read` / `Write`) **and the installed-console
index backing it**. The spawner establishes it when it admits a process
(`AddressSpaceRegistry::set_streams`, keyed by the same `TaskId` as the
address space): `spawn`'s `console` argument selects either the caller's
own table (`CONSOLE_INHERIT` — login's shell stays on login's console) or
the standard shape (`DescriptorTable::standard_on`: fd 0 readable, fd
1/2 writable, fd 3 unattached) on an explicitly named, validated console
index — PID 1 launching one login per console (`plans/PI.md` P11). The
dispatcher's handler resolves `fd` against this table **before** any
state is touched: an `fd` that is not the right direction (or a process
whose table was never established) fails closed with `NotFound`, so the
inherited descriptor — not an ambient device — is the authority
(`AGENTS.md` §4 / §5.4). `stdinfo` (fd 3) is the one advisory exception:
it carries structured records for tools that opt in, never terminal
text, so a console session leaves it **unattached** and a `stream_write`
to an unattached fd 3 is accepted and discarded — best-effort and
non-blocking (`AGENTS.md` §20.1) — rather than denied or smeared over
the terminal, where it would corrupt the primary output and every
pipeline built on it.

Every descriptor's kernel *stream backing* is one entry of the discovered
console list the boot path installed (`BootInfo::with_consoles` — index 0
the primary console, each further entry an independent console such as
the UART beside an active video console), so a console-backed stream
additionally requires `CAP_CONSOLE_WRITE` / `CAP_CONSOLE_READ` — the
coarse "may use a console-backed stream" gate the dispatcher checks, on
top of the fd-level descriptor gate. A descriptor naming an index with no
installed console fails closed with `NotImplemented`. The first-party
Rust wrappers are `rustos_rt::stdout` / `stderr` / `stdinfo` / `stdin`; a
program never names `console_*` or a device (`AGENTS.md` §20, §2.2).

## Argument validation

Every register slot of `RawArgs` is validated against the `AbiType`
declared in the source table:

| `AbiType`      | Acceptance rule                                                          | Reject `Errno`         |
| -------------- | ------------------------------------------------------------------------ | ---------------------- |
| `Unit`         | Slot must be exactly zero.                                               | `LengthOutOfRange`     |
| `I32`          | Upper 32 bits equal the sign extension of the low 32.                    | `OutOfRange`           |
| `U32`          | Upper 32 bits are zero.                                                  | `OutOfRange`           |
| `U64`          | Any value.                                                               | —                      |
| `Cap`          | `>> 16 == 0` and within `CAPABILITY_ID_MAX` (`= 255`).                   | `OutOfRange`           |
| `UserPtr`      | Non-null. Page-table walks are the owning subsystem's job.               | `BadAlignment`         |
| `Len`          | Fits in `usize` on the target.                                           | `LengthOutOfRange`     |
| `IpcEndpoint`  | Any value (opaque handle).                                               | —                      |
| `Handle`       | Any value (opaque handle).                                               | —                      |
| `Errno`        | Never an input; never appears in `args`.                                 | `OutOfRange`           |

In addition the dispatcher refuses non-zero data in slots **past**
`arg_count` with `LengthOutOfRange`. This prevents a buggy
trampoline from smuggling extra register state past a syscall's
declared arity.

## Error map

| `Errno`                 | When the dispatcher returns it                                                       |
| ----------------------- | ------------------------------------------------------------------------------------ |
| `OutOfRange`            | Syscall number above `SyscallNumber::MAX`, or an argument fails its type check.      |
| `NotFound`              | Number in range but no entry assigned at that index.                                 |
| `PermissionDenied`      | Caller lacks the syscall's `required_capability`.                                    |
| `LengthOutOfRange`      | Trailing slot non-zero, or `Len` exceeds host `usize`.                               |
| `BadAlignment`          | `UserPtr` argument is null.                                                          |
| `OutOfMemory`           | `mem_map` could not obtain a backing frame (or page-table frame); deterministic OOM, never a panic (`AGENTS.md` §4). |
| `AbiVersionUnsupported` | `verify_table_hash` ran at kernel-init time and the recomputed digest disagreed.     |
| *(propagated)*          | Anything else a handler returns is delivered to user space verbatim.                 |

## Audit events

`kernel/syscall` reserves the `5_000..6_000` `EventId` range. Successful
dispatches of *security-relevant* syscalls (`SyscallSpec::audit == true`)
emit `SYSCALL_INVOKED`; the pre-dispatch refusals (`PERMISSION_DENIED`,
`UNKNOWN`, `BAD_ARGUMENTS`) always emit, regardless of the audit flag.

| ID    | Level | Name                          | When |
| ----: | ----- | ----------------------------- | ---- |
| 5000  | Info  | `SYSCALL_INVOKED`             | A security-relevant syscall passed every check and was dispatched. |
| 5001  | Error | `SYSCALL_PERMISSION_DENIED`   | Caller lacked the required capability. |
| 5002  | Error | `SYSCALL_UNKNOWN`             | Number was outside the `abi-v1` table. |
| 5003  | Error | `SYSCALL_BAD_ARGUMENTS`       | Argument validation failed. |
| 5004  | Error | `SYSCALL_HANDLER_REJECTED`    | Owning subsystem rejected the call. |
| 5005  | Debug | `SYSCALL_HANDLER_WOULD_BLOCK` | An audited handler returned `WouldBlock` — the `abi-v1` "nothing yet, retry" signal (not a rejection: every check passed, no security decision was taken). Recorded at `Debug`, below the default `Info` filter, so a caller that legitimately polls while pending cannot flood the log; available for flood/DoS forensics when the level is lowered (`AGENTS.md` §2.1 / §19.4). |

Adding an event takes the next free identifier and a new row in this
table.

## Handler wiring (Stage 2.7 follow-up (f3))

The dispatcher trait `SyscallHandlers` is implemented in `kernel/core`
by `KernelSyscallHandlers<'a, A>` (see
`rustos_kernel_core::syscalls`). The struct borrows kernel state and
forwards every call to the owning subsystem; nothing in this layer
re-validates arguments — the dispatcher does that first.

| Handler         | Forwards to                                                                                                   | Error map                                                                 |
| --------------- | ------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------- |
| `yield_now`     | `Scheduler::yield_current(caller.task_id)`                                                                    | `NoSuchTask → NotFound`, otherwise `OutOfRange`.                          |
| `exit`          | `CapTable::remove(caller.task_id)` then `Scheduler::exit(caller.task_id)`                                     | `NoSuchTask → NotFound`, otherwise `OutOfRange`.                          |
| `ipc_send`      | `PortRegistry::lookup(endpoint)` in `KernelState.ipc`; payload copied in through `copy_from_user`, then `Port::send(caller.caps, payload)` | Unbound endpoint → `NotFound` (no extra audit). `len > port.max_payload` → `MessageTooLarge`. Faulting buffer / no registered address space → `BadAddress`. Otherwise `Port::send`'s errno (`PermissionDenied`, `MessageTooLarge`, …). |
| `ipc_recv`      | `PortRegistry::lookup(endpoint)`; `Port::recv_with` peek/commit copies the head message out through `copy_to_user`, committing the dequeue only on success | Unbound endpoint → `NotFound` (no extra audit). Bound + empty → `WouldBlock`. Buffer smaller than the message → `BufferTooSmall` (message retained). Faulting buffer / no registered address space → `BadAddress` (message retained). Otherwise `Ok(payload_len)`. |
| `cap_query`     | `caller.caps.has(cap)` mapped to `0` / `1`                                                                    | —                                                                         |
| `cap_delegate`  | `CapabilitySet` copied in through `copy_from_user`, then `CapTable::caps_for_mut(target).delegate(set, audit)` | Faulting `set_ptr` / no registered address space → `BadAddress`. Unknown `target` → `NotFound`. A widening request → `DelegationWiden`. |
| `cap_revoke`    | `CapTable::caps_for_mut(target).revoke(cap, audit)`                                                           | Unknown `target` → `NotFound`.                                            |
| `clock_get`     | `KernelArch::monotonic_ns(arch.current_cpu())`, coarsened unless the caller holds `CAP_TIME_HIRES`            | —                                                                         |
| `irq_bind`      | `IrqTable::bind(line, caller.task_id)`                                                                        | `LineOutOfRange` / `LineAlreadyBound` → `OutOfRange`; `ArchUnsupported` → `NotImplemented`. |
| `irq_wait`      | `IrqTable::try_wait_step` polled against `KernelArch::monotonic_ns`, yielding via `Scheduler::yield_current` between iterations | `Ready` → `Ok(0)`; `TimedOut` → `TimedOut`; `NotFound` → `NotFound`; scheduler `NoSuchTask` → `NotFound`. |
| `random_get`    | draws CSPRNG output from `KernelState.rng` (the `rustos_rng::OutputReserve`, see [the RNG page](../lib/rng.md)) into a fixed kernel staging buffer, each chunk copied out through `copy_to_user` | `len > RANDOM_REQUEST_MAX_BYTES` → `LengthOutOfRange`. `len == 0` → `Ok(0)`. Unseeded reserve / entropy shortage → `EntropyNotReady`. Faulting buffer / no registered address space → `BadAddress`. Otherwise `Ok(len)`. |
| `stream_write` | resolves `fd` against the caller's per-process descriptor table (`AddressSpaceRegistry::streams`, established at spawn, `AGENTS.md` §20) — direction first, then the descriptor's console index against the installed console list (`with_consoles`) — then copies the caller's bytes in through `copy_from_user` (bounded by `CONSOLE_WRITE_MAX`) and hands them to that console's output line discipline (`ConsoleDevice::write_output`), which cooks a bare line feed to CR-LF (the ONLCR output translation, the counterpart to the input echo half) so a program that writes `\n` has the cursor return to column zero as it drops a line, then writes to the `ConsoleWrite` device | An **unattached** `stdinfo` (fd 3 `Closed`) → `Ok(len)` with the bytes discarded (advisory best-effort, `AGENTS.md` §20.1 — never a device fallback). Any other `fd` not a writable inherited stream → `NotFound`. No console installed at the descriptor's index → `NotImplemented`. `len == 0` → `Ok(0)`. Faulting buffer / no registered address space → `BadAddress`. Otherwise `Ok(input_bytes_consumed)` — the input count, not the larger device count a cooked newline expands to. |
| `stream_read` | resolves `fd` against the caller's per-process descriptor table — direction first, then the descriptor's console index against the installed console list — then reads from that console's `ConsoleRead` device, wrapped by the init pipeline in kernel-core's `BlockingConsoleRead`, which parks the caller on the scheduler (`reschedule_current`, the `wait`-syscall poll-and-park loop) until the device yields input (`AGENTS.md` §20 — the backing owns blocking; each console's input is its own — the UART never feeds the video console's session, `plans/PI.md` P11) — into a kernel staging buffer (bounded by `CONSOLE_READ_MAX`), then copies the bytes read out through `copy_to_user`. A non-zero `timeout_ns` (arg 3) bounds the park: the reader registers a one-shot deadline on the console wait queue (tickless — an unbounded read still arms no timer at all) and an elapsed bound surfaces as `TimedOut`, so a full-screen program refreshes a clock or status figure without a busy poll | `fd` not a readable inherited stream → `NotFound`. No console installed at the descriptor's index → `NotImplemented`. `len == 0` → `Ok(0)`. No input pending → blocks until input arrives (an unparkable caller → `NotImplemented`); a non-zero `timeout_ns` elapsing with no input → `TimedOut`. Faulting buffer / no registered address space → `BadAddress`. Otherwise `Ok(bytes_read ≥ 1)`. |
| `spawn`         | resolves the `console` argument first (`CONSOLE_INHERIT` → the caller's own descriptor table; else a validated installed-console index → `DescriptorTable::standard_on`), copies the absolute program path in through `copy_from_user` (bounded by `SPAWN_PATH_MAX`), resolves it in the `ProgramRegistry` (the x86_64/riscv64 §18.6 boot floor) or — for an absolute `…/<Name>.app/Run` store-bundle path with an installed `AppStore` — loads and verifies the on-disk bundle through the shared `rustos_appload` gate (signature against the embedded app trust anchor, content hash, ABI/syscall hash), the bundle read running through the secured VFS under the caller's kernel-attested identity and a spawn racing the boot mount parking on the store's readiness latch (`plans/APPS.md` deliverable 8), then resolves the child's kernel-attested **credential** from `target_uid` (`SPAWN_UID_INHERIT` → snapshot the caller's own credential; else, gated by `CAP_SPAWN_AS_USER`, resolve the target user's uid/gid/groups from the authoritative identity table — spawn-as-user, `PREREQUISITES.md` P-C), and hands the validated `rxe` to the installed `ProcessSpawn` producer (`with_spawn`; default `NULL_PROCESS_SPAWN`) which builds a fresh isolated address space and admits a **Ready** user kthread (established with the resolved descriptor table and credential) through `SpawnCtx::admit_process`, returning the child PID — the caller keeps running (`plans/SPAWN.md` SP3) | Console index with no installed console → `NotFound`. Frame allocator not threaded (`with_frames`) → `NotImplemented`. Empty / over-long path → `NotFound`. Faulting path / no registered address space → `BadAddress`. Unknown path → `NotFound`. A `target_uid` switch without `CAP_SPAWN_AS_USER` → `PermissionDenied`; an unresolvable target (no identity table, or unknown uid) → `NotImplemented` / `PermissionDenied`. No producer wired → `NotImplemented`. Otherwise `Ok(pid)`. |
| `mem_map`       | rejects a zero `len`, decodes `flags` through `MapFlags::from_bits`, then hands `(len, flags, addr_hint)` to the installed `MemMap` producer (`with_mem_map`; default `NULL_MEM_MAP`) which maps a fresh zeroed `RW` region into the caller's **own** live address space and returns its base (`plans/SPAWN.md` SP5) | `len == 0` → `LengthOutOfRange`. Reserved flag bit → `OutOfRange`. No producer wired → `NotImplemented`. Frame exhaustion → `OutOfMemory`. Otherwise `Ok(base)`. |
| `mem_unmap`     | rejects a zero `len`, then hands `(base, len)` to the same `MemMap` producer, which zeroes the frames it reclaims (`AGENTS.md` §4) and fails closed when the range does not name a region the caller mapped | `len == 0` → `LengthOutOfRange`. No producer wired → `NotImplemented`. Range not mapped by the caller → producer errno. Otherwise `Ok(0)`. |
| `wait`          | decodes `flags` through `WaitFlags::from_bits`, then hands `(caller.task_id, pid)` to the installed `ProcessWait` producer (`with_process_wait`; default `NULL_PROCESS_WAIT`) which validates the parent/child relationship and reaps a reapable child; blocking (`flags` clear) parks until one is reapable, `NONBLOCK` polls via the same `ProcessTable::reap` and returns `WouldBlock` for a still-running child without parking; on a reap the exit code is copied out to `status` through `copy_to_user` and the child's PID returned (`plans/SPAWN.md` SP6) | Reserved flag bit → `OutOfRange`. No producer wired → `NotImplemented`. `pid` not a child of the caller → `NotFound`. `NONBLOCK` with a still-running child → `WouldBlock` (`status` untouched). Faulting `status` / no registered address space → `BadAddress`. Otherwise `Ok(pid)`. |
| `rlimit_get`    | validates `kind` against `LimitKind`, then reads the caller's effective limit from the installed resource-limit service and copies the encoded `ResourceLimit` out to the user buffer through `copy_to_user` (`AGENTS.md` §24.3). The default trait method fails closed until the L2 enforcement is installed | Unassigned `kind` → `OutOfRange`. No service wired → `NotImplemented`. Faulting buffer / no registered address space → `BadAddress`. Otherwise `Ok(0)`. |
| `rlimit_set`    | copies the encoded `ResourceLimit` in through `copy_from_user`, validates `kind` + the `soft <= hard` pair, and — when the request raises a hard bound above the inherited ceiling — refuses unless the caller holds `CAP_RLIMIT_RAISE` (`AGENTS.md` §24.3). The default trait method fails closed until L2 | Unassigned `kind` / malformed pair → `OutOfRange`. Raising a hard bound without the capability → `PermissionDenied`. No service wired → `NotImplemented`. Faulting buffer → `BadAddress`. Otherwise `Ok(0)`. |
| `console_count` | returns the installed console list's length (`with_consoles`) — the index space `spawn`'s `console` argument selects from (`AGENTS.md` §20, `plans/PI.md` P11) | No console list wired → `NotImplemented`. Otherwise `Ok(count)`. |
| `stream_input_mode` | decodes the mode fail-closed, resolves `fd` against the caller's per-process descriptor table (direction first), then the descriptor's console index against the installed console list, and selects that console's read discipline (`ConsoleDevice::set_input_mode`, which also resets the line-discipline column): cooked echoes the consumed bytes back to the console write half (`AGENTS.md` §20 — terminal local echo, with CR/LF cooked to CR-LF and the column-bounded `BS SP BS` rub-out), secret suppresses echo and arms the activity indicator, raw suppresses both | Reserved/unknown `mode` → `OutOfRange`. `fd` not a readable inherited stream → `NotFound`. No console installed at the descriptor's index → `NotImplemented`. Otherwise `Ok(0)`. |
| `mmio_map`      | resolves `handle` against the caller (`AddressSpaceRegistry::grant(caller.task_id, handle)`, owner-checked per-task grant table; a task with no minted grant resolves to nothing), validates the granted resource is a memory window and the `[offset, offset + len)` sub-region lies wholly inside it (`devres::mappable_subwindow` — `Mmio` / `BusWindow`, non-zero `len`, in-bounds, non-overflowing), then maps **only** that sub-region `(grant_base + offset, len)` into the caller's own address space through the installed `MmioMapFacility` (`with_mmio_map_facility`; default `NULL_MMIO_MAP_FACILITY`), returning its base virtual address — so a large outbound bus-window grant maps just one enumerated BAR, not the whole window (`AGENTS.md` §24.1; `plans/PI.md` P10 chunk 5d-0) | Unknown / non-owned handle → `NotFound`. Non-window grant or a sub-region escaping it → `OutOfRange` / `LengthOutOfRange`. No map facility wired → `NotImplemented`. Frame/virtual-window exhaustion → `OutOfMemory`. Otherwise `Ok(base)`. |
| `dma_alloc`     | resolves `handle` against the caller (same owner-checked per-task grant table), validates the grant is a DMA constraint (`devres::dma_constraint`), rejects a zero / over-the-grant-maximum `len`, then carves a physically-contiguous, zeroed, coherent `RW` buffer bounded by the grant's CPU-side `addr_limit` into the caller's own address space through the installed `DmaAllocFacility` (`with_dma_alloc_facility`; default `NULL_DMA_ALLOC_FACILITY`), resolves the device-visible base via `devres::translate_device_addr` (CPU-physical for a coherent constraint, re-based onto the far side for a translating inbound viewport, `HwResource::dma_translated`), and copies it out to `device_out`, returning the buffer's base virtual address (`plans/PI.md` P10 chunk 5d-0) | Unknown / non-owned handle → `NotFound`. Non-DMA grant → `OutOfRange`. `len == 0` → `LengthOutOfRange`. Over-max / over-limit, or a carve escaping a translating viewport → `OutOfRange`. No DMA facility wired → `NotImplemented`. Frame exhaustion → `OutOfMemory`. Faulting `device_out` → `BadAddress`. Otherwise `Ok(base)`. |
| `dma_free`      | the symmetric free for `dma_alloc`: resolves `handle` against the caller (same owner-checked per-task grant table), validates the grant is a DMA constraint (`devres::dma_constraint`), then releases the buffer based at `cpu_va` from the caller's own address space through the same `DmaAllocFacility` (`free`), zeroing every backing byte (zero-on-free, `AGENTS.md` §4) before its frames return to the allocator, and re-freezes the caller's address-space snapshot. Only `cpu_va` is taken from the caller; the buffer's extent is the allocator's authoritative record. A long-running driver reclaims each transfer's bounce buffers through this rather than leaking DMA frames until it exits (`plans/PI.md` P10) | Unknown / non-owned handle → `NotFound`. Non-DMA grant → `OutOfRange`. `cpu_va` not the base of a live carve in the caller's DMA window (covers a stale, double, or cross-task free) → `OutOfRange`. No DMA facility wired → `NotImplemented`. Otherwise `Ok(0)`. |

`spawn`'s store-bundle verification runs **once per boot** per
read-only system-store bundle (`/System/Apps`, `/System/Services` —
immutable for the life of the boot): the accepted `LoadedApp` is cached
in the kernel's `AppStore` (keyed by bundle root, LRU-evicted under a
byte budget of a fixed fraction of discovered RAM,
`appspawn::APP_CACHE_RAM_DIVISOR`), and a later launch of the same
bundle serves the cached, already-verified image after re-authorising
the **caller's** read of the bundle's `Run` through the secured VFS —
verification is hoisted off the launch hot path (`AGENTS.md` §2.16),
authorisation never is. Bundles on writable volumes (`/Apps`) are never
cached and re-verify through the full gate on every launch.

`KernelArch::monotonic_ns` is a new trait method with **no default
impl**: every architecture port must opt in so an arch that cannot
ship a monotonic clock cannot silently leak that flaw into the
`clock_get` syscall (`AGENTS.md` §5.4.5 — fail closed). The x86_64
port wires it through `apic_timer::Calibration`'s `tsc_per_second`
field, sampled across the same PIT calibration window the LAPIC is
measured over; the conversion goes through
`Calibration::tsc_ticks_to_ns` (saturating).

### Clock resolution and side channels

`clock_get` is unprivileged (no `required_capability`, not audited), so
every task — including the §19.5 parser sandboxes and untrusted
`userland/apps` — can read it. A full-resolution timer is a building
block for cache- and execution-timing side channels (`AGENTS.md`
§19.1), so the value is **gated, not the syscall**: a caller holding
`CAP_TIME_HIRES` receives the raw nanosecond reading, while every other
caller receives the reading floored to `COARSE_CLOCK_GRANULARITY_NS`
(one microsecond, `lib/abi::time`). The flooring is value-only — the
`abi-v1` `clock_get` signature (no args, `u64` return) is unchanged —
and `coarsen_clock_ns` preserves the per-CPU monotonic-non-decreasing
contract the `irq_wait` timeout loop relies on. Tightening or relaxing
the granularity changes only that one constant (`AGENTS.md` §5.7 —
security by default).

The first-party Rust wrapper is `rustos_rt::clock_get` (the raw
nanosecond reading, no coarsening of its own). Userland code that needs
a *timed wait* rather than a bare reading uses `rustos_rt::ClockDelay`,
the one userland [`Delay`](../abi/driver_traits.md) implementation
(`delay_us` parks cooperatively via `clock_get` + `yield`, never a hard
spin, `AGENTS.md` §2.1; `now_us` floors the reading to whole
microseconds). It lives in the single userland runtime so every driver
process shares one clock-backed `Delay` rather than each rolling its own
(`AGENTS.md` §2.2) — a spawned user-space driver hands it to the bring-up
code that honours hardware settle windows (`plans/PI.md` P10 chunk
5d-2-ii).

`ipc_send` / `ipc_recv` resolve the destination endpoint against the
live named-port registry composed into `KernelState`
(`ipc: RwLock<PortRegistry>`, mirroring `caps: RwLock<CapTable>`). An
endpoint that is not currently bound fails closed with `NotFound` — a
real lookup miss, not a blanket stub; only the dispatcher's standard
pipeline audits it.

`ipc_send` is **fully wired** (increment D.1 of the staged user-memory
copy path, `PLAN.md` Stage 7). For a bound endpoint it bounds `len`
against the port's `max_payload`, stages the payload through the
validated `copy_from_user` boundary
([`rustos_kernel_mem::copy_in`](./memory.md#3a-user-memory-copy-uaccess),
reached via `with_caller_aspace`), and hands it to `Port::send`, which
applies the per-send capability check (`AGENTS.md` §5.2). A faulting
user pointer — or a caller with no registered address space (a kernel
task, or one withdrawn on `exit`) — fails closed with `BadAddress`, the
RustOS `EFAULT`; the kernel returns that one code for every
faulting-pointer reason so it cannot be used as a memory-layout oracle
(`AGENTS.md` §19.1). A failed send enqueues nothing. The first-party
Rust wrapper is `rustos_rt::ipc_send`; a spawned driver process uses it
to report its `register()` outcome — a
[`DriverRegisterReply`](../abi/driver_traits.md#driverregisterreply) —
to the reply endpoint its host handed it through its startup arguments
(`rustos_rt::arg`, `PLAN.md` Stage 4.HW).

`ipc_recv` is now **fully wired** (increment D.2 of the staged
user-memory copy path, `PLAN.md` Stage 7). For a bound endpoint it
delivers the head `Port` message through a **peek/commit**:
`Port::recv_with` holds the mailbox lock while the handler copies the
payload into the caller's buffer over the validated `copy_to_user`
boundary
([`rustos_kernel_mem::copy_out`](./memory.md#3a-user-memory-copy-uaccess),
reached via `with_caller_aspace`) and dequeues the message **only** when
that copy succeeds, so a faulting pointer or an undersized buffer leaves
the message queued for a retry rather than dropping it (`AGENTS.md`
§5.4, fail closed). A bound but momentarily empty endpoint returns
`WouldBlock` (the RustOS `EAGAIN`) — retryable and distinct from the
`NotFound` an unbound endpoint returns; a buffer smaller than the
message returns `BufferTooSmall`; a faulting buffer, or a caller with no
registered address space, fails closed with the same `BadAddress`
`ipc_send` uses, never an oracle (`AGENTS.md` §19.1). On success it
returns the number of payload bytes copied.

The deferred-feature branches return a stable `Errno` and emit exactly
one extra audit record — `SYSCALL_FEATURE_UNAVAILABLE` (id 4020, see
`kernel/core::audit`) — so an external consumer can tell apart
"handler rejected because the call failed" from "handler rejected
because the backing subsystem is intentionally inert" (`AGENTS.md`
§15.1 — announce the deferral, never stub). With `random_get` now wired
(increment D.4), **no handler emits it**: every consumer of the
user-memory copy path runs its real backing subsystem. The id stays
reserved in `kernel/core::audit` for a future deferral. The dispatcher's
standard `SYSCALL_HANDLER_REJECTED`
record is *also* emitted for syscalls whose `SyscallSpec::audit == true`
(`ipc_send`, `cap_delegate`); `cap_delegate` additionally records the
delegate decision itself through `CapTable` (`TASK_CAPABILITIES_DELEGATED`
on success, `TASK_CAPABILITIES_DELEGATE_WIDEN` on a rejected widening).
`ipc_recv` is unaudited, so on a failed receive only the dispatcher's
pipeline records it, and on an unbound or empty endpoint it emits
nothing of its own.

`exit` additionally calls `IrqTable::release_for(caller.task_id)`
**before** the capability-record / scheduler eviction so no audited
capability bit survives past the IRQ subsystem's binding release
(`docs/src/security/irq.md` — the kernel unmasks no lines on exit;
a freshly created task that wants the same line must re-issue
`irq_bind`).

The Stage 2.7 follow-up tracker in `PLAN.md` records the remaining
pieces required to lift these deferrals. The named-port registry that
`ipc_send` / `ipc_recv` resolve an `EndpointId` through
(`kernel/ipc::PortRegistry`, see [the IPC page](./ipc.md#named-port-registry))
is composed into `KernelState` and borrowed by the handlers, so
endpoint resolution is live, and both `ipc_send`'s copy-in and
`ipc_recv`'s peek/commit copy-out are wired; what remains for IPC is
publishing the desktop's input ports under their well-known `PortName`s
so a userland `MessagePort` resolves to a live `ipc_recv` (increment E).

The first half of that copy path is now wired (increment C of the
staged "User-memory copy path & per-task address spaces" effort,
`PLAN.md` Stage 7). The per-task `AddressSpaceRegistry`
(`aspaces: RwLock<AddressSpaceRegistry>`, mirroring `caps` / `ipc`) is
threaded into `KernelDispatchHook` / `KernelSyscallHandlers`, and the
new `KernelSyscallHandlers::with_caller_aspace(caller, f)` accessor
resolves `caller.task_id` to the borrowed
`(&dyn UserAddressSpace, &dyn PhysMap)` pair the
[`rustos_kernel_mem::uaccess`](./memory.md#3a-user-memory-copy-uaccess)
copy path walks, running `f` under the registry's read guard and
failing closed to `None` for a caller with no registered space. The
bridge lives in `kernel/core`, so the decoupled dispatcher
(`kernel/syscall`) never gains a `kernel/mem` dependency (`AGENTS.md`
§17.4). Increment D wires `ipc_send` / `ipc_recv` / `cap_delegate` /
`random_get` through this accessor and retires their
`user_memory_copyin` deferral audits; D.1 landed `ipc_send`, D.2 landed
`ipc_recv` (both map a faulting copy to `BadAddress`, the RustOS
`EFAULT`; an empty mailbox is `WouldBlock`), and D.3 landed
`cap_delegate` — it copies the 32-byte `CapabilitySet` in (a faulting
pointer or absent address space maps to `BadAddress`) and runs the
`CapTable` delegate path (`AGENTS.md` §5.2: a widening request is
`DelegationWiden`, an unknown target is `NotFound`). **D.4 landed
`random_get`**: it draws CSPRNG output from the `rustos_rng::OutputReserve`
composed into `KernelState` (`rng: RwLock<Box<dyn RandomReserve + Send +
Sync>>`) and copies it into the caller's buffer through the same
`copy_to_user` boundary, fixed-staging-buffer chunk at a time. Before the
platform-RNG entropy seam (`AGENTS.md` §17.2) seeds the reserve it is
unseeded, so a draw fails closed with `EntropyNotReady` (`AGENTS.md` §22 —
never weak bytes) rather than stubbing; a faulting buffer or absent
address space maps to `BadAddress`. With D.4 in, the whole staged
user-memory copy path is wired; only increment E (the per-arch live
page-fault fix-up + publishing the input ports) remains.

## Dispatcher contract

`Dispatcher::dispatch` is the *only* entry point. Calling it runs the
following sequence — the order matches `AGENTS.md` §5.4 step for step:

1. Caller identification — the `CallerContext` comes from the per-CPU
   current-task slot owned by `kernel/sched`; the dispatcher does not
   accept caller-supplied identity.
2. Capability check via `TaskCapabilities::has`.
3. Argument validation against the declared `AbiType`s and trailing-zero
   rule.
4. Dispatch through the `SyscallHandlers` trait. `kernel/core` provides
   the production implementation; tests substitute a mock.
5. Audit emission via the structured sink — exactly one record per
   security-relevant decision.

## Per-architecture entry stubs

The architecture-neutral dispatcher above is reached through a thin
per-target stub that marshals the platform's syscall-instruction
registers into a `RawArgs` tuple. Stage 3a (c6) landed the x86_64
stub; Stage 3b/3c/3d will add the remaining Tier-1 ports.

| Arch | Module | Instruction | Argument registers |
| --- | --- | --- | --- |
| x86_64 | `rustos_arch_x86_64::syscall_entry` | `syscall` / `sysretq` (`IA32_LSTAR`) | `%rdi`, `%rsi`, `%rdx`, `%r10`, `%r8`, `%r9` (number in `%rax`) |
| aarch64 | — (Stage 3b) | `svc #0` | `x0`..=`x5` (number in `x8`) |
| riscv64 | — (Stage 3c) | `ecall` | `a0`..=`a5` (number in `a7`) |
| wasm32 | — (Stage 3d) | host-imported function | first six i64 arguments |

The stub never duplicates the validation surface in
`kernel/syscall::table`: it builds a `[u64; SYSCALL_MAX_ARGS]` in
the canonical order (matching `RawArgs`'s `#[repr(transparent)]`
layout) and hands it to a binary-installed callback that forwards
to `Dispatcher::dispatch`. The full description of the x86_64 stub
— MSR programming, `SyscallTls` layout, and the naked entry
sequence — lives in
[the x86_64 platform page](../platform/x86_64.md#stage-3a-c6--syscallsysret-entry).

## Out of scope (Stage 2.7)

* New syscalls beyond what Stages 2.1–2.6 require. Adding a syscall
  takes a new `SyscallSpec` row, a new `SyscallHandlers` method, and
  an entry in this document — all in the same commit.
