//! Host unit tests for the guard-arena kthread-stack allocator
//! ([`super`]). They exercise the grow *and* shrink paths over an in-memory [`BlockStore`] so the block-list arithmetic,
//! per-block live-count accounting, and the one-free-block grace run on the
//! CI host without real RAM; the production identity-mapped header access,
//! the `Drop` reclaim seam, and the `free_order` return-to-allocator step
//! are proven on the aarch64 QEMU guard verticals.

use super::*;

use core::cell::{Cell, RefCell};

use tairix_kernel_mem::{BootMemoryMap, MemoryRegion, PhysAddr, RegionKind};

use std::collections::HashMap;
use std::vec::Vec;

/// A page-aligned, plausibly RAM-resident fake arena base for the host
/// tests (the real one is the 2 MiB-aligned carved arena). It is never
/// dereferenced — header access goes through [`MapBlockStore`].
const FAKE_BASE: u64 = 0x4000_0000;

/// Regions a freshly chained 2 MiB block can hold after its header page —
/// the count an idle chained block reuses, and how many allocations fill
/// one so a *second* chained block is forced.
fn regions_per_chained_block() -> u64 {
    (ARENA_GROW_BLOCK_BYTES - BLOCK_HEADER_BYTES) / STACK_REGION_BYTES
}

/// An in-memory [`BlockStore`]: the host stand-in for the production
/// identity-mapped header, keyed by block base. Single-threaded test use,
/// so a [`RefCell`] suffices.
struct MapBlockStore {
    headers: RefCell<HashMap<u64, BlockHeader>>,
}

impl MapBlockStore {
    fn new() -> Self {
        Self {
            headers: RefCell::new(HashMap::new()),
        }
    }
}

impl BlockStore for MapBlockStore {
    fn read(&self, base: u64) -> BlockHeader {
        *self
            .headers
            .borrow()
            .get(&base)
            .expect("read of an uninitialised block header")
    }
    fn write(&self, base: u64, header: BlockHeader) {
        self.headers.borrow_mut().insert(base, header);
    }
}

/// An [`ArenaGrow`] that never grows: the arena is bounded to its installed
/// block. Mirrors a build with no allocator-backed grow source.
struct NoGrow;
impl ArenaGrow for NoGrow {
    fn grow_block(&self) -> Option<u64> {
        None
    }
}

/// An [`ArenaGrow`] that hands out a fixed sequence of block bases, then
/// fails closed — a deterministic stand-in for the frame-allocator source.
struct FakeGrow<'a> {
    bases: &'a [u64],
    next: Cell<usize>,
}
impl<'a> FakeGrow<'a> {
    fn new(bases: &'a [u64]) -> Self {
        Self {
            bases,
            next: Cell::new(0),
        }
    }
}
impl ArenaGrow for FakeGrow<'_> {
    fn grow_block(&self) -> Option<u64> {
        let i = self.next.get();
        let base = *self.bases.get(i)?;
        self.next.set(i + 1);
        Some(base)
    }
}

/// An [`ArenaShrink`] that records every release request and returns a
/// configurable result, so the grace/boot-block decisions are observable.
struct FakeShrink {
    released: RefCell<Vec<u64>>,
    succeed: bool,
}
impl FakeShrink {
    fn new(succeed: bool) -> Self {
        Self {
            released: RefCell::new(Vec::new()),
            succeed,
        }
    }
    fn releases(&self) -> Vec<u64> {
        self.released.borrow().clone()
    }
}
impl ArenaShrink for FakeShrink {
    fn release_block(&self, base: u64, len: u64) -> bool {
        assert_eq!(len, ARENA_GROW_BLOCK_BYTES, "only chained blocks released");
        self.released.borrow_mut().push(base);
        self.succeed
    }
}

/// An [`ArenaShrink`] that must never be called (used where no release is
/// expected — e.g. the boot block).
struct NeverShrink;
impl ArenaShrink for NeverShrink {
    fn release_block(&self, _base: u64, _len: u64) -> bool {
        panic!("release_block must not be called");
    }
}

