# FIX-KHEAP — Fragmentation-immune kernel-heap growth

Status: **done** (K1–K6 landed).

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
`reserve_kernel_window` / `new_kernel_window`, and `lib/kalloc/src/lib.rs`.
The architecture-level description is `docs/src/architecture/memory.md`
§7u (growth) and §7v (the two tiers).

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
   per-operation cost rose. `lib/kalloc`'s byte-granular tier is a
   segregated-fit allocator over in-band boundary tags — per-size-class free
   lists entered by bitmap scan, a recorded physical predecessor so
   coalescing reaches each neighbour directly, and a doubly-linked region
   list so a drained region is unlinked without a search. No list is walked
   on any path.
9. **A slab tier under the same façade, so a page-sized allocation is one
   frame.** Requests up to the page granule are served by per-size-class
   slab pages with the free list threaded through the free objects
   themselves — no per-object header, no rounding to a block boundary — so
   the filesystem cache's `PAGE_SIZE` chunk occupies exactly one frame
   instead of spilling a header into a second. Larger requests fall through
   to the byte-granular tier above.

The superseded design walked one address-sorted hole list on both `alloc`
and `dealloc`, plus the whole region list on *every* `dealloc`. Because a
grown region's header separator forbids coalescing across it, the hole count
could never fall below the region count, and regions accumulate with the
heap — so past the bootstrap arena every allocation in the kernel paid a
cost linear in total growth. It surfaced as the desktop wallpaper gallery
degrading from 0.46 s to 20 s per image while the filesystem driver beneath
measured 370 MB/s flat, and it taxed every unrelated subsystem equally.
`per_operation_node_reach_does_not_grow_with_the_heap` pins the bound, and
`slab_per_operation_node_reach_stays_constant_as_pages_accumulate` pins the
slab's.

## The slab tier as built (K6)

The architecture-level description is `docs/src/architecture/memory.md` §7v;
what a future change must not undo:

- **One allocator, two tiers, one lock.** The slab lives inside
  `tairix_kalloc::FreeListAllocator` behind the same `GlobalAlloc` façade,
  never a second kernel allocator. `GlobalAlloc`'s bodies route to private
  `alloc_bytes`/`dealloc_bytes` (the byte-granular tier) or to
  `slab_alloc`/`slab_dealloc`, so the slab draws a page *through* the byte
  tier without re-entering the lock.
- **Routing is a pure function of `layout`.** `slab_class` reads nothing but
  the size and alignment, so both ends of an allocation agree with no side
  table and no header. Routing that consulted whether a supply were
  installed would free a pre-install object down the wrong tier.
- **The granule is one shared compile-time constant.** The open question the
  design stopped at is settled the second way it named: `PAGE_SIZE` /
  `PAGE_SHIFT` now live once in `tairix_abi::memory` — the granule is
  user-visible through `mem_map`, so the ABI is its home — and the three
  ports' `paging.rs`, `kernel/mem::frame`, `lib/rt`'s heap, and `lib/kalloc`
  all import that one definition. Routing therefore needs no runtime state
  and cannot be wrong on an allocation made before some install.
- **Classes** are powers of two, so a class placed at a multiple of its own
  size inside a page-aligned page is aligned to that size and alignment needs
  no padding, no header, and no second mechanism. They run from the smallest
  width that holds a page's descriptor up to a **derived** ceiling
  (`SUB_GRANULE_MAX`), plus the granule class. This is the one place the
  built tier refines the design settled above, which said "up to the
  granule": a sub-granule page spends one slot on its descriptor, so a class
  of width `c` costs `PAGE_SIZE / (PAGE_SIZE / c - 1)` per object — at half
  the granule that is a whole page for a 2 KiB object, double what the byte
  tier charges. The ceiling is therefore the widest class that costs no more
  per object than a byte-tier block of the same width (128 bytes on a 64-bit
  target with 4 KiB pages), which is derived arithmetic and not the picked
  ceiling the design forbade. The granule class is exempt and always present:
  with no descriptor it both undercuts the byte tier and is the one shape
  that fits a frame exactly. Four classes result, so one retained page each
  is a bounded retention.
  `the_slab_never_costs_more_per_object_than_the_byte_tier` pins the rule.
