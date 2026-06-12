//! Host unit tests for the aarch64 stage-1 paging primitives.
//!
//! These cover the pure descriptor/index arithmetic and the host-side
//! table walk (the `&mut`-recovering `map_4k` + a manual translate
//! cross-check). The `TTBR0_EL1`/`SCTLR_EL1` activation is freestanding
//! and is exercised by the memory-isolation QEMU vertical, not here.

use super::*;

#[test]
fn mmu_off_alloc_discipline_is_monotonic_and_fails_closed() {
    // The MMU-off counter discipline (plain load + store — exclusives
    // never succeed on the BCM2711's Device-nGnRnE MMU-off memory, so
    // `fetch_add` would spin forever on real silicon) must hand out
    // every frame exactly once and then fail closed.
    static POOL: PageTablePool = PageTablePool::new();
    let mut seen = [0usize; POOL_SIZE];
    for slot in &mut seen {
        let entries = POOL
            .alloc_with(false)
            .expect("pool exhausted before POOL_SIZE frames");
        assert!(entries.iter().all(|&e| e == 0), "frame not zeroed");
        *slot = entries.as_ptr() as usize;
    }
    let mut sorted = seen;
    sorted.sort_unstable();
    assert!(
        sorted.windows(2).all(|w| w[0] != w[1]),
        "MMU-off discipline handed out an aliased frame"
    );
    // Exhaustion fails closed, repeatedly, without wrapping the counter.
    assert!(POOL.alloc_with(false).is_none());
    assert!(POOL.alloc_with(false).is_none());
    // And the MMU-on discipline agrees the pool is exhausted.
    assert!(POOL.alloc_with(true).is_none());
}

#[test]
fn mmu_disciplines_share_one_counter() {
    // A pool partially consumed MMU-off (the boot identity map) keeps
    // allocating distinct frames once translation is live.
    static POOL: PageTablePool = PageTablePool::new();
    let off = POOL.alloc_with(false).expect("first frame");
    let on = POOL.alloc_with(true).expect("second frame");
    assert_ne!(off.as_ptr(), on.as_ptr());
}

#[test]
fn identity_window_covers_highest_masked_gigapage() {
    let mut device = [0u64; GIGAPAGE_MASK_WORDS];
    let mut ram = [0u64; GIGAPAGE_MASK_WORDS];
    // Both masks empty: no window at all (callers fail closed).
    assert_eq!(identity_window_gigapages(&device, &ram), 0);
    // QEMU virt shape: Device GiB 0, RAM GiB 1 ⇒ 2 gigapages.
    device[0] = 0b0001;
    ram[0] = 0b0010;
    assert_eq!(identity_window_gigapages(&device, &ram), 2);
    // Pi 4 shape: RAM from GiB 0, MMIO in GiB 3 ⇒ 4 gigapages — a
    // shorter window would drop the UART/GIC from the space the
    // instant it activates (the metal silence after "boot completed").
    device[0] = 0b1000;
    ram[0] = 0b0001;
    assert_eq!(identity_window_gigapages(&device, &ram), 4);
    // A gigapage in a later mask word moves the window past it.
    ram[1] = 1 << 5; // gigapage 69
    assert_eq!(identity_window_gigapages(&device, &ram), 70);
    // The top representable slot yields the full 512-entry window.
    ram[GIGAPAGE_MASK_WORDS - 1] = 1 << 63;
    assert_eq!(identity_window_gigapages(&device, &ram), ENTRIES_PER_TABLE);
}

#[test]
fn dcache_line_bytes_decodes_dminline() {
    // Cortex-A72 CTR_EL0: DminLine = 4 ⇒ 16 words ⇒ 64-byte lines.
    assert_eq!(dcache_line_bytes(0x8444_C004), 64);
    // Field extremes: 0 ⇒ one word (4 bytes); 0xF ⇒ 2^15 words.
    assert_eq!(dcache_line_bytes(0), 4);
    assert_eq!(dcache_line_bytes(0xF_0000), 4 << 0xF);
    // Neighbouring fields (IminLine, ERG/CWG) must not leak in.
    assert_eq!(dcache_line_bytes(0xFFF0_FFFF), 4);
}

#[test]
fn identity_ram_mask_marks_every_overlapped_gigapage() {
    let mask = identity_ram_mask(&[
        // The Pi 4 kernel image: inside gigapage 0.
        (0x8_0000, 0x10_0000),
        // An extent straddling the gigapage 3 / 4 boundary marks both.
        (0xFFFF_FFF0, 0x20),
        // A zero-length extent contributes nothing.
        (0x40_0000_0000, 0),
    ]);
    assert_eq!(mask[0], 0b1_1001);
    assert_eq!(mask[1..], [0u64; GIGAPAGE_MASK_WORDS - 1]);
}

