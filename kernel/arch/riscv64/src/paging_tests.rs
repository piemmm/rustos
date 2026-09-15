//! Host unit tests for the Sv39 paging primitives.
//!
//! The bit-level encoders and the table walk run on the host (the walk
//! recovers each child table from the frame source that drew it, which
//! on the host is a real allocation), so every property below is
//! checkable without a riscv64 target. The `satp` write itself is
//! exercised by the memory-isolation QEMU vertical.

use super::*;

#[test]
fn constants_match_privileged_spec() {
    assert_eq!(PAGE_SIZE, 4096);
    assert_eq!(ENTRIES_PER_TABLE, 512);
    assert_eq!(SV39_LEVELS, 3);
    assert_eq!(SATP_MODE_SV39, 8);
    assert_eq!(SATP_MODE_SHIFT, 60);
}

#[test]
fn pte_encodes_ppn_into_bits_53_10() {
    // Page-aligned physical address; PPN = paddr >> 12 sits at bit 10.
    let pte = pte_from_phys(0x8020_0000, flags::VALID | flags::READ);
    assert_eq!(pte & 0b11, 0b11); // VALID | READ
    assert_eq!(pte >> 10, 0x8020_0000 >> 12);
}

#[test]
fn phys_round_trips_through_pte() {
    let paddr = 0x9ABC_D000;
    let pte = pte_from_phys(paddr, flags::VALID | flags::WRITE | flags::READ);
    assert_eq!(phys_from_pte(pte), paddr);
}

#[test]
fn vpn_index_splits_the_virtual_address() {
    // VA with distinct nibbles per level so the shift/mask is unambiguous.
    let va = (5u64 << 30) | (4u64 << 21) | (3u64 << 12) | 0x123;
    assert_eq!(vpn_index(va, 2), 5);
    assert_eq!(vpn_index(va, 1), 4);
    assert_eq!(vpn_index(va, 0), 3);
    // The page offset never leaks into VPN[0].
    assert_eq!(vpn_index(0xFFF, 0), 0);
}

#[test]
fn satp_selects_sv39_mode_and_root_ppn() {
    let satp = satp_sv39(0x8000_0000);
    assert_eq!(satp >> SATP_MODE_SHIFT, SATP_MODE_SV39);
    assert_eq!(satp & 0x0FFF_FFFF_FFFF, 0x8000_0000 >> 12);
}

#[test]
fn leaf_detection_distinguishes_pointer_from_leaf() {
    // Valid + readable = leaf.
    assert!(pte_is_leaf(flags::VALID | flags::READ));
    // Valid + executable = leaf.
    assert!(pte_is_leaf(flags::VALID | flags::EXEC));
    // Valid, no R/W/X = next-level pointer, not a leaf.
    assert!(!pte_is_leaf(flags::VALID));
    // Invalid is never a leaf.
    assert!(!pte_is_leaf(flags::READ));
}

#[test]
fn pool_hands_out_distinct_zeroed_pages_then_fails_closed() {
    static POOL: PageTablePool = PageTablePool::new();
    let pool = &POOL;
    let a = pool.alloc().expect("first");
    let b = pool.alloc().expect("second");
    assert_ne!(a.as_ptr(), b.as_ptr());
    assert!(a.iter().all(|&w| w == 0));
    // Exhaust the pool; the (POOL_SIZE - 2) remaining allocs succeed,
    // and every alloc past the end returns None (closed-fail).
    for _ in 0..(super::POOL_SIZE - 2) {
        assert!(pool.alloc().is_some());
    }
    assert!(pool.alloc().is_none());
    assert!(pool.alloc().is_none());
}

#[test]
fn identity_gigapages_install_leaf_entries() {
    static POOL: PageTablePool = PageTablePool::new();
    let pool = &POOL;
    let space = AddressSpace::new_identity_gigapages(pool, 4).expect("root");
    let root_table = pool
        .table_at(space.root_phys())
        .expect("the pool's own root");
    // SAFETY: a live table page from the pool; the space is exclusively
    // owned here, so the shared read does not alias a `&mut`.
    let root = unsafe { &*root_table };
    for (i, &pte) in root.iter().take(4).enumerate() {
        assert!(pte_is_leaf(pte), "slot {i} should be a gigapage leaf");
        assert_eq!(phys_from_pte(pte), (i as u64) << 30);
    }
    // Slot 4 and beyond are untouched (invalid).
    assert_eq!(root[4] & flags::VALID, 0);
}