- **A sub-granule page's descriptor** (free-object head, live count, a
  virgin-slot cursor so a fresh page costs no threading walk, and the
  doubly-linked partial-list links) lives in the page's own first object
  slot, so an object finds its page by masking to the granule. The granule
  class has no descriptor and needs none: one object per page.
- **Page supply, and provenance.** `HeapSource` supplies both tiers —
  `grow`/`shrink` for regions, `alloc_page`/`free_page` for pages — and is
  **set-once**, because the regions and pages outstanding belong to the
  source that issued them. After the install a page is one frame drawn
  through `kernel/mem::FramePages` (the kernel commit path) and addressed by
  the direct map, with no `kvslots` slot, which is the point. Before it, the
  slab carves a granule-aligned block out of the bootstrap region through
  the byte tier; a carve that lands outside that region is handed straight
  back, so the O(1) range test that decides how a page goes home is total.
  `FramePages` proves the direct map inverts its own translation before
  handing a page out, so a map that could not return the page refuses it
  rather than stranding it.
- **Reclaim.** A drained page goes back to its supply; one page per class is
  kept as the hysteresis that stops an allocate/free cycle at a class
  boundary thrashing the frame allocator. Those retained pages are also the
  heap's one reclaim step before it refuses: `alloc_bytes` releases every
  spare and retries once before returning null.

## The per-request read cost above the driver — measured, and it was not the block layer

Both per-operation costs this plan named as suspects were real and are fixed
(below), but measurement puts neither anywhere near the reported gap. What
the gap actually is: **contention from a desktop-side loop**, tracked as its
own defect in `plans/OPEN-DEFECTS.md`.

The instrument is the load record every bundle emits, which now carries the
bytes its load span moved (`read_bytes`) as well as the span, so `load=` is a
throughput and not a bare duration. Over the `autoload-input-qemu-aarch64`
desktop vertical the delivered rate is not a function of size at all — the
largest bundle is the *fastest* (`desktop.app`, 2.15 MB at 6.2 MB/s) and the
smallest is among the slowest (`seatmgr.app`, 35 KB at 0.15 MB/s). Sorting by
*when* each load ran explains all of it: the desktop's second thread issues a
burst of some 2500 audited `fs_open` + `fs_write` pairs between 7.47 s and
12.65 s, and exactly the two loads that fall inside that window are the slow
ones (`switchboard.app` 1.36 s, `files.app` 2.54 s, both ~0.5 MB/s), while
every load outside it completes in 0.08–0.60 s. The reported ~0.9 MB/s is a
*contended* rate, so the per-byte read path was never the term to attack.

Two per-operation costs were fixed regardless, because both were genuine
work-per-block where the answer cannot change within a request:

- `BlockCache::populate` and `RetainedWrites::record` asked the memory-pressure
  gauge once per **device block**. That reading is the physical frame
  allocator's free-frame count, so a 128-block coalesced read took the global
  frame-allocator lock 257 times per request. `tairix_reclaim::GrowthAllowance`
  is now one reading per request that each block draws its cost down from —
  and it is the *stricter* bound, because admitted bytes come from heap slack,
  so the free reading does not move and re-asking per block let one run admit
  many multiples of the headroom its first answer covered.
- `SleepLock::release` consulted its wait queue — a spin lock and a `BTreeMap`
  probe — on every release just to learn nobody was waiting, and this lock
  serialises every block-device operation on a shared disk. Contention now
  lives in the lock word (a `CONTENDED` bit a contender publishes before it
  parks), so the uncontended release is one compare-exchange and the queue is
  untouched. Flag and lock bit share one location, so no store/load fence is
  needed. A vanished waiter is now passed over for the next-oldest rather than
  unlocking with the queue still occupied, which would have stranded every
  remaining contender.

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
