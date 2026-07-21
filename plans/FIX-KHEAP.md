# FIX-KHEAP — Fragmentation-immune kernel-heap growth

Status: **planned** (design fixed below; no code landed).

Binding under `AGENTS.md`. This plan removes a first-class defect in the
kernel-heap growth path: growth demands **physically contiguous** frames,
so it fails on a fragmented pool even when ample RAM is free, and the
single-allocation size is welded to the buddy allocator's max-order
(`MAX_ORDER`) — a coupling that was previously "fixed" by *raising the
constant* (the §2.17 anti-pattern) and pinned in place with a compile-time
assertion in `appspawn.rs`.

Read first (§15.18): `kernel/core/src/kheap.rs`, `kernel/mem/src/frame.rs`
(`alloc_chunks`, `RESERVE_DIVISOR`, `MAX_ORDER`), `kernel/mem/src/vmm.rs`
(`AddressSpace::map_contiguous`/`unmap`/`translate`),
`kernel/mem/src/anon_window.rs` (the VA-window pattern — but see the
re-entrancy trap below), `kernel/core/src/appspawn.rs` (`BUNDLE_FILE_MAX`
and the `MAX_ORDER` assert).

## The defect (why the current implementation is unacceptable)

`FrameHeapSource::grow` (`kernel/core/src/kheap.rs`) draws exactly **one**
`alloc_order(order)` block and reaches it through the linear direct map
(`PhysMap::translate`). Three separate problems follow, and a first-class
fix must close all three:

1. **Physical (external) fragmentation — the root cause.** The direct map
   only makes a block *virtually* contiguous if it is already *physically*
   contiguous. So growth can draw a single buddy block only, and on a
   fragmented pool a large contiguous block does not exist even when tens
   of GiB of total RAM are free. This is the classic §24.1 "refuse even
   though more of the resource exists" failure, and it is **load-dependent**
   (§7, §26): it passes every quiet developer test and fails in production
   once the bootstrap arena fragments (the regression comment in `kheap.rs`
   says exactly this — the kernel aborted *"once the bootstrap arena
   fragmented"*).

2. **Internal fragmentation of the growth granule.** `order_for` rounds the
   request up to a power of two, wasting up to ~2× per grow (a 12 MiB
   allocation draws 16 MiB; a 16 MiB bundle draws 32 MiB). That waste is
   what forces `MAX_ORDER` to be *twice* `BUNDLE_FILE_MAX` — the §2.17
   "enlarge the buffer so the problem stops happening" smell, frozen by the
   `appspawn.rs` compile-time assert.

3. **Size welded to contiguity order.** A single kernel-heap allocation
   larger than one `MAX_ORDER` block (32 MiB today) is refused outright,
   regardless of free RAM. Heap-allocation size must scale with RAM, not
   with a hand-picked buddy order (§24.1).

Heap-level (intra-region) fragmentation is **out of scope**: the
`FreeListAllocator`'s own coalescing free list is its proper job and is not
the defect.

`MAX_ORDER` itself stays. As the buddy allocator's max physical-contiguity
order it is a legitimate fixed *bound* (§24.4), not a growable capacity.
The fix decouples heap-allocation *size* from it; it does not change it.

## Goal / invariants

The correct model — the one that survives review:

1. **Require virtual contiguity, not physical.** Heap growth manufactures a
   virtually-contiguous window from several `≤ MAX_ORDER` physical chunks,
   so growth succeeds whenever *total* free frames suffice, in **any**
   physical layout. Fail closed only on genuine total exhaustion
   (§4, §2.9).
2. **Draw the exact page count**, rounded to a page — never a power of two.
   Internal waste drops from ~2× to `< 1` page.
3. **The growth path allocates ZERO bytes from the global heap it is
   growing.** This is the re-entrancy/deadlock invariant `frame.rs` already
   warns about. No `Vec`, no `BTreeMap`, no boxed side-table on `grow` or
   `shrink`. See "The re-entrancy trap" — it is the crux of the design.
4. **Page tables are the record.** `grow` maps chunks into the window and
   returns `(base, len)`; `shrink` recovers each frame by walking the
   window's PTEs (`AddressSpace::translate`/`unmap`), so no side-table is
   needed to remember what was drawn.
5. **Kernel commit draws the reserve.** Growth uses the kernel commit path
   (`alloc_order`, not `alloc_order_user`), so it may draw the
   `RESERVE_DIVISOR` kernel reserve. Fragmentation-immunity + the reserve
   together are what make growth genuinely unable to spuriously fail while
   RAM exists (§4).
6. **W^X.** Window pages are mapped `RW`-only, never executable (§19.2).
7. **`MAX_ORDER` reverts to one meaning** — largest single physically-
   contiguous block (§24.4) — and its rustdoc drops the kheap-coupling
   claim. The `appspawn.rs` `BUNDLE_FILE_MAX ↔ MAX_ORDER` compile-time
   assert is **deleted** (§2.14), and `BUNDLE_FILE_MAX` becomes a pure
   §19.5/§24.4 validation bound on untrusted input, independent of the
   allocator's order.

## The re-entrancy trap (the reason to design, not just wire up)

Three tempting reuses each secretly allocate from the global heap and are
therefore **forbidden on the growth path**:

- `FrameAllocator::alloc_chunks` returns `Vec<(Frame, u32)>` — a heap
  allocation *during heap growth*. Do not call it on `grow`. Reuse its
  **order-step-down algorithm** inline instead (draw one chunk, map it,
  advance, repeat), never its `Vec`-returning surface.
- `AnonWindowMap` keeps its free-hole/region bookkeeping in `BTreeMap` —
  the heap. It cannot back kernel-heap growth. Do **not** reuse it here.
- Any boxed/`Vec` chunk list to remember drawn frames for `shrink` — the
  page tables already hold that information; use them.

Page-table **interior nodes** are drawn from the *frame allocator*, not the
heap (`PageTableError::AllocFailed(AllocError)`), so `map_contiguous` is
safe on the growth path. This is the one allocation growth may make, and it
is heap-independent by construction.

### Kernel-VA window bookkeeping (the one real design decision)

Growth needs a kernel virtual-address window to map chunks into, and it
must track which sub-ranges are free for reuse — bookkeeping that must
itself be heap-free. Do it the way `frame.rs` already avoids the heap:

- Reserve a generous kernel heap-growth VA region once at boot (address
  space is free until backed; size it from the arch's kernel VA layout, not
  a page-count-derived RAM cost).
- Track live/free windows with an **intrusive free-list whose nodes are
  drawn from the frame allocator** (a tiny dedicated node source),
  mirroring the frame allocator's intrusive `nodes` array — never the
  global heap and never a per-page bitmap (which would reintroduce a §24.1
  ceiling). A bump cursor plus a free-hole list keyed by slot is sufficient
  and is bounded by the number of *live + freed* windows, not by window
  page count.

This VA-window allocator is a new, heap-independent primitive; implement it
as the complete abstraction (allocate / release / first-fit-reuse / split),
not the thin slice `grow` first exercises (§27).

## Design (what `grow`/`shrink` do)

`grow(min_len)`:
1. `pages = min_len.div_ceil(PAGE_SIZE)`, floored at `MIN_GROW_ORDER`'s
   frame count for amortised small growth.
2. Reserve a `pages`-page kernel VA range from the heap-growth window
   (heap-free bookkeeping above).
3. Loop: draw the largest `≤ MAX_ORDER` chunk that fits the remainder via
   the frame allocator's **kernel** commit, stepping the order down on
   `OutOfMemory` before giving up (the `alloc_chunks` algorithm, inline, no
   `Vec`); map it into the next consecutive slot of the VA range with
   `map_contiguous`, `RW`-only; advance; repeat until `pages` mapped.
4. On any failure: unmap and free every chunk already mapped (walk the
   partially-filled window via `translate`/`unmap`), release the VA range,
   return `None` (fail closed, leaks nothing — §4, §2.9).
5. Return `(window_base, pages * PAGE_SIZE)`.

Keep the current single-block direct-map path as a **fast path** for a
small grow a single contiguous block already satisfies (avoids touching
page tables for the common 64 KiB case), selected by `translate` success;
the chunk+vmap path is the fallback that removes the cliff. Only one of the
two paths backs any given region, and `shrink` must handle both — mark the
region's kind so `shrink` knows whether to walk PTEs or reverse the direct
map. (If distinguishing them cleanly proves awkward, prefer the single
chunk+vmap path for *all* growth over a fragile fast-path flag — simplicity
wins a Torvalds review over a micro-optimisation that risks a wrong free.)

`shrink(base, len)`:
1. For a direct-map region, reverse-translate and `free_order` as today.
2. For a windowed region, walk each page `base .. base+len`:
   `translate` → `unmap` → `free` the recovered frame; then release the VA
   range back to the window allocator. No side-table consulted.

## Tasks

- **K1 — heap-free kernel-VA window allocator.** New primitive
  (`kernel/mem`), intrusive free-list nodes from the frame allocator, full
  allocate/release/reuse/split, host-unit-tested for bump, hole reuse,
  split, exhaustion-fail-closed, and heap-independence (§27).
- **K2 — rework `FrameHeapSource::grow`/`shrink`** to the chunk+vmap design
  above, over `AddressSpace::map_contiguous`/`unmap`/`translate` and the
  frame allocator's kernel commit + reserve. No heap allocation on either
  path.
- **K3 — decouple constants.** Delete the `appspawn.rs`
  `BUNDLE_FILE_MAX ↔ MAX_ORDER` compile-time assert; rewrite
  `BUNDLE_FILE_MAX` rustdoc as a standalone untrusted-input bound; rewrite
  the `MAX_ORDER` rustdoc in `frame.rs` to drop the kheap-coupling claim;
  rewrite the `kheap.rs` module docs to describe the window (not "through
  the kernel direct map").
- **K4 — tests (mandatory, §7).** See below.
- **K5 — docs.** `docs/src/architecture/` memory page + any README matrix
  note; PLAN.md entry; this plan's status.

## Tests (mandatory, §7)

- **Fragmented-pool grow** — the headline regression: pepper the frame pool
  with holes so **no single `MAX_ORDER` block exists**, then allocate a
  region larger than one `MAX_ORDER` block and assert it is served and that
  `shrink` returns **every** frame. This is the load-dependent case
  (§7/§26) the current code fails.
- **Multi-block grow** — an allocation of many multiples of a `MAX_ORDER`
  block succeeds and shrinks back fully.
- **Exact-page-count draw** — assert internal waste `< 1` page (not
  power-of-two rounding).
- **Fail-closed on true exhaustion** — genuine total-RAM exhaustion returns
  `None`/OOM, no partial leak, free-frame count unchanged.
- **No heap re-entrancy** — a host test proving `grow`/`shrink` make no
  global-heap allocation (e.g. via a counting allocator or by construction
  in the VA-window unit tests).
- The existing `grows_from_frames_and_shrinks_back` and
  `grows_for_an_allocation_larger_than_eight_mib` tests stay but are
  rewritten to be **independent of `MAX_ORDER`'s value** (they must not
  encode "old 8 MiB cap" reasoning).

## Non-goals / do not do

- Do NOT bump `MAX_ORDER` again — the number is not the fix (§2.17).
- Do NOT call `alloc_chunks`, `AnonWindowMap`, or any `Vec`/`BTreeMap`/boxed
  side-table on the growth path (re-entrancy deadlock — §2.1).
- Do NOT keep the `BUNDLE_FILE_MAX ↔ MAX_ORDER` coupling "for now" (§2.19);
  delete it in the same change (§2.14).
- Do NOT make heap growth executable (W^X — §19.2).
- Do NOT touch the `FreeListAllocator`'s intra-region coalescing (not the
  defect).
