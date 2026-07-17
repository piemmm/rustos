//! Host unit tests for the Sv39 paging primitives.
//!
//! The bit-level encoders and the table walk run on the host (the walk
//! recovers child tables through their identity-mapped pointers, which
//! on the host is the real allocation pointer), so every property below
//! is checkable without a riscv64 target. The `satp` write itself is
//! exercised by the memory-isolation QEMU vertical.

use super::*;

/// Allocate a fresh `'static` pool per test. `Box::leak` keeps the
/// 64 KiB off the test stack and gives the `&'static` the pool API
/// requires; the leak is intentional and bounded (one per test).
fn fresh_pool() -> &'static PageTablePool {
    std::boxed::Box::leak(std::boxed::Box::new(PageTablePool::new()))
}

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
    let pool = fresh_pool();
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
    let pool = fresh_pool();
    let space = AddressSpace::new_identity_gigapages(pool, 4).expect("root");
    // The root table is reachable through its identity-mapped phys addr.
    let root = unsafe { &*(space.root_phys() as *const [u64; ENTRIES_PER_TABLE]) };
    for (i, &pte) in root.iter().take(4).enumerate() {
        assert!(pte_is_leaf(pte), "slot {i} should be a gigapage leaf");
        assert_eq!(phys_from_pte(pte), (i as u64) << 30);
    }
    // Slot 4 and beyond are untouched (invalid).
    assert_eq!(root[4] & flags::VALID, 0);
}

#[test]
fn identity_gigapages_rejects_out_of_range() {
    let pool = fresh_pool();
    assert!(AddressSpace::new_identity_gigapages(pool, 0).is_none());
    assert!(AddressSpace::new_identity_gigapages(pool, ENTRIES_PER_TABLE + 1).is_none());
}

/// Host-side Sv39 walk mirroring the hardware MMU: returns the physical
/// address a 4 KiB-aligned `vaddr` resolves to, or `None` if any level
/// is invalid. Only used to verify [`AddressSpace::map_4k`].
fn translate(space: &AddressSpace, vaddr: u64) -> Option<u64> {
    let root = unsafe { &*(space.root_phys() as *const [u64; ENTRIES_PER_TABLE]) };
    let mut table = root;
    for level in (0..SV39_LEVELS).rev() {
        let pte = table[vpn_index(vaddr, level)];
        if (pte & flags::VALID) == 0 {
            return None;
        }
        if pte_is_leaf(pte) {
            return Some(phys_from_pte(pte));
        }
        table = unsafe { &*(phys_from_pte(pte) as *const [u64; ENTRIES_PER_TABLE]) };
    }
    None
}

#[test]
fn map_4k_builds_three_level_walk() {
    let pool = fresh_pool();
    let mut space = AddressSpace::new_identity_gigapages(pool, 1).expect("root");
    // A VA in gigapage slot 100 — outside the single identity gigapage,
    // so the walk allocates fresh L1/L0 tables.
    let vaddr = (100u64 << 30) | (7u64 << 21) | (9u64 << 12);
    let paddr = 0x8200_0000;
    space
        .map_4k(pool, vaddr, paddr, flags::READ | flags::WRITE)
        .expect("map");
    assert_eq!(translate(&space, vaddr), Some(paddr));
    // A neighbouring page in the same L0 table is still unmapped.
    assert_eq!(translate(&space, vaddr + PAGE_SIZE as u64), None);
}

#[test]
fn map_4k_rejects_misaligned() {
    let pool = fresh_pool();
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
    let pool = fresh_pool();
    let mut space = AddressSpace::new_identity_gigapages(pool, 1).expect("root");
    // VA 0 lives under the identity gigapage at root slot 0 — a leaf.
    assert!(space.map_4k(pool, 0x0, 0x8000_0000, flags::READ).is_none());
}

