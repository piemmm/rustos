# Resource limits and scalability

RustOS sizes resource *capacities* from the hardware it discovers at boot
and grows them on demand; it never hard-wires a `const` ceiling that caps a
large machine or wastes a small one (`AGENTS.md` §24). On top of those
discovered, growable defaults a principal may *impose* a lower ceiling on
itself or its children — the RustOS equivalent of POSIX `ulimit`/`rlimit`.

This page describes the binding §24 contract and its staged build-out. The
ABI surface (`LimitKind`, `ResourceLimit`, the `rlimit_get`/`rlimit_set`
syscalls, and the `CAP_RLIMIT_RAISE` capability), the **kernel enforcement**
of it, the **`ulimit` shell command**, and the **System Information limits
query**, and the **growable *and* shrinkable kernel-stack arena** are
**landed**, as are the **aarch64**, **riscv64**, and **x86_64** per-arch
secondary-bring-up bounds (each port's secondary-stack pool and per-CPU
`preempt`/`percpu`/`syscall_entry` state are now caller-sized); no per-arch
secondary-bring-up bound remains staged
(see *Status* below).

## Capacities scale; security bounds stay fixed

A *capacity* — how many tasks, threads, CPUs, open handles, memory regions,
or stack bytes the system or a process may use — is **derived** from the
discovered hardware (the §18 hardware tree: RAM window, CPU/hart count) and
**grows on demand** rather than failing at a literal ceiling (`AGENTS.md`
§24.1). Each scalable resource has a single default *policy* (a function of
the discovered hardware) tuned to be sensible for both an interactive desktop
and a busy server (§24.2).

This must never be confused with the fixed security/format *bounds* on
untrusted input (§24.4), which stay deliberately fixed and fail closed:
parser parameter/byte caps (`lib/vt` `MAX_PARAMS`/`MAX_STRING`, `lib/fdt`
`MAX_DEPTH`, the `lib/svg` caps), on-disk/wire format constants (ext4/FAT32,
RustFS, ABI record sizes), and the charter-blessed §22 RNG output reserve.
Turning a security bound into a growable capacity, or a capacity into a frozen
ceiling, are both defects — when in doubt, stop and ask (`AGENTS.md` §15.7).

## The limit ABI

The settable facility is defined in `lib/abi/src/rlimit.rs`, held to the same
ABI discipline as the syscall table (`AGENTS.md` §9): versioned, hashed, and
frozen from the first release.

- **`LimitKind`** is the closed, versioned set of resources a limit can
  govern. Today: `AddressSpaceBytes` (the `mem_map` capacity), `OpenStreams`
  (the descriptor-table capacity), `Processes` (the `spawn` fan-out), and
  `StackBytes` (the per-task stack). Discriminants never move; a new resource
  takes the next free discriminant and bumps `LimitKind::COUNT`.
- **`ResourceLimit { soft, hard }`** is the soft/hard pair, each a `u64`.
  `RLIMIT_INFINITY` (`u64::MAX`) means "no ceiling imposed", leaving the
  resource governed only by the discovered, growable default policy. The
  well-formedness invariant is `soft <= hard`; the decoder fails closed on a
  malformed pair or a short buffer.
- **`ResourceLimit::intersect`** is the never-widen combinator used to inherit
  and delegate a limit: a child or delegate receives the minimum of the soft
  bounds and the minimum of the hard bounds, so neither can exceed the
  inherited ceiling (the §24.3 rule, mirroring capability delegation §5.2).

## The `rlimit_get` / `rlimit_set` syscalls

`rlimit_get` (no. 17) and `rlimit_set` (no. 18) carry a `LimitKind`
discriminant (`u32`) and a `ResourceLimit` through a 16-byte user buffer (see
[the syscall page](./syscalls.md)). Both are **ungated at the dispatcher** —
reading one's own limit and *lowering* a bound are the unprivileged
own-process baseline (`AGENTS.md` §16.6). `rlimit_set` performs the finer
check **handler-side**: a request that *raises* a hard bound above the
inherited ceiling is refused with `PermissionDenied` unless the caller holds
**`CAP_RLIMIT_RAISE`** (`AGENTS.md` §24.3). `rlimit_set` is audited (it
changes enforced policy); `rlimit_get` is a pure observer and is not.

The first-party Rust wrappers are `rustos_rt::rlimit_get` /
`rustos_rt::rlimit_set`; non-Rust programs call `ros_sys_rlimit_get` /
`ros_sys_rlimit_set` over the generated `rustos_rlimit.h` view.

## Kernel enforcement

The kernel holds each task's effective limits as a `LimitSet` (one
`ResourceLimit` per `LimitKind`) in the per-task `AddressSpaceRegistry`,
alongside the standard-stream descriptor table — both share the per-process
lifecycle (established at spawn, withdrawn at exit) and the same `TaskId` key,
so there is no parallel registry (`AGENTS.md` §2.2). A task with no imposed
limit reads `LimitSet::DEFAULT`: every resource `RLIMIT_INFINITY` for now, the
single place a discovered-hardware default policy slots in later (L3) without a
second code path.

