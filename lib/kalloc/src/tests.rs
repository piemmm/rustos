//! Host unit tests for [`FreeListAllocator`].
//!
//! The tests drive the [`GlobalAlloc`] surface directly over a leaked
//! page-aligned arena — every allocation under test is served by the
//! allocator itself, never by the process heap — and assert the freeing
//! contract the kernel relies on: disjoint live blocks, reclamation on
//! free, coalescing of adjacent frees back into a single large hole, and
//! survival of sustained allocate/free churn in bounded memory (the
//! property the previous bump allocator lacked).
//!
//! Which tier a test exercises is decided by its layout, exactly as in
//! production: a request over [`PAGE_SIZE`] (or aligned above it) is the
//! byte-granular tier's, anything else the slab's. [`BYTE_TIER`] spells the
//! smallest byte-tier size, so a test of block splitting, coalescing, or
//! region growth cannot silently drift onto the slab.

use super::{
    class_size, objects_per_page, slab_class, FreeListAllocator, MIN_BLOCK, MIN_CLASS, SLAB_CLASSES,
};
use core::alloc::{GlobalAlloc, Layout};
use tairix_abi::PAGE_SIZE;

/// The smallest allocation the byte-granular tier serves.
const BYTE_TIER: usize = PAGE_SIZE + 1;

/// Build an allocator over a fresh, page-aligned, zeroed bootstrap region of
/// `bytes`.
///
/// The region is leaked rather than held on the test stack: the slab carves
/// whole pages out of it, so a realistic fixture is tens of kilobytes. Page
/// alignment matches the production `.bss` arena, so an over-aligned request
/// is not wasted on a half-aligned tail.
fn fixture(bytes: usize) -> FreeListAllocator {
    let layout = Layout::from_size_align(bytes, PAGE_SIZE).expect("valid arena layout");
    // SAFETY: `bytes` is non-zero in every fixture, so the layout has a
    // non-zero size.
    let base = unsafe { std::alloc::alloc_zeroed(layout) };
    assert!(!base.is_null(), "test arena");
    // SAFETY: the arena is never freed, so it outlives the allocator, and it
    // is page-aligned as `FreeListAllocator::new` requires.
    unsafe { FreeListAllocator::new(base, bytes) }
}

#[test]
fn alloc_hands_out_aligned_disjoint_blocks() {
    let alloc = fixture(1 << 16);
    let layout = Layout::from_size_align(48, 16).unwrap();
    // SAFETY: non-zero layout, fresh allocator.
    let a = unsafe { alloc.alloc(layout) };
    let b = unsafe { alloc.alloc(layout) };
    assert!(!a.is_null() && !b.is_null());
    assert_eq!(a as usize % 16, 0);
    assert_eq!(b as usize % 16, 0);
    // Disjoint: the two live blocks do not overlap.
    let (lo, hi) = if (a as usize) < (b as usize) {
        (a, b)
    } else {
        (b, a)
    };
    assert!(hi as usize >= lo as usize + 48);
}

#[test]
fn free_reclaims_so_the_same_byte_is_reused() {
    let alloc = fixture(1 << 16);
    let layout = Layout::from_size_align(BYTE_TIER, 8).unwrap();
    // SAFETY: non-zero layout.
    let a = unsafe { alloc.alloc(layout) };
    assert!(!a.is_null());
    assert!(alloc.used() >= BYTE_TIER);
    // SAFETY: `a` came from this allocator with `layout`.
    unsafe { alloc.dealloc(a, layout) };
    assert_eq!(alloc.used(), 0, "free must reclaim every byte");
    // The next same-size allocation reuses the freed region (first-fit from
    // the coalesced whole-heap hole returns the same address).
    // SAFETY: non-zero layout.
    let b = unsafe { alloc.alloc(layout) };
    assert_eq!(a, b, "a freed block must be handed back out");
}

#[test]
fn adjacent_frees_coalesce_into_one_large_hole() {
    // A heap that fits exactly three `block`-sized allocations; after
    // freeing all three (in an order that exercises forward and backward
    // coalescing) a single allocation of the whole span must succeed,
    // proving the three holes merged back into one.
    let block = BYTE_TIER;
    let alloc = fixture(1 << 16);
    let layout = Layout::from_size_align(block, 8).unwrap();
    // SAFETY: non-zero layout.
    let a = unsafe { alloc.alloc(layout) };
    let b = unsafe { alloc.alloc(layout) };
    let c = unsafe { alloc.alloc(layout) };
    assert!(!a.is_null() && !b.is_null() && !c.is_null());
    // Free the middle first, then the ends, so the middle coalesces with
    // each neighbour as it is freed.
    // SAFETY: each ptr came from this allocator with `layout`.
    unsafe {
        alloc.dealloc(b, layout);
        alloc.dealloc(a, layout);
        alloc.dealloc(c, layout);
    }
    assert_eq!(alloc.used(), 0);
    // A single allocation spanning all three blocks now fits — only
    // possible if they coalesced into one hole.
    let span = Layout::from_size_align(block * 3, 8).unwrap();
    // SAFETY: non-zero layout.
    let big = unsafe { alloc.alloc(span) };
    assert!(
        !big.is_null(),
        "coalesced hole must satisfy the merged span"
    );
}