#[test]
fn identity_ram_mask_clamps_at_the_identity_window() {
    // The last representable gigapage is marked; the overhang is not.
    let mask = identity_ram_mask(&[(511u64 << 30, 4 << 30)]);
    assert_eq!(mask[7], 1 << 63);
    // An extent entirely beyond the window contributes nothing.
    assert_eq!(
        identity_ram_mask(&[(512u64 << 30, 1 << 30)]),
        [0u64; GIGAPAGE_MASK_WORDS]
    );
}

#[test]
fn identity_gigapage_leaf_leaves_unbacked_slots_invalid() {
    // Device wins over RAM; RAM maps Normal; neither maps nothing — the
    // unbacked-space policy that keeps real-silicon speculation from
    // wandering onto a bus window no device answers.
    assert_eq!(
        identity_gigapage_leaf(true, false),
        Some(device_leaf_attrs(true))
    );
    assert_eq!(
        identity_gigapage_leaf(true, true),
        Some(device_leaf_attrs(true))
    );
    assert_eq!(
        identity_gigapage_leaf(false, true),
        Some(normal_leaf_attrs(true))
    );
    assert_eq!(identity_gigapage_leaf(false, false), None);
}

#[test]
fn ensure_identity_gigapage_installs_an_invalid_slot() {
    static POOL: PageTablePool = PageTablePool::new();
    let mut space = AddressSpace::new_identity_gigapages(&POOL, 2).expect("identity map");
    // Gigapage 3 lies beyond the built span: invalid until ensured.
    assert_eq!(space.translate(3 << 30), None);
    assert!(space.ensure_identity_gigapage((3 << 30) | 0x1234));
    let (pa, _) = space.translate((3 << 30) | 0x4_5000).expect("now mapped");
    assert_eq!(pa, (3 << 30) | 0x4_5000);
    // An already-valid slot is left untouched and reported installed.
    assert!(space.ensure_identity_gigapage(3 << 30));
    // A physical address beyond the identity window fails closed.
    assert!(!space.ensure_identity_gigapage(512u64 << 30));
    assert_eq!(space.translate(2 << 30), None);
}

#[test]
fn table_index_extracts_each_level() {
    // VA whose L1/L2/L3 indices are 1, 2, 3 with a 0x40 offset.
    let va = (1u64 << 30) | (2u64 << 21) | (3u64 << 12) | 0x40;
    assert_eq!(table_index(va, 1), 1);
    assert_eq!(table_index(va, 2), 2);
    assert_eq!(table_index(va, 3), 3);
}

#[test]
fn descriptor_round_trips_the_output_address() {
    let pa = 0x4_1234_5000;
    let d = descriptor(pa, normal_leaf_attrs(false));
    assert_eq!(phys_from_descriptor(d), pa);
    // The offset bits are masked off the output address.
    let d2 = descriptor(pa | 0xABC, normal_leaf_attrs(false));
    assert_eq!(phys_from_descriptor(d2), pa);
}

#[test]
fn block_vs_table_low_bits() {
    // Block descriptor: valid set, bit 1 clear (0b01).
    let block = descriptor(0x4000_0000, normal_leaf_attrs(true));
    assert!(is_block(block));
    assert_eq!(block & 0b11, 0b01);
    // Table descriptor: 0b11.
    let table = table_descriptor(0x4_0000_0000);
    assert!(!is_block(table));
    assert_eq!(table & 0b11, 0b11);
    // Page (L3) descriptor: 0b11.
    let page = descriptor(0x4000_0000, normal_leaf_attrs(false));
    assert!(!is_block(page));
    assert_eq!(page & 0b11, 0b11);
}

#[test]
fn leaf_attrs_select_the_right_mair_index() {
    assert_eq!(
        normal_leaf_attrs(true) & (0b111 << 2),
        attrs::ATTR_IDX_NORMAL
    );
    assert_eq!(
        device_leaf_attrs(true) & (0b111 << 2),
        attrs::ATTR_IDX_DEVICE
    );
    // Both set the access flag so first touch does not fault.
    assert_ne!(normal_leaf_attrs(true) & attrs::AF, 0);
    assert_ne!(device_leaf_attrs(true) & attrs::AF, 0);
}