- **`rlimit_get`** validates `kind` against the closed `LimitKind` set, reads
  the caller's *own* effective limit, and copies it out through the validated
  `copy_to_user` boundary. It is keyed by the kernel-trusted `caller.task_id`,
  never a caller-supplied id (§5.4.1), so a process can only read its own
  limits; an unregistered caller fails closed with `BadAddress` (§19.1).
- **`rlimit_set`** validates `kind`, copies the requested limit in (the decoder
  rejects a malformed `soft > hard` pair, fail closed), then applies the §24.3
  rule: lowering — or any change that does not raise the hard bound above the
  current ceiling — is free, while raising the hard bound requires
  `CAP_RLIMIT_RAISE` and is otherwise refused with `PermissionDenied`. The
  authorised limit is stored against the caller's own id. Because the syscall
  is audited, a rejection is logged automatically (§19.4).
- **Inheritance.** When a process is admitted by `spawn`, the child's limit set
  is the parent's intersected against the system default (`LimitSet::inherit`),
  so a child can never hold a bound wider than either the parent's ceiling or
  the default — the never-widen rule, mirroring capability delegation (§5.2).

Storing a limit is not enough — it is **enforced on the path that consumes the
resource**, before the resource is committed, and fails closed (`AGENTS.md`
§24.3 / §5.4):

- **`AddressSpaceBytes` on `mem_map`.** The kernel keeps each task's running
  total of anonymous memory mapped through `mem_map` in the per-task
  `AddressSpaceRegistry` (`mapped_anon_bytes`, accrued on a successful map,
  credited on `mem_unmap`, dropped at exit — the same lifecycle and `TaskId`
  key as the limit set, no parallel registry, §2.2). The `mem_map` handler
  rounds the request up to whole pages and, *before* reaching the producer,
  refuses it with `OutOfRange` if the task's live total plus the request would
  exceed its soft `AddressSpaceBytes` bound. A task at the default
  `RLIMIT_INFINITY` is never affected; a tightened ceiling is honoured rather
  than silently ignored.
- **`StackBytes` on the demand-grown stack fault path.** Each admitted
  process's reserved stack span (recorded at admission, see
  [the memory page](./memory.md) §7c) grows one page per fault through the
  stack-growth resolver, and the resolver checks the committed extent the
  faulting page would reach against the task's soft `StackBytes` bound
  *before* any page is mapped: a fault past the bound is refused, the task
  is fault-killed with the audited `stack_limit` class, and nothing is
  mapped (fail closed). The committed-bytes low-water mark is the live
  usage the `sysinfo limits` query reports for `stack-bytes`. A task at
  the default `RLIMIT_INFINITY` grows to the structural span bound; a
  lowered `ulimit` stack bound stops growth exactly where it says.

The remaining `LimitKind`s (`OpenStreams`, `Processes`) carry their
soft/hard bounds and inherit correctly, but their *consuming-path*
enforcement is not yet wired; until it is, those ceilings are observed and
settable but not yet acted on at the descriptor/spawn path, and their
reported usage stays an honest zero (no live accounter yet).

## The `ulimit` shell command

The `ulimit` builtin in the default shell (`userland/shell/elsh`) is the
command-line face of the facility. It reads and imposes the calling
process's own limits over the L1 ABI through an injected
`rustos_elsh::LimitStore` seam — backed by `rustos_rt::rlimit_get` /
`rustos_rt::rlimit_set` in the real `Run` binary, and by an in-memory double
in tests, so the parsing and policy logic is exercised without a kernel (the
same `ProcessHost`/`Console` seam pattern the rest of the shell uses). The
shell holds no ambient authority of its own (`AGENTS.md` §4): every check
stays kernel-side, and the `CAP_RLIMIT_RAISE` denial surfaces as an error the
builtin reports rather than hides (§2.9).

Usage:

```text
ulimit [-a] [-H | -S] [<resource> [<value>]]
```

- `ulimit` or `ulimit -a` reports every resource's soft bound (its hard
  bound with `-H`), one aligned line each.
- `ulimit <resource>` reports that resource's soft bound (`-H` for the hard
  bound).
- `ulimit <resource> <value>` imposes the limit. With neither flag both
  bounds are set (POSIX); `-S` sets only the soft bound and `-H` only the
  hard bound, leaving the other as the current limit reads it. `<value>` is a
  decimal byte/count or the word `unlimited` (`RLIMIT_INFINITY`).

`<resource>` is one of the canonical `LimitKind` names
(`LimitKind::name`): `address-space-bytes`, `open-streams`, `processes`,
`stack-bytes`. An unknown resource, an unknown flag, a malformed value, or a
soft bound set above its hard ceiling is rejected without touching the
kernel (fail closed, `AGENTS.md` §2.1/§2.9); the store is never written on a
rejected request.

## The `sysinfo limits` query

Because there is no `/proc`, a principal observes its *own* effective limits
and its current live usage of each through the System Information API
(`AGENTS.md` §16.6), not a virtual file. The query is
`SysinfoQueryId::RESOURCE_LIMITS` (id 7); its response is exactly
`LimitKind::COUNT` `ResourceLimitRecord`s packed back-to-back in `LimitKind`
discriminant order (`RESOURCE_LIMITS_REPORT_LEN` bytes), each carrying the
resource's `kind`, its effective `ResourceLimit { soft, hard }`, and the
caller's current `usage` (bytes for the `*Bytes` kinds, a count otherwise).

