# Memory subsystem (`kernel/mem`)

Architecture-neutral, host-testable physical and virtual memory
management. Delivered by Stage 2.2 of `PLAN.md`. The architecture
crates (`kernel/arch/*`, Stage 3) supply the only piece this crate
does not implement: the real page-table writer behind
[`PageTableOps`](#3-virtual-memory-page-table-operations).

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
[vmm](#3-virtual-memory-page-table-operations).

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

## 5. Result-returning OOM contract

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

## 6. Unsafe & pointer arithmetic discipline

Per `AGENTS.md` §4, raw pointer arithmetic is confined to the
`ptr` module's bounds-checked helpers (`offset_within`,
`end_within`, `slice_within`). Every other module routes pointer
math through them. Every `unsafe` block carries a `// SAFETY:`
rationale per `AGENTS.md` §2.10, encapsulated behind a safe public
API; no `unsafe` leaks across crate boundaries.

## 7. Testing strategy

- **Unit tests** — alongside each module under `#[cfg(all(test, not(loom)))]`:
  buddy split/merge, bitmap correctness, slab guard-page detection,
  zero-on-free, OOM paths.
- **Property tests** — `kernel/mem/tests/proptest_frame.rs` runs
  randomised alloc/free sequences and asserts the no-double-allocation
  and no-leak invariants, plus the reserved-frame untouchability
  invariant.
- **Loom tests** — `kernel/mem/tests/loom.rs` model-checks concurrent
  allocation, gated on `RUSTFLAGS="--cfg loom"` exactly like
  `kernel/sync`.

[`BootMemoryMap`]: ../../rustos_kernel_mem/struct.BootMemoryMap.html
[`AddressSpace<P: PageTableOps>`]: ../../rustos_kernel_mem/struct.AddressSpace.html
[`PageTableOps`]: ../../rustos_kernel_mem/trait.PageTableOps.html