#[test]
fn identity_gigapages_rejects_out_of_range() {
    static POOL: PageTablePool = PageTablePool::new();
    let pool = &POOL;
    assert!(AddressSpace::new_identity_gigapages(pool, 0).is_none());
    assert!(AddressSpace::new_identity_gigapages(pool, ENTRIES_PER_TABLE + 1).is_none());
    // The canonical lower half is the widest honest identity extent: a slot
    // above it names an upper-half address, identity in neither direction.
    assert!(AddressSpace::new_identity_gigapages(pool, IDENTITY_GIGAPAGES).is_some());
    assert!(AddressSpace::new_identity_gigapages(pool, IDENTITY_GIGAPAGES + 1).is_none());
}

/// Host-side Sv39 walk mirroring the hardware MMU: returns the physical
/// address a 4 KiB-aligned `vaddr` resolves to, or `None` if any level
/// is invalid. Only used to verify [`AddressSpace::map_4k`].
fn translate(frames: &dyn PageTableFrames, space: &AddressSpace, vaddr: u64) -> Option<u64> {
    let mut phys = space.root_phys();
    for level in (0..SV39_LEVELS).rev() {
        let table = frames.table_at(phys)?;
        // SAFETY: `phys` names a live table of this hierarchy, drawn from
        // `frames`; the caller owns the space exclusively.
        let pte = unsafe { &*table }[vpn_index(vaddr, level)];
        if (pte & flags::VALID) == 0 {
            return None;
        }
        if pte_is_leaf(pte) {
            return Some(phys_from_pte(pte));
        }
        phys = phys_from_pte(pte);
    }
    None
}

#[test]
fn map_4k_builds_three_level_walk() {
    static POOL: PageTablePool = PageTablePool::new();
    let pool = &POOL;
    let mut space = AddressSpace::new_identity_gigapages(pool, 1).expect("root");
    // A VA in gigapage slot 100 — outside the single identity gigapage,
    // so the walk allocates fresh L1/L0 tables.
    let vaddr = (100u64 << 30) | (7u64 << 21) | (9u64 << 12);
    let paddr = 0x8200_0000;
    space
        .map_4k(pool, vaddr, paddr, flags::READ | flags::WRITE)
        .expect("map");
    assert_eq!(translate(pool, &space, vaddr), Some(paddr));
    // A neighbouring page in the same L0 table is still unmapped.
    assert_eq!(translate(pool, &space, vaddr + PAGE_SIZE as u64), None);
}

#[test]
fn map_4k_rejects_misaligned() {
    static POOL: PageTablePool = PageTablePool::new();
    let pool = &POOL;
    let mut space = AddressSpace::new_identity_gigapages(pool, 1).expect("root");
    assert!(space
        .map_4k(pool, 0x1000_0001, 0x8000_0000, flags::READ)
        .is_none());
    assert!(space
        .map_4k(pool, 0x1000_0000, 0x8000_0001, flags::READ)
        .is_none());
}

#[test]
fn map_4k_refuses_to_shatter_a_gigapage() {
    static POOL: PageTablePool = PageTablePool::new();
    let pool = &POOL;
    let mut space = AddressSpace::new_identity_gigapages(pool, 1).expect("root");
    // VA 0 lives under the identity gigapage at root slot 0 — a leaf.
    assert!(space.map_4k(pool, 0x0, 0x8000_0000, flags::READ).is_none());
}

#[test]
fn map_gigapage_aliases_a_whole_gigabyte_at_a_high_va() {
    static POOL: PageTablePool = PageTablePool::new();
    let pool = &POOL;
    let mut space = AddressSpace::new_identity_gigapages(pool, 4).expect("root");
    // Alias the kernel's gigabyte (phys 0x8000_0000) at a high VA with the
    // USER bit — the BIAS-alias trick the crt0 QEMU vertical uses.
    let bias: u64 = 64 << 30; // 64 GiB, 1 GiB-aligned.
    let paddr: u64 = 0x8000_0000;
    let vaddr = paddr + bias;
    space
        .map_gigapage(
            vaddr,
            paddr,
            flags::USER | flags::READ | flags::WRITE | flags::EXEC,
        )
        .expect("gigapage alias");
    // Every address in the aliased gigabyte resolves to its phys base.
    assert_eq!(translate(pool, &space, vaddr), Some(paddr));
    assert_eq!(
        translate(pool, &space, vaddr + 0x20_0000),
        Some(paddr),
        "a megabyte into the gigapage still resolves to the gigapage base"
    );
    // The installed leaf carries the USER bit.
    let root_table = pool
        .table_at(space.root_phys())
        .expect("the pool's own root");
    // SAFETY: a live table page from the pool, exclusively owned here.
    assert_ne!(
        unsafe { &*root_table }[vpn_index(vaddr, 2)] & flags::USER,
        0
    );
}