The query is **self-scoped** — its answer describes the caller's own task
only — so, like `self_process_list`, it carries no capability gate and is not
audited (§16.6); observing *another* principal's limits would be a separate,
gated query. The kernel exposes no path that bypasses this; `sysinfod` serves
it from the per-task `LimitSet` (effective limits) and the live accounting
behind each `LimitKind` (usage), keyed by the kernel-trusted caller identity.

The command-line face is `sysinfo limits` (alias `rlimits`) in
`userland/shell/sysinfo`: it issues the query and prints one aligned row per
resource (soft, hard, usage), spelling `RLIMIT_INFINITY` as `unlimited`. A
reply of the wrong length fails closed rather than rendering a partial table
(`AGENTS.md` §2.1). Where `ulimit` *changes* a principal's own limits, the
`sysinfo limits` query *observes* limits and usage together.

The same query also backs the `info:`/`stats:` resource references
(`plans/ALIAS.md`, `lib/procinfo::resolve`): `info:limits/<kind>/{soft,hard}`
resolves to a configured bound (spelling `RLIMIT_INFINITY` as `unlimited`
through the one shared renderer the CLI uses) and `stats:limits/<kind>`
resolves to the live usage gauge (`bytes` for the `*Bytes` kinds, a
dimensionless `count` otherwise). Both are self-scoped and unprivileged, fail
closed on an unknown resource or a malformed reply, and read the same
`RESOURCE_LIMITS` reply — no second query and no `/proc`-style file.

## Discovered-hardware capacity policies

The kthread kernel-stack capacity is the first capacity converted off a
hand-picked constant onto a §24.2 *policy* (a function of discovered
hardware), under two knobs:

- **Per-task kernel-stack size** (`rustos_kernel_core::KTHREAD_STACK_BYTES`)
  is **release-tuned**, not a single worst-case constant (§24.2): a release
  image — the form that ships — reserves 32 KiB per kthread kernel stack,
  while an unoptimised debug build keeps the proven-ample 64 KiB its deeper
  frames need. Both are whole 4 KiB pages so the guard page below the usable
  region lands on a clean boundary in either profile. Halving the release
  reservation doubles how many guarded stacks a given arena block holds — the
  server profile's win.
- **Guard-arena size** (`rustos_kernel::mem_map::stack_arena_bytes`) is
  **derived from the discovered RAM window**, not a fixed 2 MiB block (§24.1).
  The boot path reserves roughly 1/64 of discovered RAM for kthread kernel
  stacks, clamped to `[2 MiB, 64 MiB]` and rounded down to a whole 2 MiB
  block (so every guard page still becomes its own L3 leaf after
  `prepare_guard_arena`). A 64 MiB embedded board floors at one 2 MiB block
  (tens of stacks); a 1 GiB desktop gets 16 MiB (hundreds); a large server
  caps at 64 MiB (over a thousand) rather than reserving an unbounded slab up
  front. A RAM window too small to carve even one block degrades to no arena
  and the software-canary `BoxStack` fallback (fail closed, §2.17).

## Growable *and* shrinkable kernel-stack arena

The boot-carved guard arena above is the *first* block, not a ceiling. When it
runs out of room for a whole guarded region, `rustos_kernel::stack_arena::
StackArena` **grows** by chaining a fresh, independently block-split 2 MiB
block rather than failing over to the software-canary `BoxStack`; and when a
chained block later goes idle it **shrinks** by returning that block to the
allocator. So the hardware-guarded kthread-stack capacity scales with
discovered RAM and rises *and* falls on demand (§24.1 — never a one-way
ratchet), not just with the size of the boot block.

- **The grow source** (`FrameArenaGrow`) draws each fresh block from the
  kernel's live `FrameAllocator` as a 2 MiB-aligned, contiguous
  `alloc_order(9)` block — the same block granularity the boot arena uses, so
  every guard page in a chained block still lands on its own L3 leaf when the
  spawn seam re-expresses the covering block at 4 KiB granularity
  (`split_block`) in the owning task's root and unmaps the guard page. A
  chained block therefore hosts hardware-guarded stacks exactly as the
  boot-carved block does (§4 — the guard-page invariant is preserved while the
  capacity grows).
- **Bounded to the identity window.** A chained block must lie wholly within
  the identity window every spawned address space identity-maps (on aarch64
  derived from the configured Device/RAM gigapage masks; a fixed
  `IDENTITY_GIB` on the other ports),
  so the stack stays mapped — and its guard page faults — in *every* space the
  task runs under. A block the allocator hands back above that window is
  returned to the allocator and the grow **fails closed** (`None`), dropping the
  caller to the `BoxStack` software-canary fallback rather than handing out an
  unreachable stack (§4 / §2.9).
- **Fails closed on physical exhaustion.** When the frame allocator can no
  longer supply a 2 MiB block, the grow returns `None`, and the spawn seam
  falls back to a software-canary `BoxStack` — deterministic, never a panic
  (§2.9 / §2.17).
