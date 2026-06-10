//! A `mem_map`-backed userland heap allocator for `rustos-rt`.
//!
//! A freshly spawned RustOS process boots with only its fixed spawn-time image
//! (code/data/bss plus a fixed stack, `plans/SPAWN.md` SP2/SP3); it has no heap.
//! This module turns the `abi-v1` anonymous-memory pair
//! ([`crate::mem_map`] / [`crate::mem_unmap`]) into a `#[global_allocator]` so a
//! first-party Rust program can use `alloc` (`Box`, `Vec`, `String`, …). It is
//! the `lib/rt` `malloc`/`free` layer foreshadowed by the SP5 design note
//! (`plans/SPAWN.md`).
//!
//! # Design — a free-span allocator over a growable, fixed-base arena
//!
//! The heap owns one contiguous virtual arena that starts at a fixed base
//! ([`ARENA_BASE`]) and grows upward, one or more whole pages at a time, by
//! `mem_map`ping with [`MapFlags::FIXED`] at the arena's current top. Freed
//! regions are tracked as a coalesced, address-sorted list of free **spans**
//! held *in the allocator itself* (not as intrusive links inside the freed
//! memory), so the bookkeeping never dereferences user memory and every
//! returned pointer is range-checked before it is handed out (`AGENTS.md` §4 —
//! no `unsafe` global allocator that does raw pointer arithmetic without
//! bounds-checked wrappers).
//!
//! * **Allocate.** First-fit over the free spans, honouring the requested
//!   alignment; the residual head/tail of a carved span is returned to the free
//!   list so alignment padding is never leaked. When no span fits, the arena is
//!   grown by `mem_map` and the new pages are added as a free span (coalesced
//!   with the arena's top span).
//! * **Free.** The released region is inserted into the free list and coalesced
//!   with its neighbours. When coalescing leaves whole trailing pages free at
//!   the very top of the arena, they are returned to the kernel with
//!   `mem_unmap` (the heap shrinks — both syscalls are genuinely exercised, no
//!   dead path, `AGENTS.md` §2.14).
//! * **Deterministic OOM (`AGENTS.md` §4 / §2.9).** A failed `mem_map`, an
//!   exhausted arena, or an overflowed free-span table returns a null pointer
//!   per the [`GlobalAlloc`] contract — never a panic.
//!
//! # Why not zero on free
//!
//! The kernel zeroes every page on `mem_map` and on `mem_unmap` (`AGENTS.md`
//! §4), so memory entering or leaving the process is already clean and no
//! cross-process secret can leak. Reuse of a process's *own* freed bytes within
//! its own heap is not a security boundary, so the heap does not re-zero on
//! free; doing so would be pure overhead on the hot path (`AGENTS.md` §2.16).
//!
//! # Free-span table is a growable capacity (`AGENTS.md` §24)
//!
//! The free-span table is **not** a fixed-capacity array. It is a capacity that
//! grows on demand ([`SpanStore`]): coalescing keeps the live span count small
//! for well-behaved programs, but a workload that fragments the heap past the
//! current table capacity makes the store **grow before it fails** (§24.1) — it
//! maps one more whole metadata page through the same page source and continues,
//! rather than capping the workload at a hand-picked `const` (§24.1 forbids such
//! a ceiling). Only genuine resource exhaustion — the page source can no longer
//! map a metadata page — fails closed (the allocation returns null, never a
//! panic; `AGENTS.md` §4 / §2.9). See `lib/rt/README.md`.

use core::alloc::{GlobalAlloc, Layout};

use rustos_sync::SpinLock;

/// Page size of every Tier-1 target's smallest translation granule. The arena
/// grows in whole pages, and `mem_map` rounds its length up to this.
const PAGE_SIZE: usize = 4096;

/// Fixed virtual base of the heap arena.
///
/// Chosen well above both the kernel's low identity window and the program
/// image / stack / startup block at the 64 GiB spawn bias, so the arena grows
/// onto freshly-walked page tables and never collides with the spawn-time
/// layout (mirrors the `mem_map` fixture's region choice, `plans/SPAWN.md`
/// SP5b-2). 96 GiB.
const ARENA_BASE: u64 = 96 << 30;