#[test]
fn map_gigapage_rejects_misaligned_and_occupied() {
    static POOL: PageTablePool = PageTablePool::new();
    let pool = &POOL;
    let mut space = AddressSpace::new_identity_gigapages(pool, 4).expect("root");
    // Misaligned virtual / physical addresses are refused.
    assert!(space
        .map_gigapage((64 << 30) + 0x1000, 0x8000_0000, flags::READ)
        .is_none());
    assert!(space
        .map_gigapage(64 << 30, 0x8000_0000 + 0x1000, flags::READ)
        .is_none());
    // Root slot 0 is occupied by the identity gigapage; refuse to clobber it.
    assert!(space.map_gigapage(0, 0x8000_0000, flags::READ).is_none());
}

#[test]
fn passes_mmu_conformance() {
    use tairix_arch_api::mmu;
    static POOL: PageTablePool = PageTablePool::new();
    // A second, independent pool for the object-safe erasure below.
    static ERASED_POOL: PageTablePool = PageTablePool::new();
    let pool = &POOL;
    let mut space = AddressSpace::new_identity_gigapages(pool, 1).expect("root");
    // A VA in gigapage slot 100 — outside the single identity gigapage, so
    // the conformance map allocates fresh L1/L0 tables and never shatters a
    // leaf. The phys frame is in the kernel's RAM gigabyte.
    let va = 100u64 << 30;
    let pa = 0x8200_0000;
    mmu::conformance::run_all(&mut space, va, pa);
    // And over the object-safe erasure the kernel registry stores.
    let mut dynamic = AddressSpace::new_identity_gigapages(&ERASED_POOL, 1).expect("root");
    let erased: &mut dyn mmu::AddressSpace = &mut dynamic;
    mmu::conformance::run_all(erased, va, pa);
}

/// Host-side Sv39 walk returning the *leaf* PTE a 4 KiB-aligned `vaddr`
/// resolves to (at whatever level the leaf lives) plus the level it was
/// found at (2 = gigapage, 1 = megapage, 0 = 4 KiB page), or `None` if any
/// level is invalid. Mirrors the hardware MMU's stop-at-leaf walk so the
/// split tests can assert the granularity a region is mapped at.
fn leaf_pte(
    frames: &dyn PageTableFrames,
    space: &AddressSpace,
    vaddr: u64,
) -> Option<(u64, usize)> {
    let mut phys = space.root_phys();
    for level in (0..SV39_LEVELS).rev() {
        let table = frames.table_at(phys)?;
        // SAFETY: `phys` names a live table of this hierarchy, drawn from
        // `frames`; the caller owns the space exclusively.
        let pte = unsafe { &*table }[vpn_index(vaddr, level)];
        if (pte & flags::VALID) == 0 {
            return None;
        }
        if pte_is_leaf(pte) {
            return Some((pte, level));
        }
        phys = phys_from_pte(pte);
    }
    None
}

#[test]
fn passes_tlb_conformance() {
    use tairix_arch_api::tlb;
    static POOL: PageTablePool = PageTablePool::new();
    // A second, independent pool for the object-safe erasure below.
    static ERASED_POOL: PageTablePool = PageTablePool::new();
    let mut space = AddressSpace::new_identity_gigapages(&POOL, 1).expect("root");
    // The host has no TLB, so `flush_page` is a vacuous no-op here; the
    // suite proves it is object-safe and panic-free for any address (the
    // real `sfence.vma` is exercised by the spawn QEMU vertical).
    tlb::conformance::run_all(&mut space, 100u64 << 30);
    let mut dynamic = AddressSpace::new_identity_gigapages(&ERASED_POOL, 1).expect("root");
    let erased: &mut dyn tlb::TlbShootdown = &mut dynamic;
    tlb::conformance::run_all(erased, 100u64 << 30);
}

#[test]
fn passes_frames_conformance() {
    use tairix_arch_api::frames::{self, PageTableFrames};
    // The static pool is the boot/bootstrap `PageTableFrames` source; its
    // Sv39 `phys_of` is the identity map, so the suite runs on the host.
    // A fresh pool hands out `POOL_SIZE` frames before failing closed.
    static POOL: PageTablePool = PageTablePool::new();
    // A second, independent pool for the object-safe erasure below.
    static ERASED_POOL: PageTablePool = PageTablePool::new();
    let pool = &POOL;
    frames::conformance::run_all(pool, super::POOL_SIZE);
    // And over the object-safe erasure the per-process façade holds.
    let erased: &dyn PageTableFrames = &ERASED_POOL;
    assert!(erased.alloc_table().is_some());
}