#[test]
fn el0_leaf_attrs_encode_unprivileged_access() {
    // The AP field lives in bits [7:6].
    const AP_MASK: u64 = 0b11 << 6;
    // Code: read-only at EL0 (AP=0b11), EL0-executable (UXN clear) but
    // privileged-execute-never (PXN set).
    let code = el0_code_leaf_attrs();
    assert_eq!(code & AP_MASK, attrs::AP_RO_EL0);
    assert_ne!(code & attrs::PXN, 0);
    assert_eq!(code & attrs::UXN, 0);
    assert_eq!(code & (0b111 << 2), attrs::ATTR_IDX_NORMAL);
    // It is a page descriptor (0b11) with the access flag set.
    assert_eq!(code & 0b11, 0b11);
    assert_ne!(code & attrs::AF, 0);

    // Data: read/write at EL0 (AP=0b01), execute-never at both ELs.
    let data = el0_data_leaf_attrs();
    assert_eq!(data & AP_MASK, attrs::AP_RW_EL0);
    assert_ne!(data & attrs::PXN, 0);
    assert_ne!(data & attrs::UXN, 0);
    assert_eq!(data & 0b11, 0b11);

    // Read-only data: read-only at EL0 (AP=0b11) and execute-never at both
    // ELs — unlike code, the page is *not* EL0-executable (UXN set).
    let rodata = el0_rodata_leaf_attrs();
    assert_eq!(rodata & AP_MASK, attrs::AP_RO_EL0);
    assert_ne!(rodata & attrs::PXN, 0);
    assert_ne!(rodata & attrs::UXN, 0);
    assert_eq!(rodata & (0b111 << 2), attrs::ATTR_IDX_NORMAL);
    assert_eq!(rodata & 0b11, 0b11);
    assert_ne!(rodata & attrs::AF, 0);
}

#[test]
fn map_4k_with_attrs_uses_the_supplied_leaf_attrs() {
    static POOL: PageTablePool = PageTablePool::new();
    let mut space = AddressSpace::new_identity_gigapages(&POOL, 2).expect("identity map");

    let va: u64 = 96u64 << 30;
    let pa: u64 = 0x4567_8000;
    space
        .map_4k_with_attrs(&POOL, va, pa, el0_code_leaf_attrs())
        .expect("map the EL0 page");

    // Walk to the leaf descriptor and confirm it carries the EL0 attrs.
    let leaf = host_leaf_descriptor(space.root_phys(), va).expect("va is mapped");
    assert_eq!(phys_from_descriptor(leaf), pa);
    assert_eq!(leaf & (0b11 << 6), attrs::AP_RO_EL0);
    assert_eq!(leaf & attrs::UXN, 0);
    assert_ne!(leaf & attrs::PXN, 0);
}

#[test]
fn tcr_value_encodes_a_39_bit_region() {
    // T0SZ field (bits [5:0]) is 25 → 64 - 25 = 39-bit VA.
    assert_eq!(TCR_VALUE & 0x3F, 25);
    // TTBR1 walks disabled (EPD1, bit 23).
    assert_ne!(TCR_VALUE & (1 << 23), 0);
}

#[test]
fn mair_pairs_normal_and_device() {
    // Attr0 = 0xFF (Normal WB RW-allocate), Attr1 = 0x04 (Device-nGnRE).
    assert_eq!(MAIR_VALUE & 0xFF, 0xFF);
    assert_eq!((MAIR_VALUE >> 8) & 0xFF, 0x04);
}

#[test]
fn sctlr_mmu_off_pins_the_trampoline_value() {
    // `boot.s` (`.Lin_el1`) and `smp.s` (`_start_secondary_aarch64`)
    // hard-code this exact value with `mov`/`movk`; regression test for
    // the Pi 4 hang where the architecturally UNKNOWN EL1 reset state
    // was never replaced before use.
    assert_eq!(SCTLR_MMU_OFF, 0x30D0_0800);
    // The MMU-off value is exactly the ARMv8.0 RES1 bits: no
    // translation, no caches, nothing else.
    assert_eq!(SCTLR_MMU_OFF, SCTLR_RES1);
}

#[test]
fn sctlr_mmu_on_enables_translation_and_caches_only() {
    // M (bit 0), C (bit 2), I (bit 12) on top of the RES1 bits, nothing
    // more — the whole-register write in `AddressSpace::switch` must not
    // smuggle any other behaviour in.
    assert_eq!(SCTLR_MMU_ON, SCTLR_RES1 | (1 << 0) | (1 << 2) | (1 << 12));
}