/// A usable-only [`BootMemoryMap`] of `[base, base + len)`.
fn usable_map(base: u64, len: u64) -> BootMemoryMap {
    let mut map = BootMemoryMap::new();
    map.push(MemoryRegion {
        kind: RegionKind::Usable,
        start: PhysAddr::new(base),
        length: len,
    });
    map
}

#[test]
fn uninstalled_arena_allocates_nothing() {
    let arena = StackArena::new();
    let store = MapBlockStore::new();
    assert_eq!(arena.alloc(&NoGrow, &store), None);
}

#[test]
fn uninstalled_arena_free_reports_not_installed() {
    let arena = StackArena::new();
    let store = MapBlockStore::new();
    assert_eq!(
        arena.free(FAKE_BASE, &NeverShrink, &store),
        FreeOutcome::NotInstalled
    );
}

#[test]
fn install_is_once_and_refuses_re_entry() {
    let arena = StackArena::new();
    let store = MapBlockStore::new();
    assert!(arena.install(FAKE_BASE, 2 * 1024 * 1024, &store));
    // A second install must be refused so a live list is never silently
    // re-based.
    assert!(!arena.install(FAKE_BASE + 0x1000, 4 * 1024 * 1024, &store));
}

#[test]
fn install_rejects_overflow_and_too_small_blocks() {
    let store = MapBlockStore::new();

    let arena = StackArena::new();
    assert!(!arena.install(u64::MAX - 1, 16, &store));
    // A refused install leaves the arena uninstalled.
    assert_eq!(arena.alloc(&NoGrow, &store), None);

    // A block that cannot hold its header page plus one region is refused.
    let arena2 = StackArena::new();
    assert!(!arena2.install(
        FAKE_BASE,
        BLOCK_HEADER_BYTES + STACK_REGION_BYTES - 1,
        &store
    ));
    assert_eq!(arena2.alloc(&NoGrow, &store), None);

    // Exactly one region (plus the header page) is accepted.
    let arena3 = StackArena::new();
    assert!(arena3.install(FAKE_BASE, BLOCK_HEADER_BYTES + STACK_REGION_BYTES, &store));
}

#[test]
fn consecutive_regions_step_by_one_region_above_the_header_page() {
    let arena = StackArena::new();
    let store = MapBlockStore::new();
    assert!(arena.install(
        FAKE_BASE,
        BLOCK_HEADER_BYTES + 4 * STACK_REGION_BYTES,
        &store
    ));

    let first = arena.alloc(&NoGrow, &store).expect("first region fits");
    let second = arena.alloc(&NoGrow, &store).expect("second region fits");

    // Regions start above the block's reserved header page.
    assert_eq!(first.guard_page(), FAKE_BASE + BLOCK_HEADER_BYTES);
    assert_eq!(
        second.guard_page(),
        FAKE_BASE + BLOCK_HEADER_BYTES + STACK_REGION_BYTES
    );
    // Each guard page is 4 KiB-aligned, as `split_block` requires.
    assert_eq!(first.guard_page() % 4096, 0);
    assert_eq!(second.guard_page() % 4096, 0);
}

#[test]
fn top_is_above_the_guard_and_aligned() {
    let arena = StackArena::new();
    let store = MapBlockStore::new();
    assert!(arena.install(FAKE_BASE, BLOCK_HEADER_BYTES + STACK_REGION_BYTES, &store));
    let stack = arena.alloc(&NoGrow, &store).expect("region fits");

    let guard = stack.guard_page();
    assert_eq!(
        stack.top(),
        guard + STACK_GUARD_BYTES + KTHREAD_STACK_BYTES as u64
    );
    assert!(stack.top() >= guard + STACK_GUARD_BYTES);
    assert_eq!(stack.top() % STACK_ALIGN, 0);
}

