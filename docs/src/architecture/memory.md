# Memory subsystem (`kernel/mem`)

Architecture-neutral, host-testable physical and virtual memory
management. Delivered by Stage 2.2 of `PLAN.md`. The architecture
crates (`kernel/arch/*`, Stage 3) supply the only piece this crate
does not implement: the real page-table writer behind
[`PageTableOps`](#3-virtual-memory--page-table-operations).

## Layered design

```text
                ┌──────────────────────────────────────────────┐
                │   sensitive — zero-on-free for credentials   │
                │   (`alloc_sensitive` / `free_sensitive`)     │
                ├──────────────────────────────────────────────┤
                │   slab — fixed-size objects + guard pages    │
                ├──────────────────────────────────────────────┤
                │   vmm — `AddressSpace<P: PageTableOps>`      │
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
UEFI, DTB, WASM) and hand it to `FrameAllocator::new`. Reserved
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

[`AddressSpace<P: PageTableOps>`] is the per-process virtual address
space. It owns a page-table object and serialises `map` / `unmap` /
`translate` through it.

The [`PageTableOps`] trait is the architecture boundary:

| Method | Description |
| --- | --- |
| `map(page, frame, flags)` | Install a translation. |
| `unmap(page)` | Tear it down, return the frame. |
| `translate(page)` | Read-only lookup. Returns `Option`, never errors. |
| `flush(page)` | TLB flush for the current CPU (no-op on host). |

The arch crates land their `X86PageTable`, `Aarch64PageTable`, …
implementations in Stage 3. To keep `kernel/mem` fully host-testable
today the crate ships a `HostPageTable` test double behind
`#[cfg(test)]`: a `BTreeMap`-backed implementation that, additionally,
enforces W^X (rejects `WRITE | EXEC` mappings) so the security
default is exercised in tests.

`MapFlags` is a small `bitflags`-style set:
`READ | WRITE | EXEC | USER | NO_CACHE`. Architecture code translates
these into native page-table bits during Stage 3.

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
`copy_in` / `copy_out`) are increment D (see `PLAN.md`). Because the
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
[CapabilityId::MEM_DMA]: ../../rustos_abi/capability/struct.CapabilityId.html#associatedconstant.MEM_DMA

### 5.1 Slab hand-off to user-space drivers

The user-space virtio driver crates carry an owned
`DmaSlab { phys, ptr: NonNull<u8>, len, pool_id, slot, /* erased
free shim */ }` rather than borrowing the pool on every accessor
(Stage 4.D Item 0a). The pool exposes a single companion accessor,
`slot_base(&self, buf: &DmaBuffer) -> Result<NonNull<u8>, DmaError>`,
that hands out the base pointer of `buf`'s data slots. The
disjointness witness is the pool's slot bitmap (one slot ↔ one
allocation); the slab carries `(pool_id, slot, len)` so its drop
can invoke a type-erased free shim that returns the slot to the
pool. See [Virtio transport — DMA ownership model](../drivers/virtio.md#dma-ownership-model)
for the consumer-side view. The kernel-side wiring of a
`KernelVirtioHost` that builds slabs from `alloc_dma` is the
subject of Stage 4.D Item 0.

### 5.2 MMIO register-window mapper

Device drivers also need their *register block* mapped — a PCI memory
BAR or a virtio-MMIO transport slot. `kernel/mem::mmio::MmioMap`
provides this. Unlike [`DmaPool`][DmaPool] it does **not** allocate
frames: the physical address is fixed by the hardware, so the mapper
maps the *device's own* frames into a per-process `AddressSpace<P>`
with caching disabled (`MapFlags::NO_CACHE`) and the same unmapped
guard-page bracketing the DMA pool uses. `MmioMap::map(phys_base, len)`
returns an `MmioRegion`; `region_base` resolves the region's device
physical base through the direct physical map (`PhysMap`) into the base
pointer the kernel-side mapper (`KernelMmioMapper`, in `kernel/virtio`)
turns into an
[ABI `RegisterWindow`](../drivers/bus.md#register-window-hand-off), so
the window addresses the device's real registers.

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
  type 6) and the EFI memory-map tag (type 17) handed in by GRUB-EFI
  + OVMF. Both parsers are zero-copy and `no_alloc`.
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
[`AddressSpace<P: PageTableOps>`]: ../../rustos_kernel_mem/struct.AddressSpace.html
[`PageTableOps`]: ../../rustos_kernel_mem/trait.PageTableOps.html
