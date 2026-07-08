# Memory subsystem (`kernel/mem`)

Architecture-neutral, host-testable physical and virtual memory
management. Delivered by Stage 2.2 of `PLAN.md`. The architecture
crates (`kernel/arch/*`) supply the only piece this crate
does not implement: the real page-table writer behind the Arch HAL
page-table surface
([`PageTable`](#3-virtual-memory--page-table-operations)).

## Layered design

```text
                ┌──────────────────────────────────────────────┐
                │   sensitive — zero-on-free for credentials   │
                │   (`alloc_sensitive` / `free_sensitive`)     │
                ├──────────────────────────────────────────────┤
                │   slab — fixed-size objects + guard pages    │
                ├──────────────────────────────────────────────┤
                │   vmm — `AddressSpace<P: PageTable>`         │
                ├──────────────────────────────────────────────┤
                │   frame — buddy + bitmap physical allocator  │
                └──────────────────────────────────────────────┘
                          │
                  `BootMemoryMap` from `kernel/arch/*`
```

Every layer above depends only on the layer immediately below it; the
trait that crosses the architecture boundary lives in
[vmm](#3-virtual-memory--page-table-operations).

## 1. Physical frame allocator (`frame`)

Hybrid **buddy + bitmap**:

- A single bitmap covers the whole physical address range described by
  the [`BootMemoryMap`]. `0 = free`, `1 = allocated, reserved, or
  non-existent`. The bitmap is the source of truth for ownership, so
  every double-free or stray-free is detected and reported as
  `AllocError::InvariantViolation`.
- A `BTreeSet<usize>` per buddy order tracks the starting frame
  indices of free blocks at that order. Splits push two half-blocks
  down one order; merges pop a buddy at the same order and push the
  parent up one order. Merging consults the bitmap so it never
  reaches across a reserved region.

The allocator never panics on OOM: `alloc` / `alloc_order` return
`AllocError::OutOfMemory`. The constructor refuses overlapping or
malformed boot maps.

**Bootloader handoff.** The arch crates synthesise a
[`BootMemoryMap`] from whatever protocol the platform uses (multiboot2,
PVH, UEFI, DTB, WASM) and hand it to `FrameAllocator::new`. Reserved
regions are merged into the bitmap as "used" so they can never be
handed out; usable regions are rounded *inward* to whole-frame
boundaries.

## 2. Slab allocator with guard pages

`AGENTS.md` §4 mandates guard pages around kernel slabs. The slab's
backing buffer is laid out as

```
[ GUARD | data: object_size × slot_count | GUARD ]
```

with each guard region being exactly one architectural page.

- **On hardware:** the guards are unmapped virtual pages. An overrun
  faults loudly.
- **On the host (`cargo test`):** the guards are filled with `0xCC`
  and validated on every alloc/free and on demand via
  `Slab::check_guards`. A pattern mismatch surfaces as
  `SlabError::GuardViolation` — the same error channel as the
  hardware fault, so callers and tests are written once.

Each slab also zero-fills a slot on free, preventing leftover bytes
from leaking into the next caller. This is a cheap defence-in-depth
measure; for **credentials, keys, and capability tokens** the caller
must use the sensitive-region API below.

Zero-on-free is an *enforced* invariant, not an incidental one
(`AGENTS.md` §3.3, CWE-908/200): because `free` wipes every byte and a
fresh slab starts zeroed, a free slot is always all-zero. `Slab::alloc`
verifies this before reusing a slot and refuses one whose contents are
non-zero with `SlabError::DirtySlot`, so a skipped or corrupted wipe
fails closed rather than leaking the previous occupant's bytes to the
next caller.

## 3. Virtual memory & page-table operations

[`AddressSpace<P: PageTable>`] is the per-process virtual address
space. It owns a page-table object and serialises `map` / `unmap` /
`translate` through it.

The architecture boundary is the Arch HAL page-table surface, not a
`kernel/mem`-local trait: [`PageTable`] is merely the bound alias
`rustos_arch_api::mmu::AddressSpace + rustos_arch_api::tlb::TlbShootdown`,
so `kernel/mem` names only the HAL vocabulary (`AGENTS.md` §2.2,
`plans/WIRING.md` Stage W5b-2). The HAL surface the façade drives:

| Method | Description |
| --- | --- |
| `map_page(vaddr, paddr, flags)` | Install a 4 KiB translation. |
| `unmap(vaddr)` | Tear it down, return the physical page. |
| `translate(vaddr)` | Read-only walk. Returns `Option`, never errors. |
| `root_phys()` | Physical address of the root translation table. |
| `flush_page(vaddr)` | Per-CPU TLB invalidation (no-op on host). |

`flush_page` invalidates only the *calling* CPU. The system-wide
counterpart — invalidating a stale translation on every other online CPU
after a shared mapping is torn down — is a sibling Arch HAL slice,
`rustos_arch_api::xtlb::CrossCpuTlbShootdown`, implemented on each port's
`SchedulerArch` handle rather than on the page-table object (see
[the modularity page](./modularity.md) and `plans/WIRING.md` Stage W13).

The façade bridges its own `Page` / `Frame` / `MapFlags` currency to the
HAL's `u64` / `PageFlags` at the boundary. Each arch crate's `paging`
`AddressSpace` implements the HAL traits directly. To keep `kernel/mem`
fully host-testable the crate ships a `HostPageTable` test double behind
`#[cfg(test)]`: a `BTreeMap`-backed implementation of the same HAL
traits that, additionally, enforces W^X (rejects `WRITE | EXEC`
mappings) so the security default is exercised in tests.

`MapFlags` is a small `bitflags`-style set:
`READ | WRITE | EXEC | USER | NO_CACHE`. Architecture code translates
these into native page-table bits during Stage 3.

**Backing a port's page tables with the frame allocator
(`FrameTableSource`).** A port's `AddressSpace` is built from 4 KiB
page-table frames it draws through the Arch HAL `PageTableFrames` seam
(`rustos_arch_api::frames`). The boot/bootstrap source is the static
`PageTablePool` each port ships; the production source is
[`FrameTableSource`], which draws a physical frame from the
`FrameAllocator`, maps it to a CPU view through the direct
[`PhysMap`] (§3b), zeroes it, and hands the port a `TableFrame`
(physical address + `'static` entry view). A frame outside the direct
map is returned to the allocator and the request fails closed
(`AGENTS.md` §2.9), never synthesising a pointer. This keeps the §17.4
one-way edge intact — `kernel/arch/*` names only the HAL trait, never
`kernel/mem` — while a real per-process address space's tables come from
ordinary reclaimable RAM rather than a fixed `.bss` pool
(`plans/WIRING.md` Stage W5b-3). Host tests run the HAL
`frames::conformance` suite over `FrameTableSource` and assert each
table is drawn from the allocator, zeroed, and distinct.

The runtime `spawn` producers (aarch64 and x86_64) are the first
production consumers: each builds a spawned child's page tables over a
boot-cached `FrameTableSource` rather than a fixed `.bss` `PageTablePool`
reserve, so the number of processes that can be spawned scales with
discovered RAM and grows on demand instead of being a hard `const`
ceiling (`AGENTS.md` §24.1; see
[the resource-limits page](./resource-limits.md)). The source is shared
across CPUs, so its direct-map handle is `Sync`.

**Reclaiming a dead process's whole footprint (`plans/APPS.md` I2).**
The seam is symmetric: `PageTableFrames::free_table` is the teardown
half, and a task's exit returns everything it owned. The retained
per-task `LiveSpace` (the object the `mem_map` / `mmio_map` / `dma_alloc`
syscalls mutate) is owned by the task's kernel-thread control block and
dropped when the scheduler reaps the exited task; its `Drop` (1) drains
every live DMA carve (zero-on-free, frames back to the allocator),
(2) walks every remaining tracked mapping — a page inside the
device-window or shared-memory window is only *unmapped* (its frames
belong to a device or to the shared-region registry), while every other
page (image segments, user stack, startup block, anonymous heap) is
unmapped, its frame **zeroed** through the direct map so a dead
process's bytes are never recycled readable, and freed — and (3) hands
the page-table hierarchy itself back post-order (children before
parents, the root last) through the one shared
`rustos_arch_api::frames::reclaim_hierarchy` walk each port's
`reclaim_table_frames` drives, so every stage-1 table frame returns to
the `FrameTableSource` for reuse. Teardown is SMP-safe by an invariant
the dispatcher maintains: after every switch-back from a user task the
CPU re-parks its translation on the permanent boot root (published
set-once at boot; the port's `park_kernel_root`), so a user root is
active on a CPU only while its task runs there and a dead root can never
be freed under a live walk (the port's reclaim additionally re-parks
defensively if the calling CPU still holds the dying root, and retires
the frames unreclaimed rather than dismantling an active translation —
fail closed). Host tests pin the whole discipline: the `LiveSpace` drop
test and the `spawn_image` spawn/exit-cycle test assert
`free_frames` returns exactly to its pre-spawn value (registry-owned
shared frames excluded), and the aarch64/riscv64 paging tests assert the
walk returns every drawn table exactly once, root last, leaves never.

## 3a. User-memory copy (`uaccess`)

A syscall handler is handed a raw user pointer (`ptr`, `len`) and must
move bytes to or from that buffer without ever trusting it — the
`copy_from_user` / `copy_to_user` boundary (`AGENTS.md` §5.4,
`tests/SECURITY.md` §5). The [`uaccess`] module is the
architecture-neutral half of that boundary: [`copy_in`] reads from the
caller's address space into a kernel slice, [`copy_out`] writes a kernel
slice into it. Both take the address space as a `&dyn UserAddressSpace`
(the read-only `translate` view, §3b) and compose the two layers above —
that one `translate` operation and the `PhysMap` direct map — so the
copy path walks any task's page-table backend behind a single
non-generic call site, with one validated traversal and never a second
pointer-walk implementation (`AGENTS.md` §2.2).

The copy walks the user range **one page at a time** (user memory is
contiguous in the virtual address space but its frames need not be
contiguous in physical RAM): for each `[page_start, page_start +
PAGE_SIZE)` window the range touches it `translate`s the page to its
`(Frame, MapFlags)`, turns the in-page physical span into a CPU pointer
through the `PhysMap`, and moves only the bytes of the caller's buffer
that fall inside that page. The first page may begin at a non-zero
offset and the last may end before the page boundary.

Every page is checked, fail-closed, before a byte moves:

| Reason | `UaccessError` |
| --- | --- |
| Null base on a non-empty copy | `Null` |
| `ptr + len` overflows the address space | `LengthOverflow` |
| A page in range is unmapped | `NotMapped` |
| A page in range is not user-accessible (kernel-pointer confusion) | `NotUser` |
| `copy_in` page lacks `READ` | `NotReadable` |
| `copy_out` page lacks `WRITE` (read-only / executable — the §19.2 W^X guard) | `NotWritable` |
| Backing frame outside the direct map | `PhysUnmapped` |

A page missing `USER` is rejected **before** a missing data permission,
so a kernel-pointer-confusion attempt is never downgraded to a mere "not
readable". A zero-length copy touches no memory and succeeds for any
base. The two entry points carry one encapsulated `unsafe` block each
(the in-page `core::ptr::copy`), with a `// SAFETY:` rationale and full
host-test coverage (`AGENTS.md` §2.10): cross-page, mid-page-offset,
round-trip, and every fail-closed branch are exercised with
`HostPageTable` + `SimPhysMap`.

This module is the foundational primitive of the staged user-memory
work: the per-task address-space registry, the syscall wiring of
`ipc_send` / `ipc_recv` / `cap_delegate` / `random_get`, and the
per-architecture page-fault fix-up all build on it (see `PLAN.md`).

[`uaccess`]: ../../rustos_kernel_mem/uaccess/index.html
[`copy_in`]: ../../rustos_kernel_mem/uaccess/fn.copy_in.html
[`copy_out`]: ../../rustos_kernel_mem/uaccess/fn.copy_out.html

## 3b. Per-task address-space registry

`copy_in` / `copy_out` take a `&dyn UserAddressSpace` (so the call site
names no concrete page-table backend), but a syscall handler only knows
the caller's `TaskId`. The bridge is the **address-space
registry** (`kernel/core`, `aspace` module): a
`BTreeMap<TaskId, (address space, PhysMap)>` that the syscall path reads
to resolve the calling task to the pair the copy walk consumes. It is
composed into `KernelState` next to the capability and IPC registries,
wrapped in the same reader-preferring `RwLock`, so the hot path takes
only a shared lock (`AGENTS.md` §2.1).

`AddressSpace<P>` is generic over its page-table backend `P`, so the
kernel cannot key one map on a single concrete `AddressSpace<P>` —
different tasks may run on different architecture page tables. The
registry therefore stores each entry behind [`UserAddressSpace`], an
object-safe, **read-only** view that exposes only `translate` (a blanket
impl forwards it to `AddressSpace::translate`, so there is one
translation definition, not two — `AGENTS.md` §2.2). Exposing only
`translate` keeps the copy path from ever mutating a caller's mappings;
map / unmap stay behind `AddressSpace`'s own accounted API (`AGENTS.md`
§2.4). The physical map is erased the same way the rest of the kernel
already erases it (`&dyn PhysMap`), so the registry is one concrete,
non-generic type.

Lifecycle is fail-closed: an entry is registered when a task's image is
mapped and withdrawn when it exits. Registering an id that is already
present is **refused** (`AspaceError::AlreadyPresent`) rather than
silently replacing a live mapping, withdrawal is idempotent, and
resolving an unknown id yields `None`. The registry is a pure data
structure with no audit sink of its own — the spawner and the `exit`
handler that drive the lifecycle own the security-relevant logging,
exactly as the syscall dispatcher (not the IPC `PortRegistry`) audits
endpoint lookups.

The registry is reached from a syscall handler through
`KernelSyscallHandlers::with_caller_aspace(caller, f)` (increment C of
the staged copy path): it is threaded into the
`KernelDispatchHook` / `KernelSyscallHandlers` borrows next to `caps`
and `ipc`, takes a read guard, resolves `caller.task_id`, and runs `f`
with the borrowed `(&dyn UserAddressSpace, &dyn PhysMap)` pair — the
guard living exactly as long as the borrowed references — failing
closed to `None` when the caller has no registered space. Keeping the
accessor in `kernel/core` is deliberate: the decoupled dispatcher
(`kernel/syscall`) reaches user memory without ever depending on
`kernel/mem` (`AGENTS.md` §17.4). The handler-side copies that consume
it (`ipc_send` / `ipc_recv` / `cap_delegate` / `random_get` through
`copy_in` / `copy_out`) are increment D, now **fully landed** (D.1–D.4;
`random_get` draws from the `rustos_rng::OutputReserve` composed into
`KernelState` and copies it out, see `PLAN.md`). Because the
copy entry points already accept `&dyn UserAddressSpace`, the pair that
`with_caller_aspace` yields drives them directly, with no concrete
`AddressSpace<P>` re-erasure at the boundary.

[`UserAddressSpace`]: ../../rustos_kernel_mem/vmm/trait.UserAddressSpace.html

## 4. Sensitive-region API

`alloc_sensitive(len) -> Result<SensitiveBuffer, AllocError>` hands
back a fixed-size byte buffer that **zeroes itself on drop**, using
the audited [`zeroize`](https://crates.io/crates/zeroize) crate (no
hand-rolled crypto per `AGENTS.md` §6). `free_sensitive(buf)` is a
named drop equivalent provided for documentation symmetry.

`SensitiveBuffer` is fixed-size (`Box<[u8]>`, not `Vec<u8>`) to avoid
silent reallocations that would leak a secret into the old
allocation. Its `Debug` impl deliberately redacts the contents.

## 5. DMA buffers

User-space drivers (`drivers/storage/virtio_blk`, `drivers/network/virtio_net`,
future NVMe / GPU bus-master devices) need page-aligned, contiguous-by-physical
buffers that a device can address directly. The
[`DmaPool<P>`][DmaPool] ships that facility,
composing the layers above:

- **Physical contiguity** — frames are taken from the buddy allocator at a
  single buddy order; the buffer is therefore physically contiguous up to
  `MAX_ORDER` pages.
- **Per-process virtual window** — the pool owns a slice of one process's
  `AddressSpace<P>`. Each allocation maps `data_pages` consecutive pages
  with `READ | WRITE | USER`; no `EXEC`, no global sharing.
- **Guard pages** — every allocation is bracketed by one *unmapped* virtual
  page on each side, so an overrun faults on the MMU rather than reaching a
  neighbouring allocation.
- **CPU access via the direct map** — the bytes the driver reads/writes are
  the buffer's *physical frames*, reached through the kernel direct physical
  map (`PhysMap`): `bytes` / `bytes_mut` / `slot_base` translate the buffer's
  `phys` into a pointer. The CPU therefore sees exactly the frames the device
  DMAs to — there is no disconnected copy. Production wires a `DirectPhysMap`
  (the boot identity map over low physical memory); host tests wire a
  `SimPhysMap` standing in for physical RAM.
- **Zero-on-free** — every byte of the data region is wiped with
  [`zeroize`](https://crates.io/crates/zeroize) before the frames return to
  the buddy allocator. A later allocation that lands on the same slot sees
  zeros; a forensic read of free physical memory cannot recover the
  credentials, keys, or capability tokens the buffer once held
  (`AGENTS.md` §4).
- **Bounded failure** — `alloc` / `free` return `Result<_, DmaError>`. No
  panic on resource exhaustion, no `expect` on hot paths, no `unsafe` leaks
  across the crate boundary. Allocation requests larger than `MAX_ORDER`
  return `DmaError::SizeUnsupported`; exhaustion of either the virtual
  window or the frame allocator returns `DmaError::Alloc(OutOfMemory)`.

```text
[ guard | data_0 | data_1 | … | data_{n-1} | guard ]
   |       └────────── mapped (R/W/U) ──────────┘   |
   └──────────────── unmapped (fault) ──────────────┘
```

The data frames are reached by the CPU through the direct physical map
(`phys`), keyed on each buffer's `phys` address, so the driver's view
and the device's view are the same memory.

The pool itself is **capability-agnostic**: it does not consult the calling
task's capability set. The capability gate is the companion module
`kernel/sec::dma`, whose `alloc_dma` / `free_dma` entry points verify
[`CapabilityId::MEM_DMA`][CapabilityId::MEM_DMA]
before dispatching to the pool, and emit
[`AuditEvent::DmaAllocated`] / [`AuditEvent::DmaAllocDenied`] records on
every decision (IDs 1030 / 1031, see [Security audit catalogue](./security.md)).
A future syscall wrapper maps gate failures to `Errno` via
`DmaGateError::as_errno`:

| Gate error | `Errno` |
| --- | --- |
| `CapabilityMissing` | `PermissionDenied` |
| `Pool(ZeroSize)` | `BufferTooSmall` |
| `Pool(Alloc)` / `Pool(SizeUnsupported)` | `LengthOutOfRange` |
| Other internal pool failures | `OutOfRange` |

[`AuditEvent::DmaAllocated`]: ../../rustos_kernel_sec/enum.AuditEvent.html#variant.DmaAllocated
[`AuditEvent::DmaAllocDenied`]: ../../rustos_kernel_sec/enum.AuditEvent.html#variant.DmaAllocDenied
[DmaPool]: ../../rustos_kernel_mem/dma/struct.DmaPool.html
[DmaWindowMap]: ../../rustos_kernel_mem/dma/struct.DmaWindowMap.html
[CapabilityId::MEM_DMA]: ../../rustos_abi/capability/struct.CapabilityId.html#associatedconstant.MEM_DMA

The guarded carve itself lives in the borrowed-space
[`DmaWindowMap`][DmaWindowMap] (its virtual-window base, slot bitmap, and
live-allocation records); [`DmaPool`][DmaPool] is the thin owning adapter
over a space it owns outright, exactly as `MmioMap` wraps `MmioWindowMap`
(§5.2) — there is one carve definition (`AGENTS.md` §2.2). The retained
per-task live address space (`LiveSpace`, §7e, the `dma_alloc` syscall
path) drives the *same* `DmaWindowMap` against the
space it owns and lends, adding an `addr_limit` bound (the granted device
DMA constraint, §18.3): a contiguous block that would reach at or above the
limit is returned to the allocator and the carve refused
(`DmaError::AddrLimitExceeded`). `LiveSpace` reclaims (zeroes and frees)
every live DMA block when it is dropped on task exit, so a driver's exit
leaks no frames and leaves no secret-bearing buffer recoverable
(`AGENTS.md` §4).

### 5.1 Slab hand-off to user-space drivers

The user-space virtio driver crates carry an owned
`DmaSlab { phys, ptr: NonNull<u8>, len, pool_id, slot, /* erased
free shim */ }` rather than borrowing the pool on every accessor
(Stage 4.D Item 0a). The pool exposes a single companion accessor,
`slot_base`, which takes a `&DmaBuffer` and returns
`Result<NonNull<u8>, DmaError>`, handing out the base pointer of the
buffer's data slots. The disjointness witness is the pool's slot
bitmap (one slot ↔ one
allocation); the slab carries `(pool_id, slot, len)` so its drop
can invoke a type-erased free shim that returns the slot to the
pool. See [Virtio transport — DMA ownership model](../drivers/virtio.md#dma-ownership-model)
for the consumer-side view. The kernel-side wiring of a
`KernelVirtioHost` that builds slabs from `alloc_dma` is the
subject of Stage 4.D Item 0.

### 5.2 MMIO register-window mapper

Device drivers also need their *register block* mapped — a PCI memory
BAR or a virtio-MMIO transport slot. The guarded-mapping mechanism is
`kernel/mem::mmio::MmioWindowMap`: the per-task bookkeeping (a bounded
virtual window, a slot bitmap, and the per-region guard/data accounting)
that maps a device window into a **borrowed** `&mut AddressSpace<P>`.
Unlike [`DmaPool`][DmaPool] it allocates **no** frames — the physical
address is fixed by the hardware, so it maps the *device's own* frames
with caching disabled (`MapFlags::NO_CACHE`), never executable (W^X,
`AGENTS.md` §19.2), and the same unmapped guard-page bracketing the DMA
pool uses; a part-way page-table failure unwinds every page it added
(all-or-nothing, `AGENTS.md` §2.9). `MmioWindowMap::map_into(space,
phys_base, len)` returns an `MmioRegion`; `region_base(region, phys)`
resolves the region's device physical base through the direct physical
map (`PhysMap`) into a base pointer. It is the device-window analogue of
[`map_anonymous`](#7c-anonymous-user-memory-mem_map--mem_unmap):
an architecture-neutral mechanism over a borrowed live address space,
shared without duplication (`AGENTS.md` §2.2) by two consumers — the
owning adapter `MmioMap`, which bundles `MmioWindowMap` with an
`AddressSpace<P>` it owns so the kernel-side mapper (`KernelMmioMapper`,
in `kernel/virtio`) turns an `MmioRegion` into an
[ABI `RegisterWindow`](../drivers/bus.md#register-window-hand-off) for
the in-kernel driver host, and the `mmio_map` syscall facility (`plans/PI.md`
P10 chunk 5d-0), which maps a granted device window into the caller's
*own running* address space (the production wiring of that facility, over
a retained live address space, is staged with the arch-level live-space
retention).

The mapper is **capability-agnostic**; the gate is
`kernel/sec::mmio`, whose `map_mmio` / `unmap_mmio` verify
[`CapabilityId::MMIO_MAP`][CapabilityId::MMIO_MAP] and emit
`MmioMapped` / `MmioMapDenied` audit records (IDs 1040 / 1041, see
[Security audit catalogue](./security.md)). `MmioGateError::as_errno`
maps refusals to `Errno` exactly as the DMA gate does.

[CapabilityId::MMIO_MAP]: ../../rustos_abi/capability/struct.CapabilityId.html#associatedconstant.MMIO_MAP

## 6. Result-returning OOM contract

Every fallible operation in this crate returns
`Result<_, AllocError>`. No path panics on out-of-memory
(`AGENTS.md` §4). The error variants:

| Variant | Meaning |
| --- | --- |
| `OutOfMemory` | No free block of the requested size. |
| `SizeUnsupported` | Request exceeds the allocator's capacity / `MAX_ORDER`. |
| `ZeroSize` | Zero-byte / zero-slot requests are rejected. |
| `OutOfRange` | Frame / address outside the allocator's window. |
| `MetadataAllocFailed` | Allocator could not bootstrap itself. |
| `InvariantViolation` | Double-free, free of a reserved frame, malformed boot map. |

## 7. Unsafe & pointer arithmetic discipline

Per `AGENTS.md` §4, raw pointer arithmetic is confined to the
`ptr` module's bounds-checked helpers (`offset_within`,
`end_within`, `slice_within`). Every other module routes pointer
math through them. Every `unsafe` block carries a `// SAFETY:`
rationale per `AGENTS.md` §2.10, encapsulated behind a safe public
API; no `unsafe` leaks across crate boundaries.

## 7a. Platform memory-map sources

The `BootMemoryMap` is *fed* by the architecture port, not constructed
by `kernel/mem`. On x86_64 (Stage 3a (a)) the discovery surface lives
in `kernel/arch/x86_64`:

- `multiboot2` parses the BIOS-derived memory-map tag (Multiboot2
  type 6) and the EFI memory-map tag (type 17) a GRUB-EFI boot hands
  in; `pvh` parses the E820-style `hvm_start_info` memory-map table a
  QEMU PVH direct boot hands in. All three parsers are zero-copy and
  `no_alloc`.
- `bootmemory` bridges those typed entries into
  `MemoryRegionDescriptor`s with a `RegionKind` mirror that is locked
  to `rustos_kernel_mem::RegionKind` by a host-side dev-dep
  round-trip test (`AGENTS.md` §2.2 — no duplication).

The kernel binary (which links against `kernel/mem`) is responsible
for draining the descriptor stream into a `BootMemoryMap` via
`BootMemoryMap::push`. This split keeps `kernel/arch/x86_64` free of
`alloc` so it can be linked into the freestanding Stage-2 QEMU test
binaries that do not yet provide a `#[global_allocator]`.

## 7b. Encrypted swap (`swap`)

When a pager writes a page of anonymous, stack, or capability-bearing
memory out to a backing store, that page leaves RAM — and the
zero-on-free guarantees of [§4](#4-sensitive-region-api) and
[§5](#5-dma-buffers) would be void if the bytes could be read back off
an unencrypted swap device. The `swap` module closes that gap: every
page is sealed with `lib/crypto`'s ChaCha20-Poly1305 AEAD before it
reaches the device (`AGENTS.md` §4).

**Fail-closed by construction.** `AGENTS.md` §4 requires that the kernel
"refuses to activate a swap device that is not wrapped by the
encrypted-swap layer". RustOS enforces this in the type system rather
than with a runtime flag: a [`SwapBackend`] (the raw, slot-addressed
device) exposes only opaque [`SWAP_RECORD_LEN`]-byte records and makes
no cryptographic decision, and the **only** way to read or write a page
through it is [`EncryptedSwap`], whose sole constructor
[`EncryptedSwap::activate`] takes a [`SwapKey`]. There is no plaintext
code path to fall back to, so plaintext swap is unrepresentable
(`AGENTS.md` §2.11).

**Ephemeral per-boot key.** The [`SwapKey`] is drawn from the platform
RNG (the §19.2 entropy source, injected as the [`EntropySource`] seam
until that subsystem lands), zeroed on drop, and never persisted: there
is no serialisation path and no accessor that copies the key out of the
crate. A power cycle destroys the key, so paged-out secrets cannot be
recovered at rest.

**Record layout & nonce discipline.** Each on-device record is
`nonce(12) ‖ tag(16) ‖ ciphertext(4096)`. ChaCha20-Poly1305 fails
catastrophically on `(key, nonce)` reuse, so each `EncryptedSwap`
draws a random 32-bit salt at activation and appends a 64-bit
monotonic counter; counter exhaustion fails closed
([`SwapError::NonceExhausted`]) rather than wrapping. The slot index is
bound as associated data, so a record relocated to a different slot
fails authentication. On any failure — bad slot, backend fault, or a
forged/tampered record — `load` zeroes the caller's buffer before
returning the error, so a caller can never observe stale or forged
plaintext (`AGENTS.md` §5.4).

The pager that calls `store` / `load`, and the capability gate on
*activating* a swap device, are Stage 8 work; this module is the
cryptographic layer they are required to route through.

[`SwapBackend`]: ../../rustos_kernel_mem/swap/trait.SwapBackend.html
[`EncryptedSwap`]: ../../rustos_kernel_mem/swap/struct.EncryptedSwap.html
[`EncryptedSwap::activate`]: ../../rustos_kernel_mem/swap/struct.EncryptedSwap.html#method.activate
[`SwapKey`]: ../../rustos_kernel_mem/swap/struct.SwapKey.html
[`EntropySource`]: ../../rustos_kernel_mem/swap/trait.EntropySource.html
[`SwapError::NonceExhausted`]: ../../rustos_kernel_mem/swap/enum.SwapError.html#variant.NonceExhausted
[`SWAP_RECORD_LEN`]: ../../rustos_kernel_mem/swap/constant.SWAP_RECORD_LEN.html

## 7c. Anonymous user memory (`mem_map` / `mem_unmap`)

A spawned process boots with exactly its fixed spawn-time image
(code/data/bss plus a fixed user stack, `plans/SPAWN.md` SP2/SP3). The
`mem_map` (`abi-v1` no. 14) / `mem_unmap` (no. 15) syscalls are the one
mechanism by which a process obtains and releases *additional* memory at
runtime — the foundation the `lib/rt` userland heap allocator (§7d) layers its
`malloc` / `free` over. The ABI shape is fixed in `rustos_abi::memory`
(`MapFlags`) and `rustos_abi::syscall`; the syscall-layer contract is the
`mem_map` / `mem_unmap` rows of [the syscall page](./syscalls.md).

This is staged (`plans/SPAWN.md` SP5):

- **SP5a (landed).** The `abi-v1` surface (`MapFlags`, the two syscall
  numbers, the `Errno::OutOfMemory` variant), the C-callable stubs
  (`ros_sys_mem_map` / `ros_sys_mem_unmap`) and generated header
  (`include/rustos/rustos_memory.h`), the dispatcher arms, and an
  arch-neutral fail-closed seam in `kernel/core` (`MemMap`, defaulting to
  `NULL_MEM_MAP` → `Errno::NotImplemented`, installed through
  `KernelSyscallHandlers::with_mem_map`, mirroring the console and spawn
  seams). The handler rejects a zero `len` with `LengthOutOfRange` and a
  reserved flag bit with `OutOfRange` before reaching the producer.
- **SP5b-1 (landed).** The reusable, architecture-neutral `kernel/mem`
  producer (`map_anonymous` / `unmap_anonymous` in the `anon` module) that
  mutates a *live* user address space: it maps fresh frames into the
  caller's own [`AddressSpace<P: PageTable>`] as `RW|USER` (the single
  `ANON_FLAGS` set, never executable), zeroes each frame through the kernel
  direct map *before* the mapping is visible, and on unmap validates the
  whole range is mapped before zeroing-on-free and releasing every frame. A
  frame exhaustion part-way through a map unwinds every page it already
  added, so a failed map leaves the space unchanged (`AGENTS.md` §2.9). The
  per-page TLB invalidation rides the existing `AddressSpace::map` /
  `AddressSpace::unmap` flush (the §17.2 `TlbShootdown` slice); the
  cross-CPU shootdown is part of SP5b-2 when the producer is driven from a
  live multi-CPU regime. Host-proven over `HostPageTable` + `SimPhysMap`.
- **SP5b-2 (landed).** The aarch64 EL0 `-M virt` vertical
  (`tests/integration/mem_map_qemu_aarch64`) wires the SP5b-1 producer
  through the `kernel/core` `MemMap` seam: it builds one isolated EL0 space
  with `spawn_image`, **retains** it live behind a `MemMap` producer backed
  by `map_anonymous` / `unmap_anonymous`, and routes the program's
  `mem_map` / `mem_unmap` `svc`s through it. A pure-Rust EL0 fixture
  (`tests/integration/mem_map_program`) `mem_map`s a region (FIXED), writes
  and reads back a pattern (proving the pages are real `RW` memory),
  `mem_unmap`s it, then touches the released range — the data abort the
  fault handler reports as PASS. The `rustos_rt::mem_map` / `mem_unmap`
  wrappers are the program's interface. The **riscv64 sibling**
  (`tests/integration/mem_map_qemu_riscv64`) is now landed too: it reuses the
  same pure-Rust `mem_map_program` fixture and the same `kernel/mem` producer
  over an Sv39 U-mode space, drops into the program through `spawn_image` + a
  direct `EnterUser::enter_user` (no scheduler — a single task only
  direct-returns from its `ecall`s, so the cooperative-switch trap-save path is
  off the critical path), and reports the use-after-unmap page fault as PASS on
  `-M virt`. The x86_64 sibling and the production per-task live-space retention
  follow; wasm32's linear-memory model is an honest n/a.

The binding invariants the producer must honour (settled here as the SP5
design note, `AGENTS.md` §15.2):

- **W^X, `RW` only (`AGENTS.md` §19.2).** A region is always readable and
  writable and **never** executable; `mem_map` never produces an `RWX`
  mapping. An executable (JIT) mapping is a separate, later
  `CAP_JIT_MAP_EXEC`-gated `RW`→`RX` flip and is explicitly **not** part of
  SP5 — `mem_map` does not add an `mprotect`-equivalent.
- **Per-process, never global (`AGENTS.md` §4).** A region is mapped only
  into the **caller's own** hardware-isolated address space. There is no
  global user heap and no cross-process mapping; shared memory stays the
  capability-checked IPC object. Because it only ever grows the caller's
  own space, the pair is unprivileged (no capability, `AGENTS.md` §16.6).
- **Zero on map and on free (`AGENTS.md` §4 — secret hygiene).** Pages are
  zeroed before the mapping is visible — no stale kernel or other-process
  bytes — and the frames `mem_unmap` reclaims are zeroed on free, the same
  guarantee [§4](#4-sensitive-region-api) and [§5](#5-dma-buffers) give
  the rest of the crate.
- **Deterministic OOM (`AGENTS.md` §4 / §2.9).** A frame- or
  page-table-allocation failure surfaces as `Errno::OutOfMemory`, never a
  panic — the user-facing projection of the
  [§6 result-returning OOM contract](#6-result-returning-oom-contract).
  There is no per-process quota; a process is bounded only by the physical
  frames available.

The immutable-`FrozenAddressSpace` snapshot the post-spawn registry stores
(§3b) is read-only; the production `mem_map` / `mmio_map` producers instead
mutate a task's **retained live** address space, the single live-space
mutation path (§7e) rather than a second parallel address-space model
(`AGENTS.md` §2.2).

## 7d. Userland heap allocator (`rustos-rt`)

The `mem_map` / `mem_unmap` pair is a page-granularity primitive; ordinary
`alloc` types (`Box`, `Vec`, `String`) need a byte-granularity `malloc` /
`free`. `lib/rt` supplies it as a `#[global_allocator]` (`lib/rt/src/heap.rs`),
so a first-party Rust program (the shell, `init`) can use `alloc`. It is a
userland allocator — outside `kernel/mem` — but is documented here because it
is the consumer the `mem_map` ABI exists for (§7c).

- **Free-span allocator over a growable, fixed-base arena.** The heap owns one
  contiguous virtual arena that starts at a fixed base and grows upward, one or
  more whole pages at a time, by `mem_map`ping with `MapFlags::FIXED` at the
  arena's current top. Freed regions are tracked as a coalesced,
  address-sorted free list held *inside the allocator* (a fixed-capacity span
  table), not as intrusive links in user memory, so the bookkeeping never
  dereferences freed memory and every returned pointer is range-checked before
  it is handed out (`AGENTS.md` §4 — no `unsafe` allocator doing raw pointer
  arithmetic without a checked wrapper).
- **Real free, with shrink.** Allocation is first-fit honouring the requested
  alignment, returning alignment padding to the free list; free coalesces with
  neighbours, and when whole trailing pages become free at the arena top they
  are returned to the kernel with `mem_unmap` — both syscalls are genuinely
  exercised, no dead path (`AGENTS.md` §2.14).
- **Deterministic OOM (`AGENTS.md` §4 / §2.9).** A failed `mem_map` or an
  overflowed span table returns a null pointer per the `GlobalAlloc` contract,
  never a panic.
- **No re-zero on free (`AGENTS.md` §2.16).** The kernel already zeroes pages
  on map and on free (§7c), so memory entering or leaving the process is clean;
  a process reusing its own freed bytes within its own heap is not a security
  boundary, so the heap does not re-zero on the hot path.

The pure free-span bookkeeping is host-unit-tested over a fake pager; the
aarch64 `-M virt` vertical `tests/integration/heap_qemu_aarch64` proves it end
to end — a pure-Rust EL0 fixture (`tests/integration/heap_program`)
Box-allocates, grows a `Vec` across several pages, reallocates after freeing,
verifies every value, and exits 0, with the program's allocator-issued
`mem_map` / `mem_unmap` `svc`s routed through the live `MemMap` producer
(`plans/PI.md` P6e-3b prerequisite).

## 7e. Retained live address space (`live`) and the production producers

The post-spawn registry holds a read-only `FrozenAddressSpace` snapshot (§3b)
for the user-memory copy path, but `mem_map` / `mmio_map` must mutate the
*running* space — grow a process's heap, or map a driver's granted device
window into its own space. A live arch `AddressSpace<P>` cannot sit behind
the registry's `Send + Sync` shared lock (the production page-table backend
is `!Send`/`!Sync`), so the live space is retained **per task and reached
only by the CPU currently running it**, never a global lock over a live page
table (`plans/PI.md` 5d-0-ii (b′)).

- **`kernel/mem::live` — the object-safe boundary.** `LiveUserSpace` is a
  `Send` object-safe trait (`map_anonymous` / `unmap_anonymous` /
  `map_device_window`); the generic `LiveSpace<P, M>` implements it by
  composing the audited `map_anonymous` / `unmap_anonymous` (§7c) and the
  `MmioWindowMap` device-window allocator (§5.2) — there is exactly one
  mapping path for each (`AGENTS.md` §2.2). Erasing the space behind the
  trait keeps `kernel/core` free of any concrete page-table backend `P`
  (`AGENTS.md` §17.4). `LiveSpaceError` unions the two mechanisms' errors.
- **Per-task ownership + per-CPU publication.** `kernel/core::kthread` owns
  the boxed live space in the task's `ThreadControl` (so it — and its
  page-table frames — is reclaimed when the task exits). A new per-CPU
  `USER_LIVE_SPACE` table publishes a pointer to it immediately before the
  task is switched in and clears it the instant the task switches back —
  the exact lifecycle as the `USER_RESUME` reschedule handle — so the slot
  is populated only while that CPU runs the (now trapped) task. The
  `with_current_live_space(cpu, f)` accessor hands a producer an exclusive
  `&mut dyn LiveUserSpace` that cannot alias: the task is suspended in its
  own syscall trap for the whole call, and a task runs on at most one CPU
  (`AGENTS.md` §4 — the access is genuinely exclusive). The
  `spawn_user_kthread_with_stack_live` admission entry carries the space.
- **The production producers.** `kernel/core::live_producer` provides
  `LiveMemMap<A>` (`MemMap`) and `LiveMmioMap<A>` (`MmioMapFacility`); each
  holds a `&'static A` (mirroring `KernelProcessWait`), reads
  `arch.current_cpu()`, routes through `with_current_live_space`, folds
  `LiveSpaceError` onto a stable `Errno`, and **fails closed**
  (`NotImplemented`) when the running task has no retained space
  (`AGENTS.md` §2.9 / §5.4 — it never touches another task's memory).
  `mmio_map` is fully served (the guarded `MmioWindowMap` chooses the user
  virtual window); anonymous `mem_map` is fully served for both `FIXED`
  placement (the caller names `addr_hint`) and **non-`FIXED`** placement
  (the kernel chooses the base out of the per-task heap window via
  `LiveSpace::map_anonymous_placed`, §7f) — never a guessed base.

The retention is wired into the **aarch64** spawn path (`plans/PI.md`
5d-0-ii (b′)-2): the live space threads through the `admit_init` /
`admit_process` seam as `Option<Box<dyn LiveUserSpace + Send>>` (the x86_64 /
riscv64 ports pass `None` until their turn), the aarch64 `init_spawn` /
`spawn_producer` freeze a snapshot for the copy path **and** retain a
`LiveSpace` built from the same arch space, admitting through
`spawn_user_kthread_with_stack_live`, and `kernel_main` installs `LiveMemMap` /
`LiveMmioMap` for every port (a port that retains no live space simply fails
those syscalls closed). A device window a user-space driver maps through
`mmio_map` is given the EL0-accessible device leaf
(`kernel/arch/aarch64::el0_device_leaf_attrs`, `AP_RW_EL0`) so the driver can
read its own register without a permission fault (§5.2). The aarch64
`mmio_map_qemu_aarch64` `-M virt` vertical proves the chain end to end (a
spawned EL0 program maps a minted virtio-MMIO window grant, reads its
`MagicValue` register, **and** round-trips a non-`FIXED` `mem_map`: map →
write a sentinel → read it back → `mem_unmap`). The `dma_alloc` DMA half is
the remaining staged 5d-0-ii (c) follow-on.

## 7f. Non-`FIXED` `mem_map` placement allocator (`anon_window`)

A non-`FIXED` `mem_map` asks the kernel to choose the base. That placement
decision is `kernel/mem::AnonWindowMap`: a per-task user-virtual-address
allocator over one configured heap window, driven against a borrowed live
`AddressSpace<P>` by `LiveSpace::map_anonymous_placed` (`plans/PI.md`
5d-0-ii (c)).

- **Placement only.** It allocates and releases page-aligned virtual ranges;
  the actual mapping is the audited `map_anonymous` (§7c) — one mapping path
  (`AGENTS.md` §2.2). `LiveSpace::map_anonymous_placed` reserves a base, maps
  it, and releases the reservation on a mapping failure (so a failed call
  consumes no address space); `unmap_anonymous` validates the placement
  record and releases its range, failing closed on a wrong base/extent before
  any teardown (§5.4).
- **Bump cursor + free-list, §24.1-scalable.** A bump cursor serves fresh
  ranges and a free-list of released holes (first-fit, split on a partial
  match) serves reuse, so the allocator's own memory is bounded by the
  live-plus-freed region count, never the page count of the window. The
  window is *address space*, not a physical resource, and its size is
  **derived from discovered RAM, never a hard-wired constant** (`AGENTS.md`
  §24.1): each port places it as the **topmost** user region (4 GiB above
  the image bias, `spawn_layout::ANON_WINDOW_OFFSET`, above the device, DMA,
  and shared-memory windows) so it has room to grow, and sizes it through
  `anon_layout::anon_window_pages(total_frames, base, USER_VA_TOP)` — the
  size of physical RAM (the true upper bound on backable pages), clamped to
  the addressable user VA above the base and floored at
  `ANON_WINDOW_MIN_PAGES` (16 MiB) for a tiny machine. A 1 GiB machine gets
  the same 1 GiB window the former fixed constant gave; a large server
  scales up instead of being capped at 1 GiB. The window costs no RAM until
  the frame allocator backs a mapping — and that backing fails closed as a
  deterministic OOM (§4), so a 20 GiB request on a 1 GiB machine is refused
  (at the virtual reservation if it exceeds the window, else at frame
  exhaustion), never over-committed.
- **Tested.** `AnonWindowMap` host-unit tests (bump/no-overlap, exhaustion,
  release+reuse, fail-closed release), `LiveSpace` placement tests (real
  `HostPageTable` map + zero-on-map + reuse + fail-closed wrong-extent
  unmap), the `LiveMemMap` routing test, and the extended
  `mmio_map_qemu_aarch64` `-M virt` vertical's `mem_map` round-trip.

## 7g. Reclaimable-memory model (`reclaim`) and the filesystem cache

`kernel/mem::reclaim` is the one definition of how a reclaimable cache —
memory holding *derived* state that can always be rebuilt from its
canonical source — is classed, bounded, and accounted
(`plans/SMARTRAM.md`):

- **Classes.** Each entry belongs to one `ReclaimClass` with a
  deterministic `reclaim_priority` following the `plans/SMARTRAM.md`
  section 7 pressure order (first reclaimed first): `DisposableUi`,
  `PredictivePrefetch`, `BackgroundValidation`, `SemanticAppCache`,
  `RuntimeCache`, `CleanFileData` (page chunks of clean file bytes,
  one bounded device read to rebuild), `TransformCache`, `FsMetadata`
  (stat/security/lookup/directory-entry records — small, hot, rebuilt
  by a tree walk, so they outlive file data under pressure), and
  `ReliabilityAssist`. The taxonomy is the complete SMARTRAM class
  set; consumers beyond the filesystem cache arrive with the stages
  that build them.
- **Classification and admission (fail closed).** Before a cache
  admits anything it declares a `CacheCandidate` — class, a
  `ReclaimOwner` to charge (a kernel subsystem, a filesystem volume by
  its stable per-boot mount handle, or a task; session/service owners
  arrive with their identities), a `RebuildCost`, a `Sensitivity`, an
  `InvalidationSource`, a `ReclaimRule`, and its worst-case per-entry
  bookkeeping bytes — and passes `CacheCandidate::classify`, a pure
  (deterministic) gate producing a `CachePolicy` or a typed
  `AdmissionRefusal`. An unknown class or owner, unruled-out sensitive
  material (credentials, keys, capability tokens — and an undeclared
  sensitivity is treated as the most sensitive), per-entry metadata
  over the fixed `MAX_ENTRY_METADATA` validation bound, a missing
  reclaim rule (non-reclaimable), or a missing invalidation source is
  refused, and the producer serves uncached: no unowned,
  unclassifiable, or uninvalidatable memory exists in the model.
- **Budgets with hysteresis.** A `CacheBudget` is derived from the
  backing resource's size (`CacheBudget::from_backing` — 1/16 of the
  kernel heap arena per cache; each boot volume carries two, the clean
  filesystem cache and the transform cache, so the boot volumes' four
  caches together stay at or under 1/4 of the heap and cache growth can
  never exhaust it). Growth runs to the *hard* limit; a forced shrink
  evicts down to the *low* watermark (3/4 of hard), never both on one
  threshold.
- **Fail-closed accounting.** `CacheAccounting` keeps per-class byte
  ledgers with checked arithmetic (typed `AccountingError` on
  overflow/underflow, never wrapping) plus saturating hit/miss/
  insertion/invalidation/eviction/refusal counters.

The first consumer is the **clean, rebuildable filesystem cache**
(`kernel/core::fs::CachedFs`, `plans/SMARTRAM.md` section 6.1): a
wrapper around each mounted volume's filesystem driver, *below* the VFS
policy layer, applied at driver registration (`system_mount`). Key
properties:

- **Never bypasses authorisation.** Every permission check still runs
  in the secured VFS per operation; the cache only spares the driver a
  repeated structural read. A `security` record is cached but
  invalidated by `set_security`, so a tightened mode is seen by the
  very next check.
- **One volume, one writer.** Every mutation flows through the wrapper:
  the `fs_*` syscalls and the `CAP_USER_ADMIN` account-administration
  engine share the single registered driver behind one `SleepLock`
  (`LateFilesystem::register` returns the leaked lock precisely so a
  second, coherence-breaking window over the same device cannot exist).
- **Precise, fail-closed invalidation.** Writes/truncates drop the
  file's chunks and stat; create/remove/rename drop the affected
  directory's *entire* lookup set (driver name matching may fold case),
  its directory entries, and its stat; an unidentifiable mutation
  target purges the whole cache; a detected ledger imbalance poisons
  the cache (purge + admit nothing) while the driver keeps serving.
- **Bounded and zeroing.** Payload copies are fallibly allocated
  (`try_reserve`); oversized names are refused; reads above four chunks
  bypass the cache so bulk streams cannot evict the hot working set;
  and every cached buffer (file bytes, names) is zeroed on
  invalidation, eviction, purge, and teardown — the volumes are
  encrypted at rest, so cached bytes are decrypted user data that must
  not linger in reusable heap memory.

## 7h. VM pressure bands and reclaim ordering (`pressure`)

`kernel/mem::pressure` is the one definition of the system's
memory-pressure state and of the order reclaimable caches shrink in as
pressure rises (`plans/SMARTRAM.md` SMART2). The band vocabulary —
normal, mild, moderate, severe, critical — is shared with
`plans/SWAPSWAPSWAP.md`; there is no parallel model.

- **The gauge.** `MemoryPressure` samples a `FreeMemorySource` — in
  production the physical `FrameAllocator` (free frames are the
  authoritative reading; the boot path builds one gauge over the leaked
  allocator and every mounted volume's cache shares it) — and folds
  each reading into a banded state machine with **hysteresis**: a band
  is entered below one watermark and left above a strictly higher one
  (initial targets: mild 20%/25% free, moderate 10%/14%, severe
  6.25%/8%, critical 3.125%/5% — implementation constants in the
  `plans/SWAPSWAPSWAP.md` section 6 shape, to be tuned by benchmark,
  never ABI). Deepening applies immediately; relaxing steps one band at
  a time past each exit watermark. Sampling happens on the consumers'
  own operations — no background worker, no periodic tick.
- **Reserves, fail closed.** The thresholds carry a reserve floor
  (1/64 of the backing). A reading at or below it is critical
  regardless of history; a zero-size (unknown) backing reports critical
  for every reading and admits nothing. `growth_permitted` allows cache
  growth only at normal pressure and never lets it take the free
  reading into the reserve — cache expansion can never be the cause of
  reserve exhaustion.
- **Reclaim ordering.** `shrink_target(band, class, budget)` is the
  pure per-band ceiling each `ReclaimClass` must shrink to: at mild
  pressure the disposable/speculative classes drop and semantic,
  runtime, and clean-file classes shrink to the low watermark; at
  moderate, clean file and transform cache drain fully while metadata
  and recovery assist are capped at the low watermark; at severe and
  critical every class shrinks to zero. Targets are monotonically
  non-increasing with depth.
- **`ramzip` handoff and escalation.** `ramzip_handoff` fixes the
  `plans/SWAPSWAPSWAP.md` ordering: no compression at normal/mild; at
  moderate, compression of cold anonymous pages may start only once
  clean and transform cache are drained (reconstructable clean cache is
  cheaper than encrypted compressed anonymous storage); at severe
  `ramzip` owns cold-anonymous policy; at critical speculative work
  stops and `escalation` owns the next step. `escalation` is the
  deterministic answer when reclaim cannot help: reclaim caches while
  any remain, then hand off to `ramzip` (moderate/severe), then the VM
  pressure policy (critical). These are the seams the SWAP3 stage binds
  to when the `ramzip` store lands.
- **The consumers.** `CachedFs` (§7g) and the transform cache (§7i)
  sample the gauge at the head of every cache-touching operation: the
  band's forced-shrink targets are applied (data before metadata, every
  evicted buffer zeroed) before the cache is read, and admission is
  refused outside normal pressure — the volume is always still served
  straight from the driver.

## 7i. The RustFS transform cache (SMART3)

The transformation cache (`plans/SMARTRAM.md` SMART3, section 6.2)
retains the expensive intermediate form RustFS produces on every read of
a compressed cluster: the verified, decrypted, decompressed cluster
plaintext. Without it, each read that touches a compressed cluster pays
the full pipeline — a device read, an AEAD decrypt, and integrity checks
per stored block, then a whole-frame decompression — once per *call*;
with it, once per *cluster*.

- **A driver seam, a kernel implementation.** The RustFS driver stays
  kernel-independent: it exposes the `ClusterCache` trait
  (`rustos_drv_fs_rustfs::ClusterCache`) and consults an injected
  implementation only in its serving read path. The production
  implementation is `rustos_kernel::transform_cache::TransformClusterCache`,
  installed by the boot path on both mounted volumes (`system_mount`
  for the read-only `/System` volume, the unlock path for the writable
  root) via `RustFs::with_cluster_cache`. A volume without a cache
  behaves exactly as before, and the integrity passes (scrub, check,
  rescue) never consult it — they exist to verify the on-disk bytes.
- **Complementary to `CachedFs`, not duplicate.** `CachedFs` (§7g)
  retains page chunks of *served* plaintext for small reads; the
  transform cache sits below the driver's read path and covers what
  `CachedFs` cannot: the large sequential reads (bundle and
  driver-store loads) that bypass `CachedFs` by design, and `CachedFs`
  misses — both of which otherwise re-run the whole transform per call.
- **Classified, budgeted, pressure-governed.** The cache declares a
  `CacheCandidate` (class `TransformCache`, owned by the volume's
  stable per-boot mount handle, expensive to rebuild, decrypted user
  data, source-mutation invalidated, droppable) through the §7g
  admission gate — a refusal poisons it from birth and the driver keeps
  serving. Entries are LRU-evicted against a `CacheBudget`, admission
  obeys `growth_permitted`, and every operation first applies the
  band's `shrink_target`: the class is preserved at mild pressure and
  drained to zero from moderate on, before `ramzip` is handed anything
  (§7h).
- **Coherent by construction.** Entries are keyed by the cluster's
  first stored physical block and carry the run length. Every block
  free in the driver funnels through one choke point, which invalidates
  the covering entry *before* the block can be recycled; a transaction
  rollback (whose frees bypass that choke point) purges the whole cache;
  a defective entry that would stall the read loop fails the read closed
  (`DeviceFault`) instead. Reflink-shared clusters are only invalidated
  when their stored run is actually freed — a surviving referrer keeps
  the (still identical) plaintext.
- **Secret hygiene.** The plaintext is decrypted user data from an
  encrypted-at-rest volume: every buffer is volatilely wiped
  (`zeroize`) when its entry is invalidated, evicted, replaced, purged,
  or torn down, and the driver wipes its own transient frame and
  plaintext scratch on every path of the cluster read, clone, and
  decompose routines.

## 7j. The semantic application-launch cache (SMART4)

The semantic app/runtime cache (`plans/SMARTRAM.md` SMART4, section 6.3)
retains the result of the one shared application load gate
(`lib/appload`) for bundles on the immutable read-only system stores
(`/System/Apps`, `/System/Services`): the parsed signed `AppInfo`
manifest, the content-hash and syscall-interface-hash verdicts, the
dynamic-loader library policy decisions, and the validated `rxe`
entry-point image — one `LoadedApp` per bundle. Without it, every launch
of a system command re-reads and re-hashes the whole bundle tree and
re-verifies its Ed25519 signature; with it, once per boot.

- **The cache is `rustos_kernel_core::launch_cache::LaunchCache`,**
  held by the `AppStore` behind the `/System`-mount readiness latch. The
  boot path that publishes the mount installs the cache's budget and the
  system pressure gauge (`AppStore::install_reclaim`, called by
  `install_system_mount` just before it resolves the latch); until then
  — and on any classification refusal — every launch is served uncached
  through the full load gate (fail closed).
- **Only immutable bundles are cacheable.** A bundle on a writable
  volume (`/Apps`, a user's own store) can change between launches and
  is re-verified through the full gate every time
  (`AppStore::cacheable_bundle`). The read-only stores cannot change
  within a boot, so the boot *is* the entry's generation
  (`InvalidationSource::GenerationToken`): an app or system update is a
  new volume image and a new boot, and there is no stale-entry window to
  invalidate across.
- **A hit carries no caller authority.** The cached ceiling is the
  manifest request itself (the spawn path loads under the full-word
  intersection identity before inserting, making the result
  caller-independent); the per-caller capability intersection happens on
  every admit, and the spawn path re-authorises the *caller's* read of
  the entry point through the secured VFS before serving a hit — so a
  policy or grant change can never be replayed from the cache, and a
  hit and a miss produce identical load decisions.
- **Classified, budgeted, pressure-governed.** The cache declares a
  `CacheCandidate` (class `SemanticAppCache`, owned by the kernel
  app-store subsystem, expensive to rebuild, system data,
  generation-invalidated, droppable) through the §7g admission gate — a
  refusal poisons it from birth. Entries are LRU-evicted against a
  `CacheBudget` (the same kernel-heap-derived policy as §7g/§7i),
  admission obeys `growth_permitted`, and every operation first applies
  the band's `shrink_target`: shrunk to the low watermark at mild
  pressure and drained to zero from moderate on, before `ramzip` is
  handed anything (§7h). Reclaim can never make an app unlaunchable — a
  miss re-runs the gate over the intact on-disk bundle.
- **No secret content.** Entries are shared `Arc`s to signed, public
  system code (`Sensitivity::SystemData`) — never credentials, keys, or
  user plaintext — so eviction drops the cache's reference without
  wiping: a launched process legitimately holds the same image.

Two SMART4 families are deliberately **not** cached, and are recorded as
scope decisions in `plans/SMARTRAM.md`: command-resolution output
(`lib/cmdres` is a pure spelling function with no I/O — recomputing it
is cheaper than any cache, and the expensive verification behind the
winning candidate is exactly what this cache retains), and a separate
RXE relocation-preparation cache (the loader model has no separate
relocation stage; the validated image in the `LoadedApp` *is* the
cached RXE state).

## 8. Testing strategy

- **Unit tests** — alongside each module under `#[cfg(all(test, not(loom)))]`:
  buddy split/merge, bitmap correctness, slab guard-page detection,
  zero-on-free, OOM paths, the encrypted-swap round-trip /
  tamper-rejection / fail-closed cases, and the `uaccess` user-memory
  copy (cross-page, mid-page-offset, round-trip, and every fail-closed
  branch over `HostPageTable` + `SimPhysMap`).
- **Property tests** — `kernel/mem/tests/proptest_frame.rs` runs
  randomised alloc/free sequences and asserts the no-double-allocation
  and no-leak invariants, plus the reserved-frame untouchability
  invariant.
- **Fuzzing** — `kernel/mem/tests/fuzz_swap.rs` drives the encrypted-swap
  restore path with arbitrary device contents (`AGENTS.md` §19.6),
  asserting that tampering is always rejected and the output buffer is
  zeroed on failure.
- **Loom tests** — `kernel/mem/tests/loom.rs` model-checks concurrent
  allocation, gated on `RUSTFLAGS="--cfg loom"` exactly like
  `lib/sync`.

[`BootMemoryMap`]: ../../rustos_kernel_mem/struct.BootMemoryMap.html
[`AddressSpace<P: PageTable>`]: ../../rustos_kernel_mem/struct.AddressSpace.html
[`PageTable`]: ../../rustos_kernel_mem/trait.PageTable.html