/// A recording [`PageTableFrames`] double: a fixed bump pool of
/// page-aligned tables plus a log of every `free_table` return, so the
/// reclaim test can assert teardown hands back exactly the frames the
/// hierarchy drew.
///
/// The 4 KiB alignment of each slot is load-bearing: a PTE's PPN field
/// carries only bits 12 and up of the physical address, so an unaligned
/// table would be rounded down by the encode/decode round trip and the
/// walk would read and write a *different* address than the one leased —
/// silent memory corruption whose symptoms shift with the heap layout
/// (the flaky 7-vs-5 reclaim count this replaced).
struct RecordingFrames {
    storage: [UnsafeCell<Table>; Self::CAPACITY],
    used: AtomicUsize,
    freed: std::sync::Mutex<std::vec::Vec<u64>>,
}

// SAFETY: each slot is handed out exactly once via the monotonic `used`
// counter, so the `&'static mut` views never alias; the freed log is
// behind its own mutex.
unsafe impl Sync for RecordingFrames {}

impl RecordingFrames {
    const CAPACITY: usize = 8;

    const fn new() -> Self {
        // The array initialiser needs a `const`, and copying it per slot is
        // the point: each element must be its own independent table.
        #[allow(clippy::declare_interior_mutable_const)]
        const ZERO: UnsafeCell<Table> = UnsafeCell::new(Table::new());
        // `const`, so the pool lives in `.bss` and never on a test's stack
        // frame — the same discipline as `PageTablePool::new`.
        #[allow(clippy::large_stack_arrays)]
        Self {
            storage: [ZERO; Self::CAPACITY],
            used: AtomicUsize::new(0),
            freed: std::sync::Mutex::new(std::vec::Vec::new()),
        }
    }
}

impl PageTableFrames for RecordingFrames {
    fn alloc_table(&self) -> Option<TableFrame> {
        let idx = self.used.fetch_add(1, Ordering::SeqCst);
        if idx >= Self::CAPACITY {
            self.used.store(Self::CAPACITY, Ordering::SeqCst);
            return None;
        }
        // SAFETY: the monotonic index makes this slot exclusively ours.
        let table: &'static mut Table = unsafe { &mut *self.storage[idx].get() };
        let entries = &mut table.0;
        let phys = phys_of(entries.as_ptr() as u64);
        Some(TableFrame { phys, entries })
    }

    fn table_at(&self, phys: u64) -> Option<*mut [u64; ENTRIES_PER_TABLE]> {
        let base = phys_of(self.storage.as_ptr() as u64);
        let index = pool_slot_of(base, Self::CAPACITY, phys)?;
        Some(self.storage[index].get().cast())
    }

    fn free_table(&self, phys: u64) {
        self.freed.lock().expect("freed log").push(phys);
    }
}

#[test]
fn reclaim_table_frames_returns_every_drawn_table_exactly_once() {
    static POOL: RecordingFrames = RecordingFrames::new();
    let pool = &POOL;
    let mut space = AddressSpace::new_identity_gigapages(pool, 2).expect("identity map");
    let root_phys = space.root_phys();

    // Two pages in distinct gigapages far above the identity window, so
    // the walk draws two independent L1+L0 pairs: 1 root + 4 tables.
    let leaf_flags = flags::READ | flags::WRITE;
    let pa: u64 = 0x8123_4000;
    space
        .map_4k(pool, 64u64 << 30, pa, leaf_flags)
        .expect("map A");
    space
        .map_4k(pool, 65u64 << 30, pa + PAGE_SIZE as u64, leaf_flags)
        .expect("map B");

    // SAFETY: the space is no hart's active translation (host test) and
    // no other reference into its tables is live.
    unsafe { tairix_arch_api::mmu::AddressSpace::reclaim_table_frames(&mut space) };

    // Every drawn table frame came back exactly once, the root last, and
    // no leaf frame was ever freed.
    let freed = pool.freed.lock().expect("freed log").clone();
    assert_eq!(freed.len(), 5, "root + two L1/L0 pairs were returned");
    assert_eq!(*freed.last().expect("non-empty"), root_phys, "root last");
    let mut dedup = freed.clone();
    dedup.sort_unstable();
    dedup.dedup();
    assert_eq!(dedup.len(), freed.len(), "no table is freed twice");
    assert!(
        !freed.contains(&pa) && !freed.contains(&(pa + PAGE_SIZE as u64)),
        "a leaf frame is never freed"
    );
}