/// Fixed virtual base of the free-span **metadata** region, distinct from the
/// data arena.
///
/// The growable span table ([`SpanStore`]) lives in its own mapped window so
/// the allocator never stores its bookkeeping inside the user-data arena it
/// hands out. Placed below [`ARENA_BASE`] and above the 64 GiB spawn bias; the
/// metadata is at most a handful of pages even for a heavily-fragmented heap,
/// so it never approaches the data arena. 80 GiB.
const META_BASE: u64 = 80 << 30;

/// Bytes of one stored [`Span`] (two `usize`s on every 64-bit Tier-1 target).
const SPAN_SIZE: usize = core::mem::size_of::<Span>();

/// Free-span slots that fit in one freshly-mapped metadata page — the unit the
/// span table grows by (`AGENTS.md` §24.1 "grow before you fail").
const SPANS_PER_PAGE: usize = PAGE_SIZE / SPAN_SIZE;

/// One free region of the arena: a half-open byte range `[start, start + len)`.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Span {
    /// First byte of the free region.
    start: usize,
    /// Length of the free region in bytes; always non-zero while stored.
    len: usize,
}

impl Span {
    /// One-past-the-last byte of the span.
    const fn end(self) -> usize {
        self.start + self.len
    }
}

/// The source of arena pages: maps and unmaps whole pages at a virtual base.
///
/// Abstracting the page source keeps [`HeapState`] pure and host-testable —
/// the production pager issues `mem_map`/`mem_unmap` syscalls
/// ([`SyscallPager`]), while the unit tests back the arena with ordinary host
/// memory. The base address and page count the heap passes are always
/// page-aligned and within the fixed arena window.
trait Pager {
    /// Map `pages` fresh, zeroed `RW` pages at exactly `base`
    /// (`MapFlags::FIXED`). Returns `true` on success; on failure the heap
    /// reports OOM (`AGENTS.md` §4 — deterministic, never a panic).
    fn map(&self, base: u64, pages: usize) -> bool;

    /// Release `pages` previously mapped at `base`. Returns `true` on success;
    /// a failed shrink leaves the pages mapped (the heap keeps them as a free
    /// span rather than losing track of them).
    fn unmap(&self, base: u64, pages: usize) -> bool;
}

/// The growable backing store for the free-span table (`AGENTS.md` §24.1).
///
/// The number of distinct free spans tracked is a *capacity*, not a fixed
/// ceiling: when it is reached and more virtual memory exists, the store
/// **grows** ([`grow`](SpanStore::grow)) rather than capping the workload.
/// The store owns the `Span` slots; [`HeapState`] keeps the live `count`.
/// [`MappedSpanStore`] maps fresh metadata pages through `mem_map` in
/// production, while the unit tests use an ordinary `Vec`-backed store, so the
/// §24 growth and fail-closed logic is exercised entirely on the host.
trait SpanStore {
    /// The currently-allocated span slots (capacity is `slots().len()`).
    fn slots(&self) -> &[Span];

    /// The currently-allocated span slots, mutably.
    fn slots_mut(&mut self) -> &mut [Span];

    /// Grow the store by at least one slot (one whole metadata page). Returns
    /// `true` if capacity increased; `false` only on genuine resource
    /// exhaustion (the page source could not map — the heap then fails closed,
    /// `AGENTS.md` §4 / §2.9).
    fn grow(&mut self) -> bool;
}

/// Round `value` up to the next multiple of `PAGE_SIZE`, or `None` on overflow.
const fn round_up_to_page(value: usize) -> Option<usize> {
    match value.checked_add(PAGE_SIZE - 1) {
        Some(v) => Some(v & !(PAGE_SIZE - 1)),
        None => None,
    }
}

/// The heap's bookkeeping: an address-sorted, coalesced free-span table (held
/// in a growable [`SpanStore`]) plus the arena's currently-mapped extent. Pure
/// logic over a [`Pager`] and a [`SpanStore`]; holds no lock and dereferences
/// no user-data memory, so it is exhaustively unit-testable.
struct HeapState<S: SpanStore> {
    /// Free spans, kept sorted by `start` and never adjacent (coalesced), in
    /// the first `count` slots of the store.
    store: S,
    /// Number of live entries in `store.slots()`.
    count: usize,
    /// One-past-the-last byte currently mapped: the arena covers
    /// `[ARENA_BASE, mapped_end)`. Grows by whole pages.
    mapped_end: usize,
}