#[test]
fn map_gigapage_aliases_a_whole_gigabyte_at_a_high_va() {
    let pool = fresh_pool();
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
    assert_eq!(translate(&space, vaddr), Some(paddr));
    assert_eq!(
        translate(&space, vaddr + 0x20_0000),
        Some(paddr),
        "a megabyte into the gigapage still resolves to the gigapage base"
    );
    // The installed leaf carries the USER bit.
    let root = unsafe { &*(space.root_phys() as *const [u64; ENTRIES_PER_TABLE]) };
    assert_ne!(root[vpn_index(vaddr, 2)] & flags::USER, 0);
}

#[test]
fn map_gigapage_rejects_misaligned_and_occupied() {
    let pool = fresh_pool();
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
    let pool = fresh_pool();
    let mut space = AddressSpace::new_identity_gigapages(pool, 1).expect("root");
    // A VA in gigapage slot 100 — outside the single identity gigapage, so
    // the conformance map allocates fresh L1/L0 tables and never shatters a
    // leaf. The phys frame is in the kernel's RAM gigabyte.
    let va = 100u64 << 30;
    let pa = 0x8200_0000;
    mmu::conformance::run_all(&mut space, va, pa);
    // And over the object-safe erasure the kernel registry stores.
    let mut dynamic = AddressSpace::new_identity_gigapages(fresh_pool(), 1).expect("root");
    let erased: &mut dyn mmu::AddressSpace = &mut dynamic;
    mmu::conformance::run_all(erased, va, pa);
}

/// Host-side Sv39 walk returning the *leaf* PTE a 4 KiB-aligned `vaddr`
/// resolves to (at whatever level the leaf lives) plus the level it was
/// found at (2 = gigapage, 1 = megapage, 0 = 4 KiB page), or `None` if any
/// level is invalid. Mirrors the hardware MMU's stop-at-leaf walk so the
/// split tests can assert the granularity a region is mapped at.
fn leaf_pte(space: &AddressSpace, vaddr: u64) -> Option<(u64, usize)> {
    let root = unsafe { &*(space.root_phys() as *const [u64; ENTRIES_PER_TABLE]) };
    let mut table = root;
    for level in (0..SV39_LEVELS).rev() {
        let pte = table[vpn_index(vaddr, level)];
        if (pte & flags::VALID) == 0 {
            return None;
        }
        if pte_is_leaf(pte) {
            return Some((pte, level));
        }
        table = unsafe { &*(phys_from_pte(pte) as *const [u64; ENTRIES_PER_TABLE]) };
    }
    None
}

#[test]
fn declares_block_split_supported_with_no_justification() {
    use tairix_arch_api::mmu::BlockSplit;
    let space = AddressSpace::new_identity_gigapages(fresh_pool(), 1).expect("root");
    // riscv64 now re-expresses coarse Sv39 leaves at 4 KiB granularity
    // (G1/G2), so it declares `Supported` (the sibling of aarch64).
    assert_eq!(space.block_split_support(), BlockSplit::Supported);
}

#[test]
fn split_block_shatters_a_gigapage_to_pages_preserving_the_identity_mapping() {
    let mut space = AddressSpace::new_identity_gigapages(fresh_pool(), 2).expect("identity map");
    // A page well inside identity gigapage slot 1. Before the split it is
    // mapped by the 1 GiB gigapage *leaf* — there is no 4 KiB entry.
    let va: u64 = (1u64 << 30) + 0x10_0000;
    let (_, level) = leaf_pte(&space, va).expect("gigapage maps va");
    assert_eq!(level, 2, "va starts out mapped by a level-2 gigapage leaf");

    space.split_block(va).expect("split the gigapage to pages");

    // The same address now resolves through a 4 KiB page leaf (level 0),
    // translates to the same physical frame (identity), and carries the
    // identical R|W|X|A|D bits the gigapage had.
    let (leaf, level) = leaf_pte(&space, va).expect("page maps va");
    assert_eq!(level, 0, "va is now mapped by a level-0 4 KiB page leaf");
    assert_eq!(phys_from_pte(leaf), va, "identity translation preserved");
    let expected = pte_from_phys(
        va,
        flags::VALID | flags::READ | flags::WRITE | flags::EXEC | flags::ACCESSED | flags::DIRTY,
    );
    assert_eq!(
        leaf, expected,
        "the page leaf reproduces the gigapage's bits"
    );

    // A neighbouring page in the same shattered region resolves identically.
    let nbr = va + PAGE_SIZE as u64;
    assert_eq!(translate(&space, nbr), Some(nbr));
}