#[test]
fn the_lock_masks_interrupts_via_the_installed_control() {
    use core::sync::atomic::{AtomicUsize, Ordering};

    // Recording interrupt-control hooks. `disable` returns a sentinel token
    // that `restore` asserts it received back; both bump a monotone counter.
    // A completed allocation then proves the allocator masks interrupts
    // around its lock (`disable` before, `restore` after) — the property that
    // forecloses the single-CPU self-deadlock a plain, non-interrupt-safe
    // allocator lock would suffer when an interrupt handler reenters `alloc`
    // on a CPU already holding the lock.
    //
    // The hooks are crate-global once installed, so every other test thread's
    // allocations run them too. The counters are therefore only ever read as
    // monotone lower bounds against a snapshot this thread took itself, and
    // the pairing is asserted inside `restore` where it cannot race.
    static DISABLES: AtomicUsize = AtomicUsize::new(0);
    static RESTORES: AtomicUsize = AtomicUsize::new(0);
    const TOKEN: usize = 0xC0FF_EE00;

    fn rec_disable() -> usize {
        DISABLES.fetch_add(1, Ordering::Relaxed);
        TOKEN
    }
    fn rec_restore(token: usize) {
        assert_eq!(token, TOKEN, "restore receives the token disable returned");
        RESTORES.fetch_add(1, Ordering::Relaxed);
    }
    fn never_disable() -> usize {
        panic!("a second install must be refused");
    }
    fn never_restore(_token: usize) {
        panic!("a second install must be refused");
    }

    let alloc = fixture(1 << 16);
    let layout = Layout::from_size_align(64, 8).unwrap();

    // Before any hook is installed the lock does not mask (the early-boot /
    // host / wasm32 window, which is single-CPU with interrupts already
    // masked, so no ISR can reenter). This test is the crate's only
    // installer, so nothing else can have raised the counter yet.
    // SAFETY: non-zero layout, fresh allocator.
    let p0 = unsafe { alloc.alloc(layout) };
    assert!(!p0.is_null());
    // SAFETY: `p0` came from this allocator with `layout`.
    unsafe { alloc.dealloc(p0, layout) };
    assert_eq!(
        DISABLES.load(Ordering::Relaxed),
        0,
        "no hook installed: the lock does not mask"
    );

    // Once installed, every lock hold masks then restores interrupts.
    crate::install_irq_control(rec_disable, rec_restore);
    let (d0, r0) = (
        DISABLES.load(Ordering::Relaxed),
        RESTORES.load(Ordering::Relaxed),
    );
    // SAFETY: non-zero layout.
    let p = unsafe { alloc.alloc(layout) };
    assert!(!p.is_null());
    // SAFETY: `p` came from this allocator with `layout`.
    unsafe { alloc.dealloc(p, layout) };

    assert!(
        DISABLES.load(Ordering::Relaxed) >= d0 + 2,
        "the alloc and the dealloc each mask interrupts around the lock"
    );
    assert!(
        RESTORES.load(Ordering::Relaxed) >= r0 + 2,
        "both of this thread's holds restored the prior interrupt state"
    );

    // The control describes the machine, not one heap: an allocator built
    // after the install and never published to any registry is interrupt-safe
    // too. Binding the hooks per instance instead left every heap the install
    // site had not been told about spinning on a plain lock — the
    // self-deadlock the freestanding test bins, which declare their own
    // `#[global_allocator]` and register it nowhere, were still exposed to.
    let unregistered = fixture(1 << 16);
    let (d1, r1) = (
        DISABLES.load(Ordering::Relaxed),
        RESTORES.load(Ordering::Relaxed),
    );
    // SAFETY: non-zero layout, fresh allocator.
    let q = unsafe { unregistered.alloc(layout) };
    assert!(!q.is_null());
    // SAFETY: `q` came from `unregistered` with `layout`.
    unsafe { unregistered.dealloc(q, layout) };
    assert!(
        DISABLES.load(Ordering::Relaxed) >= d1 + 2,
        "an allocator no registry knows about masks interrupts as well"
    );
    assert!(
        RESTORES.load(Ordering::Relaxed) >= r1 + 2,
        "and restores the prior state after each hold"
    );

    // Set-once: a later install is refused rather than swapping the live
    // pair, which could hand a holder mid-hold a `restore` that does not
    // match the `disable` it called. `never_*` panic if reached, and every
    // allocation in the process runs the installed hooks.
    crate::install_irq_control(never_disable, never_restore);
    let d2 = DISABLES.load(Ordering::Relaxed);
    // SAFETY: non-zero layout.
    let r = unsafe { alloc.alloc(layout) };
    assert!(!r.is_null());
    // SAFETY: `r` came from this allocator with `layout`.
    unsafe { alloc.dealloc(r, layout) };
    assert!(
        DISABLES.load(Ordering::Relaxed) >= d2 + 2,
        "the originally installed pair is still the one in force"
    );
}

#[test]
fn alloc_returns_null_when_exhausted_then_recovers_after_free() {
    // Small heap: a handful of blocks, then exhaustion → null (never a
    // panic). Freeing one block lets the next alloc succeed.
    let alloc = fixture(1 << 16);
    let layout = Layout::from_size_align(6 * 1024, 8).unwrap();
    let mut live = alloc::vec::Vec::new();
    loop {
        // SAFETY: non-zero layout.
        let p = unsafe { alloc.alloc(layout) };
        if p.is_null() {
            break;
        }
        live.push(p);
    }
    assert!(!live.is_empty(), "some allocations must have succeeded");
    // Exhausted: another alloc is null, not a panic.
    // SAFETY: non-zero layout.
    assert!(unsafe { alloc.alloc(layout) }.is_null());
    // Free one and re-allocate: the freed block is handed back.
    let freed = live.pop().unwrap();
    // SAFETY: `freed` came from this allocator with `layout`.
    unsafe { alloc.dealloc(freed, layout) };
    // SAFETY: non-zero layout.
    let reused = unsafe { alloc.alloc(layout) };
    assert_eq!(reused, freed, "exhaustion recovers after a free");
}

#[test]
fn honours_alignment_above_the_header() {
    let alloc = fixture(1 << 16);
    // Burn a small odd block so the next hole start is not already aligned.
    let small = Layout::from_size_align(BYTE_TIER + 8, 8).unwrap();
    // SAFETY: non-zero layout.
    let _ = unsafe { alloc.alloc(small) };
    let aligned = Layout::from_size_align(BYTE_TIER, 512).unwrap();
    // SAFETY: non-zero layout.
    let p = unsafe { alloc.alloc(aligned) };
    assert!(!p.is_null());
    assert_eq!(p as usize % 512, 0, "alloc must honour the requested align");
}