#[test]
fn map_page_translates_neutral_flags_and_walks() {
    use tairix_arch_api::mmu::{self, PageFlags};
    static POOL: PageTablePool = PageTablePool::new();
    let pool = &POOL;
    let mut space = AddressSpace::new_identity_gigapages(pool, 1).expect("root");
    let vaddr = (100u64 << 30) | (7u64 << 21) | (9u64 << 12);
    let paddr = 0x8200_0000;
    mmu::AddressSpace::map_page(&mut space, vaddr, paddr, PageFlags::READ | PageFlags::WRITE)
        .expect("neutral map");
    assert_eq!(translate(pool, &space, vaddr), Some(paddr));
    // The installed leaf carries exactly the translated R|W bits (plus the
    // always-set VALID/ACCESSED/DIRTY), not EXEC or USER.
    let leaf = leaf_pte(pool, &space, vaddr).expect("the 4 KiB leaf").0;
    assert_ne!(leaf & flags::READ, 0);
    assert_ne!(leaf & flags::WRITE, 0);
    assert_eq!(leaf & flags::EXEC, 0);
    assert_eq!(leaf & flags::USER, 0);
}

/// A parent PTE whose PPN the frame source never handed out is what a
/// clobbered or hostile table looks like. Every walk must read it as
/// "nothing mapped here" rather than dereference the address the integer
/// happens to name.
#[test]
fn a_pte_the_source_cannot_reach_fails_the_walk_closed() {
    use tairix_arch_api::mmu::{self, PageFlags};
    static POOL: PageTablePool = PageTablePool::new();
    let mut space = AddressSpace::new_identity_gigapages(&POOL, 2).expect("identity map");
    let va = 100u64 << 30;
    mmu::AddressSpace::map_page(&mut space, va, 0x8123_4000, PageFlags::READ)
        .expect("map the probe page");
    // A page-aligned table the pool never handed out, holding a valid
    // leaf at the index the walk would read next. Recovering a table by
    // dereferencing its address — what the walk did before it asked the
    // frame source — would read this and answer with a mapping; asking
    // the source refuses the address outright.
    let mut foreign = Table::new();
    foreign.0[vpn_index(va, 1)] = pte_from_phys(
        0x8000_0000,
        flags::VALID | flags::READ | flags::ACCESSED | flags::DIRTY,
    );
    let foreign_phys = foreign.0.as_ptr() as u64;

    // Overwrite the root non-leaf PTE to point at it, valid and non-leaf
    // so the walk would follow it.
    let root_table = POOL
        .table_at(space.root_phys())
        .expect("the pool's own root");
    // SAFETY: this space's live root table from the process-static pool,
    // exclusively owned here.
    unsafe {
        (*root_table)[vpn_index(va, 2)] = pte_from_phys(foreign_phys, flags::VALID);
    }

    assert_eq!(mmu::AddressSpace::translate(&space, va), None);
    assert_eq!(
        mmu::AddressSpace::unmap(&mut space, va),
        Err(MapError::NotMapped)
    );
    assert_eq!(
        mmu::AddressSpace::test_and_clear_accessed(&mut space, va),
        Err(MapError::NotMapped)
    );
    // SAFETY: `root_phys` is the live root table of this exclusively-owned
    // space, drawn from `POOL`.
    assert!(!unsafe { set_accessed_flag_in_root(&POOL, space.root_phys(), va, AccessKind::Load) });
    // And a fresh map over the unreachable branch is refused rather than
    // walked into: `leaf_present` reads it as absent, then `ensure_child`
    // refuses the PTE it cannot recover.
    assert_eq!(
        mmu::AddressSpace::map_page(&mut space, va, 0x8123_4000, PageFlags::READ),
        Err(MapError::PoolExhausted)
    );
}

#[test]
fn declares_access_tracking_supported() {
    use tairix_arch_api::mmu::{self, AccessTracking};
    static POOL: PageTablePool = PageTablePool::new();
    let pool = &POOL;
    let space = AddressSpace::new_identity_gigapages(pool, 1).expect("root");
    // riscv64 manages the Accessed bit through clear + the Svade fault
    // path (or hardware update on Svadu), so the referenced bit is
    // honestly Supported.
    assert_eq!(
        mmu::AddressSpace::access_tracking(&space),
        AccessTracking::Supported
    );
}

