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
//! # Documented limit
//!
//! The free-span table is a fixed-capacity array ([`MAX_SPANS`]). Coalescing
//! keeps the live span count small for well-behaved programs; a workload that
//! fragments the heap beyond the table's capacity fails closed (the allocation
//! returns null) rather than corrupting state. This is the heap's analogue of
//! the boot bump allocator's fixed `HEAP_BYTES` ceiling and is documented in
//! `lib/rt/README.md`.

use core::alloc::{GlobalAlloc, Layout};

use rustos_sync::SpinLock;

/// Page size of every Tier-1 target's smallest translation granule. The arena
/// grows in whole pages, and `mem_map` rounds its length up to this.
const PAGE_SIZE: usize = 4096;

/// Maximum number of distinct free spans the heap tracks at once.
///
/// Coalescing keeps the live count far below this for ordinary allocation
/// traffic; a workload that fragments past it fails closed (see the
/// module-level "Documented limit"). 256 spans is generous for the first
/// userland programs (the shell, `init`) while keeping the allocator's state a
/// fixed, small `.bss` footprint.
const MAX_SPANS: usize = 256;

/// Fixed virtual base of the heap arena.
///
/// Chosen well above both the kernel's low identity window and the program
/// image / stack / startup block at the 64 GiB spawn bias, so the arena grows
/// onto freshly-walked page tables and never collides with the spawn-time
/// layout (mirrors the `mem_map` fixture's region choice, `plans/SPAWN.md`
/// SP5b-2). 96 GiB.
const ARENA_BASE: u64 = 96 << 30;

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

/// Round `value` up to the next multiple of `PAGE_SIZE`, or `None` on overflow.
const fn round_up_to_page(value: usize) -> Option<usize> {
    match value.checked_add(PAGE_SIZE - 1) {
        Some(v) => Some(v & !(PAGE_SIZE - 1)),
        None => None,
    }
}

/// The heap's bookkeeping: an address-sorted, coalesced free-span table plus
/// the arena's currently-mapped extent. Pure logic over a [`Pager`]; holds no
/// lock and dereferences no user memory, so it is exhaustively unit-testable.
struct HeapState {
    /// Free spans, kept sorted by `start` and never adjacent (coalesced).
    spans: [Span; MAX_SPANS],
    /// Number of live entries in `spans`.
    count: usize,
    /// One-past-the-last byte currently mapped: the arena covers
    /// `[ARENA_BASE, mapped_end)`. Grows by whole pages.
    mapped_end: usize,
}

impl HeapState {
    /// An empty heap: no mapped pages, no free spans.
    const fn new() -> Self {
        #[allow(clippy::cast_possible_truncation)] // ARENA_BASE fits usize on every 64-bit target.
        Self {
            spans: [Span { start: 0, len: 0 }; MAX_SPANS],
            count: 0,
            mapped_end: ARENA_BASE as usize,
        }
    }

    /// Remove the free span at `index`, shifting the tail down.
    fn remove(&mut self, index: usize) {
        let mut i = index;
        while i + 1 < self.count {
            self.spans[i] = self.spans[i + 1];
            i += 1;
        }
        self.count -= 1;
    }

    /// Insert `span` at `index`, shifting the tail up. The caller has checked
    /// `self.count < MAX_SPANS`.
    fn insert_at(&mut self, index: usize, span: Span) {
        let mut i = self.count;
        while i > index {
            self.spans[i] = self.spans[i - 1];
            i -= 1;
        }
        self.spans[index] = span;
        self.count += 1;
    }