#[test]
fn churn_runs_in_bounded_memory() {
    // The defining freeing property: a long allocate/free loop must NOT
    // march `used` upward (the bump allocator's fatal flaw). A pseudo-random
    // mix of allocs and frees over many iterations keeps a bounded working
    // set; `used` returns to zero once everything is freed.
    let alloc = fixture(1 << 16);
    let mut live: alloc::vec::Vec<(*mut u8, Layout)> = alloc::vec::Vec::new();
    // Simple xorshift so the test is deterministic and dependency-free.
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..5_000 {
        let free = !live.is_empty() && next() % 2 == 0;
        if free {
            // Reduce the u64 PRNG word modulo the live count first, so the
            // value provably fits a `usize` on any target (no truncation).
            let idx = usize::try_from(next() % live.len() as u64).unwrap_or(0);
            let (p, l) = live.swap_remove(idx);
            // SAFETY: `p` came from this allocator with `l`.
            unsafe { alloc.dealloc(p, l) };
        } else {
            // `next() % 200` is in `0..200`, so it always fits a `usize`.
            let sz = 1 + usize::try_from(next() % 200).unwrap_or(0);
            // Every fifth request is a byte-tier one, so the churn crosses
            // both tiers rather than settling on either.
            let sz = if next() % 5 == 0 { sz + PAGE_SIZE } else { sz };
            let layout = Layout::from_size_align(sz, 8).unwrap();
            // SAFETY: non-zero layout.
            let p = unsafe { alloc.alloc(layout) };
            if !p.is_null() {
                // Touch the whole block to catch any overlap with a live one
                // (a double-hand-out would corrupt this write under Miri /
                // the address-disjointness assert below).
                // SAFETY: `p` owns `sz` writable bytes per the alloc contract.
                unsafe { core::ptr::write_bytes(p, 0xAB, sz) };
                live.push((p, layout));
            }
        }
    }
    // Free everything.
    for (p, l) in live.drain(..) {
        // SAFETY: each came from this allocator with its layout.
        unsafe { alloc.dealloc(p, l) };
    }
    // The whole heap is one coalesced hole again: a near-heap-sized
    // allocation succeeds. It can only be served if the slab's retained
    // spare pages came back first, so this is also the reclaim-before-refusal
    // path.
    let big = Layout::from_size_align((1 << 16) - MIN_BLOCK, 8).unwrap();
    // SAFETY: non-zero layout.
    let whole = unsafe { alloc.alloc(big) };
    assert!(
        !whole.is_null(),
        "post-churn heap must coalesce back to one large hole"
    );
    // SAFETY: `whole` came from this allocator with `big`.
    unsafe { alloc.dealloc(whole, big) };
    assert_eq!(alloc.used(), 0, "all memory reclaimed after churn");
}

#[test]
fn allocate_all_memory_then_free_it_all_reclaims_fully_every_round() {
    // The defining long-uptime property an OS allocator must hold: claim
    // *every* byte the heap can serve, release it all, and find the heap
    // exactly as empty and exactly as capacious as before — round after round,
    // with no drift, no stranding, and no leak. A bump allocator fails this on
    // round two; a freeing allocator passes every round identically.
    let alloc = fixture(1 << 16);
    let layout = Layout::from_size_align(64, 8).unwrap();

    let mut first_round_blocks: Option<usize> = None;
    let mut first_round_idle: Option<usize> = None;
    for round in 0..8 {
        if round == 0 {
            assert_eq!(alloc.used(), 0, "the first round starts on an empty heap");
        } else {
            // From the second round on the heap holds nothing but the one
            // page the exercised class keeps back, so the idle figure settles
            // and never drifts upward.
            match first_round_idle {
                None => first_round_idle = Some(alloc.used()),
                Some(idle) => assert_eq!(
                    alloc.used(),
                    idle,
                    "round {round} started holding more than the round before"
                ),
            }
        }
        // Claim every block the heap can serve until it is genuinely
        // exhausted (null, never a panic).
        let mut live = alloc::vec::Vec::new();
        loop {
            // SAFETY: non-zero layout.
            let p = unsafe { alloc.alloc(layout) };
            if p.is_null() {
                break;
            }
            // Touch the whole block so a double-hand-out would corrupt a live
            // neighbour (caught under Miri / by the count check below).
            // SAFETY: `p` owns 64 writable bytes per the alloc contract.
            unsafe { core::ptr::write_bytes(p, 0xCD, 64) };
            live.push(p);
        }
        let blocks = live.len();
        assert!(blocks > 0, "the heap must serve at least one block");
        // Exhausted: a further request fails closed rather than panicking.
        // SAFETY: non-zero layout.
        assert!(unsafe { alloc.alloc(layout) }.is_null());
        // Every round must serve the identical number of blocks: a leak or
        // progressive fragmentation would shrink this on a later round.
        match first_round_blocks {
            None => first_round_blocks = Some(blocks),
            Some(expected) => assert_eq!(
                blocks, expected,
                "round {round} served {blocks} blocks, expected {expected} — \
                 capacity must not drift across rounds"
            ),
        }
        // Release everything.
        for p in live.drain(..) {
            // SAFETY: each `p` came from this allocator with `layout`.
            unsafe { alloc.dealloc(p, layout) };
        }
    }
    // After all the churn the heap has coalesced back to a single hole: a
    // near-heap-sized allocation succeeds, proving no permanent fragmentation
    // and that the slab handed its retained page back rather than refuse.
    let big = Layout::from_size_align((1 << 16) - MIN_BLOCK, 8).unwrap();
    // SAFETY: non-zero layout.
    let whole = unsafe { alloc.alloc(big) };
    assert!(
        !whole.is_null(),
        "the heap must coalesce back to one large hole after every round"
    );
    // SAFETY: `whole` came from this allocator with `big`.
    unsafe { alloc.dealloc(whole, big) };
    assert_eq!(alloc.used(), 0, "every byte reclaimed once the slab drains");
}

#[test]
fn over_aligned_request_is_served_whatever_the_free_block_offset() {
    // Regression: a block start is only `ALIGN`-aligned, so a request aligned
    // *above* `ALIGN` can find its aligned payload sitting a sub-`MIN_BLOCK`
    // distance above the block base — a front remnant too small to become its
    // own free block. The allocator must advance a whole alignment stride and
    // still serve the request rather than skip the block: skipping stranded
    // the entire heap behind one large-but-misaligned hole, failing a
    // 656-byte over-aligned allocation with ~63 MiB free.
    //
    // Which relative offset triggers the stride bump depends on the header and
    // alignment arithmetic, so the whole range of leading offsets is swept
    // instead of one hand-computed case.
    for lead in 0..8usize {
        let alloc = fixture(1 << 16);
        // Push the surviving free block to a range of offsets past the
        // page-aligned base, one of which lands the aligned payload a
        // sub-`MIN_BLOCK` step above the block start.
        let filler = Layout::from_size_align(BYTE_TIER + MIN_BLOCK * lead + 8, 8).unwrap();
        // SAFETY: non-zero layout, fresh allocator.
        let head = unsafe { alloc.alloc(filler) };
        assert!(!head.is_null(), "lead {lead}: filler must be served");

        let over = Layout::from_size_align(BYTE_TIER + 656, 64).unwrap();
        // SAFETY: non-zero layout.
        let p = unsafe { alloc.alloc(over) };
        assert!(
            !p.is_null(),
            "lead {lead}: over-aligned request must be served, not skipped"
        );
        assert_eq!(p as usize % 64, 0, "lead {lead}: alignment honoured");
        // Disjoint from the live filler block.
        assert!(
            p as usize >= head as usize + filler.size()
                || p as usize + over.size() <= head as usize,
            "lead {lead}: blocks must not overlap"
        );
        // Freeing both strands nothing: the heap returns fully to empty.
        // SAFETY: each pointer came from this allocator with its layout.
        unsafe {
            alloc.dealloc(p, over);
            alloc.dealloc(head, filler);
        }
        assert_eq!(
            alloc.used(),
            0,
            "lead {lead}: no bytes stranded by the front remnant"
        );
    }
}

