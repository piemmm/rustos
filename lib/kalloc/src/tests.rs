//! Host unit tests for [`FreeListAllocator`].
//!
//! The tests drive the [`GlobalAlloc`] surface directly over a page-aligned
//! backing buffer (never the process allocator) and assert the freeing
//! contract the kernel relies on: disjoint live blocks, reclamation on
//! free, coalescing of adjacent frees back into a single large hole, and
//! survival of sustained allocate/free churn in bounded memory (the
//! property the previous bump allocator lacked).

use super::{FreeListAllocator, MIN_BLOCK};
use core::alloc::{GlobalAlloc, Layout};

/// Page-aligned backing buffer: `FreeListAllocator::new` requires an
/// `ALIGN`-aligned base, and a page-aligned buffer satisfies every test
/// alignment up to a page.
#[repr(C, align(4096))]
struct Backing<const N: usize>([u8; N]);

fn fixture<const N: usize>(storage: &mut Backing<N>) -> FreeListAllocator {
    // SAFETY: `storage` outlives the allocator (borrowed for the test), is
    // exclusively owned by the caller's local, and is page-aligned via the
    // `Backing` newtype as `FreeListAllocator::new` requires.
    unsafe { FreeListAllocator::new(storage.0.as_mut_ptr(), N) }
}