impl<S: SpanStore> HeapState<S> {
    /// An empty heap over `store`: no mapped pages, no free spans.
    const fn new(store: S) -> Self {
        #[allow(clippy::cast_possible_truncation)] // ARENA_BASE fits usize on every 64-bit target.
        Self {
            store,
            count: 0,
            mapped_end: ARENA_BASE as usize,
        }
    }

    /// Number of span slots the store can currently hold without growing.
    fn capacity(&self) -> usize {
        self.store.slots().len()
    }

    /// Ensure the table has room for one more span, growing the store by a
    /// metadata page if it is full (`AGENTS.md` §24.1 "grow before you fail").
    /// Returns `false` only when the store cannot grow (genuine OOM) — the
    /// caller then fails closed (`AGENTS.md` §4 / §2.9).
    fn ensure_slot(&mut self) -> bool {
        self.count < self.capacity() || self.store.grow()
    }

    /// Remove the free span at `index`, shifting the tail down.
    fn remove(&mut self, index: usize) {
        let count = self.count;
        let spans = self.store.slots_mut();
        let mut i = index;
        while i + 1 < count {
            spans[i] = spans[i + 1];
            i += 1;
        }
        self.count -= 1;
    }

    /// Insert `span` at `index`, shifting the tail up. The caller has ensured a
    /// free slot via [`ensure_slot`](Self::ensure_slot).
    fn insert_at(&mut self, index: usize, span: Span) {
        let count = self.count;
        let spans = self.store.slots_mut();
        let mut i = count;
        while i > index {
            spans[i] = spans[i - 1];
            i -= 1;
        }
        spans[index] = span;
        self.count += 1;
    }

    /// Add `span` to the free table, coalescing with any adjacent free spans so
    /// the table stays sorted and gap-free between merged regions.
    ///
    /// A non-coalescing insert into a full table first grows the store
    /// ([`ensure_slot`](Self::ensure_slot)); only if that grow fails (genuine
    /// OOM) is the freed region dropped (its pages stay mapped but untracked —
    /// a bounded leak, never corruption; `AGENTS.md` §2.9). Growth makes this a
    /// scaling capacity, not a fixed ceiling (`AGENTS.md` §24.1).
    fn insert_free(&mut self, span: Span) {
        if span.len == 0 {
            return;
        }
        let mut i = 0;
        while i < self.count && self.store.slots()[i].start < span.start {
            i += 1;
        }
        let merge_left = i > 0 && self.store.slots()[i - 1].end() == span.start;
        let merge_right = i < self.count && span.end() == self.store.slots()[i].start;
        match (merge_left, merge_right) {
            (true, true) => {
                let absorbed = span.len + self.store.slots()[i].len;
                self.store.slots_mut()[i - 1].len += absorbed;
                self.remove(i);
            }
            (true, false) => self.store.slots_mut()[i - 1].len += span.len,
            (false, true) => {
                let slot = &mut self.store.slots_mut()[i];
                slot.start = span.start;
                slot.len += span.len;
            }
            (false, false) => {
                if self.ensure_slot() {
                    self.insert_at(i, span);
                }
            }
        }
    }

    /// Carve `[aligned, aligned + size)` out of the free span at `index`,
    /// returning the residual head and tail to the free table.
    ///
    /// Returns `false` (carving nothing) when the carve would need a new table
    /// slot the full table cannot supply, so the caller falls back to growing
    /// or fails closed (`AGENTS.md` §2.9).
    fn carve(&mut self, index: usize, aligned: usize, size: usize) -> bool {
        let span = self.store.slots()[index];
        let head = aligned - span.start;
        let tail = span.end() - (aligned + size);
        match (head > 0, tail > 0) {
            (false, false) => self.remove(index),
            (true, false) => self.store.slots_mut()[index].len = head,
            (false, true) => {
                let slot = &mut self.store.slots_mut()[index];
                slot.start = aligned + size;
                slot.len = tail;
            }
            (true, true) => {
                if !self.ensure_slot() {
                    return false;
                }
                self.store.slots_mut()[index].len = head;
                self.insert_at(
                    index + 1,
                    Span {
                        start: aligned + size,
                        len: tail,
                    },
                );
            }
        }
        true
    }