// --- Growable-heap tests (the injected `HeapSource`) --------------------

extern crate std;

use super::HeapSource;
use core::ptr::NonNull;

/// A test [`HeapSource`] that bump-allocates out of a fixed arena — 8 KiB
/// chunks for the byte-granular tier's regions, single pages for the slab —
/// and records what it handed out, recycling what comes back so a
/// grow-then-shrink or page cycle can reuse space.
struct MockSource {
    state: std::sync::Mutex<MockState>,
}

struct MockState {
    /// Arena base as an integer address (keeps the state `Send`/`Sync`
    /// without unsafe marker impls; the arena is leaked so the address
    /// stays valid for the test).
    base: usize,
    len: usize,
    cursor: usize,
    grow_calls: usize,
    shrink_calls: usize,
    /// Returned chunks available for reuse: `(offset, len)`.
    freelist: std::vec::Vec<(usize, usize)>,
    /// Returned page offsets available for reuse.
    page_freelist: std::vec::Vec<usize>,
    page_allocs: usize,
    page_frees: usize,
    /// Once `grow_calls` reaches this, `grow` returns `None` (models a
    /// genuinely exhausted source for the deterministic-OOM test).
    fail_after: usize,
    /// The same for [`HeapSource::alloc_page`], separately settable so a
    /// test can exhaust one supply while the other still answers.
    page_fail_after: usize,
}

const GROW_QUANTUM: usize = 8 * 1024;

/// Bytes of the leaked, page-aligned arena a [`MockSource`] hands regions and
/// pages out of. Ample for the growth tests, and being leaked it satisfies
/// [`FreeListAllocator::install_source`]'s `'static` bound.
const MOCK_ARENA: usize = 1 << 20;

impl MockSource {
    fn with_limits(fail_after: usize, page_fail_after: usize) -> Self {
        let layout = Layout::from_size_align(MOCK_ARENA, PAGE_SIZE).expect("valid arena layout");
        // SAFETY: `MOCK_ARENA` is non-zero, so the layout has a non-zero size.
        let arena = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!arena.is_null(), "mock source arena");
        Self {
            state: std::sync::Mutex::new(MockState {
                base: arena as usize,
                len: MOCK_ARENA,
                cursor: 0,
                grow_calls: 0,
                shrink_calls: 0,
                freelist: std::vec::Vec::new(),
                page_freelist: std::vec::Vec::new(),
                page_allocs: 0,
                page_frees: 0,
                fail_after,
                page_fail_after,
            }),
        }
    }

    fn new(fail_after: usize) -> Self {
        Self::with_limits(fail_after, usize::MAX)
    }

    fn grow_calls(&self) -> usize {
        self.state.lock().unwrap().grow_calls
    }

    fn shrink_calls(&self) -> usize {
        self.state.lock().unwrap().shrink_calls
    }

    /// Pages handed out and not yet returned.
    fn live_pages(&self) -> usize {
        let s = self.state.lock().unwrap();
        s.page_allocs - s.page_frees
    }

    fn page_allocs(&self) -> usize {
        self.state.lock().unwrap().page_allocs
    }

    /// Whether `addr` names a page this source handed out.
    fn owns_page(&self, addr: usize) -> bool {
        let s = self.state.lock().unwrap();
        addr >= s.base && addr - s.base < s.len
    }
}

impl HeapSource for MockSource {
    fn grow(&self, min_len: usize) -> Option<(*mut u8, usize)> {
        let mut s = self.state.lock().unwrap();
        s.grow_calls += 1;
        if s.grow_calls > s.fail_after {
            return None;
        }
        let want = min_len.div_ceil(GROW_QUANTUM).max(1) * GROW_QUANTUM;
        // Reuse a returned chunk that is big enough, else bump.
        if let Some(pos) = s.freelist.iter().position(|&(_, len)| len >= want) {
            let (off, len) = s.freelist.swap_remove(pos);
            return Some(((s.base + off) as *mut u8, len));
        }
        if s.cursor + want > s.len {
            return None;
        }
        let off = s.cursor;
        s.cursor += want;
        Some(((s.base + off) as *mut u8, want))
    }

    fn shrink(&self, base: *mut u8, len: usize) {
        let mut s = self.state.lock().unwrap();
        s.shrink_calls += 1;
        let off = base as usize - s.base;
        s.freelist.push((off, len));
    }

    fn alloc_page(&self) -> Option<NonNull<u8>> {
        let mut s = self.state.lock().unwrap();
        if s.page_allocs >= s.page_fail_after {
            return None;
        }
        let off = if let Some(off) = s.page_freelist.pop() {
            off
        } else {
            if s.cursor + PAGE_SIZE > s.len {
                return None;
            }
            let off = s.cursor;
            s.cursor += PAGE_SIZE;
            off
        };
        s.page_allocs += 1;
        // The arena is page-aligned and every carve is a whole number of
        // pages, so the offset keeps that alignment.
        NonNull::new((s.base + off) as *mut u8)
    }

    fn free_page(&self, page: NonNull<u8>) {
        let mut s = self.state.lock().unwrap();
        s.page_frees += 1;
        let off = page.as_ptr() as usize - s.base;
        s.page_freelist.push(off);
    }
}