#[test]
fn test_and_clear_accessed_drives_the_clock_round_trip() {
    use tairix_arch_api::mmu::{self, MapError, PageFlags};
    static POOL: PageTablePool = PageTablePool::new();
    let pool = &POOL;
    let mut space = AddressSpace::new_identity_gigapages(pool, 1).expect("root");

    // A page in gigapage slot 100 (fresh L1/L0 tables). `map_page` sets A
    // (and D) eagerly, so a fresh leaf reads accessed.
    let va = 100u64 << 30;
    let pa = 0x8200_0000;
    mmu::AddressSpace::map_page(&mut space, va, pa, PageFlags::READ | PageFlags::WRITE)
        .expect("map the probe page");

    // Fail-closed edges: a misaligned address and an unmapped one report a
    // typed error, never a fabricated verdict.
    assert_eq!(
        mmu::AddressSpace::test_and_clear_accessed(&mut space, va + 0x123),
        Err(MapError::Misaligned)
    );
    assert_eq!(
        mmu::AddressSpace::test_and_clear_accessed(&mut space, va + PAGE_SIZE as u64),
        Err(MapError::NotMapped)
    );

    // Probe 1: the eager map left A set → reads accessed, clears A.
    assert_eq!(
        mmu::AddressSpace::test_and_clear_accessed(&mut space, va),
        Ok(true)
    );
    assert_eq!(
        leaf_pte(pool, &space, va).expect("mapped").0 & flags::ACCESSED,
        0,
        "A must be cleared after a probe"
    );

    // Probe 2: no access since the clear (the host has no MMU to re-set
    // A) → reads cold, the "genuinely untouched" verdict.
    assert_eq!(
        mmu::AddressSpace::test_and_clear_accessed(&mut space, va),
        Ok(false)
    );

    // Simulate a load access the way the Svade trap path does: set A back
    // on the leaf.
    // SAFETY: `root_phys` is the live, host-identity-addressed root table
    // of this exclusively-owned space.
    assert!(unsafe { set_accessed_flag_in_root(pool, space.root_phys(), va, AccessKind::Load) });

    // Probe 3: the page reads accessed again — the full clock/second-chance
    // transition, end to end.
    assert_eq!(
        mmu::AddressSpace::test_and_clear_accessed(&mut space, va),
        Ok(true)
    );
}

#[test]
fn set_accessed_flag_in_root_respects_permission_and_clears() {
    use tairix_arch_api::mmu::{self, PageFlags};
    static POOL: PageTablePool = PageTablePool::new();
    let pool = &POOL;
    let mut space = AddressSpace::new_identity_gigapages(pool, 1).expect("root");

    // A read-only (no WRITE, no EXEC) user page.
    let va = 100u64 << 30;
    mmu::AddressSpace::map_page(
        &mut space,
        va,
        0x8200_0000,
        PageFlags::READ | PageFlags::USER,
    )
    .expect("map ro page");
    let root = space.root_phys();

    // An unmapped address: nothing to set (fail closed).
    // SAFETY: `root` is the live, host-identity-addressed root table.
    assert!(!unsafe {
        set_accessed_flag_in_root(pool, root, va + PAGE_SIZE as u64, AccessKind::Load)
    });

    // A store to a read-only leaf is a genuine permission fault, not an
    // A/D update: the WRITE permission is absent, so nothing is set.
    // SAFETY: as above.
    assert!(!unsafe { set_accessed_flag_in_root(pool, root, va, AccessKind::Store) });
    // An instruction fetch from a non-executable leaf likewise.
    // SAFETY: as above.
    assert!(!unsafe { set_accessed_flag_in_root(pool, root, va, AccessKind::Instruction) });

    // The eager map already set A, so a load "fault" finds nothing to do.
    // SAFETY: as above.
    assert!(!unsafe { set_accessed_flag_in_root(pool, root, va, AccessKind::Load) });

    // Clear A, then a permitted load access sets it once; a second finds
    // it already set.
    assert_eq!(
        mmu::AddressSpace::test_and_clear_accessed(&mut space, va),
        Ok(true)
    );
    // SAFETY: as above.
    assert!(unsafe { set_accessed_flag_in_root(pool, root, va, AccessKind::Load) });
    // SAFETY: as above.
    assert!(!unsafe { set_accessed_flag_in_root(pool, root, va, AccessKind::Load) });
    assert_ne!(
        leaf_pte(pool, &space, va).expect("mapped").0 & flags::ACCESSED,
        0,
        "A must be set after the fault fix-up"
    );
}

#[test]
fn the_kernel_remap_window_is_canonical_and_clear_of_the_identity_map() {
    let base = kernel_window_base();
    // Sv39 sign-extends from bit 38, so an upper-half window's base carries
    // ones in bits 63:39; the bare `slot << 30` spelling would fault.
    assert_eq!(base >> 39, u64::MAX >> 39, "the base is canonical");
    assert_eq!(vpn_index(base, 2), KERNEL_WINDOW_FIRST_SLOT);
    assert_eq!(base % (1 << 30), 0, "the base is gigapage-aligned");

    assert!(
        KernelWindow::is_representable(base, KERNEL_WINDOW_PAGES),
        "the window extent is representable"
    );
    let window_bytes = (KERNEL_WINDOW_PAGES as u64) * PAGE_SIZE as u64;
    // The identity map stops exactly where the direct physical map begins,
    // and the map stops exactly where the window begins, so no two of the
    // three can claim the same root slot.
    assert_eq!(IDENTITY_GIGAPAGES, PHYSMAP_FIRST_SLOT);
    assert_eq!(PHYSMAP_FIRST_SLOT + PHYSMAP_SLOTS, KERNEL_WINDOW_FIRST_SLOT);
    // And the window stops one gigapage below the top of the address space,
    // so its exclusive top is representable.
    assert_eq!(vpn_index(base + window_bytes - 1, 2), ENTRIES_PER_TABLE - 2);
}