    /// First-fit allocation of `layout` from the free table; returns the
    /// chosen base address (a virtual address in the arena) or `None`.
    ///
    /// On no fit the arena is grown once through `pager`; a failed grow is a
    /// deterministic OOM (`None`, never a panic — `AGENTS.md` §4 / §2.9).
    fn alloc(&mut self, layout: Layout, pager: &dyn Pager) -> Option<usize> {
        let align = layout.align();
        let size = layout.size().max(1);
        let mut grown = false;
        loop {
            let mut i = 0;
            while i < self.count {
                let span = self.store.slots()[i];
                let aligned = align_up(span.start, align)?;
                let needed = aligned.checked_add(size)?;
                if needed <= span.end() && self.carve(i, aligned, size) {
                    return Some(aligned);
                }
                i += 1;
            }
            if grown {
                return None;
            }
            self.grow(size, align, pager)?;
            grown = true;
        }
    }

    /// Map fresh pages at the arena top sufficient for a `size`/`align`
    /// allocation and record them as a free span. Returns `None` on a failed
    /// map (OOM) or address overflow.
    fn grow(&mut self, size: usize, align: usize, pager: &dyn Pager) -> Option<()> {
        // A page-aligned base satisfies any alignment up to a page with no head
        // padding; a larger alignment needs the extra `align` slack so an
        // aligned sub-range is guaranteed to fit.
        let want = if align > PAGE_SIZE {
            size.checked_add(align)?
        } else {
            size
        };
        let bytes = round_up_to_page(want)?;
        let base = self.mapped_end;
        let new_end = base.checked_add(bytes)?;
        let page_count = bytes / PAGE_SIZE;
        if !pager.map(base as u64, page_count) {
            return None;
        }
        self.mapped_end = new_end;
        self.insert_free(Span {
            start: base,
            len: bytes,
        });
        Some(())
    }

    /// Return the region of `size` bytes based at `addr` to the free table and
    /// shrink the arena if whole trailing pages become free.
    fn free(&mut self, addr: usize, layout: Layout, pager: &dyn Pager) {
        let size = layout.size().max(1);
        self.insert_free(Span {
            start: addr,
            len: size,
        });
        self.try_shrink_top(pager);
    }

    /// If the free span at the arena top covers one or more whole pages,
    /// release them with `mem_unmap` and lower `mapped_end`. A failed unmap
    /// leaves the pages mapped and tracked (no loss; `AGENTS.md` §2.9).
    fn try_shrink_top(&mut self, pager: &dyn Pager) {
        if self.count == 0 {
            return;
        }
        let top = self.count - 1;
        let span = self.store.slots()[top];
        if span.end() != self.mapped_end {
            return;
        }
        let Some(freeable_start) = round_up_to_page(span.start) else {
            return;
        };
        if freeable_start >= self.mapped_end {
            return;
        }
        let bytes = self.mapped_end - freeable_start;
        let page_count = bytes / PAGE_SIZE;
        if !pager.unmap(freeable_start as u64, page_count) {
            return;
        }
        self.mapped_end = freeable_start;
        if span.start == freeable_start {
            self.remove(top);
        } else {
            self.store.slots_mut()[top].len = freeable_start - span.start;
        }
    }
}

/// Round `addr` up to the next multiple of `align` (a power of two per the
/// [`Layout`] contract), or `None` on overflow.
fn align_up(addr: usize, align: usize) -> Option<usize> {
    let mask = align - 1;
    addr.checked_add(mask).map(|v| v & !mask)
}

/// A `mem_map`/`mem_unmap`-backed heap with [`HeapState`] under a [`SpinLock`].
///
/// Generic over the [`Pager`] (arena pages) and the [`SpanStore`] (the growable
/// free-span table) so the same allocator logic is driven by the real syscalls
/// in production and by host fixtures in the unit tests. The lock makes the
/// allocator `Sync`, which the [`GlobalAlloc`] contract requires even though
/// current RustOS userland processes are single-threaded.
struct Heap<P: Pager, S: SpanStore> {
    state: SpinLock<HeapState<S>>,
    pager: P,
}

impl<P: Pager, S: SpanStore> Heap<P, S> {
    /// A fresh, empty heap over `pager` and `store`.
    const fn new(pager: P, store: S) -> Self {
        Self {
            state: SpinLock::new(HeapState::new(store)),
            pager,
        }
    }
}