#[test]
fn grows_from_the_source_when_the_bootstrap_is_exhausted() {
    // A tiny bootstrap that cannot satisfy a single 4 KiB request forces
    // the allocator to grow from the source.
    let alloc = fixture(64);
    let source = std::boxed::Box::leak(std::boxed::Box::new(MockSource::new(usize::MAX)));
    alloc.install_source(source);

    let layout = Layout::from_size_align(2 * PAGE_SIZE, 8).unwrap();
    // SAFETY: non-zero layout.
    let p = unsafe { alloc.alloc(layout) };
    assert!(
        !p.is_null(),
        "growth must satisfy a request the bootstrap cannot"
    );
    assert!(
        source.grow_calls() >= 1,
        "the source must have been asked to grow"
    );
    // Capacity now exceeds the bootstrap region.
    assert!(alloc.remaining() > 0);
    // SAFETY: `p` came from this allocator with `layout`.
    unsafe { alloc.dealloc(p, layout) };
}

#[test]
fn draining_a_grown_region_returns_it_to_the_source() {
    // Bootstrap too small for the request, so the block lands in a grown
    // region; freeing it drains that region, which must be handed back.
    let alloc = fixture(64);
    let source = std::boxed::Box::leak(std::boxed::Box::new(MockSource::new(usize::MAX)));
    alloc.install_source(source);

    let layout = Layout::from_size_align(2 * PAGE_SIZE, 8).unwrap();
    // SAFETY: non-zero layout.
    let p = unsafe { alloc.alloc(layout) };
    assert!(!p.is_null());
    assert_eq!(source.shrink_calls(), 0, "nothing returned while in use");
    // SAFETY: `p` came from this allocator with `layout`.
    unsafe { alloc.dealloc(p, layout) };
    assert_eq!(
        source.shrink_calls(),
        1,
        "a wholly-drained grown region must be returned to the source"
    );
}

#[test]
fn grow_shrink_cycles_are_stable_and_reuse_space() {
    // Repeated grow/shrink cycles must neither leak regions nor exhaust the
    // arena: the source recycles returned chunks, so many rounds succeed.
    let alloc = fixture(64);
    let source = std::boxed::Box::leak(std::boxed::Box::new(MockSource::new(usize::MAX)));
    alloc.install_source(source);

    let layout = Layout::from_size_align(2 * PAGE_SIZE, 8).unwrap();
    for _ in 0..1000 {
        // SAFETY: non-zero layout.
        let p = unsafe { alloc.alloc(layout) };
        assert!(
            !p.is_null(),
            "each round must grow (reusing space) and succeed"
        );
        // SAFETY: `p` came from this allocator with `layout`.
        unsafe { alloc.dealloc(p, layout) };
    }
    // Every grow was matched by a shrink: no region stranded.
    assert_eq!(source.grow_calls(), source.shrink_calls());
}

#[test]
fn deterministic_oom_when_the_source_is_exhausted() {
    // A source that refuses to grow makes `alloc` fail closed with null
    // (never a panic), and the heap recovers once the request shrinks to
    // fit the bootstrap.
    let alloc = fixture(1 << 15);
    let source = std::boxed::Box::leak(std::boxed::Box::new(MockSource::with_limits(0, 0)));
    alloc.install_source(source);

    // Larger than the whole bootstrap region, so only a grown region could
    // serve it.
    let big = Layout::from_size_align(1 << 16, 8).unwrap();
    // SAFETY: non-zero layout.
    assert!(
        unsafe { alloc.alloc(big) }.is_null(),
        "an exhausted source must fail closed with null, never panic"
    );
    // A request the bootstrap can serve still succeeds — the slab falls back
    // to carving its page there when the source has no frame left.
    let small = Layout::from_size_align(16, 8).unwrap();
    // SAFETY: non-zero layout.
    let p = unsafe { alloc.alloc(small) };
    assert!(!p.is_null());
    // SAFETY: `p` came from this allocator with `small`.
    unsafe { alloc.dealloc(p, small) };
}

#[test]
fn grown_region_does_not_coalesce_into_the_bootstrap() {
    // The bootstrap has a usable hole, but a request too big for it grows a
    // region. Even if that region is physically adjacent to the bootstrap,
    // the boundary guard keeps them distinct so the grown region can still
    // be recognised as wholly free and returned.
    let alloc = fixture(8192);
    let source = std::boxed::Box::leak(std::boxed::Box::new(MockSource::new(usize::MAX)));
    alloc.install_source(source);

    // Hold a small bootstrap allocation so the bootstrap region is partly in
    // use throughout. It is a byte-tier one, so the bootstrap holds no slab
    // page and the final reclamation figure is exact.
    let small = Layout::from_size_align(BYTE_TIER, 8).unwrap();
    // SAFETY: non-zero layout.
    let keep = unsafe { alloc.alloc(small) };
    assert!(!keep.is_null());

    let big = Layout::from_size_align(2 * PAGE_SIZE, 8).unwrap();
    // SAFETY: non-zero layout.
    let p = unsafe { alloc.alloc(big) };
    assert!(!p.is_null());
    // SAFETY: `p` came from this allocator with `big`.
    unsafe { alloc.dealloc(p, big) };
    assert_eq!(
        source.shrink_calls(),
        1,
        "the grown region must return despite the live bootstrap block"
    );
    // SAFETY: `keep` came from this allocator with `small`.
    unsafe { alloc.dealloc(keep, small) };
    assert_eq!(alloc.used(), 0);
}

extern crate alloc;