#[test]
fn sctlr_values_keep_the_unknown_reset_traps_clear() {
    // The bits whose UNKNOWN reset state broke real silicon stay clear
    // in both installed values: A (1, alignment check), SA (3) / SA0 (4,
    // SP alignment), WXN (19, writable ⇒ execute-never), E0E (24) / EE
    // (25, big-endian data).
    for sctlr in [SCTLR_MMU_OFF, SCTLR_MMU_ON] {
        for trap_bit in [1, 3, 4, 19, 24, 25] {
            assert_eq!(sctlr & (1 << trap_bit), 0, "bit {trap_bit} must be clear");
        }
        // And every ARMv8.0 RES1 bit is set.
        assert_eq!(sctlr & SCTLR_RES1, SCTLR_RES1);
    }
}

#[test]
fn identity_gigapages_map_device_then_normal() {
    static POOL: PageTablePool = PageTablePool::new();
    let space = AddressSpace::new_identity_gigapages(&POOL, 2).expect("two gigapages");
    // The host walk reads the root through its identity-mapped address.
    let root = space.root_phys() as *const u64;
    // SAFETY: `root_phys` is the address of a live table page from the
    // process-static pool; reading the first two entries is sound.
    let (e0, e1) = unsafe { (*root, *root.add(1)) };
    // Under the default mask GiB 0 is Device, GiB 1 is Normal; both are
    // valid blocks.
    assert!(is_block(e0));
    assert!(is_block(e1));
    assert_eq!(e0 & (0b111 << 2), attrs::ATTR_IDX_DEVICE);
    assert_eq!(e1 & (0b111 << 2), attrs::ATTR_IDX_NORMAL);
    assert_eq!(phys_from_descriptor(e0), 0);
    assert_eq!(phys_from_descriptor(e1), 1 << 30);
}

#[test]
fn identity_device_mask_derives_the_virt_layout() {
    // QEMU `virt`: PL011 + GICD/GICC in GiB 0, the kernel image in
    // GiB 1 (`aarch64-virt.ld`, load 0x4020_0000) — the historic
    // "GiB 0 Device" layout falls out of the derivation.
    let mask = identity_device_mask(
        &[0x0900_0000, 0x0800_0000, 0x0801_0000],
        0x4020_0000,
        0x4060_0000,
    );
    assert_eq!(mask, DEFAULT_DEVICE_GIGAPAGES);
}

#[test]
fn identity_device_mask_derives_the_pi4_layout() {
    // Raspberry Pi 4: the kernel image at 0x8_0000 keeps GiB 0 Normal
    // (the CPU executes from it), and the discovered PL011/GIC-400
    // bases put GiB 3 — the BCM2711 high-peripheral window — on the
    // Device side.
    let mask = identity_device_mask(
        &[0xFE20_1000, 0xFF84_1000, 0xFF84_2000],
        0x8_0000,
        0x48_0000,
    );
    let mut expected = [0u64; GIGAPAGE_MASK_WORDS];
    expected[0] = 1 << 3;
    assert_eq!(mask, expected);
    assert!(gigapage_is_device(&mask, 3));
    assert!(!gigapage_is_device(&mask, 0));
}

#[test]
fn identity_device_mask_keeps_the_kernel_gigapages_normal() {
    // A discovered MMIO base sharing the kernel image's gigapage cannot
    // be expressed at 1 GiB granularity; the kernel's gigapages win
    // (Normal, executable) — including every gigapage the image spans.
    let mask = identity_device_mask(&[0x0900_0000], 0, 0x8000_0000);
    assert_eq!(mask, [0u64; GIGAPAGE_MASK_WORDS]);

    // A base beyond the 512 GiB identity window has no slot to set.
    let mask = identity_device_mask(&[1u64 << 60], 0x4020_0000, 0x4060_0000);
    assert_eq!(mask, [0u64; GIGAPAGE_MASK_WORDS]);
}

