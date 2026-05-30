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

```rust
pub fn slot_base(&self, buf: &DmaBuffer) -> Result<NonNull<u8>, DmaError>;
```

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
pointer the kernel-host mapper turns into an
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

## 8. Testing strategy

- **Unit tests** — alongside each module under `#[cfg(all(test, not(loom)))]`:
  buddy split/merge, bitmap correctness, slab guard-page detection,
  zero-on-free, OOM paths.
- **Property tests** — `kernel/mem/tests/proptest_frame.rs` runs
  randomised alloc/free sequences and asserts the no-double-allocation
  and no-leak invariants, plus the reserved-frame untouchability
  invariant.
- **Loom tests** — `kernel/mem/tests/loom.rs` model-checks concurrent
  allocation, gated on `RUSTFLAGS="--cfg loom"` exactly like
  `lib/sync`.

[`BootMemoryMap`]: ../../rustos_kernel_mem/struct.BootMemoryMap.html
[`AddressSpace<P: PageTableOps>`]: ../../rustos_kernel_mem/struct.AddressSpace.html
[`PageTableOps`]: ../../rustos_kernel_mem/trait.PageTableOps.html