/// The regression gate for the scaling cliff this allocator was rewritten to
/// remove: allocate and free at a *constant* cost however far the heap has
/// grown.
///
/// The predecessor walked an address-sorted hole list on both `alloc` and
/// `dealloc` and the whole region list on every `dealloc`, and a region
/// separator kept the hole count at or above the region count — so per-operation
/// cost rose linearly with how much the heap had ever grown, taxing every
/// allocation in the kernel. Wall-clock is load-dependent and never a gate, so
/// the assertion is on the *number of other nodes reached*, which is what a
/// reintroduced walk would inflate.
#[test]
fn per_operation_node_reach_does_not_grow_with_the_heap() {
    // A handful of neighbour touches, whatever the heap size: a segregated
    // list is entered by bit-scan and coalescing reaches one block each way.
    const BOUND: usize = 8;
    let alloc = fixture(4096);
    let source = std::boxed::Box::leak(std::boxed::Box::new(MockSource::new(usize::MAX)));
    alloc.install_source(source);

    let layout = Layout::from_size_align(2 * PAGE_SIZE, 8).unwrap();
    let mut live = std::vec::Vec::new();
    let mut worst_when_small = 0usize;
    let mut worst_when_large = 0usize;

    // Grow the heap well past its bootstrap arena, sampling the per-operation
    // reach early (few regions) and late (many regions).
    for round in 0..600 {
        let _ = alloc.take_steps();
        // SAFETY: non-zero layout.
        let p = unsafe { alloc.alloc(layout) };
        if p.is_null() {
            break;
        }
        let alloc_steps = alloc.take_steps();
        live.push(p);

        // Free an older block so both paths are exercised against a heap that
        // holds many regions at once.
        let free_steps = if live.len() > 4 {
            let old = live.remove(0);
            // SAFETY: `old` came from this allocator with `layout`.
            unsafe { alloc.dealloc(old, layout) };
            alloc.take_steps()
        } else {
            0
        };
        let worst = alloc_steps.max(free_steps);
        if round < 8 {
            worst_when_small = worst_when_small.max(worst);
        } else if round >= 400 {
            worst_when_large = worst_when_large.max(worst);
        }
    }
    for p in live {
        // SAFETY: each pointer came from this allocator with `layout`.
        unsafe { alloc.dealloc(p, layout) };
    }

    assert!(
        worst_when_large <= BOUND,
        "per-operation node reach must stay constant as the heap grows: \
         {worst_when_large} nodes reached with a large heap (bound {BOUND})"
    );
    assert!(
        worst_when_large <= worst_when_small.max(BOUND),
        "per-operation node reach grew with the heap: {worst_when_small} \
         nodes when small, {worst_when_large} when large"
    );
}

// --- Slab-tier tests ----------------------------------------------------

/// Every slab class, smallest first.
fn slab_classes() -> impl Iterator<Item = usize> {
    0..SLAB_CLASSES
}

#[test]
fn routing_is_a_pure_function_of_the_layout() {
    // Whatever a layout routes to, it routes there every time, and a class the
    // slab claims is wide enough for both the size and the alignment.
    for size in [
        1usize,
        8,
        MIN_CLASS,
        MIN_CLASS + 1,
        1000,
        PAGE_SIZE - 1,
        PAGE_SIZE,
    ] {
        for align in [1usize, 8, 64, 512, PAGE_SIZE] {
            let layout = Layout::from_size_align(size, align).unwrap();
            let routed = slab_class(layout);
            assert_eq!(
                routed,
                slab_class(layout),
                "routing must not depend on anything but the layout"
            );
            let Some(class) = routed else {
                continue;
            };
            let width = class_size(class);
            assert!(width >= size, "class {width} must hold {size} bytes");
            assert!(width >= align, "class {width} must satisfy align {align}");
            assert!(width.is_power_of_two() && width <= PAGE_SIZE);
        }
    }
    // A page-sized request is always the slab's — that is what the granule
    // class exists for.
    assert_eq!(
        slab_class(Layout::from_size_align(PAGE_SIZE, 8).unwrap()),
        Some(SLAB_CLASSES - 1)
    );
    // Above the granule — in size or in alignment — the byte-granular tier
    // takes it.
    assert_eq!(
        slab_class(Layout::from_size_align(PAGE_SIZE + 1, 8).unwrap()),
        None
    );
    assert_eq!(
        slab_class(Layout::from_size_align(8, 2 * PAGE_SIZE).unwrap()),
        None
    );
}

#[test]
fn the_slab_never_costs_more_per_object_than_the_byte_tier() {
    // The ceiling on sub-granule classes is derived from exactly this
    // comparison, so a change to either side of it must keep the property: no
    // class the slab claims may cost more per object than a byte-tier block of
    // the same width would.
    for class in slab_classes() {
        let width = class_size(class);
        let cost = if width == PAGE_SIZE {
            // No descriptor: the page is the object.
            PAGE_SIZE
        } else {
            PAGE_SIZE / objects_per_page(width)
        };
        let block = super::block_size(width).expect("a class fits a block");
        assert!(
            cost <= block,
            "class {width} costs {cost} bytes per object where the byte tier \
             would charge {block}"
        );
    }
    // And the first width above the ceiling is one the byte tier serves.
    let over = class_size(SLAB_CLASSES - 2) * 2;
    if over < PAGE_SIZE {
        assert_eq!(
            slab_class(Layout::from_size_align(over, 8).unwrap()),
            None,
            "width {over} costs more as a slab, so the byte tier must take it"
        );
    }
}

#[test]
fn every_class_round_trips_and_hands_the_object_back() {
    let alloc = fixture(1 << 17);
    for class in slab_classes() {
        let size = class_size(class);
        let layout = Layout::from_size_align(size, 8).unwrap();
        // SAFETY: non-zero layout.
        let a = unsafe { alloc.alloc(layout) };
        assert!(!a.is_null(), "class {size} must be served");
        assert_eq!(a as usize % size, 0, "class {size} object is size-aligned");
        // The whole object is writable and disjoint from the descriptor.
        // SAFETY: `a` owns `size` writable bytes per the alloc contract.
        unsafe { core::ptr::write_bytes(a, 0x5A, size) };
        // SAFETY: `a` came from this allocator with `layout`.
        unsafe { alloc.dealloc(a, layout) };
        // SAFETY: non-zero layout.
        let b = unsafe { alloc.alloc(layout) };
        assert_eq!(a, b, "class {size}: a freed object must be reusable");
        // SAFETY: `b` came from this allocator with `layout`.
        unsafe { alloc.dealloc(b, layout) };
    }
}

#[test]
fn a_page_full_of_objects_is_disjoint_and_never_overlaps_the_descriptor() {
    let alloc = fixture(1 << 17);
    // A middling class, so a page holds several objects.
    let size = MIN_CLASS * 4;
    let layout = Layout::from_size_align(size, 8).unwrap();
    let per_page = objects_per_page(size);

    let mut live = std::vec::Vec::new();
    for _ in 0..per_page {
        // SAFETY: non-zero layout.
        let p = unsafe { alloc.alloc(layout) };
        assert!(!p.is_null());
        // SAFETY: `p` owns `size` writable bytes.
        unsafe { core::ptr::write_bytes(p, 0xC3, size) };
        live.push(p as usize);
    }
    let page = live[0] & !(PAGE_SIZE - 1);
    assert!(
        live.iter().all(|&p| p & !(PAGE_SIZE - 1) == page),
        "one page serves its whole object count before another is drawn"
    );
    assert!(
        live.iter().all(|&p| p >= page + size),
        "no object may overlap the page's own descriptor slot"
    );
    live.sort_unstable();
    live.dedup();
    assert_eq!(live.len(), per_page, "objects must be disjoint");

    for p in live {
        // SAFETY: each came from this allocator with `layout`.
        unsafe { alloc.dealloc(p as *mut u8, layout) };
    }
}