#[test]
fn split_block_then_unmap_tears_down_exactly_one_page() {
    use tairix_arch_api::mmu::{self, MapError};
    let mut space = AddressSpace::new_identity_gigapages(fresh_pool(), 2).expect("identity map");
    let va: u64 = (1u64 << 30) + 0x20_0000;

    // A 4 KiB page cannot be unmapped while it is part of a coarse leaf.
    assert_eq!(
        mmu::AddressSpace::unmap(&mut space, va),
        Err(MapError::NotMapped),
        "a page inside a live coarse leaf has no 4 KiB entry to tear down"
    );

    space.split_block(va).expect("split");
    // After the split the page exists as a level-0 leaf and unmaps cleanly,
    // returning its (identity) frame; its neighbour stays mapped.
    assert_eq!(
        mmu::AddressSpace::unmap(&mut space, va),
        Ok(va),
        "the split page unmaps to its identity frame"
    );
    assert_eq!(translate(&space, va), None, "page is gone");
    assert_eq!(
        translate(&space, va + PAGE_SIZE as u64),
        Some(va + PAGE_SIZE as u64),
        "the neighbouring page is untouched"
    );
}

#[test]
fn split_block_is_idempotent_and_fails_closed() {
    use tairix_arch_api::mmu::MapError;
    let mut space = AddressSpace::new_identity_gigapages(fresh_pool(), 2).expect("identity map");
    let va: u64 = (1u64 << 30) + 0x30_0000;

    space.split_block(va).expect("first split");
    let leaf_once = leaf_pte(&space, va).expect("mapped");
    // Re-splitting an already-fine region changes nothing and allocates
    // nothing (idempotent).
    space.split_block(va).expect("second split is a no-op");
    assert_eq!(
        leaf_pte(&space, va),
        Some(leaf_once),
        "an already-split region is left untouched"
    );

    // Fail closed: a misaligned address and an address with no live
    // mapping are both rejected without mutating the space.
    assert_eq!(space.split_block(va | 0x1), Err(MapError::Misaligned));
    assert_eq!(space.split_block(100u64 << 30), Err(MapError::NotMapped));
}

#[test]
fn prepare_guard_arena_splits_every_covering_block_preserving_translation() {
    use tairix_arch_api::mmu::{self};
    let mut space = AddressSpace::new_identity_gigapages(fresh_pool(), 2).expect("identity map");

    // An arena that straddles a 2 MiB boundary inside identity gigapage 1:
    // it starts 2 MiB-aligned and is 2 MiB + one page long, so it spans
    // two distinct megapage blocks. Both must end up as 4 KiB leaves.
    let base: u64 = (1u64 << 30) + 4 * BLOCK_2MIB;
    let len: u64 = BLOCK_2MIB + PAGE_SIZE as u64;
    assert_eq!(
        leaf_pte(&space, base).expect("gigapage maps base").1,
        2,
        "base starts out under a gigapage leaf"
    );

    space
        .prepare_guard_arena(base, len)
        .expect("prepare the arena");

    // A page in the first covering block, a page in the second, and the
    // arena's last page all resolve through 4 KiB page leaves now, each
    // identity-translating exactly as the gigapage did.
    for va in [base, base + BLOCK_2MIB, base + len - PAGE_SIZE as u64] {
        let (leaf, level) = leaf_pte(&space, va).expect("page maps va");
        assert_eq!(level, 0, "arena page {va:#x} is now a 4 KiB leaf");
        assert_eq!(phys_from_pte(leaf), va, "identity preserved at {va:#x}");
    }

    // A single arena page now unmaps cleanly while its neighbour stays
    // mapped — the property the guard page relies on.
    let guard = base + 3 * PAGE_SIZE as u64;
    assert_eq!(mmu::AddressSpace::unmap(&mut space, guard), Ok(guard));
    assert_eq!(translate(&space, guard), None);
    assert_eq!(
        translate(&space, guard + PAGE_SIZE as u64),
        Some(guard + PAGE_SIZE as u64),
    );
}