- **Serialised, off the hot path.** The whole bump-and-chain is serialised by a
  `SpinLock`; `alloc` is a per-spawn operation, not a hot path, so a lock is the
  simplest correct way to chain a fresh block atomically (§2.16). The chaining
  arithmetic, the frame-allocator-backed grow, the identity-window rejection,
  and the fail-closed-on-OOM path are all exercised by host unit tests over a
  real `FrameAllocator`; the per-stack guard-page split/unmap a chained block
  relies on is the same mechanism the `stack_arena_qemu_aarch64` and
  `stack_overrun_qemu_aarch64` verticals already prove on a 2 MiB identity
  block.

**Shrinks on demand too.** Reclamation is symmetric: when a task exits, the
scheduler drops its `Box<dyn KernelStack>`, which drops the `ArenaStack`, whose
`Drop` returns the region to its owning block (`StackArena::free`). The capacity
falls as well as rises, without thrashing:

- **Per-block live-count accounting.** Each block (boot-carved + each chained)
  carries the count of guarded regions currently handed out from it. A free
  locates its owning block by address range and checked-decrements that count;
  a foreign/misaligned address or an already-zero count is rejected without
  underflowing — fail closed (§2.9), surfaced as a typed `FreeOutcome`. A block
  whose count reaches zero is *idle*. The per-block `{ next, live, cursor }`
  records live in a reserved, identity-mapped header at each block's own base —
  outside the guarded regions, accessed through the `BlockStore` seam — so the
  block list is itself a §24.1 capacity (no second allocation, no fixed block
  cap; the host tests drive an in-memory `BlockStore`).
- **One-free-block grace (hysteresis).** Exactly one idle chained block stays
  resident: a chained block is returned to the allocator only when it goes idle
  *and* another idle chained block already exists, so an alloc/free oscillation
  across a block boundary reuses the retained idle block instead of repeatedly
  free→chain (amortised, no thrash, §2.16). Reclamation is at most one block
  return per free — never a spin/retry loop (§2.1) — under the same `SpinLock`
  off the hot path. An idle block is reset and reused before a fresh one is
  chained.
- **Boot block is never returned.** The boot-carved first block
  (`RegionKind::Reserved`, kernel-image-owned) is never released; only
  `FrameArenaGrow`-chained blocks are reclaimed, through a symmetric
  `FrameArenaShrink` over `free_order(9)`.
- **Secure.** A reclaimed block is fully zeroed before it returns to the
  allocator (§4 zero-on-free — a kthread kernel stack can hold spilled
  capability tokens or credentials); a block that cannot be safely
  scrubbed/returned is retained rather than released (fail closed, §2.17). The
  per-stack guard `split_block`/`unmap` was applied in the *task's own* root,
  which is torn down on exit, so reclaiming the block aliases no live mapping
  in the kernel's identity map.

The live-count accounting, the grace hysteresis (release only on the second
idle block, zero releases under boundary oscillation), idle-block reuse, the
fail-closed double-/foreign-/misaligned-free paths, the boot-block-never-
released invariant, and the real-buffer zero-on-free scrub are all covered by
host unit tests; the `Drop` reclaim seam and the `free_order` return-to-
allocator step run on the `stack_arena_qemu_aarch64` / `stack_overrun_qemu_
aarch64` verticals. Sizing the per-arch CPU/hart secondary-bring-up bound from
§18 discovery is the remaining §24.1 sweep work (see *Per-arch CPU/hart handle
bookkeeping* below).

## Userland heap free-span table (grow-on-demand)

The `rustos-rt` `mem_map`-backed heap (`lib/rt/src/heap.rs`) tracks freed
regions as an address-sorted, coalesced list of free **spans**. The number of
distinct spans is a *capacity*, not a fixed `const` ceiling (§24.1): the table
lives in a growable `SpanStore`, and when a fragmenting workload fills it the
store **grows before it fails** — it maps one more whole metadata page (its own
fixed virtual window, distinct from the data arena) and continues. Only genuine
resource exhaustion (`mem_map` can no longer supply a metadata page) makes a
non-coalescing free drop a span, and an allocation that cannot record its
residual fails closed with a null pointer — deterministic OOM, never a panic
(§4 / §2.9). In production the store maps its metadata through the same
`abi-v1` anonymous-memory syscalls; the host unit tests drive a `Vec`-backed
store, so the growth and fail-closed logic is exercised entirely on the host.
This is a §24.1 sweep conversion (replacing the former fixed `MAX_SPANS = 256`
array), independent of the kernel-stack arena work.

## Supplementary-group ceiling (capability-raisable)

The number of supplementary groups a single user record may carry
(`kernel/sec`) is a *capacity*, not a hard-wired ceiling (§24.1). A fresh
`IdentityTableBuilder` starts at the `DEFAULT_MAX_SUPPLEMENTARY_GROUPS`
default policy (32, matching POSIX `NGROUPS_MAX`; §24.2), and a deployment
that genuinely needs larger group sets raises the per-builder ceiling at
runtime with `IdentityTableBuilder::with_supplementary_group_limit`.
Lowering the ceiling is always free (a principal may tighten its own limit,
§24.3); raising it above the default grows the capacity and so requires the
caller to hold `CAP_RLIMIT_RAISE` (§24.3), otherwise it fails closed with
`Errno::PermissionDenied` and leaves the ceiling unchanged (§5.4). The
storage was already a growable `Vec`, so only the fixed ceiling changed.
Crucially, a candidate record can never raise the ceiling — only a capable
principal can — so a hostile or corrupted on-disk record can never force
unbounded kernel allocation: the §24.4 anti-DoS bound is preserved while the
capacity itself becomes settable.