#[test]
fn alloc_hands_out_aligned_disjoint_blocks() {
    let mut backing = Backing([0u8; 4096]);
    let alloc = fixture(&mut backing);
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
    let mut backing = Backing([0u8; 4096]);
    let alloc = fixture(&mut backing);
    let layout = Layout::from_size_align(64, 8).unwrap();
    // SAFETY: non-zero layout.
    let a = unsafe { alloc.alloc(layout) };
    assert!(!a.is_null());
    let used_after_alloc = alloc.used();
    assert!(used_after_alloc >= 64);
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
    let block = 64usize;
    let mut backing = Backing([0u8; 4096]);
    let alloc = fixture(&mut backing);
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
fn alloc_returns_null_when_exhausted_then_recovers_after_free() {
    // Small heap: a handful of blocks, then exhaustion → null (never a
    // panic). Freeing one block lets the next alloc succeed.
    let mut backing = Backing([0u8; 256]);
    let alloc = fixture(&mut backing);
    let layout = Layout::from_size_align(48, 8).unwrap();
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
    let mut backing = Backing([0u8; 8192]);
    let alloc = fixture(&mut backing);
    // Burn a small odd block so the next hole start is not already aligned.
    let small = Layout::from_size_align(8, 8).unwrap();
    // SAFETY: non-zero layout.
    let _ = unsafe { alloc.alloc(small) };
    let aligned = Layout::from_size_align(64, 512).unwrap();
    // SAFETY: non-zero layout.
    let p = unsafe { alloc.alloc(aligned) };
    assert!(!p.is_null());
    assert_eq!(p as usize % 512, 0, "alloc must honour the requested align");
}

#[test]
// A 64 KiB on-stack backing is intentional: this test needs a heap large
// enough that the post-churn near-full single allocation proves full
// coalescing, and a host test thread's stack (megabytes) accommodates it
// comfortably. Heap-boxing it would only move an identical-sized buffer.
#[allow(clippy::large_stack_arrays)]
fn churn_runs_in_bounded_memory() {
    // The defining freeing property: a long allocate/free loop must NOT
    // march `used` upward (the bump allocator's fatal flaw). A pseudo-random
    // mix of allocs and frees over many iterations keeps a bounded working
    // set; `used` returns to zero once everything is freed.
    let mut backing = Backing([0u8; 1 << 16]);
    let alloc = fixture(&mut backing);
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
    // Free everything; a freeing allocator returns to zero used.
    for (p, l) in live.drain(..) {
        // SAFETY: each came from this allocator with its layout.
        unsafe { alloc.dealloc(p, l) };
    }
    assert_eq!(alloc.used(), 0, "all memory reclaimed after churn");
    // And the whole heap is one coalesced hole again: a near-heap-sized
    // allocation succeeds.
    let big = Layout::from_size_align((1 << 16) - MIN_BLOCK, 8).unwrap();
    // SAFETY: non-zero layout.
    assert!(
        !unsafe { alloc.alloc(big) }.is_null(),
        "post-churn heap must coalesce back to one large hole"
    );
}

#[test]
// A 64 KiB on-stack backing: large enough to hold many blocks so each round's
// exhaustion is meaningful, and comfortably within a host test thread's stack.
#[allow(clippy::large_stack_arrays)]
fn allocate_all_memory_then_free_it_all_reclaims_fully_every_round() {
    // The defining long-uptime property an OS allocator must hold: claim
    // *every* byte the heap can serve, release it all, and find the heap
    // exactly as empty and exactly as capacious as before — round after round,
    // with no drift, no stranding, and no leak. A bump allocator fails this on
    // round two; a freeing allocator passes every round identically.
    let mut backing = Backing([0u8; 1 << 16]);
    let alloc = fixture(&mut backing);
    let layout = Layout::from_size_align(64, 8).unwrap();

    let mut first_round_blocks: Option<usize> = None;
    for round in 0..8 {
        assert_eq!(
            alloc.used(),
            0,
            "round {round} must start from a fully reclaimed heap"
        );
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
        assert_eq!(
            alloc.used(),
            0,
            "round {round} must reclaim every byte after freeing all blocks"
        );
    }
    // After all the churn the heap has coalesced back to a single hole: a
    // near-heap-sized allocation succeeds, proving no permanent fragmentation.
    let big = Layout::from_size_align((1 << 16) - MIN_BLOCK, 8).unwrap();
    // SAFETY: non-zero layout.
    assert!(
        !unsafe { alloc.alloc(big) }.is_null(),
        "the heap must coalesce back to one large hole after every round"
    );
}

#[test]
fn over_aligned_request_uses_a_hole_that_is_not_already_over_aligned() {
    // Regression: a free hole is only ever `ALIGN` (8)-aligned, so an
    // over-aligned (16-byte) request can find the aligned start sitting a
    // sub-`MIN_BLOCK` distance above the hole base. The allocator must still
    // serve the request from such a hole instead of skipping it — otherwise a
    // single large but 8-aligned hole stranded the whole heap, panicking a
    // 656-byte/16-align allocation with ~63 MiB free.
    let mut backing = Backing([0u8; 4096]);
    let alloc = fixture(&mut backing);
    // Carve 24 bytes (an odd multiple of `ALIGN`) from the page-aligned base
    // so the remaining hole begins 8 bytes past a 16-byte boundary: an
    // 8-aligned-but-not-16-aligned hole.
    let odd = Layout::from_size_align(24, 8).unwrap();
    // SAFETY: non-zero layout, fresh allocator.
    let head = unsafe { alloc.alloc(odd) };
    assert!(!head.is_null());
    assert_eq!(alloc.remaining() % 16, 8, "remaining hole must be 8 mod 16");
    // A 16-aligned request whose aligned start leaves an 8-byte front
    // remnant. Before the fix this returned null with the rest of the heap
    // free; it must now succeed and stay 16-aligned.
    let over = Layout::from_size_align(656, 16).unwrap();
    // SAFETY: non-zero layout.
    let p = unsafe { alloc.alloc(over) };
    assert!(
        !p.is_null(),
        "over-aligned request must be served from an 8-aligned hole"
    );
    assert_eq!(p as usize % 16, 0);
    // Disjoint from the first block.
    assert!(p as usize >= head as usize + 24 || p as usize + 656 <= head as usize);
    // Freeing both strands nothing: the heap returns fully to empty.
    // SAFETY: each pointer came from this allocator with its layout.
    unsafe {
        alloc.dealloc(p, over);
        alloc.dealloc(head, odd);
    }
    assert_eq!(
        alloc.used(),
        0,
        "no bytes stranded by advancing past the sub-MIN_BLOCK front remnant"
    );
}

extern crate alloc;