#[test]
fn exhaustion_without_a_grow_source_fails_closed() {
    let arena = StackArena::new();
    let store = MapBlockStore::new();
    assert!(arena.install(
        FAKE_BASE,
        BLOCK_HEADER_BYTES + 2 * STACK_REGION_BYTES,
        &store
    ));
    assert!(arena.alloc(&NoGrow, &store).is_some());
    assert!(arena.alloc(&NoGrow, &store).is_some());
    // The third does not fit and cannot grow: fail closed.
    assert_eq!(arena.alloc(&NoGrow, &store), None);
}

#[test]
fn grows_onto_a_fresh_chained_block_when_the_first_is_exhausted() {
    const SECOND_BLOCK: u64 = FAKE_BASE + 0x1000_0000;
    let bases = [SECOND_BLOCK];
    let grow = FakeGrow::new(&bases);
    let store = MapBlockStore::new();

    let arena = StackArena::new();
    assert!(arena.install(FAKE_BASE, BLOCK_HEADER_BYTES + STACK_REGION_BYTES, &store));

    let first = arena.alloc(&grow, &store).expect("first region fits");
    assert_eq!(first.guard_page(), FAKE_BASE + BLOCK_HEADER_BYTES);

    // The installed block is now exhausted; the next region is served from
    // the freshly chained block (above its own header page).
    let second = arena.alloc(&grow, &store).expect("chained region fits");
    assert_eq!(second.guard_page(), SECOND_BLOCK + BLOCK_HEADER_BYTES);
    let third = arena
        .alloc(&grow, &store)
        .expect("chained block holds more");
    assert_eq!(
        third.guard_page(),
        SECOND_BLOCK + BLOCK_HEADER_BYTES + STACK_REGION_BYTES
    );

    // Exactly one block was chained for the run above.
    assert_eq!(grow.next.get(), 1);
}

#[test]
fn fails_closed_when_the_grow_source_is_exhausted() {
    let grow = FakeGrow::new(&[]);
    let store = MapBlockStore::new();
    let arena = StackArena::new();
    assert!(arena.install(FAKE_BASE, BLOCK_HEADER_BYTES + STACK_REGION_BYTES, &store));
    assert!(arena.alloc(&grow, &store).is_some());
    assert_eq!(arena.alloc(&grow, &store), None);
}

#[test]
fn free_decrements_the_owning_block_live_count() {
    let arena = StackArena::new();
    let store = MapBlockStore::new();
    assert!(arena.install(
        FAKE_BASE,
        BLOCK_HEADER_BYTES + 2 * STACK_REGION_BYTES,
        &store
    ));

    let a = arena.alloc(&NoGrow, &store).expect("a").guard_page();
    let b = arena.alloc(&NoGrow, &store).expect("b").guard_page();
    assert_eq!(store.read(FAKE_BASE).live, 2);

    // Freeing one returns `Freed` (boot block stays in use) and decrements.
    assert_eq!(arena.free(a, &NeverShrink, &store), FreeOutcome::Freed);
    assert_eq!(store.read(FAKE_BASE).live, 1);
    assert_eq!(arena.free(b, &NeverShrink, &store), FreeOutcome::Freed);
    assert_eq!(store.read(FAKE_BASE).live, 0);
}

#[test]
fn double_free_fails_closed_without_underflow() {
    let arena = StackArena::new();
    let store = MapBlockStore::new();
    assert!(arena.install(FAKE_BASE, BLOCK_HEADER_BYTES + STACK_REGION_BYTES, &store));
    let a = arena.alloc(&NoGrow, &store).expect("a").guard_page();

    assert_eq!(arena.free(a, &NeverShrink, &store), FreeOutcome::Freed);
    assert_eq!(store.read(FAKE_BASE).live, 0);
    // Second free of the same region is rejected without underflowing.
    assert_eq!(arena.free(a, &NeverShrink, &store), FreeOutcome::DoubleFree);
    assert_eq!(store.read(FAKE_BASE).live, 0);
}