## Spawn page-table capacity (allocator-backed, grow-on-demand)

The runtime `spawn` syscall's fan-out — how many distinct processes can be
built — was a hard `const MAX_SPAWNS = 8` backing a fixed `[PageTablePool; 8]`
`.bss` reserve in each production producer (`kernel/rustos-kernel/
src/spawn_producer.rs` and `…_x86_64.rs`): a §24.1 capacity ceiling that
wasted RAM on a small machine and starved a large one. It is now a *capacity*
that scales with discovered RAM and grows on demand. Each child's page-table
hierarchy is drawn from the kernel's live `FrameAllocator` through
`kernel/mem`'s `FrameTableSource` (the W5b-3 allocator-backed page-table
frame source), cached once per boot in a `static Once<FrameTableSource>` over
the leaked-`'static` allocator the boot path threads through the new
`KernelSyscallHandlers::with_page_table_frames` seam and exposes to the
producer as `SpawnCtx::page_table_allocator`. There is no fixed reserve and so
no hard process cap: the system spawns until physical RAM is genuinely
exhausted, when `FrameTableSource::alloc_table` returns `None` and the build
fails closed with `Errno::NoSpace` — deterministic OOM, never a panic (§4 /
§2.9).

The frame source is backed by an **identity** `DirectPhysMap` covering the
same low window each child space identity-maps, because every bare-metal
port recovers an existing child page table by dereferencing its physical
address directly (`paging::ensure_child`, `phys as *mut`), so the frame view
the source hands the port must satisfy `virtual == physical`; a frame outside
that window fails the translate and the spawn fails closed (§2.9) — the same
window the child's image data frames already resolve under. Page-table frames
are handed out monotonically and not reclaimed while a child lives (the
discipline the pool used, §2.1); reclaiming a dead process's page-table
frames is a later stage. aarch64 and x86_64 (the two ports with a production
spawn producer) share this conversion; riscv64 has no production spawn
producer yet, so the capacity does not exist there to convert.

## Per-arch CPU/hart handle bookkeeping (discovered-count-sized)

Each architecture handle keeps per-CPU bookkeeping — the dense-`CpuId` →
hardware-id affinity map, the host-only IPI ledger, and (on the SMP ports) the
per-core `CoreClass` table — historically in fixed `[T; MAX_*]` arrays whose
hand-picked length (`MAX_CPUS` / `MAX_HARTS` / `MAX_WORKERS`) a larger machine
outgrew and a single-CPU machine wasted (§24.1).

**wasm32 (`WasmArch`) — done.** The `cpu_to_worker` map and `host_ipi_count`
ledger are now allocator-backed boxed slices sized to the **discovered worker
count** — `with_workers(workers).len()`, floored at `boot_cpu + 1`
(`worker_storage_len`) so the boot slot is always representable. Web-Worker
contexts are fixed at boot, so the discovered count *is* the hardware quantity;
no speculative headroom is reserved beyond it (§24.2). Every per-slot access is
bounds-checked and fails closed (a stray IPI target is dropped and counted),
and the handle is `Arc`-constructed, so the one-shot allocation is safe (§4).
`WasmArch::worker_capacity()` reports the size. (`MAX_WORKERS` survives only as
the `smp::start_worker` host worker-index bound — the secondary-bring-up item
below.)

The bare-metal ports cannot use the boxed-slice approach directly:
introducing `extern crate alloc` into a bare-metal arch crate puts the
`alloc` crate in the *dependency graph* of **every** freestanding binary that
links it, so rustc then demands a `#[global_allocator]`. The deliberately
minimal Stage-2 QEMU bins (e.g. `memory_isolation_qemu_aarch64`, which "links
only the arch port" and tests page-table isolation without ever constructing
the arch handle) would be forced to carry a 64 MiB bump heap they have no use
for — the opposite of the §2.3 / §5.4.5 minimalism those bins were built for.
The proper, scalable fix is therefore **no `alloc` in the arch crate**: the
handle holds `&'static` per-CPU slices the caller provides — the
allocator-having callers (the production boot path, the `Arc`-using test
kernels) leak a right-sized backing, while the allocator-free
handle-constructing bins supply a small `static`; paging-only bins are
untouched.