#[test]
fn a_page_sized_allocation_takes_exactly_one_page_and_no_header() {
    let alloc = fixture(1 << 16);
    let source = std::boxed::Box::leak(std::boxed::Box::new(MockSource::new(usize::MAX)));
    alloc.install_source(source);

    // One byte-tier round trip first, so the bootstrap region is accounted
    // and the reading below moves only if the page draw moves it.
    let warm = Layout::from_size_align(BYTE_TIER, 8).unwrap();
    // SAFETY: non-zero layout.
    let w = unsafe { alloc.alloc(warm) };
    assert!(!w.is_null());
    // SAFETY: `w` came from this allocator with `warm`.
    unsafe { alloc.dealloc(w, warm) };

    let layout = Layout::from_size_align(PAGE_SIZE, 8).unwrap();
    let before = alloc.remaining();
    // SAFETY: non-zero layout.
    let p = unsafe { alloc.alloc(layout) };
    assert!(!p.is_null(), "a page-sized request must be served");
    assert_eq!(
        source.live_pages(),
        1,
        "a page-sized allocation must cost exactly one page"
    );
    assert_eq!(
        source.grow_calls(),
        0,
        "no byte-tier region is grown for it"
    );
    assert_eq!(
        p as usize % PAGE_SIZE,
        0,
        "the object starts at the page base, so it carries no header"
    );
    assert_eq!(
        alloc.remaining(),
        before,
        "a page from the supply was never part of the byte tier's free space"
    );
    // SAFETY: `p` owns a whole page per the alloc contract; writing it all
    // proves nothing else shares the page.
    unsafe { core::ptr::write_bytes(p, 0xA5, PAGE_SIZE) };
    // SAFETY: `p` came from this allocator with `layout`.
    unsafe { alloc.dealloc(p, layout) };
}

#[test]
fn a_drained_page_goes_back_and_exactly_one_is_kept() {
    let alloc = fixture(64);
    let source = std::boxed::Box::leak(std::boxed::Box::new(MockSource::new(usize::MAX)));
    alloc.install_source(source);

    let layout = Layout::from_size_align(PAGE_SIZE, 8).unwrap();
    // SAFETY: non-zero layout.
    let a = unsafe { alloc.alloc(layout) };
    // SAFETY: non-zero layout.
    let b = unsafe { alloc.alloc(layout) };
    // SAFETY: non-zero layout.
    let c = unsafe { alloc.alloc(layout) };
    assert!(!a.is_null() && !b.is_null() && !c.is_null());
    assert_eq!(source.live_pages(), 3);

    // SAFETY: each came from this allocator with `layout`.
    unsafe {
        alloc.dealloc(a, layout);
        alloc.dealloc(b, layout);
        alloc.dealloc(c, layout);
    }
    assert_eq!(
        source.live_pages(),
        1,
        "an idle class keeps one page and returns the rest"
    );

    // The kept page is handed straight back out rather than drawn again.
    let allocs = source.page_allocs();
    // SAFETY: non-zero layout.
    let d = unsafe { alloc.alloc(layout) };
    assert!(!d.is_null());
    assert_eq!(
        source.page_allocs(),
        allocs,
        "the retained page must serve the next allocation"
    );
    // SAFETY: `d` came from this allocator with `layout`.
    unsafe { alloc.dealloc(d, layout) };
}

#[test]
fn routing_and_provenance_survive_the_source_install() {
    // The trap this forecloses: an object allocated before the source exists
    // must be freed exactly the way it was allocated. Its page came from the
    // bootstrap region, so it goes back there — never to the source, which
    // never issued it.
    let alloc = fixture(1 << 17);

    let mut live = std::vec::Vec::new();
    for class in slab_classes() {
        let layout = Layout::from_size_align(class_size(class), 8).unwrap();
        // SAFETY: non-zero layout.
        let p = unsafe { alloc.alloc(layout) };
        assert!(
            !p.is_null(),
            "class {} served before the install",
            class_size(class)
        );
        live.push((p, layout));
    }

    let source = std::boxed::Box::leak(std::boxed::Box::new(MockSource::new(usize::MAX)));
    alloc.install_source(source);

    for (p, layout) in live.drain(..) {
        // SAFETY: each came from this allocator with its layout.
        unsafe { alloc.dealloc(p, layout) };
    }
    assert_eq!(
        source.page_allocs(),
        0,
        "the source issued no page, so none may be returned to it"
    );
    assert_eq!(source.live_pages(), 0);

    // Each class kept one bootstrap page back, so the next allocation reuses
    // that and only the one after it has to draw — and that one comes from
    // the source. Freeing both then returns each page to its own supply.
    let layout = Layout::from_size_align(PAGE_SIZE, 8).unwrap();
    // SAFETY: non-zero layout.
    let kept = unsafe { alloc.alloc(layout) };
    // SAFETY: non-zero layout.
    let drawn = unsafe { alloc.alloc(layout) };
    assert!(!kept.is_null() && !drawn.is_null());
    assert!(
        !source.owns_page(kept as usize),
        "the page kept back before the install serves first"
    );
    assert!(
        source.owns_page(drawn as usize),
        "a page drawn after the install comes from the source"
    );
    // SAFETY: each came from this allocator with `layout`.
    unsafe {
        alloc.dealloc(kept, layout);
        alloc.dealloc(drawn, layout);
    }
    assert_eq!(
        source.live_pages(),
        0,
        "the source's page went back to the source, not to the bootstrap"
    );
}