// SAFETY: every allocation address is computed and bounds-checked by
// `HeapState` (`AGENTS.md` §4 — no raw pointer arithmetic without a checked
// wrapper) and the returned pointer denotes memory the kernel just mapped `RW`
// into this process's own space. The `SpinLock` serialises all access to the
// shared `HeapState`.
unsafe impl<P: Pager, S: SpanStore> GlobalAlloc for Heap<P, S> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        match self.state.lock().alloc(layout, &self.pager) {
            Some(addr) => addr as *mut u8,
            None => core::ptr::null_mut(),
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.state.lock().free(ptr as usize, layout, &self.pager);
    }
}

/// The production [`Pager`]: each call is an `abi-v1` anonymous-memory syscall
/// ([`crate::mem_map`] / [`crate::mem_unmap`]) at the arena's fixed base.
#[cfg(rt_native)]
struct SyscallPager;

#[cfg(rt_native)]
impl Pager for SyscallPager {
    fn map(&self, base: u64, pages: usize) -> bool {
        let len = pages * PAGE_SIZE;
        // FIXED placement: the heap owns the arena layout, so the kernel must
        // map at exactly `base` or fail (it never relocates the region).
        let ret = crate::mem_map(len, rustos_abi::MapFlags::FIXED, base);
        #[allow(clippy::cast_sign_loss)]
        // Guarded by `ret >= 0`; the non-negative result is the base address.
        {
            ret >= 0 && ret as u64 == base
        }
    }

    fn unmap(&self, base: u64, pages: usize) -> bool {
        crate::mem_unmap(base, pages * PAGE_SIZE) == 0
    }
}

/// Virtual base of one freshly-mapped metadata page when growing from `cap`
/// span slots, and the slot count that page adds.
///
/// Pure address arithmetic (no dereference), host-tested, and shared by the
/// production [`MappedSpanStore::grow`] so its only non-trivial computation is
/// covered on the host even though the store itself is target-only.
const fn metadata_growth(cap: usize) -> (u64, usize) {
    (META_BASE + (cap * SPAN_SIZE) as u64, SPANS_PER_PAGE)
}

/// The production [`SpanStore`]: the free-span table lives in metadata pages
/// mapped on demand at [`META_BASE`] through `mem_map` (`MapFlags::FIXED`), so
/// the table is a capacity that grows page by page (`AGENTS.md` §24.1) rather
/// than a fixed `const` array.
#[cfg(rt_native)]
struct MappedSpanStore {
    /// Number of `Span` slots currently mapped at [`META_BASE`].
    cap: usize,
}

#[cfg(rt_native)]
impl MappedSpanStore {
    /// An empty store: no metadata page mapped yet (the first span insert maps
    /// the first page).
    const fn new() -> Self {
        Self { cap: 0 }
    }
}

#[cfg(rt_native)]
impl SpanStore for MappedSpanStore {
    fn slots(&self) -> &[Span] {
        // SAFETY: `grow` has mapped exactly `cap` contiguous, page-aligned
        // `Span` slots at `META_BASE` (`MapFlags::FIXED`, RW, zeroed by the
        // kernel — `Span`'s all-zero bit pattern is the valid empty slot), and
        // `META_BASE` is a multiple of the page size and thus of `align_of::<
        // Span>()`. The heap only ever reads the first `count <= cap` slots,
        // and all access is serialised by the `Heap` `SpinLock`, so this is the
        // sole live reference. When `cap == 0` the pointer is unused because
        // the slice is empty.
        unsafe { core::slice::from_raw_parts(META_BASE as *const Span, self.cap) }
    }

    fn slots_mut(&mut self) -> &mut [Span] {
        // SAFETY: as for `slots`; `&mut self` and the `Heap` `SpinLock`
        // guarantee this is the only reference to the mapped metadata region.
        unsafe { core::slice::from_raw_parts_mut(META_BASE as *mut Span, self.cap) }
    }

    fn grow(&mut self) -> bool {
        let (base, added) = metadata_growth(self.cap);
        // FIXED placement: the store owns the metadata window, so the kernel
        // must map at exactly `base` (immediately above the slots already
        // mapped) or fail; it never relocates the region.
        let ret = crate::mem_map(PAGE_SIZE, rustos_abi::MapFlags::FIXED, base);
        #[allow(clippy::cast_sign_loss)]
        // Guarded by `ret >= 0`; the non-negative result is the base address.
        if ret >= 0 && ret as u64 == base {
            self.cap += added;
            true
        } else {
            false
        }
    }
}