#[test]
fn configured_device_gigapages_select_the_leaf_attributes() {
    static POOL: PageTablePool = PageTablePool::new();

    // Add GiB 3 (the Pi 4 high-peripheral window) to the Device set
    // while keeping the default GiB-0 bit, so concurrently-running
    // tests that rely on the default "GiB 0 Device / GiB 1 Normal"
    // layout observe no change.
    let mut mask = DEFAULT_DEVICE_GIGAPAGES;
    mask[0] |= 1 << 3;
    configure_device_gigapages(mask);
    assert_eq!(device_gigapages(), mask);

    let space = AddressSpace::new_identity_gigapages(&POOL, 4).expect("four gigapages");
    let root = space.root_phys() as *const u64;
    // SAFETY: `root_phys` is the address of a live table page from the
    // process-static pool; reading the first four entries is sound.
    let entries = unsafe { [*root, *root.add(1), *root.add(2), *root.add(3)] };
    assert_eq!(entries[0] & (0b111 << 2), attrs::ATTR_IDX_DEVICE);
    assert_eq!(entries[1] & (0b111 << 2), attrs::ATTR_IDX_NORMAL);
    assert_eq!(entries[2] & (0b111 << 2), attrs::ATTR_IDX_NORMAL);
    assert_eq!(entries[3] & (0b111 << 2), attrs::ATTR_IDX_DEVICE);

    // Restore the default so the process-global slot is left as the
    // other host tests expect.
    configure_device_gigapages(DEFAULT_DEVICE_GIGAPAGES);
    assert_eq!(device_gigapages(), DEFAULT_DEVICE_GIGAPAGES);
}

#[test]
fn map_4k_walks_and_translates() {
    static POOL: PageTablePool = PageTablePool::new();
    let mut space = AddressSpace::new_identity_gigapages(&POOL, 2).expect("identity map");

    // Map a 4 KiB page well above the identity window (64 GiB) so the
    // walk allocates fresh L2/L3 tables rather than shattering a block.
    let va: u64 = 64u64 << 30;
    let pa: u64 = 0x4123_4000;
    space.map_4k(&POOL, va, pa).expect("map the page");

    // Manually walk the just-built hierarchy and confirm it translates
    // `va` to `pa` (the host analogue of an MMU lookup).
    let translated = host_translate(space.root_phys(), va).expect("va is mapped");
    assert_eq!(translated, pa);

    // A neighbouring page in the same L3 table is absent.
    assert!(host_translate(space.root_phys(), va + PAGE_SIZE as u64).is_none());
}

#[test]
fn map_4k_rejects_misaligned_inputs() {
    static POOL: PageTablePool = PageTablePool::new();
    let mut space = AddressSpace::new_identity_gigapages(&POOL, 1).expect("identity map");
    assert!(space.map_4k(&POOL, 0x1_0001, 0x4000_0000).is_none());
    assert!(space.map_4k(&POOL, 0x1_0000, 0x4000_0001).is_none());
}

/// Host-side translation of `va` through the table hierarchy rooted at
/// `root_phys`, following table descriptors and returning the output
/// physical address of the leaf (block or page) that maps `va`, or
/// `None` if no valid leaf is reached.
fn host_translate(root_phys: u64, va: u64) -> Option<u64> {
    host_leaf_descriptor(root_phys, va).map(phys_from_descriptor)
}

/// As [`host_translate`], but returns the full leaf *descriptor* (output
/// address plus attributes) so a test can assert the leaf's permission
/// bits, not just its translation.
fn host_leaf_descriptor(root_phys: u64, va: u64) -> Option<u64> {
    let mut table = root_phys as *const u64;
    for level in 1..=LEVELS {
        let idx = table_index(va, level);
        // SAFETY: `table` points at a live, identity-addressed table page
        // built by `map_4k`/`new_identity_gigapages`; `idx < 512`.
        let entry = unsafe { *table.add(idx) };
        if (entry & attrs::VALID) == 0 {
            return None;
        }
        if is_block(entry) || level == LEVELS {
            return Some(entry);
        }
        table = phys_from_descriptor(entry) as *const u64;
    }
    None
}

#[test]
fn split_block_shatters_a_gigapage_to_pages_preserving_the_identity_mapping() {
    static POOL: PageTablePool = PageTablePool::new();
    let mut space = AddressSpace::new_identity_gigapages(&POOL, 2).expect("identity map");

    // A page well inside the Normal RAM gigapage (GiB 1). Before the
    // split it is mapped by the 1 GiB L1 *block* — there is no 4 KiB leaf.
    let va: u64 = (1u64 << 30) + 0x10_0000;
    let before = host_leaf_descriptor(space.root_phys(), va).expect("block maps va");
    assert!(is_block(before), "va starts out mapped by a 1 GiB block");

    space.split_block(va).expect("split the gigapage to pages");

    // The same address now resolves through a 4 KiB *page* leaf (0b11),
    // translates to the same physical frame (identity), and carries the
    // identical Normal attributes the block had.
    let leaf = host_leaf_descriptor(space.root_phys(), va).expect("page maps va");
    assert!(!is_block(leaf), "va is now mapped by a 4 KiB page leaf");
    assert_eq!(
        phys_from_descriptor(leaf),
        va,
        "identity translation preserved"
    );
    assert_eq!(
        leaf,
        descriptor(va, normal_leaf_attrs(false)),
        "the page leaf reproduces the block's Normal attributes"
    );

    // A neighbouring page in the same shattered 2 MiB region also resolves
    // identically (the whole block was faithfully re-expressed).
    let nbr = va + PAGE_SIZE as u64;
    assert_eq!(host_translate(space.root_phys(), nbr), Some(nbr));
}