#[test]
fn prepare_guard_arena_is_idempotent_and_fails_closed() {
    use tairix_arch_api::mmu::MapError;
    let mut space = AddressSpace::new_identity_gigapages(fresh_pool(), 2).expect("identity map");
    let base: u64 = (1u64 << 30) + 6 * BLOCK_2MIB;

    space.prepare_guard_arena(base, BLOCK_2MIB).expect("first");
    let leaf_once = leaf_pte(&space, base).expect("mapped");
    space
        .prepare_guard_arena(base, BLOCK_2MIB)
        .expect("re-prepare is a no-op");
    assert_eq!(
        leaf_pte(&space, base),
        Some(leaf_once),
        "an already-fine arena is left untouched",
    );

    // Zero length, a misaligned base, an arena over unmapped memory, and a
    // length that wraps the address space are each rejected.
    assert_eq!(
        space.prepare_guard_arena(base, 0),
        Err(MapError::Misaligned)
    );
    assert_eq!(
        space.prepare_guard_arena(base | 0x1, BLOCK_2MIB),
        Err(MapError::Misaligned),
    );
    assert_eq!(
        space.prepare_guard_arena(100u64 << 30, BLOCK_2MIB),
        Err(MapError::NotMapped),
    );
    assert_eq!(
        space.prepare_guard_arena(base, u64::MAX),
        Err(MapError::Misaligned),
    );
}

#[test]
fn hal_split_and_arena_forward_to_the_inherent_bodies() {
    use tairix_arch_api::mmu;
    let mut space = AddressSpace::new_identity_gigapages(fresh_pool(), 2).expect("identity map");

    // Driving `split_block` through the object-safe HAL trait must reach
    // the same body as the inherent method: a page inside a gigapage
    // becomes a 4 KiB page leaf that then unmaps cleanly.
    let va: u64 = (1u64 << 30) + 0x40_0000;
    {
        let erased: &mut dyn mmu::AddressSpace = &mut space;
        erased
            .split_block(va)
            .expect("HAL split_block forwards to the inherent split");
    }
    assert_eq!(
        mmu::AddressSpace::unmap(&mut space, va),
        Ok(va),
        "the HAL-split page unmaps to its identity frame"
    );

    // `prepare_guard_arena` likewise forwards through the object-safe HAL
    // trait to the inherent body.
    let arena: u64 = (1u64 << 30) + 10 * BLOCK_2MIB;
    {
        let erased: &mut dyn mmu::AddressSpace = &mut space;
        erased
            .prepare_guard_arena(arena, BLOCK_2MIB)
            .expect("HAL prepare_guard_arena forwards to the inherent body");
    }
    let arena_page = arena + 2 * PAGE_SIZE as u64;
    assert_eq!(
        mmu::AddressSpace::unmap(&mut space, arena_page),
        Ok(arena_page),
        "a page in the HAL-prepared arena unmaps to its identity frame"
    );
}

#[test]
fn passes_tlb_conformance() {
    use tairix_arch_api::tlb;
    let mut space = AddressSpace::new_identity_gigapages(fresh_pool(), 1).expect("root");
    // The host has no TLB, so `flush_page` is a vacuous no-op here; the
    // suite proves it is object-safe and panic-free for any address (the
    // real `sfence.vma` is exercised by the spawn QEMU vertical).
    tlb::conformance::run_all(&mut space, 100u64 << 30);
    let mut dynamic = AddressSpace::new_identity_gigapages(fresh_pool(), 1).expect("root");
    let erased: &mut dyn tlb::TlbShootdown = &mut dynamic;
    tlb::conformance::run_all(erased, 100u64 << 30);
}