#[test]
fn a_page_the_source_cannot_supply_is_never_carved_from_a_grown_region() {
    // Provenance is recovered by a range test against the bootstrap region,
    // so a page carved out of a *grown* region could not be told from one the
    // source issued. The slab refuses it and fails closed instead, leaving
    // the region it briefly touched wholly drained and handed back.
    let alloc = fixture(64);
    // Regions yes, pages no.
    let source =
        std::boxed::Box::leak(std::boxed::Box::new(MockSource::with_limits(usize::MAX, 0)));
    alloc.install_source(source);

    let layout = Layout::from_size_align(64, 8).unwrap();
    // SAFETY: non-zero layout.
    assert!(
        unsafe { alloc.alloc(layout) }.is_null(),
        "with no page supply and no bootstrap room the slab fails closed"
    );
    assert_eq!(
        source.grow_calls(),
        source.shrink_calls(),
        "the region the refused carve touched was handed straight back"
    );
    assert_eq!(source.live_pages(), 0);
}

#[test]
fn the_source_is_set_once() {
    let alloc = fixture(64);
    let first = std::boxed::Box::leak(std::boxed::Box::new(MockSource::new(usize::MAX)));
    let second = std::boxed::Box::leak(std::boxed::Box::new(MockSource::new(usize::MAX)));
    alloc.install_source(first);
    alloc.install_source(second);

    let layout = Layout::from_size_align(PAGE_SIZE, 8).unwrap();
    // SAFETY: non-zero layout.
    let p = unsafe { alloc.alloc(layout) };
    assert!(!p.is_null());
    assert_eq!(first.live_pages(), 1, "the first source still supplies");
    assert_eq!(
        second.page_allocs(),
        0,
        "a later install must not take over memory the first one issued"
    );
    // SAFETY: `p` came from this allocator with `layout`.
    unsafe { alloc.dealloc(p, layout) };
}

#[test]
fn retained_pages_are_reclaimed_before_an_allocation_is_refused() {
    let alloc = fixture(1 << 16);

    // Touch a small class so it retains a page, then release it.
    let small = Layout::from_size_align(64, 8).unwrap();
    // SAFETY: non-zero layout.
    let p = unsafe { alloc.alloc(small) };
    assert!(!p.is_null());
    // SAFETY: `p` came from this allocator with `small`.
    unsafe { alloc.dealloc(p, small) };
    assert!(
        alloc.used() > 0,
        "the drained page is kept back, so the heap still holds it"
    );

    // A byte-tier request that only fits if the retained page comes back.
    let whole = Layout::from_size_align((1 << 16) - MIN_BLOCK, 8).unwrap();
    // SAFETY: non-zero layout.
    let big = unsafe { alloc.alloc(whole) };
    assert!(
        !big.is_null(),
        "the heap must reclaim its retained pages before refusing"
    );
    // SAFETY: `big` came from this allocator with `whole`.
    unsafe { alloc.dealloc(big, whole) };
    assert_eq!(alloc.used(), 0);
}

#[test]
fn slab_per_operation_node_reach_stays_constant_as_pages_accumulate() {
    // The slab's own version of the O(1) gate: a page that fills or drains is
    // unlinked through its own two links, never found by walking the class's
    // page list.
    const BOUND: usize = 4;
    let alloc = fixture(1 << 17);
    let source = std::boxed::Box::leak(std::boxed::Box::new(MockSource::new(usize::MAX)));
    alloc.install_source(source);

    let size = MIN_CLASS * 2;
    let layout = Layout::from_size_align(size, 8).unwrap();
    let per_page = objects_per_page(size);

    let mut live = std::vec::Vec::new();
    let mut worst_when_small = 0usize;
    let mut worst_when_large = 0usize;
    // Enough objects to fill many pages, so the partial list is long.
    for round in 0..(per_page * 40) {
        let _ = alloc.take_steps();
        // SAFETY: non-zero layout.
        let p = unsafe { alloc.alloc(layout) };
        assert!(!p.is_null());
        let alloc_steps = alloc.take_steps();
        live.push(p);

        // Free an older object so both paths run against a long list.
        let free_steps = if live.len() > per_page * 2 {
            let old = live.remove(0);
            // SAFETY: `old` came from this allocator with `layout`.
            unsafe { alloc.dealloc(old, layout) };
            alloc.take_steps()
        } else {
            0
        };
        let worst = alloc_steps.max(free_steps);
        if round < per_page {
            worst_when_small = worst_when_small.max(worst);
        } else if round >= per_page * 20 {
            worst_when_large = worst_when_large.max(worst);
        }
    }
    for p in live {
        // SAFETY: each came from this allocator with `layout`.
        unsafe { alloc.dealloc(p, layout) };
    }
    assert!(
        worst_when_large <= BOUND,
        "slab per-operation node reach must stay constant: {worst_when_large} \
         nodes reached with many pages live (bound {BOUND})"
    );
    assert!(
        worst_when_large <= worst_when_small.max(BOUND),
        "slab per-operation node reach grew with the page count: \
         {worst_when_small} when few, {worst_when_large} when many"
    );
}

#[test]
fn freeing_from_a_filled_page_puts_it_back_on_the_partial_list() {
    let alloc = fixture(1 << 17);
    let source = std::boxed::Box::leak(std::boxed::Box::new(MockSource::new(usize::MAX)));
    alloc.install_source(source);

    let size = MIN_CLASS * 2;
    let layout = Layout::from_size_align(size, 8).unwrap();
    let per_page = objects_per_page(size);

    // Fill two whole pages.
    let mut live = std::vec::Vec::new();
    for _ in 0..(per_page * 2) {
        // SAFETY: non-zero layout.
        let p = unsafe { alloc.alloc(layout) };
        assert!(!p.is_null());
        live.push(p);
    }
    assert_eq!(source.live_pages(), 2, "two full pages, no more");

    // Free one object out of the *first* page, which is full and so on no
    // list; the next allocation must reuse it rather than draw a third page.
    let freed = live[0];
    // SAFETY: `freed` came from this allocator with `layout`.
    unsafe { alloc.dealloc(freed, layout) };
    // SAFETY: non-zero layout.
    let again = unsafe { alloc.alloc(layout) };
    assert_eq!(again, freed, "the refilled page must serve the next object");
    assert_eq!(source.live_pages(), 2, "no extra page was drawn");
    live[0] = again;

    for p in live {
        // SAFETY: each came from this allocator with `layout`.
        unsafe { alloc.dealloc(p, layout) };
    }
    assert_eq!(source.live_pages(), 1, "one page kept, the rest returned");
}