#[test]
fn split_block_then_unmap_tears_down_exactly_one_page() {
    static POOL: PageTablePool = PageTablePool::new();
    let mut space = AddressSpace::new_identity_gigapages(&POOL, 2).expect("identity map");
    let va: u64 = (1u64 << 30) + 0x20_0000;

    // A 4 KiB page cannot be unmapped while it is part of a block.
    assert_eq!(
        rustos_arch_api::mmu::AddressSpace::unmap(&mut space, va),
        Err(MapError::NotMapped),
        "a page inside a live block has no 4 KiB leaf to tear down"
    );

    space.split_block(va).expect("split");
    // After the split the page exists as an L3 leaf and unmaps cleanly,
    // returning its (identity) frame; its neighbour stays mapped.
    assert_eq!(
        rustos_arch_api::mmu::AddressSpace::unmap(&mut space, va),
        Ok(va),
        "the split page unmaps to its identity frame"
    );
    assert_eq!(host_translate(space.root_phys(), va), None, "page is gone");
    assert_eq!(
        host_translate(space.root_phys(), va + PAGE_SIZE as u64),
        Some(va + PAGE_SIZE as u64),
        "the neighbouring page is untouched"
    );
}

#[test]
fn split_block_preserves_device_attributes() {
    static POOL: PageTablePool = PageTablePool::new();
    let mut space = AddressSpace::new_identity_gigapages(&POOL, 2).expect("identity map");
    // GiB 0 is the Device MMIO gigapage.
    let va: u64 = 0x10_0000;
    space.split_block(va).expect("split the device gigapage");
    let leaf = host_leaf_descriptor(space.root_phys(), va).expect("page maps va");
    assert_eq!(
        leaf & (0b111 << 2),
        attrs::ATTR_IDX_DEVICE,
        "the shattered Device block keeps its Device memory attribute"
    );
    assert_eq!(leaf & 0b11, 0b11, "the leaf is a page descriptor");
}

#[test]
fn split_block_is_idempotent_and_fails_closed() {
    static POOL: PageTablePool = PageTablePool::new();
    let mut space = AddressSpace::new_identity_gigapages(&POOL, 2).expect("identity map");
    let va: u64 = (1u64 << 30) + 0x30_0000;

    space.split_block(va).expect("first split");
    let leaf_once = host_leaf_descriptor(space.root_phys(), va).expect("mapped");
    // Re-splitting an already-fine region changes nothing and allocates
    // nothing (idempotent).
    space.split_block(va).expect("second split is a no-op");
    assert_eq!(
        host_leaf_descriptor(space.root_phys(), va),
        Some(leaf_once),
        "an already-split region is left untouched"
    );

    // Fail closed: a misaligned address and an address with no live
    // mapping are both rejected without mutating the space.
    assert_eq!(space.split_block(va | 0x1), Err(MapError::Misaligned));
    assert_eq!(space.split_block(64u64 << 30), Err(MapError::NotMapped));
}