#[test]
fn foreign_and_misaligned_free_fail_closed() {
    let arena = StackArena::new();
    let store = MapBlockStore::new();
    assert!(arena.install(FAKE_BASE, BLOCK_HEADER_BYTES + STACK_REGION_BYTES, &store));
    let a = arena.alloc(&NoGrow, &store).expect("a").guard_page();
    assert_eq!(store.read(FAKE_BASE).live, 1);

    // An address in no block at all.
    assert_eq!(
        arena.free(0xDEAD_0000, &NeverShrink, &store),
        FreeOutcome::ForeignAddress
    );
    // An address inside the block but not a region start.
    assert_eq!(
        arena.free(a + 8, &NeverShrink, &store),
        FreeOutcome::ForeignAddress
    );
    // An address inside the header page (below the first region).
    assert_eq!(
        arena.free(FAKE_BASE, &NeverShrink, &store),
        FreeOutcome::ForeignAddress
    );
    // None of the rejects touched the live count.
    assert_eq!(store.read(FAKE_BASE).live, 1);
}

#[test]
fn boot_block_is_never_released() {
    let arena = StackArena::new();
    let store = MapBlockStore::new();
    assert!(arena.install(
        FAKE_BASE,
        BLOCK_HEADER_BYTES + 2 * STACK_REGION_BYTES,
        &store
    ));
    let a = arena.alloc(&NoGrow, &store).expect("a").guard_page();
    let b = arena.alloc(&NoGrow, &store).expect("b").guard_page();

    // Freeing both makes the boot block idle, but it is never released
    // (its `Reserved` frames are kernel-image-owned). `NeverShrink` would
    // panic if a release were attempted.
    assert_eq!(arena.free(a, &NeverShrink, &store), FreeOutcome::Freed);
    assert_eq!(arena.free(b, &NeverShrink, &store), FreeOutcome::Freed);
    assert_eq!(store.read(FAKE_BASE).live, 0);
}

#[test]
fn release_fires_only_on_the_second_idle_chained_block() {
    const BLOCK_A: u64 = FAKE_BASE + 0x0100_0000;
    const BLOCK_B: u64 = FAKE_BASE + 0x0200_0000;
    let bases = [BLOCK_A, BLOCK_B];
    let grow = FakeGrow::new(&bases);
    let shrink = FakeShrink::new(true);
    let store = MapBlockStore::new();
    let cap = regions_per_chained_block();

    // Boot block holds exactly one region, so further allocations chain.
    let arena = StackArena::new();
    assert!(arena.install(FAKE_BASE, BLOCK_HEADER_BYTES + STACK_REGION_BYTES, &store));
    let boot = arena
        .alloc(&grow, &store)
        .expect("boot region")
        .guard_page();

    // Fill chained block A completely.
    let mut a_guards = Vec::new();
    for _ in 0..cap {
        a_guards.push(arena.alloc(&grow, &store).expect("A region").guard_page());
    }
    assert_eq!(store.read(BLOCK_A).live, cap);
    // The next allocation must chain block B (A is full).
    let b_guard = arena.alloc(&grow, &store).expect("B region").guard_page();
    assert_eq!(grow.next.get(), 2, "two chained blocks");
    assert_eq!(store.read(BLOCK_B).live, 1);

    // Free all of A: the last free makes A idle — kept by the grace.
    for (i, g) in a_guards.iter().enumerate() {
        let outcome = arena.free(*g, &shrink, &store);
        if i + 1 == a_guards.len() {
            assert_eq!(outcome, FreeOutcome::FreedKeptIdle);
        } else {
            assert_eq!(outcome, FreeOutcome::Freed);
        }
    }
    assert!(
        shrink.releases().is_empty(),
        "no release while one idle block"
    );

    // Free B's region: B is the *second* idle chained block, so it is
    // released; A stays resident as the spare.
    assert_eq!(
        arena.free(b_guard, &shrink, &store),
        FreeOutcome::FreedReleasedBlock
    );
    assert_eq!(shrink.releases(), std::vec![BLOCK_B]);

    // The boot block is untouched and still serviceable after reuse.
    assert_eq!(arena.free(boot, &shrink, &store), FreeOutcome::Freed);
}