#[test]
fn an_identity_space_leaves_the_kernel_window_slots_alone() {
    static POOL: PageTablePool = PageTablePool::new();
    let space =
        AddressSpace::new_identity_gigapages(&POOL, IDENTITY_GIGAPAGES).expect("identity map");
    let root_table = POOL
        .table_at(space.root_phys())
        .expect("the pool's own root");
    // SAFETY: a live table page from the process-static pool; reading its
    // top entries is sound.
    let (first_window, top) = unsafe {
        (
            (*root_table)[KERNEL_WINDOW_FIRST_SLOT],
            (*root_table)[ENTRIES_PER_TABLE - 1],
        )
    };
    // No window is reserved in a host test, so both stay invalid — the
    // identity fill never reaches them.
    assert_eq!(first_window & flags::VALID, 0);
    assert_eq!(
        top & flags::VALID,
        0,
        "the topmost gigapage is never claimed"
    );
}

#[test]
fn tearing_a_space_down_never_frees_the_shared_kernel_window_tables() {
    // The window's root entries point at tables *every* root shares, so a
    // teardown walk that treats them as this hierarchy's own would free the
    // live kernel heap's page tables. Plant an entry in the window slot by
    // hand — the reservation itself needs a live root — and prove the walk
    // never reaches it.
    static POOL: PageTablePool = PageTablePool::new();
    let mut space = AddressSpace::new_identity_gigapages(&POOL, 2).expect("identity map");
    let shared = POOL.alloc().expect("a stand-in shared window table");
    let shared_phys = shared.as_ptr() as u64;

    let root_table = POOL
        .table_at(space.root_phys())
        .expect("the pool's own root");
    // SAFETY: `root_table` is this space's live root table from the
    // process-static pool; writing its own window slot is what every root
    // constructor does once a window is reserved.
    unsafe {
        (*root_table)[KERNEL_WINDOW_FIRST_SLOT] = pte_from_phys(shared_phys, flags::VALID);
    }

    // SAFETY: the space is not the active translation regime (the host has
    // none), which is `reclaim_table_frames`' contract.
    unsafe {
        MmuAddressSpace::reclaim_table_frames(&mut space);
    }
    // SAFETY: as above — reading the root's own window slot.
    let slot = unsafe { (*root_table)[KERNEL_WINDOW_FIRST_SLOT] };
    assert_eq!(slot, 0, "the shared window entry was dropped, not walked");
}

#[test]
fn the_direct_map_claims_the_upper_half_below_the_window() {
    // The port's user region is exactly the canonical lower half, so the
    // map's first slot is the first slot no user address can name.
    assert_eq!((1u64 << 38) >> 30, PHYSMAP_FIRST_SLOT as u64);
    assert_eq!(PHYSMAP_FIRST_SLOT, 256);
    assert_eq!(PHYSMAP_SLOTS, 191);
    assert_eq!(PHYSMAP_VMA_BASE, 0xFFFF_FFC0_0000_0000);
    // Sv39 sign-extends from bit 38, so the base is canonical and lands in
    // its own slot.
    assert_eq!(PHYSMAP_VMA_BASE >> 39, u64::MAX >> 39);
    assert_eq!(vpn_index(PHYSMAP_VMA_BASE, 2), PHYSMAP_FIRST_SLOT);
    // A root-level leaf is a gigapage, so a slot is a gigabyte of reach and
    // the map's top lands exactly on the remap window's first slot.
    assert_eq!(MAX_PHYSMAP_GIB, PHYSMAP_SLOTS);
    assert_eq!(
        physmap_virt((MAX_PHYSMAP_GIB as u64) << 30),
        kernel_window_base()
    );
}

#[test]
fn physmap_virt_offsets_by_the_map_base() {
    assert_eq!(physmap_virt(0), PHYSMAP_VMA_BASE);
    assert_eq!(physmap_virt(0x8123_4000), PHYSMAP_VMA_BASE + 0x8123_4000);
    // A gigabyte in is one root slot along.
    assert_eq!(vpn_index(physmap_virt(1 << 30), 2), PHYSMAP_FIRST_SLOT + 1);
}