**riscv64 (`RiscvArch`) — done.** `RiscvArch` no longer holds
`[T; MAX_HARTS]` arrays; it borrows two `&'static [AtomicU64]` slices — the
dense-`CpuId` → hart-id map and the host-only IPI ledger — from a
caller-provided `RiscvArchStorage<N>` backing, where `N` is the logical-CPU
count the caller sizes for its machine (a single-hart vertical uses
`RiscvArchStorage<1>`, a two-hart vertical `<2>`, a multi-hart boot path sizes
`N` from the device-tree hart count). The storage is a `static` (the
allocator-free pattern every QEMU vertical uses) or a leaked allocation, so the
arch crate stays `alloc`-free and the Stage-2 paging-only bins are untouched.
The map encodes an unpopulated slot as the `u64::MAX` (`NO_HARTID`) sentinel —
a real hart id is a `u32`, so it can never collide — and the constructor
populates the map through the shared borrow with atomic stores (no `&'static
mut` needed). Every per-slot access is bounds-checked against the slice length
and the cross-CPU shootdown / IPI loops iterate that length, so there is no
`MAX_HARTS` ceiling in the handle. (riscv64 no longer has a `MAX_HARTS`
constant at all — the secondary-bring-up stack pool and per-CPU `preempt`
statics are *also* now caller-sized; see *Per-arch secondary-bring-up bound*
below.) The host suite and all nine riscv64 QEMU verticals (single- and
two-hart) construct through the new backing.

**aarch64 (`Aarch64Arch`) — done.** `Aarch64Arch` no longer holds
`[T; MAX_CPUS]` arrays; it borrows three `&'static` slices — the dense-`CpuId`
→ `MPIDR_EL1` affinity map (`&[AtomicU64]`), the host-only IPI ledger
(`&[AtomicU64]`), and the per-core `CoreClass` table (`&[AtomicU8]`) — from a
caller-provided `Aarch64ArchStorage<N>` backing, exactly as riscv64 borrows
from `RiscvArchStorage<N>`. The affinity map encodes an unpopulated slot as the
`u64::MAX` (`NO_MPIDR`) sentinel — a real `MPIDR_EL1` affinity can never be all
ones, since bits `[63:40]` are `RES0` — and the constructor populates the map
through the shared borrow with atomic stores. Every per-slot access is
bounds-checked against the slice length, `send_ipi` bounds its target by the
slice length, and `classify_from_fdt` finds the peak rating and classifies each
core in two device-tree passes (the pure `hetcore::class_for_capacity`) with no
fixed-size buffer, so there is no `MAX_CPUS` ceiling in the handle. (The
aarch64 secondary-bring-up pool and per-CPU `preempt` statics are *also* now
caller-sized — see *Per-arch secondary-bring-up bound* below — so aarch64 no
longer has a `MAX_CPUS` constant at all.) The production boot path supplies a
`static Aarch64ArchStorage<1>` (the boot slice brings up the boot core only)
and every aarch64 QEMU vertical constructs through a right-sized `static`.

**x86_64 (`X86_64Arch`) — done.** `X86_64Arch` no longer holds
`[T; MAX_CPUS]` arrays; it borrows three `&'static` slices — the dense-`CpuId`
→ LAPIC-ID map (`&[AtomicU16]`), the host-only IPI ledger (`&[AtomicU64]`), and
the per-core `CoreClass` table (`&[AtomicU8]`) — from a caller-provided
`X86_64ArchStorage<N>` backing, exactly as riscv64/aarch64 borrow from their
storage. The LAPIC map encodes an unpopulated slot as the `u16::MAX`
(`NO_LAPIC`) sentinel — a real LAPIC ID is a `u8`, so it can never collide —
and the constructor populates the map from the caller's `&[Option<u8>]` MADT
map through the shared borrow with atomic stores. Every per-slot access is
bounds-checked against the slice length, `send_ipi` bounds its target by it,
and `shootdown_page` no longer fills a fixed `[u8; MAX_CPUS]` scratch buffer —
it streams the other CPUs' LAPIC ids straight out of the borrowed map into
`tlb_shootdown::shootdown` (now an `Iterator + Clone` consumer that walks the
ids twice: once to publish the acknowledge count, once to raise the IPIs), so
there is no `MAX_CPUS` ceiling in the handle. (The per-CPU
`percpu`/`syscall_entry` arenas and the AP stack pool are *also* now
caller-sized — see *Per-arch secondary-bring-up bound* below — so x86_64 no
longer has a `MAX_CPUS` constant at all.) The production boot path supplies a
`static X86_64ArchStorage<1>` (production `rustos-kernel` runs single-CPU) and
every x86_64 QEMU vertical constructs through a right-sized `static`.

## Per-arch secondary-bring-up bound (discovered-count-sized)

Starting a secondary CPU needs two pieces of per-CPU state the handle's
bookkeeping (above) does *not* cover: the **stack** the freshly-started core
runs on before it has one, and the per-CPU **timer/preempt** slots the tick
path records into. Both were historically fixed `[T; MAX_*]` reserves keyed to
a hand-picked core count — the assembly `.bss` secondary-stack pool
(`smp.s` `.skip SECONDARY_MAX_* * STACK`) and the `preempt`/`percpu` per-CPU
`static` arrays — so a larger machine outgrew them and a small one wasted the
reserve (§24.1). Unlike the handle bookkeeping, an *assembly* `.bss` reserve
cannot be sized from runtime discovery at all, so closing this is a genuine
SMP-bring-up redesign rather than a bookkeeping resize.

