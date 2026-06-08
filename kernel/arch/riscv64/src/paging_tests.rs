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
    use rustos_arch_api::mmu;
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

#[test]
fn block_split_is_pending_and_fails_closed() {
    use rustos_arch_api::mmu::{self, MapError};
    let mut space = AddressSpace::new_identity_gigapages(fresh_pool(), 1).expect("root");
    // riscv64 honestly declares the Sv39 block-split a tracked gap, not a
    // pretend no-op (`AGENTS.md` §2.17): the profile is `Pending` with a
    // non-empty tracking note.
    let support = space.block_split_support();
    assert!(support.is_pending());
    assert!(support.detail().is_some_and(|n| !n.trim().is_empty()));
    // The default `split_block` therefore fails closed rather than
    // silently doing nothing (`AGENTS.md` §2.9), and so does the
    // guard-page arena (G3b) that builds on it — riscv64 falls back to the
    // software canary until its Sv39 split lands.
    assert_eq!(
        mmu::AddressSpace::split_block(&mut space, 100u64 << 30),
        Err(MapError::Unsupported)
    );
    assert_eq!(
        mmu::AddressSpace::prepare_guard_arena(&mut space, 100u64 << 30, 0x20_0000),
        Err(MapError::Unsupported)
    );
}

#[test]
fn passes_tlb_conformance() {
    use rustos_arch_api::tlb;
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
    use rustos_arch_api::frames::{self, PageTableFrames};
    // The static pool is the boot/bootstrap `PageTableFrames` source; its
    // Sv39 `phys_of` is the identity map, so the suite runs on the host.
    // A fresh pool hands out `POOL_SIZE` frames before failing closed.
    let pool = fresh_pool();
    frames::conformance::run_all(pool, super::POOL_SIZE);
    // And over the object-safe erasure the per-process façade holds.
    let erased: &dyn PageTableFrames = fresh_pool();
    assert!(erased.alloc_table().is_some());
}

#[test]
fn map_page_translates_neutral_flags_and_walks() {
    use rustos_arch_api::mmu::{self, PageFlags};
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