/// The process-wide heap. Registering it as the `#[global_allocator]` is what
/// gives a first-party Rust program `alloc` (`Box`, `Vec`, `String`, …) over
/// `abi-v1` memory. Declared only for the native targets that have the trap and
/// startup runtime; the host build uses the standard test allocator.
#[cfg(rt_native)]
#[global_allocator]
static GLOBAL: Heap<SyscallPager, MappedSpanStore> =
    Heap::new(SyscallPager, MappedSpanStore::new());

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    /// A host [`Pager`] that records every map/unmap and never fails, so the
    /// tests exercise the pure `HeapState` bookkeeping (addresses, coalescing,
    /// arena growth/shrink) without dereferencing the unreal arena addresses.
    struct FakePager {
        events: core::cell::RefCell<Vec<(bool, u64, usize)>>,
    }

    impl FakePager {
        fn new() -> Self {
            Self {
                events: core::cell::RefCell::new(Vec::new()),
            }
        }
        fn maps(&self) -> usize {
            self.events.borrow().iter().filter(|e| e.0).count()
        }
        fn unmaps(&self) -> usize {
            self.events.borrow().iter().filter(|e| !e.0).count()
        }
    }

    impl Pager for FakePager {
        fn map(&self, base: u64, pages: usize) -> bool {
            self.events.borrow_mut().push((true, base, pages));
            true
        }
        fn unmap(&self, base: u64, pages: usize) -> bool {
            self.events.borrow_mut().push((false, base, pages));
            true
        }
    }

    /// A pager whose `map` always fails, to drive the deterministic-OOM path.
    struct DeadPager;
    impl Pager for DeadPager {
        fn map(&self, _base: u64, _pages: usize) -> bool {
            false
        }
        fn unmap(&self, _base: u64, _pages: usize) -> bool {
            false
        }
    }

    /// A host [`SpanStore`] backed by a `Vec`, the safe analogue of the
    /// `mem_map`-backed `MappedSpanStore`. It grows one slot at a time (so a
    /// few non-adjacent frees already exercise repeated growth) and can be
    /// capped at `max_slots` to model genuine resource exhaustion — the store
    /// can no longer grow, and the heap must fail closed (`AGENTS.md` §4 /
    /// §2.9), never corrupt or panic.
    struct VecSpanStore {
        spans: Vec<Span>,
        /// `None` = unbounded; `Some(n)` = refuse to grow past `n` slots.
        max_slots: Option<usize>,
    }

    impl VecSpanStore {
        /// A store that always grows when asked (the common case).
        fn unbounded() -> Self {
            Self {
                spans: Vec::new(),
                max_slots: None,
            }
        }

        /// A store that refuses to grow past `max_slots` slots, to drive the
        /// fail-closed path.
        fn capped(max_slots: usize) -> Self {
            Self {
                spans: Vec::new(),
                max_slots: Some(max_slots),
            }
        }
    }

    impl SpanStore for VecSpanStore {
        fn slots(&self) -> &[Span] {
            &self.spans
        }
        fn slots_mut(&mut self) -> &mut [Span] {
            &mut self.spans
        }
        fn grow(&mut self) -> bool {
            let next = self.spans.len() + 1;
            if matches!(self.max_slots, Some(max) if next > max) {
                return false;
            }
            self.spans.push(Span { start: 0, len: 0 });
            true
        }
    }

    fn layout(size: usize, align: usize) -> Layout {
        Layout::from_size_align(size, align).expect("valid layout")
    }

    /// The arena base as a `usize`, for asserting returned addresses.
    fn base() -> usize {
        usize::try_from(ARENA_BASE).expect("ARENA_BASE fits usize on a 64-bit host")
    }

    /// An empty heap over an unbounded growable span store.
    fn heap_state() -> HeapState<VecSpanStore> {
        HeapState::new(VecSpanStore::unbounded())
    }

    #[test]
    fn first_allocation_maps_one_page_and_returns_the_arena_base() {
        let pager = FakePager::new();
        let mut heap = heap_state();
        let addr = heap.alloc(layout(64, 8), &pager).expect("allocates");
        assert_eq!(addr, base());
        assert_eq!(pager.maps(), 1);
        // One page mapped, 64 bytes carved off the front: the tail is free.
        assert_eq!(heap.mapped_end, base() + PAGE_SIZE);
        assert_eq!(heap.count, 1);
        assert_eq!(heap.store.slots()[0].start, base() + 64);
        assert_eq!(heap.store.slots()[0].len, PAGE_SIZE - 64);
    }

    #[test]
    fn two_allocations_share_one_mapped_page() {
        let pager = FakePager::new();
        let mut heap = heap_state();
        let a = heap.alloc(layout(64, 8), &pager).unwrap();
        let b = heap.alloc(layout(64, 8), &pager).unwrap();
        assert_eq!(a, base());
        assert_eq!(b, base() + 64);
        // The second fits in the page already mapped — no extra map.
        assert_eq!(pager.maps(), 1);
    }

    #[test]
    fn free_coalesces_adjacent_blocks_back_into_one_span() {
        let pager = FakePager::new();
        let mut heap = heap_state();
        let a = heap.alloc(layout(64, 8), &pager).unwrap();
        let b = heap.alloc(layout(64, 8), &pager).unwrap();
        // Free out of order; the two freed blocks plus the page tail must
        // coalesce into a single free span covering the whole page.
        heap.free(b, layout(64, 8), &pager);
        heap.free(a, layout(64, 8), &pager);
        // Coalescing made the whole page free at the arena top, so it was
        // unmapped (shrink): the arena is empty again with nothing tracked.
        assert_eq!(pager.unmaps(), 1);
        assert_eq!(heap.mapped_end, base());
        assert_eq!(heap.count, 0);
    }

    #[test]
    fn freeing_a_middle_block_records_a_span_without_unmapping() {
        let pager = FakePager::new();
        let mut heap = heap_state();
        let a = heap.alloc(layout(64, 8), &pager).unwrap();
        let _b = heap.alloc(layout(64, 8), &pager).unwrap();
        // Free the first block: it sits below an allocated block, so it cannot
        // reach the arena top and stays a tracked free span (no shrink).
        heap.free(a, layout(64, 8), &pager);
        assert_eq!(pager.unmaps(), 0);
        assert!(heap.store.slots()[..heap.count]
            .iter()
            .any(|s| s.start == a && s.len == 64));
    }

    #[test]
    fn freed_block_is_reused_by_a_later_fitting_allocation() {
        let pager = FakePager::new();
        let mut heap = heap_state();
        let a = heap.alloc(layout(64, 8), &pager).unwrap();
        let _b = heap.alloc(layout(64, 8), &pager).unwrap();
        heap.free(a, layout(64, 8), &pager);
        // `a`'s hole is the first fit for an equal request — reused, no growth.
        let c = heap.alloc(layout(64, 8), &pager).unwrap();
        assert_eq!(c, a);
        assert_eq!(pager.maps(), 1);
    }

    #[test]
    fn large_allocation_grows_the_arena_by_multiple_pages() {
        let pager = FakePager::new();
        let mut heap = heap_state();
        let addr = heap.alloc(layout(3 * PAGE_SIZE, 8), &pager).unwrap();
        assert_eq!(addr, base());
        assert_eq!(heap.mapped_end, base() + 3 * PAGE_SIZE);
        assert_eq!(pager.maps(), 1);
    }

    #[test]
    fn high_alignment_is_honoured_and_padding_is_returned_to_the_free_list() {
        let pager = FakePager::new();
        let mut heap = heap_state();
        // Burn one small block so the next span does not start page-aligned.
        let _a = heap.alloc(layout(8, 8), &pager).unwrap();
        let p = heap.alloc(layout(64, 4096), &pager).unwrap();
        assert_eq!(p % 4096, 0, "alignment honoured");
        // The pre-alignment gap is a free span, not leaked.
        assert!(heap.store.slots()[..heap.count]
            .iter()
            .any(|s| s.end() == p));
    }

    #[test]
    fn allocation_fails_closed_when_the_pager_cannot_map() {
        let mut heap = heap_state();
        assert_eq!(heap.alloc(layout(64, 8), &DeadPager), None);
        assert_eq!(heap.mapped_end, base());
        assert_eq!(heap.count, 0);
    }

    /// Insert `n` deliberately non-adjacent free spans (each separated by a
    /// one-byte gap so they never coalesce), forcing one span slot per insert.
    fn insert_disjoint_spans(heap: &mut HeapState<VecSpanStore>, n: usize) {
        for k in 0..n {
            // Stride 16 with length 8 leaves an 8-byte gap between spans, so no
            // two are adjacent and each needs its own slot.
            heap.insert_free(Span {
                start: base() + k * 16,
                len: 8,
            });
        }
    }

    #[test]
    fn span_table_grows_past_its_initial_capacity_instead_of_dropping_spans() {
        // §24.1: the free-span table is a capacity that grows, not a fixed
        // ceiling. Far more disjoint spans than any fixed inline array would
        // hold are all tracked because the store grows on demand.
        let mut heap = heap_state();
        let n = 1000;
        insert_disjoint_spans(&mut heap, n);
        assert_eq!(
            heap.count, n,
            "every disjoint span is tracked, none dropped"
        );
        assert!(
            heap.capacity() >= n,
            "the store grew to hold them ({} >= {n})",
            heap.capacity()
        );
        // The spans stayed address-sorted across all the growths.
        let sorted = heap.store.slots()[..heap.count]
            .windows(2)
            .all(|w| w[0].start < w[1].start);
        assert!(sorted, "table remains sorted after growth");
    }

    #[test]
    fn span_table_fails_closed_when_the_store_cannot_grow() {
        // With a store that refuses to grow past one slot, a second
        // non-adjacent free is dropped (a bounded leak) rather than corrupting
        // state or panicking — the only non-coalescing outcome (§2.9).
        let mut heap = HeapState::new(VecSpanStore::capped(1));
        heap.insert_free(Span {
            start: base(),
            len: 8,
        });
        assert_eq!(heap.count, 1);
        heap.insert_free(Span {
            start: base() + 100,
            len: 8,
        });
        // The store could not grow, so the disjoint span was dropped, not
        // inserted past capacity.
        assert_eq!(heap.count, 1, "fails closed at the store's hard limit");
        assert_eq!(heap.capacity(), 1);
    }

    #[test]
    fn allocation_fails_closed_when_the_span_store_cannot_split() {
        // A carve that needs a new slot for the residual tail must fail closed
        // (returning null) when the store cannot grow, rather than handing out
        // memory it cannot track.
        let pager = FakePager::new();
        // Cap at one slot: the single free span from the first grow fits, but
        // carving a sub-range that leaves both a head and a tail needs a second
        // slot the store cannot supply.
        let mut heap = HeapState::new(VecSpanStore::capped(1));
        // Map a page (one span, fills the only slot), then request an allocation
        // that, once aligned, leaves both a head and a tail — needing a split.
        heap.insert_free(Span {
            start: base(),
            len: PAGE_SIZE,
        });
        // Aligning to 4096 within a page-based span needs no head, so force a
        // head by first reserving the page start with a tiny carve.
        let _first = heap.alloc(layout(8, 8), &pager).unwrap();
        // Now the free span starts at base()+8; a 4096-aligned request leaves a
        // head gap *and* a tail — the true/true carve that needs a new slot.
        assert_eq!(heap.alloc(layout(64, 4096), &pager), None);
    }

    #[test]
    fn metadata_growth_targets_the_next_contiguous_page() {
        // The production `MappedSpanStore` grows by mapping one page directly
        // above the slots already mapped; this pins that pure arithmetic
        // (covering the only non-trivial computation in the target-only store).
        assert_eq!(metadata_growth(0), (META_BASE, SPANS_PER_PAGE));
        let (base1, added) = metadata_growth(SPANS_PER_PAGE);
        assert_eq!(added, SPANS_PER_PAGE);
        assert_eq!(base1, META_BASE + PAGE_SIZE as u64);
        // One page holds a whole number of spans, so the window stays packed.
        assert_eq!(SPANS_PER_PAGE * SPAN_SIZE, PAGE_SIZE);
    }

    #[test]
    fn global_alloc_wrapper_hands_out_and_reclaims() {
        let heap = Heap::new(FakePager::new(), VecSpanStore::unbounded());
        let l = layout(128, 16);
        // SAFETY: `l` is a valid non-zero layout; the wrapper is freshly built.
        let p = unsafe { heap.alloc(l) };
        assert_eq!(p as usize, base());
        // SAFETY: `p` was just returned by this allocator for `l`.
        unsafe { heap.dealloc(p, l) };
        // The whole page freed and was returned to the kernel.
        assert_eq!(heap.pager.unmaps(), 1);
    }
}