**aarch64 — done.** The fixed `.bss` pool and the `MAX_CPUS` constant are
gone. The secondary-stack pool is now a caller-provided
`smp::SecondaryStackPool<N>` (`N` = the core count the caller sizes for its
machine, a `static` for the allocator-free bins per the §24.1 watch-out); its
`register` publishes the pool base and per-core stride to the `smp.s`
trampoline (which now computes each started core's stack top as
`base + (cpuid + 1) * stride` from those runtime globals rather than indexing a
baked-in array) and the covered count to `is_valid_cpu`, ordered ahead of any
PSCI `CPU_ON` by a `dsb sy`. Registration is set-once and an unstarted system
fails closed — every id is invalid until a pool is registered, so a `CPU_ON`
for an unbacked core is refused (§2.9 / §5.4.5). The per-CPU timer slots are
likewise a caller-provided `preempt::PreemptStorage<N>`, published as
`&'static [AtomicU64]` slices (interval + recorded `CpuId`) through a set-once
`register`; `init_local_preempt` and the timer IRQ path index the published
slices and fail closed (no arm, no dispatch) when none is registered or the id
is out of range. The per-stack size (`SECONDARY_STACK_BYTES`, 64 KiB) stays a
fixed *bound* — that is a per-stack quantity, not a CPU-count capacity, so it
is correctly a constant (§24.4). The two-core `ipi_smp_qemu_aarch64` and
`cross_cpu_tlb_shootdown_qemu_aarch64` verticals register a
`SecondaryStackPool<2>`; the single-CPU `timer_preempt_qemu_aarch64` and
`sched_drive_qemu_aarch64` verticals register a `PreemptStorage<1>`; all four
still bring up and drive their cores on the `virt` board. Production
`rustos-kernel` runs single-CPU and starts no secondaries, so it registers
neither.

**riscv64 — done.** The fixed `.bss` pool (`smp.s` `.equ SECONDARY_MAX_HARTS`
+ `.skip`) and the `smp::MAX_HARTS` constant are gone, exactly as on aarch64.
The secondary-stack pool is a caller-provided `smp::SecondaryStackPool<N>` (a
`static` for the allocator-free bins); its `register` publishes the pool base
and the per-hart slice's log2 size to the `smp.s` trampoline (which computes
each started hart's stack top as `base + (hartid + 1) << shift` from those
runtime globals — a left shift, since the freestanding stub avoids the `M`
multiply extension) and the covered count to `is_valid_hartid`, ordered ahead
of any SBI `hart_start` by a `fence`. Registration is set-once and an unstarted
system fails closed (every id invalid until a pool is registered, so a
`hart_start` for an unbacked hart is refused, §2.9 / §5.4.5). The per-hart
timer slots are likewise a caller-provided `preempt::PreemptStorage<N>`,
published as `&'static [AtomicU64]` slices (interval + recorded `CpuId`) through
a set-once `register`; `init_local_preempt` and the timer trap path index the
published slices and fail closed when none is registered or the id is out of
range. The per-stack size (`SECONDARY_STACK_BYTES`, 16 KiB) stays a fixed
*bound* (§24.4). The two-hart `ipi_smp_qemu_riscv64` and
`cross_cpu_tlb_shootdown_qemu_riscv64` verticals register a
`SecondaryStackPool<2>`; the single-hart `timer_preempt_qemu_riscv64` registers
a `PreemptStorage<1>`.

**x86_64 — done.** The `percpu::MAX_CPUS` constant is gone, and the three
per-CPU `[T; MAX_CPUS]` `static` arenas it sized are now caller-provided,
runtime-sized storages published through set-once `register` calls before the
first use, each failing closed (every index out of range → `CpuIndexOutOfRange`
/ `CpuIdOutOfRange`, no panic) before registration (§2.9 / §24.1):

- `percpu::PerCpuStorage<N>` — the per-CPU GDT/TSS/IST + IDT arena `percpu::init`
  finalises and `install_vector`/`install_tss_rsp0` mutate. Its payload is held
  in an `UnsafeCell` so the `static` lands in writable memory and the
  through-the-published-base writes are sound (the GDT/IDT are mutated by Rust
  and reached by `gs`-relative / computed addressing, not atomics).
- `syscall_entry::SyscallTlsStorage<N>` — the per-CPU `syscall`-entry TLS
  (`kernel_rsp0` + transient user-`rsp` save) `install_kernel_rsp0` /
  `set_kernel_rsp0` write and the `swapgs`-relative stub reaches; also
  `UnsafeCell`-backed.
- `smp::ApStackPool<N>` — the AP bootstrap-stack pool `start_secondary` computes
  each AP's stack top from (`base + (cpu - 1) * stride`, in-bounds-checked
  against the published length). Slot `idx` backs the AP with dense `CpuId`
  `idx + 1`. The per-stack 16 KiB size stays a fixed §24.4 *bound*.