    /// Add `span` to the free table, coalescing with any adjacent free spans so
    /// the table stays sorted and gap-free between merged regions.
    ///
    /// If the freed region is adjacent to no existing span and the table is
    /// full, the region is dropped (its pages stay mapped but untracked — a
    /// bounded leak, never corruption; see the module "Documented limit"). This
    /// is the only non-coalescing outcome and is unreachable for the
    /// allocation traffic the first userland programs produce.
    fn insert_free(&mut self, span: Span) {
        if span.len == 0 {
            return;
        }
        let mut i = 0;
        while i < self.count && self.spans[i].start < span.start {
            i += 1;
        }
        let merge_left = i > 0 && self.spans[i - 1].end() == span.start;
        let merge_right = i < self.count && span.end() == self.spans[i].start;
        match (merge_left, merge_right) {
            (true, true) => {
                self.spans[i - 1].len += span.len + self.spans[i].len;
                self.remove(i);
            }
            (true, false) => self.spans[i - 1].len += span.len,
            (false, true) => {
                self.spans[i].start = span.start;
                self.spans[i].len += span.len;
            }
            (false, false) => {
                if self.count < MAX_SPANS {
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
        let span = self.spans[index];
        let head = aligned - span.start;
        let tail = span.end() - (aligned + size);
        match (head > 0, tail > 0) {
            (false, false) => self.remove(index),
            (true, false) => self.spans[index].len = head,
            (false, true) => {
                self.spans[index].start = aligned + size;
                self.spans[index].len = tail;
            }
            (true, true) => {
                if self.count >= MAX_SPANS {
                    return false;
                }
                self.spans[index].len = head;
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
                let span = self.spans[i];
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
        let span = self.spans[top];
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
            self.spans[top].len = freeable_start - span.start;
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
/// Generic over the [`Pager`] so the same allocator logic is driven by the
/// real syscalls in production and by a host fixture in the unit tests. The
/// lock makes the allocator `Sync`, which the [`GlobalAlloc`] contract requires
/// even though current RustOS userland processes are single-threaded.
struct Heap<P: Pager> {
    state: SpinLock<HeapState>,
    pager: P,
}

impl<P: Pager> Heap<P> {
    /// A fresh, empty heap over `pager`.
    const fn new(pager: P) -> Self {
        Self {
            state: SpinLock::new(HeapState::new()),
            pager,
        }
    }
}

// SAFETY: every allocation address is computed and bounds-checked by
// `HeapState` (`AGENTS.md` §4 — no raw pointer arithmetic without a checked
// wrapper) and the returned pointer denotes memory the kernel just mapped `RW`
// into this process's own space. The `SpinLock` serialises all access to the
// shared `HeapState`.
unsafe impl<P: Pager> GlobalAlloc for Heap<P> {
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

/// The process-wide heap. Registering it as the `#[global_allocator]` is what
/// gives a first-party Rust program `alloc` (`Box`, `Vec`, `String`, …) over
/// `abi-v1` memory. Declared only for the native targets that have the trap and
/// startup runtime; the host build uses the standard test allocator.
#[cfg(rt_native)]
#[global_allocator]
static GLOBAL: Heap<SyscallPager> = Heap::new(SyscallPager);

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

    fn layout(size: usize, align: usize) -> Layout {
        Layout::from_size_align(size, align).expect("valid layout")
    }

    /// The arena base as a `usize`, for asserting returned addresses.
    fn base() -> usize {
        usize::try_from(ARENA_BASE).expect("ARENA_BASE fits usize on a 64-bit host")
    }

    #[test]
    fn first_allocation_maps_one_page_and_returns_the_arena_base() {
        let pager = FakePager::new();
        let mut heap = HeapState::new();
        let addr = heap.alloc(layout(64, 8), &pager).expect("allocates");
        assert_eq!(addr, base());
        assert_eq!(pager.maps(), 1);
        // One page mapped, 64 bytes carved off the front: the tail is free.
        assert_eq!(heap.mapped_end, base() + PAGE_SIZE);
        assert_eq!(heap.count, 1);
        assert_eq!(heap.spans[0].start, base() + 64);
        assert_eq!(heap.spans[0].len, PAGE_SIZE - 64);
    }

    #[test]
    fn two_allocations_share_one_mapped_page() {
        let pager = FakePager::new();
        let mut heap = HeapState::new();
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
        let mut heap = HeapState::new();
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
        let mut heap = HeapState::new();
        let a = heap.alloc(layout(64, 8), &pager).unwrap();
        let _b = heap.alloc(layout(64, 8), &pager).unwrap();
        // Free the first block: it sits below an allocated block, so it cannot
        // reach the arena top and stays a tracked free span (no shrink).
        heap.free(a, layout(64, 8), &pager);
        assert_eq!(pager.unmaps(), 0);
        assert!(heap.spans[..heap.count]
            .iter()
            .any(|s| s.start == a && s.len == 64));
    }

    #[test]
    fn freed_block_is_reused_by_a_later_fitting_allocation() {
        let pager = FakePager::new();
        let mut heap = HeapState::new();
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
        let mut heap = HeapState::new();
        let addr = heap.alloc(layout(3 * PAGE_SIZE, 8), &pager).unwrap();
        assert_eq!(addr, base());
        assert_eq!(heap.mapped_end, base() + 3 * PAGE_SIZE);
        assert_eq!(pager.maps(), 1);
    }

    #[test]
    fn high_alignment_is_honoured_and_padding_is_returned_to_the_free_list() {
        let pager = FakePager::new();
        let mut heap = HeapState::new();
        // Burn one small block so the next span does not start page-aligned.
        let _a = heap.alloc(layout(8, 8), &pager).unwrap();
        let p = heap.alloc(layout(64, 4096), &pager).unwrap();
        assert_eq!(p % 4096, 0, "alignment honoured");
        // The pre-alignment gap is a free span, not leaked.
        assert!(heap.spans[..heap.count].iter().any(|s| s.end() == p));
    }

    #[test]
    fn allocation_fails_closed_when_the_pager_cannot_map() {
        let mut heap = HeapState::new();
        assert_eq!(heap.alloc(layout(64, 8), &DeadPager), None);
        assert_eq!(heap.mapped_end, base());
        assert_eq!(heap.count, 0);
    }

    #[test]
    fn global_alloc_wrapper_hands_out_and_reclaims() {
        let heap = Heap::new(FakePager::new());
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
