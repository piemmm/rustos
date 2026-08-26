# FIX-KHEAP — Fragmentation-immune kernel-heap growth

Status: **done** (K1–K5 landed).

Binding under `AGENTS.md`. The kernel heap grows by adding whole regions,
and one allocation must fit inside one region, so growth needs a
*virtually contiguous* run. It used to draw that run as a single
physically contiguous buddy block, which meant three defects at once:
growth refused a large region on a fragmented pool while gigabytes were
free (load-dependent — green on a quiet developer machine, fatal once the
bootstrap arena fragmented); rounding the request up to a power of two
wasted up to 2× per grow; and the largest serviceable single allocation
was welded to `MAX_ORDER` rather than to installed RAM. The last of those
had been "fixed" by *raising the constant* and pinning it with a
compile-time assertion in `appspawn.rs` — mitigation, not the structural
control.

Read first: `kernel/mem/src/kvmap.rs`, `kernel/mem/src/kvslots.rs`,
`kernel/core/src/kheap.rs`, each port's `paging.rs`
`reserve_kernel_window` / `new_kernel_window`. The architecture-level
description is `docs/src/architecture/memory.md` §7u.

## What it now guarantees

1. **Virtual contiguity, not physical.** A region is assembled from as many
   `≤ MAX_ORDER` chunks as the pool can offer, mapped into one run of a
   kernel remap window, so growth succeeds whenever *total* free frames
   suffice in **any** physical layout and fails closed only on genuine
   exhaustion.
2. **The exact page count**, rounded to a page — never a power of two.
   Internal waste is under one page.
3. **Zero global-heap allocation on `grow`/`shrink`.** Both run under the
   heap's own non-reentrant lock, so a single allocation there would
   deadlock. A counting-allocator host test pins it.
4. **The page tables are the record.** `shrink` recovers each frame by
   walking them; no side table remembers what was drawn.
5. **Kernel commit.** Growth uses `alloc_order` (not `alloc_order_user`), so
   it may draw the `RESERVE_DIVISOR` kernel reserve and keeps making
   progress under user memory pressure.
6. **W^X.** Window leaves are `RW`, kernel-only, never executable.
7. **`MAX_ORDER` has one meaning** — the largest physically contiguous draw,
   a hardware-shaped bound. `BUNDLE_FILE_MAX` is an untrusted-input bound
   independent of it, and the compile-time assertion tying them is gone.
8. **O(1) allocate, free, and region reclaim.** Intra-region behaviour is
   *not* separable from growth, and the earlier claim that it was out of
   scope was wrong: the more the heap grew, the more the allocator's own
   per-operation cost rose. `lib/kalloc` is a segregated-fit allocator over
   in-band boundary tags — per-size-class free lists entered by bitmap scan,
   a recorded physical predecessor so coalescing reaches each neighbour
   directly, and a doubly-linked region list so a drained region is unlinked
   without a search. No list is walked on any path.

The superseded design walked one address-sorted hole list on both `alloc`
and `dealloc`, plus the whole region list on *every* `dealloc`. Because a
grown region's header separator forbids coalescing across it, the hole count
could never fall below the region count, and regions accumulate with the
heap — so past the bootstrap arena every allocation in the kernel paid a
cost linear in total growth. It surfaced as the desktop wallpaper gallery
degrading from 0.46 s to 20 s per image while the filesystem driver beneath
measured 370 MB/s flat, and it taxed every unrelated subsystem equally.
`per_operation_node_reach_does_not_grow_with_the_heap` pins the bound.

## Remaining

Two items were separated out of the allocator fix rather than folded into it,
both surfaced by measuring the same read path:

- **Slab allocation (SLUB).** Page-sized objects are the heap's dominant
  traffic (the filesystem cache's chunk is `PAGE_SIZE`), and a byte-granular
  heap serves them with a header, so they never pack into a frame. Per-size-class
  slabs drawn a page at a time from the frame allocator, with the free list
  threaded through the free objects themselves, give a page-sized allocation
  exactly one frame and no header. The kernel's direct physical map makes the
  slab page addressable with no window slot, which matters: `kvslots` allocates
  first-fit, so one window slot per single-page slab would reintroduce a walk
  one layer down.
- **The per-request read cost above the driver.** Delivered throughput is
  ~0.9 MB/s where the filesystem driver measures 370 MB/s over RAM — a gap the
  allocator fix does not touch, present from boot (a 2.8 s bundle load). The
  measured suspects are the `SharedBlock` sleep-lock round-trip per device
  operation and `BlockCache::populate` sampling the pressure gauge (and so
  taking the global frame-allocator lock) once per *device block* rather than
  once per request.

**Kernel code runs with the current task's translation root active**, so a
kernel address must resolve identically under every root. Each port
therefore reserves the window by pointing the covering top-level entry of
every root it builds at one **shared sub-hierarchy**: `reserve_kernel_window`
draws that sub-hierarchy and publishes the entries, every root constructor
installs them, and the live boot root is patched in place. Leaves are
installed through `new_kernel_window` — a root that maps *only* the window,
so the remap handle can reach nothing else and its intermediate tables come
from the allocator-backed page-table source rather than a fixed `.bss` pool.

Placement is derived from each port's VA layout, not a byte constant: the
top eighth of the `TTBR0_EL1` / Sv39 root table (64 GiB), and the highest
free canonical PML4 slot on x86_64 (512 GiB). On aarch64 the reservation
refuses a slot the discovered RAM or Device mask claims (fail closed) and
`ensure_identity_gigapage` refuses to widen into one; on riscv64 the
identity extent (`paging::IDENTITY_GIGAPAGES`) is derived from the same
figure, so the window and the identity map cannot overlap and the direct
physical map is sized from what the MMU actually maps.

**The re-entrancy trap** is why the bookkeeping is new code rather than a
reuse. `FrameAllocator::alloc_chunks` returns a `Vec` — its order-step-down
*algorithm* is reused inline, never its surface. `AnonWindowMap` keeps its
holes in `BTreeMap`s and would deadlock, so `kvslots::SlotWindow` is its
heap-free counterpart: one address-sorted boundary-tag list covering
everything below a bump cursor, first-fit with splitting, coalescing on
release, retracting the cursor when a freed run reaches it, with its entry
records drawn a frame at a time from the frame allocator. Record storage is
bounded by live-plus-freed runs, never by the window's page count.

**Teardown synchronises before it frees; installation does not synchronise
at all.** `unmap_run` unmaps a batch, issues one system-wide invalidation for
the batch's range, and only then hands the recovered frames back — freeing
first would leave a stale translation aliasing reallocated memory. A batch
that tore down no leaf owes nothing and skips it. `CrossCpuTlbShootdown`
gained a range form for this (per-page on the default, one broadcast barrier
on aarch64, one SBI RFENCE on riscv64, one IPI round-trip on x86_64), so a
large region does not pay one cross-CPU round-trip per 4 KiB leaf.

Installing a leaf is the opposite case and must not be confused with it: a
not-present entry is never cached, so what the walker needs is the table
store *ordered*, not an invalidation. `TlbShootdown::publish_mappings` is
that distinct operation — a store barrier on aarch64, nothing at all on
x86_64, the fence riscv64's ISA requires because it permits caching invalid
entries. Reaching for the range *flush* here instead cost a whole-domain
`tlbi vmalle1is` broadcast per chunk on aarch64, so a fragmented pool's
many-chunk growth wiped every TLB on every core once per chunk — the growth
path's cost then rose with fragmentation, which is exactly what the design
exists to stop.

**Ordering.** `slots` is always locked before the remap map, on both paths;
`shrink` releases the address space *first* (which accepts only an exact
live run, so a mismatched `(base, len)` frees nothing) and tears down under
the same guard, so a released run cannot be re-handed out while still
mapped.

## Deliberate carve-outs

- **`wasm32` gets no window.** It has no MMU and no direct map, so
  `install_kernel_remap` returns `None` and the heap stays on its bootstrap
  region — the pre-existing, fail-closed behaviour.
- **The host cannot dereference the window.** A host test has no hardware to
  map its addresses, so `kernel/core::kheap`'s tests drive the growth
  source's *contract* (extents, frame accounting, fail-closed paths,
  no-allocation proof) over a non-allocating page-table double; the
  end-to-end dereference is proven by every QEMU vertical, none of which can
  finish booting unless the window works.
- **The teardown batch is a batching granule**, not a limit on how much may
  be torn down: a longer run is simply processed in more batches.