#[test]
fn passes_frames_conformance() {
    use tairix_arch_api::frames::{self, PageTableFrames};
    // The static pool is the boot/bootstrap `PageTableFrames` source; its
    // Sv39 `phys_of` is the identity map, so the suite runs on the host.
    // A fresh pool hands out `POOL_SIZE` frames before failing closed.
    let pool = fresh_pool();
    frames::conformance::run_all(pool, super::POOL_SIZE);
    // And over the object-safe erasure the per-process façade holds.
    let erased: &dyn PageTableFrames = fresh_pool();
    assert!(erased.alloc_table().is_some());
}

/// A recording [`PageTableFrames`] double: identity-phys leaked tables
/// plus a log of every `free_table` return, so the reclaim test can
/// assert teardown hands back exactly the frames the hierarchy drew.
struct RecordingFrames {
    freed: std::sync::Mutex<std::vec::Vec<u64>>,
}

/// A page-aligned table for the double to lease out. The alignment is
/// load-bearing: a PTE's PPN field carries only bits 12 and up of the
/// physical address, so an unaligned heap allocation would be rounded
/// down by the encode/decode round trip and the walk would read and
/// write a *different* heap address than the one leased — silent memory
/// corruption whose symptoms shift with the heap layout (the flaky
/// 7-vs-5 reclaim count this replaced).
#[repr(C, align(4096))]
struct AlignedTable([u64; ENTRIES_PER_TABLE]);

impl RecordingFrames {
    fn new() -> Self {
        Self {
            freed: std::sync::Mutex::new(std::vec::Vec::new()),
        }
    }
}

impl PageTableFrames for RecordingFrames {
    fn alloc_table(&self) -> Option<TableFrame> {
        let table: &'static mut AlignedTable = std::boxed::Box::leak(std::boxed::Box::new(
            AlignedTable([0u64; ENTRIES_PER_TABLE]),
        ));
        let phys = table.0.as_ptr() as u64;
        assert_eq!(
            phys % PAGE_SIZE as u64,
            0,
            "a leased table must survive the PPN encoding"
        );
        Some(TableFrame {
            phys,
            entries: &mut table.0,
        })
    }

    fn free_table(&self, phys: u64) {
        self.freed.lock().expect("freed log").push(phys);
    }
}

#[test]
fn reclaim_table_frames_returns_every_drawn_table_exactly_once() {
    let pool: &'static RecordingFrames =
        std::boxed::Box::leak(std::boxed::Box::new(RecordingFrames::new()));
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
    let pool = fresh_pool();
    let mut space = AddressSpace::new_identity_gigapages(pool, 1).expect("root");
    let vaddr = (100u64 << 30) | (7u64 << 21) | (9u64 << 12);
    let paddr = 0x8200_0000;
    mmu::AddressSpace::map_page(&mut space, vaddr, paddr, PageFlags::READ | PageFlags::WRITE)
        .expect("neutral map");
    assert_eq!(translate(&space, vaddr), Some(paddr));
    // The installed leaf carries exactly the translated R|W bits (plus the
    // always-set VALID/ACCESSED/DIRTY), not EXEC or USER.
    let root = unsafe { &*(space.root_phys() as *const [u64; ENTRIES_PER_TABLE]) };
    let l1 =
        unsafe { &*(phys_from_pte(root[vpn_index(vaddr, 2)]) as *const [u64; ENTRIES_PER_TABLE]) };
    let l0 =
        unsafe { &*(phys_from_pte(l1[vpn_index(vaddr, 1)]) as *const [u64; ENTRIES_PER_TABLE]) };
    let leaf = l0[vpn_index(vaddr, 0)];
    assert_ne!(leaf & flags::READ, 0);
    assert_ne!(leaf & flags::WRITE, 0);
    assert_eq!(leaf & flags::EXEC, 0);
    assert_eq!(leaf & flags::USER, 0);
}