(Unlike aarch64/riscv64, the x86_64 AP trampoline reads its stack top from the
per-AP boot slot the BSP stamps, so the *Rust* `start_secondary` computes it —
no assembly `.bss` reserve and no asm change.) Production `rustos-kernel` runs
single-CPU: it registers `PerCpuStorage<1>` + `SyscallTlsStorage<1>` and no AP
pool. The single-CPU x86_64 verticals register a `PerCpuStorage<1>` (when they
drive `percpu::init` directly) and size their arch handle / TLS to one slot; the
two-CPU `cross_cpu_tlb_shootdown_qemu_x86_64` registers `PerCpuStorage<2>` +
`ApStackPool<1>`, and `scheduler_stress_qemu` registers a
`PerCpuStorage<MAX_CPUS>` + `ApStackPool<MAX_CPUS - 1>` sized to its own test
capacity (the old `MAX_CPUS <= percpu::MAX_CPUS` agreement const-assert is
deleted). (wasm32 has no secondary-stack pool; its worker contexts are
host-provided.)

## Status

- **L1 — ABI (landed).** `lib/abi` `LimitKind` / `ResourceLimit` /
  `RLIMIT_INFINITY`, the `rlimit_get`/`rlimit_set` syscalls, the
  `CAP_RLIMIT_RAISE` capability, the `abi-sys` C stubs, the `lib/rt`
  wrappers, and the generated C header. The dispatcher routes both syscalls;
  the kernel handler default fails closed with `NotImplemented` until L2.
- **L2 — kernel enforcement (landed).** Per-task `LimitSet` storage in the
  address-space registry, inheritance on spawn intersected against the default
  policy (never widened), the typed-`Result` denial-and-audit path, and the
  `CAP_RLIMIT_RAISE` gate on raising a hard bound. The `rlimit_get`/`rlimit_set`
  handlers are wired in `kernel/core`.
- **L3a — discovered-hardware capacity policies (landed).** The release-tuned
  per-task kernel-stack size (`KTHREAD_STACK_BYTES`, 32 KiB release / 64 KiB
  debug) and the RAM-window-derived guard-arena policy
  (`stack_arena_bytes`, ≈1/64 of RAM clamped to `[2 MiB, 64 MiB]`,
  2 MiB-rounded). See *Discovered-hardware capacity policies* above.
- **L3b — growable arena + per-arch arrays (in progress).** The userland-heap
  free-span table is now a grow-on-demand capacity (see *Userland heap
  free-span table* above), the `kernel/sec` supplementary-group ceiling is now
  a capability-raisable capacity (see *Supplementary-group ceiling* above), the
  spawn fan-out (`MAX_SPAWNS`) is now an allocator-backed grow-on-demand
  capacity on both production producers (see *Spawn page-table capacity*
  above), the **wasm32** per-CPU handle bookkeeping is now
  discovered-count-sized, and the **riscv64**, **aarch64**, and **x86_64**
  per-CPU handle bookkeeping are now caller-provided `&'static`-slice capacities
  via `RiscvArchStorage<N>` / `Aarch64ArchStorage<N>` / `X86_64ArchStorage<N>`
  (see *Per-arch CPU/hart handle bookkeeping* above), and the kthread-stack
  arena now both **grows** *past* its policy size on genuine exhaustion (by
  chaining a fresh, independently block-split 2 MiB block from the live
  `FrameAllocator` rather than failing over to a `BoxStack`) **and shrinks**
  (returning an idle chained block through `FrameArenaShrink`, zeroed-on-free,
  with a one-free-block grace and fail-closed double-/foreign-free) — see
  *Growable and shrinkable kernel-stack arena* above; both aarch64 production
  spawn seams draw through it and reclaim on `ArenaStack` drop. The per-arch
  **secondary-bring-up** bound is now converted on **aarch64** *and*
  **riscv64** (the `.bss`/`SECONDARY_MAX_*` pool and the `MAX_CPUS` /
  `MAX_HARTS` constant are gone; the secondary stack is a caller-sized
  `SecondaryStackPool<N>` published to the `smp.s` trampoline and the timer
  slots a caller-sized `PreemptStorage<N>` — see *Per-arch secondary-bring-up
  bound* above), preserving the §17.2 break-before-make and §4 guard-page
  invariants. The same conversion is now also done on **x86_64**: the
  `percpu::MAX_CPUS` constant is gone and the per-CPU GDT/IDT/IST arena, the
  `syscall`-entry TLS, and the AP bootstrap-stack pool are caller-provided
  `PerCpuStorage<N>` / `SyscallTlsStorage<N>` / `ApStackPool<N>` storages
  (`UnsafeCell`-backed where Rust writes them), fail-closed before their
  set-once `register`. No per-arch secondary-bring-up bound remains.
- **L4a — `ulimit` shell command (landed).** The `ulimit` builtin in the
  default shell over the L1 ABI, through the injected `LimitStore` seam
  (`RtLimitStore` over `rustos_rt::rlimit_get`/`rlimit_set` in the `Run`
  binary). See *The `ulimit` shell command* above.
- **L4b — `sysinfo` limits query (landed).** The
  `SysinfoQueryId::RESOURCE_LIMITS` System Information query (§16.6) returning
  one `ResourceLimitRecord` per `LimitKind` (effective soft/hard bound + live
  usage), self-scoped and ungated; served by `sysinfod` over the per-task
  `LimitSet`, with the `sysinfo limits` command-line face. See *The `sysinfo
  limits` query* above.