#[test]
fn prepare_guard_arena_splits_every_covering_block_preserving_translation() {
    static POOL: PageTablePool = PageTablePool::new();
    let mut space = AddressSpace::new_identity_gigapages(&POOL, 2).expect("identity map");

    // An arena that straddles a 2 MiB boundary inside the RAM gigapage:
    // it starts 2 MiB-aligned and is 2 MiB + one page long, so it spans
    // two distinct L2 blocks. Both must end up as 4 KiB leaves.
    let base: u64 = (1u64 << 30) + 4 * BLOCK_2MIB;
    let len: u64 = BLOCK_2MIB + PAGE_SIZE as u64;
    assert!(is_block(
        host_leaf_descriptor(space.root_phys(), base).expect("block maps base")
    ));

    space
        .prepare_guard_arena(base, len)
        .expect("prepare the arena");

    // A page in the first covering block, a page in the second, and the
    // arena's last page all resolve through 4 KiB page leaves now, each
    // identity-translating exactly as the block did.
    for va in [base, base + BLOCK_2MIB, base + len - PAGE_SIZE as u64] {
        let leaf = host_leaf_descriptor(space.root_phys(), va).expect("page maps va");
        assert!(!is_block(leaf), "arena page {va:#x} is now a 4 KiB leaf");
        assert_eq!(
            phys_from_descriptor(leaf),
            va,
            "identity preserved at {va:#x}"
        );
    }

    // A single arena page now unmaps cleanly while its neighbour stays
    // mapped — the property the guard page relies on.
    let guard = base + 3 * PAGE_SIZE as u64;
    assert_eq!(
        rustos_arch_api::mmu::AddressSpace::unmap(&mut space, guard),
        Ok(guard),
    );
    assert_eq!(host_translate(space.root_phys(), guard), None);
    assert_eq!(
        host_translate(space.root_phys(), guard + PAGE_SIZE as u64),
        Some(guard + PAGE_SIZE as u64),
    );
}

#[test]
fn prepare_guard_arena_is_idempotent() {
    static POOL: PageTablePool = PageTablePool::new();
    let mut space = AddressSpace::new_identity_gigapages(&POOL, 2).expect("identity map");
    let base: u64 = (1u64 << 30) + 6 * BLOCK_2MIB;

    space.prepare_guard_arena(base, BLOCK_2MIB).expect("first");
    let leaf_once = host_leaf_descriptor(space.root_phys(), base).expect("mapped");
    space
        .prepare_guard_arena(base, BLOCK_2MIB)
        .expect("re-prepare is a no-op");
    assert_eq!(
        host_leaf_descriptor(space.root_phys(), base),
        Some(leaf_once),
        "an already-fine arena is left untouched",
    );
}

#[test]
fn prepare_guard_arena_fails_closed() {
    static POOL: PageTablePool = PageTablePool::new();
    let mut space = AddressSpace::new_identity_gigapages(&POOL, 2).expect("identity map");
    let base: u64 = (1u64 << 30) + 8 * BLOCK_2MIB;

    // Zero length, a misaligned base, and an arena over unmapped memory
    // are each rejected (`AGENTS.md` §2.9).
    assert_eq!(
        space.prepare_guard_arena(base, 0),
        Err(MapError::Misaligned)
    );
    assert_eq!(
        space.prepare_guard_arena(base | 0x1, BLOCK_2MIB),
        Err(MapError::Misaligned),
    );
    assert_eq!(
        space.prepare_guard_arena(64u64 << 30, BLOCK_2MIB),
        Err(MapError::NotMapped),
    );
    // A length that wraps the address space is a degenerate arena, refused
    // rather than truncated.
    assert_eq!(
        space.prepare_guard_arena(base, u64::MAX),
        Err(MapError::Misaligned),
    );
}

#[test]
fn passes_mmu_conformance() {
    use rustos_arch_api::mmu;
    static POOL: PageTablePool = PageTablePool::new();
    static POOL2: PageTablePool = PageTablePool::new();
    let mut space = AddressSpace::new_identity_gigapages(&POOL, 2).expect("identity map");
    // A VA well above the identity window so the conformance map allocates
    // fresh L2/L3 tables and never shatters a block; the phys frame is RAM.
    let va = 64u64 << 30;
    let pa = 0x4123_4000;
    mmu::conformance::run_all(&mut space, va, pa);
    // And over the object-safe erasure the kernel registry stores.
    let mut dynamic = AddressSpace::new_identity_gigapages(&POOL2, 2).expect("identity map");
    let erased: &mut dyn mmu::AddressSpace = &mut dynamic;
    mmu::conformance::run_all(erased, va, pa);
}