#[test]
fn a_retained_release_keeps_the_block_reusable() {
    const BLOCK_A: u64 = FAKE_BASE + 0x0100_0000;
    const BLOCK_B: u64 = FAKE_BASE + 0x0200_0000;
    let bases = [BLOCK_A, BLOCK_B];
    let grow = FakeGrow::new(&bases);
    // A shrink source that refuses to release (cannot safely scrub/unmap).
    let shrink = FakeShrink::new(false);
    let store = MapBlockStore::new();
    let cap = regions_per_chained_block();

    let arena = StackArena::new();
    assert!(arena.install(FAKE_BASE, BLOCK_HEADER_BYTES + STACK_REGION_BYTES, &store));
    arena.alloc(&grow, &store).expect("boot region");
    let mut a_guards = Vec::new();
    for _ in 0..cap {
        a_guards.push(arena.alloc(&grow, &store).expect("A region").guard_page());
    }
    let b_guard = arena.alloc(&grow, &store).expect("B region").guard_page();

    for g in &a_guards {
        arena.free(*g, &shrink, &store);
    }
    // B is eligible for release but the shrink source retains it: fail
    // closed, the block is kept (still in the list, still reusable).
    assert_eq!(
        arena.free(b_guard, &shrink, &store),
        FreeOutcome::FreedRetainedBlock
    );
    // A subsequent allocation reuses a retained idle block rather than
    // growing a fresh one (the grow source is exhausted).
    assert!(arena.alloc(&grow, &store).is_some());
}

#[test]
fn boundary_oscillation_yields_zero_releases() {
    const BLOCK_A: u64 = FAKE_BASE + 0x0100_0000;
    let bases = [BLOCK_A];
    let grow = FakeGrow::new(&bases);
    let shrink = FakeShrink::new(true);
    let store = MapBlockStore::new();

    let arena = StackArena::new();
    assert!(arena.install(FAKE_BASE, BLOCK_HEADER_BYTES + STACK_REGION_BYTES, &store));
    arena.alloc(&grow, &store).expect("boot region");

    // Repeatedly allocate-from / free-back the single chained block across
    // its boundary: the one-free-block grace keeps it resident, so no
    // release ever fires (no thrash).
    for _ in 0..8 {
        let g = arena
            .alloc(&grow, &store)
            .expect("chained region")
            .guard_page();
        assert_eq!(arena.free(g, &shrink, &store), FreeOutcome::FreedKeptIdle);
    }
    assert!(shrink.releases().is_empty());
    // Only the one chained block was ever created.
    assert_eq!(grow.next.get(), 1);
}

#[test]
fn reused_idle_block_serves_again() {
    const BLOCK_A: u64 = FAKE_BASE + 0x0100_0000;
    let bases = [BLOCK_A];
    let grow = FakeGrow::new(&bases);
    let shrink = FakeShrink::new(true);
    let store = MapBlockStore::new();

    let arena = StackArena::new();
    assert!(arena.install(FAKE_BASE, BLOCK_HEADER_BYTES + STACK_REGION_BYTES, &store));
    arena.alloc(&grow, &store).expect("boot region");

    let g1 = arena.alloc(&grow, &store).expect("A region 1").guard_page();
    assert_eq!(g1, BLOCK_A + BLOCK_HEADER_BYTES);
    assert_eq!(arena.free(g1, &shrink, &store), FreeOutcome::FreedKeptIdle);

    // The idle block is reset and reused; its first region is handed out
    // again (no fresh block chained).
    let g2 = arena
        .alloc(&grow, &store)
        .expect("A region 2 reuses")
        .guard_page();
    assert_eq!(g2, BLOCK_A + BLOCK_HEADER_BYTES);
    assert_eq!(grow.next.get(), 1);
    assert_eq!(store.read(BLOCK_A).live, 1);
}