#[test]
fn kernel_slots_are_everything_from_the_map_upward() {
    assert!(!is_kernel_slot(PHYSMAP_FIRST_SLOT - 1));
    assert!(is_kernel_slot(PHYSMAP_FIRST_SLOT));
    assert!(is_kernel_slot(KERNEL_WINDOW_FIRST_SLOT));
    assert!(is_kernel_slot(ENTRIES_PER_TABLE - 1));
}

/// A user leaf in a kernel root slot would hand U-mode the direct physical
/// map or the shared remap window. Both entry points refuse it whatever the
/// caller computed, and the HAL names the cause rather than reporting
/// exhaustion.
#[test]
fn a_user_mapping_is_refused_in_a_kernel_slot() {
    use tairix_arch_api::mmu::{self, PageFlags};
    static POOL: PageTablePool = PageTablePool::new();
    let pool = &POOL;
    let mut space = AddressSpace::new_identity_gigapages(pool, 2).expect("identity map");
    let pa = 0x8123_4000;
    for slot in [
        PHYSMAP_FIRST_SLOT,
        KERNEL_WINDOW_FIRST_SLOT,
        ENTRIES_PER_TABLE - 1,
    ] {
        let va = upper_half_slot_base(slot);
        assert_eq!(
            mmu::AddressSpace::map_page(&mut space, va, pa, PageFlags::READ | PageFlags::USER),
            Err(MapError::InvalidFlags),
            "slot {slot} is the kernel's"
        );
        assert!(
            space
                .map_4k(pool, va, pa, flags::USER | flags::READ)
                .is_none(),
            "slot {slot} is the kernel's"
        );
    }
    // Not a blanket refusal: a user address below the map still maps.
    let user_va = 100u64 << 30;
    assert_eq!(
        mmu::AddressSpace::map_page(&mut space, user_va, pa, PageFlags::READ | PageFlags::USER),
        Ok(())
    );
}

/// Both refusals are extent checks that run before the set-once gate, so
/// this holds whether or not the publication test has already run — the
/// state is process-global and the harness fixes no order.
#[test]
fn publish_physmap_refuses_an_extent_the_slots_cannot_express() {
    assert!(!publish_physmap(0), "an empty map covers nothing");
    assert!(
        !publish_physmap(MAX_PHYSMAP_GIB + 1),
        "wider than the claimed slots can express"
    );
}

/// The one test that drives the set-once publication, because it is
/// process-global: a second caller is refused by design. Every other test
/// here is insensitive to it — the map's slots are gigapage leaves the
/// reclaim walk drops before it descends, and every other walk stays in the
/// lower half.
#[test]
fn every_root_installs_the_published_direct_map() {
    use tairix_arch_api::mmu;
    static POOL: PageTablePool = PageTablePool::new();
    const GIB: usize = 5;

    assert!(publish_physmap(GIB), "the first publication");
    assert_eq!(physmap_gigapages(), GIB);
    assert_eq!(physmap_bytes(), (GIB as u64) << 30);
    assert!(!publish_physmap(GIB), "the map is installed once per boot");

    let probe = 0x1_2345_6000u64;
    for space in [
        AddressSpace::new_identity_gigapages(&POOL, 2).expect("an identity root"),
        AddressSpace::new_kernel_window(&POOL).expect("a kernel-window root"),
    ] {
        assert_eq!(
            mmu::AddressSpace::translate(&space, physmap_virt(probe)).map(|(phys, _)| phys),
            Some(probe),
            "every root reaches a frame through the direct map"
        );
    }
    // The map is never executable and never user-accessible: nothing is
    // fetched through it and no user address can name it.
    let space = AddressSpace::new_identity_gigapages(&POOL, 2).expect("an identity root");
    let leaf = leaf_pte(&POOL, &space, physmap_virt(probe)).expect("a gigapage leaf");
    assert_eq!(leaf.1, 2, "a root-level leaf is a gigapage");
    assert_ne!(leaf.0 & flags::READ, 0);
    assert_ne!(leaf.0 & flags::WRITE, 0);
    assert_eq!(leaf.0 & flags::EXEC, 0);
    assert_eq!(leaf.0 & flags::USER, 0);
    // Past the published extent nothing is mapped, so a frame the map does
    // not cover faults rather than reading a neighbour's.
    assert!(
        mmu::AddressSpace::translate(&space, physmap_virt(physmap_bytes())).is_none(),
        "the map stops at its published extent"
    );
    // The host has no live root, so the boot install finds none to patch
    // and refuses rather than publishing a map nothing carries.
    assert!(!install_boot_physmap(&POOL, GIB));
}