#[test]
fn declares_block_split_supported_and_the_hal_method_forwards() {
    use rustos_arch_api::mmu::{self, BlockSplit};
    static POOL: PageTablePool = PageTablePool::new();
    let mut space = AddressSpace::new_identity_gigapages(&POOL, 2).expect("identity map");

    // aarch64 honestly declares it can split coarse blocks (G1/G2).
    assert_eq!(space.block_split_support(), BlockSplit::Supported);

    // Driving `split_block` through the object-safe HAL trait must reach
    // the same body as the inherent method: a page inside a 1 GiB block
    // becomes a 4 KiB page leaf that then unmaps cleanly.
    let va: u64 = (1u64 << 30) + 0x40_0000;
    {
        let erased: &mut dyn mmu::AddressSpace = &mut space;
        erased
            .split_block(va)
            .expect("HAL split_block forwards to the inherent split");
    }
    assert_eq!(
        rustos_arch_api::mmu::AddressSpace::unmap(&mut space, va),
        Ok(va),
        "the HAL-split page unmaps to its identity frame"
    );

    // `prepare_guard_arena` (G3b) likewise forwards through the object-safe
    // HAL trait to the inherent body: a 2 MiB arena in a fresh block is
    // re-expressed at 4 KiB granularity, so a single page in it then unmaps.
    let arena: u64 = (1u64 << 30) + 10 * BLOCK_2MIB;
    {
        let erased: &mut dyn mmu::AddressSpace = &mut space;
        erased
            .prepare_guard_arena(arena, BLOCK_2MIB)
            .expect("HAL prepare_guard_arena forwards to the inherent body");
    }
    let arena_page = arena + 2 * PAGE_SIZE as u64;
    assert_eq!(
        rustos_arch_api::mmu::AddressSpace::unmap(&mut space, arena_page),
        Ok(arena_page),
        "a page in the HAL-prepared arena unmaps to its identity frame"
    );
}

#[test]
fn passes_tlb_conformance() {
    use rustos_arch_api::tlb;
    static POOL: PageTablePool = PageTablePool::new();
    let mut space = AddressSpace::new_identity_gigapages(&POOL, 2).expect("identity map");
    // The host has no TLB, so `flush_page` is a vacuous no-op here; the
    // suite proves it is object-safe and panic-free for any address (the
    // real `tlbi` is exercised by the spawn QEMU vertical).
    tlb::conformance::run_all(&mut space, 64u64 << 30);
    let mut dynamic = AddressSpace::new_identity_gigapages(&POOL, 2).expect("identity map");
    let erased: &mut dyn tlb::TlbShootdown = &mut dynamic;
    tlb::conformance::run_all(erased, 64u64 << 30);
}

#[test]
fn passes_frames_conformance() {
    use rustos_arch_api::frames::{self, PageTableFrames};
    // The static pool is the boot/bootstrap `PageTableFrames` source; its
    // `phys_of` is the identity map (kernel memory), so the suite runs on
    // the host. A fresh pool hands out `POOL_SIZE` frames before failing
    // closed; a second pool exercises the object-safe erasure.
    static POOL: PageTablePool = PageTablePool::new();
    static POOL2: PageTablePool = PageTablePool::new();
    frames::conformance::run_all(&POOL, super::POOL_SIZE);
    let erased: &dyn PageTableFrames = &POOL2;
    assert!(erased.alloc_table().is_some());
}

#[test]
fn map_page_translates_neutral_user_flags_to_wx_safe_leaves() {
    use rustos_arch_api::mmu::{self, PageFlags};
    static POOL: PageTablePool = PageTablePool::new();
    let mut space = AddressSpace::new_identity_gigapages(&POOL, 2).expect("identity map");

    // A USER + EXEC page must be mapped EL0-executable and read-only
    // (W^X): the leaf is `el0_code_leaf_attrs` (AP_RO_EL0, UXN clear).
    let code_va = 64u64 << 30;
    mmu::AddressSpace::map_page(
        &mut space,
        code_va,
        0x4100_0000,
        PageFlags::READ | PageFlags::EXEC | PageFlags::USER,
    )
    .expect("user code map");
    let code_leaf = host_leaf_descriptor(space.root_phys(), code_va).expect("mapped");
    assert_eq!(
        code_leaf & attrs::UXN,
        0,
        "user code must be EL0-executable"
    );
    assert_ne!(code_leaf & attrs::PXN, 0, "user code must be PXN at EL1");

    // A USER + WRITE page must be execute-never at both ELs (W^X).
    let data_va = (64u64 << 30) + (1u64 << 21);
    mmu::AddressSpace::map_page(
        &mut space,
        data_va,
        0x4101_0000,
        PageFlags::READ | PageFlags::WRITE | PageFlags::USER,
    )
    .expect("user data map");
    let data_leaf = host_leaf_descriptor(space.root_phys(), data_va).expect("mapped");
    assert_ne!(data_leaf & attrs::UXN, 0, "user data must be EL0 XN");
    assert_ne!(data_leaf & attrs::PXN, 0, "user data must be EL1 XN");
}