#[test]
fn scrub_block_zeroes_a_real_region() {
    // A real, owned host buffer stands in for an idle block's RAM.
    let mut buf = std::vec![0xAAu8; 8192];
    let base = buf.as_mut_ptr() as u64;
    // SAFETY: `[base, base + buf.len())` is the owned, mapped, writable
    // backing of `buf`, exclusively borrowed for the duration of the call.
    unsafe {
        scrub_block(base, buf.len());
    }
    assert!(buf.iter().all(|&b| b == 0), "the whole region is zeroed");
}

#[test]
fn frame_arena_shrink_retains_on_wrong_len() {
    // A wrong length is rejected before any dereference, so this is safe to
    // call with a fabricated base (fail closed).
    let map = usable_map(0, 8 * 1024 * 1024);
    let frames = FrameAllocator::new(&map).expect("allocator builds");
    let shrink = FrameArenaShrink::new(&frames);
    assert!(!shrink.release_block(FAKE_BASE, ARENA_GROW_BLOCK_BYTES + 4096));
    assert!(!shrink.release_block(FAKE_BASE, 4096));
}

#[test]
fn frame_allocator_grow_chains_a_real_block_past_the_first() {
    const FIRST_BLOCK: u64 = 0x8000_0000;
    let map = usable_map(0, 8 * 1024 * 1024);
    let frames = FrameAllocator::new(&map).expect("allocator builds");
    let grow = FrameArenaGrow::new(&frames, 8 * 1024 * 1024);
    let store = MapBlockStore::new();

    let arena = StackArena::new();
    assert!(arena.install(FIRST_BLOCK, BLOCK_HEADER_BYTES + STACK_REGION_BYTES, &store));

    let first = arena.alloc(&grow, &store).expect("first region fits");
    assert_eq!(first.guard_page(), FIRST_BLOCK + BLOCK_HEADER_BYTES);

    let second = arena.alloc(&grow, &store).expect("grows onto a real block");
    assert!(second.guard_page() < 8 * 1024 * 1024);
    assert_eq!(
        second.guard_page() % ARENA_GROW_BLOCK_BYTES,
        BLOCK_HEADER_BYTES
    );
    assert_ne!(second.guard_page(), first.guard_page());
}

#[test]
fn frame_allocator_grow_fails_closed_on_physical_exhaustion() {
    let map = usable_map(0, 1024 * 1024); // 1 MiB < a 2 MiB block
    let frames = FrameAllocator::new(&map).expect("allocator builds");
    let grow = FrameArenaGrow::new(&frames, 1 << 31);
    let store = MapBlockStore::new();

    let arena = StackArena::new();
    assert!(arena.install(FAKE_BASE, BLOCK_HEADER_BYTES + STACK_REGION_BYTES, &store));
    assert!(arena.alloc(&grow, &store).is_some());
    assert_eq!(
        arena.alloc(&grow, &store),
        None,
        "no 2 MiB block to chain: fail closed (§2.9)"
    );
}

#[test]
fn frame_allocator_grow_rejects_a_block_outside_the_identity_window() {
    let map = usable_map(0, 8 * 1024 * 1024);
    let frames = FrameAllocator::new(&map).expect("allocator builds");
    let grow = FrameArenaGrow::new(&frames, 1024 * 1024); // 1 MiB window
    let store = MapBlockStore::new();

    let arena = StackArena::new();
    assert!(arena.install(FAKE_BASE, BLOCK_HEADER_BYTES + STACK_REGION_BYTES, &store));
    assert!(arena.alloc(&grow, &store).is_some());
    assert_eq!(arena.alloc(&grow, &store), None);

    // The rejected block was returned to the allocator, not leaked.
    let grow_ok = FrameArenaGrow::new(&frames, 8 * 1024 * 1024);
    let store2 = MapBlockStore::new();
    let arena2 = StackArena::new();
    assert!(arena2.install(FAKE_BASE, BLOCK_HEADER_BYTES + STACK_REGION_BYTES, &store2));
    assert!(arena2.alloc(&grow_ok, &store2).is_some());
    assert!(
        arena2.alloc(&grow_ok, &store2).is_some(),
        "the earlier rejected block was freed, so RAM is still available"
    );
}
