//! Unit tests for the MMIO register-window mapper.
//!
//! These exercise the architecture-neutral mapping logic against the
//! `HostPageTable` test double and a [`SimPhysMap`] standing in for the
//! device's register block, so the same code paths that run on
//! hardware are validated on the host: the pointer a driver writes
//! through addresses the very (simulated) registers.

use super::*;
use crate::frame::{Frame, PhysAddr, PAGE_SHIFT, PAGE_SIZE};
use crate::phys::SimPhysMap;
use crate::vmm::{AddressSpace, HostPageTable, MapFlags, Page, PageTableError, VirtAddr};

/// Physical base of the simulated register space and its span. Wide
/// enough to cover every BAR address the tests below map.
const SIM_BASE: u64 = 0xFEBD_0000;
const SIM_LEN: usize = 0x0004_0000;

/// Simulated physical RAM standing in for the device register block.
fn sim() -> SimPhysMap {
    SimPhysMap::new(PhysAddr::new(SIM_BASE), SIM_LEN)
}

/// Build a fresh mapper with a `capacity_pages`-page virtual window
/// anchored at a fixed, page-aligned base.
fn fresh(phys: &SimPhysMap, capacity_pages: usize) -> MmioMap<'_, HostPageTable> {
    MmioMap::new(
        AddressSpace::new(HostPageTable::new()),
        VirtAddr::new(0x4000_0000),
        capacity_pages,
        phys,
    )
    .expect("mapper constructs")
}

/// Read a little-endian `u32` through the region's base pointer +
/// offset. Byte-wise so the helper makes no alignment assumption
/// about the host backing (the production `RegisterWindow` asserts
/// the alignment contract instead).
fn read_u32(map: &MmioMap<'_, HostPageTable>, region: &MmioRegion, offset: usize) -> u32 {
    let base = map.region_base(region).expect("live region");
    let mut bytes = [0u8; 4];
    for (i, b) in bytes.iter_mut().enumerate() {
        // SAFETY: `offset + 4 <= region.len()` is the caller's
        // contract in these tests; each byte is inside the live
        // window backing.
        *b = unsafe { base.as_ptr().add(offset + i).read() };
    }
    u32::from_le_bytes(bytes)
}

/// Write a little-endian `u32` through the region's base pointer +
/// offset. Byte-wise, mirroring [`read_u32`].
fn write_u32(map: &MmioMap<'_, HostPageTable>, region: &MmioRegion, offset: usize, value: u32) {
    let base = map.region_base(region).expect("live region");
    for (i, b) in value.to_le_bytes().iter().enumerate() {
        // SAFETY: as in `read_u32`.
        unsafe { base.as_ptr().add(offset + i).write(*b) };
    }
}

#[test]
fn new_rejects_invalid_config() {
    let phys = sim();
    let r = MmioMap::new(
        AddressSpace::new(HostPageTable::new()),
        VirtAddr::new(0x4000_0000),
        0,
        &phys,
    );
    assert_eq!(r.err(), Some(MmioError::InvalidMapConfig));
    let r = MmioMap::new(
        AddressSpace::new(HostPageTable::new()),
        VirtAddr::new(0x4000_0001),
        4,
        &phys,
    );
    assert_eq!(r.err(), Some(MmioError::InvalidMapConfig));
}

#[test]
fn map_then_round_trip_register() {
    let phys = sim();
    let mut map = fresh(&phys, 16);
    let region = map.map(0xFEBD_0000, 0x1000).expect("page-aligned BAR maps");
    assert_eq!(region.phys(), 0xFEBD_0000);
    assert_eq!(region.len(), 0x1000);
    // A freshly mapped window reads as zero (simulated RAM is zeroed).
    assert_eq!(read_u32(&map, &region, 0), 0);
    write_u32(&map, &region, 0x10, 0xCAFE_F00D);
    assert_eq!(read_u32(&map, &region, 0x10), 0xCAFE_F00D);
    // One data page mapped (plus two unmapped guard slots).
    assert_eq!(map.mapped_pages(), 1);
    assert_eq!(map.live(), 1);
}

#[test]
fn region_base_addresses_the_device_physical_frame() {
    // The hardware-realism invariant: the pointer `region_base` hands
    // out addresses the region's *device physical* base through the
    // direct map, so a byte written "as the device" at that physical
    // address is seen through the register window.
    let phys = sim();
    let mut map = fresh(&phys, 16);
    let region = map.map(0xFEBD_0000, 0x1000).expect("maps");
    let dev = phys
        .translate(PhysAddr::new(0xFEBD_0000), 0x1000)
        .expect("dev");
    // SAFETY: `dev` names the region's frame in the simulator; no
    // other live region covers it.
    unsafe { dev.as_ptr().add(0x24).write(0x7E) };
    assert_eq!(read_u32(&map, &region, 0x24) & 0xFF, 0x7E);
}

#[test]
fn map_preserves_within_page_offset() {
    let phys = sim();
    let mut map = fresh(&phys, 16);
    // A BAR whose base sits 0x40 bytes into its page.
    let phys_base = 0xFEBD_0040;
    let region = map.map(phys_base, 0x20).expect("sub-page region maps");
    assert_eq!(region.phys(), phys_base);
    // The window virtual base carries the same within-page offset.
    assert_eq!((region.virt().as_u64() & 0xFFF), 0x40);
    write_u32(&map, &region, 0, 0x1234_5678);
    assert_eq!(read_u32(&map, &region, 0), 0x1234_5678);
    // Spanning a single page is enough.
    assert_eq!(map.mapped_pages(), 1);
}

#[test]
fn region_spanning_two_pages_maps_two_frames() {
    let phys = sim();
    let mut map = fresh(&phys, 16);
    // Base 0xF00 into the page + 0x200 length straddles the page
    // boundary, so two frames must be mapped.
    let region = map.map(0xFEBD_0F00, 0x200).expect("straddling region maps");
    assert_eq!(map.mapped_pages(), 2);
    // The tail byte of the second page is reachable.
    write_u32(&map, &region, 0x1FC, 0xABCD_0001);
    assert_eq!(read_u32(&map, &region, 0x1FC), 0xABCD_0001);
}

#[test]
fn two_regions_are_disjoint() {
    let phys = sim();
    let mut map = fresh(&phys, 32);
    let a = map.map(0xFEBD_0000, 0x1000).expect("first");
    let b = map.map(0xFEBE_0000, 0x1000).expect("second");
    assert_ne!(a.virt(), b.virt());
    write_u32(&map, &a, 0, 0xAAAA_AAAA);
    write_u32(&map, &b, 0, 0xBBBB_BBBB);
    assert_eq!(read_u32(&map, &a, 0), 0xAAAA_AAAA);
    assert_eq!(read_u32(&map, &b, 0), 0xBBBB_BBBB);
    assert_eq!(map.live(), 2);
}

#[test]
fn zero_length_is_invalid() {
    let phys = sim();
    let mut map = fresh(&phys, 8);
    assert_eq!(map.map(0xFEBD_0000, 0), Err(MmioError::InvalidRegion));
}

#[test]
fn physical_overflow_is_invalid() {
    let phys = sim();
    let mut map = fresh(&phys, 8);
    assert_eq!(map.map(u64::MAX - 1, 0x10), Err(MmioError::InvalidRegion));
}

#[test]
fn exhausted_virtual_window_reports_no_space() {
    // Capacity 4 pages: a 0x1000 region needs 1 data + 2 guard = 3
    // slots; a second identical request cannot fit.
    let phys = sim();
    let mut map = fresh(&phys, 4);
    let _a = map.map(0xFEBD_0000, 0x1000).expect("first fits");
    assert_eq!(map.map(0xFEBE_0000, 0x1000), Err(MmioError::NoVirtualSpace));
}

#[test]
fn scanout_sized_window_maps_in_a_span_ceiling() {
    // A ~4 MiB linear scan-out surface (1024 data pages) maps out of a
    // window whose ceiling is a whole reserved 1 GiB virtual span, and
    // the occupancy bitmap grows only as far as the mapping actually
    // reaches: the ceiling is structural, never an up-front cost.
    let phys = sim();
    let span_pages = 0x4000_0000usize / PAGE_SIZE;
    let mut map = fresh(&phys, span_pages);
    let len = 1024 * PAGE_SIZE;
    let region = map.map(0x8000_0000, len).expect("scan-out surface maps");
    assert_eq!(region.len(), len);
    assert_eq!(map.mapped_pages(), 1024);
    // 1024 data slots plus the two guard slots — and nothing more.
    assert_eq!(map.window.slot_used.len(), 1024 + 2);
    assert_eq!(map.window.capacity_pages(), span_pages);
}

#[test]
fn request_beyond_the_span_ceiling_fails_closed() {
    // An 8-page ceiling cannot hold 7 data pages + 2 guards; the
    // request is refused as a value before any page-table mutation.
    let phys = sim();
    let mut map = fresh(&phys, 8);
    assert_eq!(
        map.map(0x8000_0000, 7 * PAGE_SIZE),
        Err(MmioError::NoVirtualSpace)
    );
    assert_eq!(map.live(), 0);
    assert_eq!(map.mapped_pages(), 0);
}

#[test]
fn guard_slots_are_left_unmapped() {
    // Guard pages bracketing the register window are never mapped, so
    // a register-block over-run faults instead of reaching a
    // neighbouring device.
    let phys = sim();
    let mut map = fresh(&phys, 16);
    let region = map.map(0xFEBD_0000, 0x1000).expect("maps");
    let data_virt = region.virt().as_u64() & !0xFFF;
    let leading = VirtAddr::new(data_virt - PAGE_SIZE as u64);
    let trailing = VirtAddr::new(data_virt + PAGE_SIZE as u64);
    assert!(map
        .address_space
        .translate(Page::from_addr(leading).unwrap())
        .is_none());
    assert!(map
        .address_space
        .translate(Page::from_addr(trailing).unwrap())
        .is_none());
}

#[test]
fn unmap_releases_slots_and_frames() {
    let phys = sim();
    let mut map = fresh(&phys, 16);
    let region = map.map(0xFEBD_0000, 0x1000).expect("maps");
    assert_eq!(map.mapped_pages(), 1);
    map.unmap(region).expect("clean unmap");
    assert_eq!(map.live(), 0);
    assert_eq!(map.mapped_pages(), 0);
    // The freed slots can be reused.
    let again = map.map(0xFEC0_0000, 0x1000).expect("reuse after unmap");
    assert_eq!(map.live(), 1);
    let _ = again;
}

#[test]
fn unmap_unknown_region_is_rejected() {
    let phys = sim();
    let mut map = fresh(&phys, 16);
    let region = map.map(0xFEBD_0000, 0x1000).expect("maps");
    map.unmap(region).expect("first unmap");
    // A second unmap of the same region is a double-free.
    assert_eq!(map.unmap(region), Err(MmioError::UnknownRegion));
}

#[test]
fn region_base_after_unmap_is_unknown() {
    let phys = sim();
    let mut map = fresh(&phys, 16);
    let region = map.map(0xFEBD_0000, 0x1000).expect("maps");
    map.unmap(region).expect("unmap");
    assert_eq!(
        map.region_base(&region).err(),
        Some(MmioError::UnknownRegion)
    );
}

#[test]
fn mapped_frame_matches_physical_base() {
    let phys = sim();
    let mut map = fresh(&phys, 16);
    let phys_base = 0xFEBD_0000u64;
    let region = map.map(phys_base, 0x1000).expect("maps");
    // The data page's virtual address must translate to the device's
    // physical frame in the address space.
    let page =
        Page::from_addr(VirtAddr::new(region.virt().as_u64() & !0xFFF)).expect("aligned page");
    let (frame, flags) = map
        .address_space
        .translate(page)
        .expect("data page is mapped");
    assert_eq!(frame.start().as_u64(), phys_base);
    assert!(flags.contains(MapFlags::NO_CACHE));
    assert!(flags.contains(MapFlags::READ));
    assert!(flags.contains(MapFlags::WRITE));
}

#[test]
fn display_renders_each_variant() {
    use alloc::format;
    assert!(!format!("{}", MmioError::InvalidRegion).is_empty());
    assert!(!format!("{}", MmioError::NoVirtualSpace).is_empty());
    assert!(!format!("{}", MmioError::UnknownRegion).is_empty());
    assert!(!format!("{}", MmioError::DirectMap).is_empty());
    assert!(!format!("{}", MmioError::InvalidMapConfig).is_empty());
    assert!(!format!("{}", MmioError::PageTable(PageTableError::NotMapped)).is_empty());
}

// -------------------------------------------------------------------------
// `MmioWindowMap`: the guarded MMIO mapper over a *borrowed* `&mut
// AddressSpace<P>` (the mechanism the `mmio_map` syscall facility drives
// against a caller's retained live space, `plans/PI.md` P10 chunk 5d-0).
// These prove the borrowed-space API independently of the owning `MmioMap`
// wrapper, which the tests above already cover.
// -------------------------------------------------------------------------

/// A fresh, empty user address space to lend the window mapper.
fn borrowed_space() -> AddressSpace<HostPageTable> {
    AddressSpace::new(HostPageTable::new())
}

/// A window mapper anchored at the same fixed base the `fresh` helper uses.
fn window(capacity_pages: usize) -> MmioWindowMap {
    MmioWindowMap::new(VirtAddr::new(0x4000_0000), capacity_pages).expect("window constructs")
}

#[test]
fn window_map_new_rejects_invalid_config() {
    assert_eq!(
        MmioWindowMap::new(VirtAddr::new(0x4000_0000), 0).err(),
        Some(MmioError::InvalidMapConfig)
    );
    assert_eq!(
        MmioWindowMap::new(VirtAddr::new(0x4000_0001), 4).err(),
        Some(MmioError::InvalidMapConfig)
    );
}

#[test]
fn window_map_into_borrowed_space_round_trips_and_leaves_space_to_caller() {
    let phys = sim();
    let mut space = borrowed_space();
    let mut win = window(16);

    let region = win
        .map_into(&mut space, 0xFEBD_0000, 0x1000)
        .expect("page-aligned BAR maps into the borrowed space");
    assert_eq!(region.phys(), 0xFEBD_0000);
    assert_eq!(win.live(), 1);
    // One data page is mapped into the *caller's* space; the caller still
    // owns `space` and can observe the mapping itself.
    assert_eq!(space.mapped_pages(), 1);

    // The pointer `region_base` hands out addresses the device's own frame
    // through the direct map, so a register round-trips.
    let base = win.region_base(&region, &phys).expect("live region");
    // SAFETY: `base` covers `region.len()` bytes of the simulated register
    // block the test owns; writing/reading 4 bytes at offset 0x10 is inside it.
    unsafe {
        for (i, b) in 0xCAFE_F00Du32.to_le_bytes().iter().enumerate() {
            base.as_ptr().add(0x10 + i).write(*b);
        }
        let mut got = [0u8; 4];
        for (i, b) in got.iter_mut().enumerate() {
            *b = base.as_ptr().add(0x10 + i).read();
        }
        assert_eq!(u32::from_le_bytes(got), 0xCAFE_F00D);
    }
}

#[test]
fn window_map_brackets_unmapped_guard_pages_in_the_borrowed_space() {
    // The guard pages bracketing the window are never mapped in the borrowed
    // space, so a register-block over-run faults instead of reaching a
    // neighbouring device.
    let mut space = borrowed_space();
    let mut win = window(16);
    let region = win.map_into(&mut space, 0xFEBD_0000, 0x1000).expect("maps");
    let data_virt = region.virt().as_u64() & !0xFFF;
    let leading = VirtAddr::new(data_virt - PAGE_SIZE as u64);
    let trailing = VirtAddr::new(data_virt + PAGE_SIZE as u64);
    assert!(space.translate(Page::from_addr(leading).unwrap()).is_none());
    assert!(space
        .translate(Page::from_addr(trailing).unwrap())
        .is_none());
    // The data page carries the device flags: uncached, never executable.
    let data_page = Page::from_addr(VirtAddr::new(data_virt)).unwrap();
    let (_, flags) = space.translate(data_page).expect("data page mapped");
    assert!(flags.contains(MapFlags::NO_CACHE));
    assert!(flags.contains(MapFlags::READ));
    assert!(flags.contains(MapFlags::WRITE));
    assert!(flags.contains(MapFlags::USER));
    assert!(!flags.contains(MapFlags::EXEC));
}

#[test]
fn window_unmap_from_releases_slots_and_pages_for_reuse() {
    let mut space = borrowed_space();
    let mut win = window(16);
    let region = win.map_into(&mut space, 0xFEBD_0000, 0x1000).expect("maps");
    assert_eq!(space.mapped_pages(), 1);

    win.unmap_from(&mut space, region).expect("clean unmap");
    assert_eq!(win.live(), 0);
    assert_eq!(space.mapped_pages(), 0);

    // The freed slots can be reused for another window.
    let again = win
        .map_into(&mut space, 0xFEC0_0000, 0x1000)
        .expect("reuse after unmap");
    assert_eq!(win.live(), 1);
    let _ = again;
}

#[test]
fn window_unmap_from_unknown_region_is_rejected() {
    let mut space = borrowed_space();
    let mut win = window(16);
    let region = win.map_into(&mut space, 0xFEBD_0000, 0x1000).expect("maps");
    win.unmap_from(&mut space, region).expect("first unmap");
    // A second unmap of the same region is a double-free.
    assert_eq!(
        win.unmap_from(&mut space, region),
        Err(MmioError::UnknownRegion)
    );
}

#[test]
fn window_region_base_of_unmapped_region_is_unknown() {
    let phys = sim();
    let mut space = borrowed_space();
    let mut win = window(16);
    let region = win.map_into(&mut space, 0xFEBD_0000, 0x1000).expect("maps");
    win.unmap_from(&mut space, region).expect("unmap");
    assert_eq!(
        win.region_base(&region, &phys).err(),
        Some(MmioError::UnknownRegion)
    );
}

#[test]
fn window_map_into_rejects_zero_overflow_and_exhaustion() {
    let mut space = borrowed_space();
    let mut win = window(4);
    assert_eq!(
        win.map_into(&mut space, 0xFEBD_0000, 0),
        Err(MmioError::InvalidRegion)
    );
    assert_eq!(
        win.map_into(&mut space, u64::MAX - 1, 0x10),
        Err(MmioError::InvalidRegion)
    );
    // Capacity 4 pages: a 0x1000 region needs 1 data + 2 guard = 3 slots; a
    // second identical request cannot fit.
    let _a = win
        .map_into(&mut space, 0xFEBD_0000, 0x1000)
        .expect("first fits");
    assert_eq!(
        win.map_into(&mut space, 0xFEBE_0000, 0x1000),
        Err(MmioError::NoVirtualSpace)
    );
}

#[test]
fn window_map_into_fails_closed_and_unwinds_on_page_table_conflict() {
    // Pre-map the page the mapper would use for the first data slot so the
    // borrowed space rejects the mapping mid-way; the mapper must unwind every
    // page it added, leaving the space exactly as it found it (all-or-nothing).
    let mut space = borrowed_space();
    let mut win = window(16);
    // A 2-page window's data lands at slots 1 and 2 (slot 0 is the leading
    // guard). Pre-map slot 2's page so the *second* data page collides: the
    // first data page maps, then the second fails, forcing a real unwind of
    // the page this call already added.
    let second_data_va = VirtAddr::new(0x4000_0000 + 2 * PAGE_SIZE as u64);
    space
        .map(
            Page::from_addr(second_data_va).unwrap(),
            crate::frame::Frame(0x1234),
            MapFlags::READ | MapFlags::USER,
        )
        .expect("pre-map a conflicting page");
    let before = space.mapped_pages();

    let err = win
        .map_into(&mut space, 0xFEBD_0000, 0x2000)
        .expect_err("conflicting second data page fails the map");
    assert!(matches!(err, MmioError::PageTable(_)));
    // No new mapping survived; the pre-existing page is untouched and the
    // mapper recorded no live region.
    assert_eq!(space.mapped_pages(), before);
    assert_eq!(win.live(), 0);
}

#[test]
fn window_map_cacheable_chunks_maps_blocks_into_one_contiguous_window() {
    let mut space = borrowed_space();
    let mut win = window(16);

    // Two physically-disjoint blocks — a two-page block and a one-page block
    // — become one flat three-page virtual window (the display frame ring a
    // single buddy block could not hold).
    let chunk_a: u64 = 0x1000_0000; // frame 0x10000
    let chunk_b: u64 = 0x2000_0000; // frame 0x20000
    let region = win
        .map_cacheable_chunks_into(&mut space, &[(chunk_a, 2), (chunk_b, 1)])
        .expect("chunk list maps");
    assert_eq!(region.len(), 3 * PAGE_SIZE);
    assert_eq!(win.live(), 1);
    assert_eq!(space.mapped_pages(), 3);

    // Each window page resolves to the expected frame, in order, so the
    // blocks are laid out back-to-back in virtual space (one flat buffer).
    let base = region.virt().as_u64();
    let expect = [
        chunk_a >> PAGE_SHIFT,
        (chunk_a >> PAGE_SHIFT) + 1,
        chunk_b >> PAGE_SHIFT,
    ];
    for (i, &want) in expect.iter().enumerate() {
        let va = VirtAddr::new(base + (i as u64) * PAGE_SIZE as u64);
        let (frame, flags) = space
            .translate(Page::from_addr(va).unwrap())
            .expect("window page mapped");
        assert_eq!(
            frame,
            Frame(usize::try_from(want).unwrap()),
            "page {i} frame"
        );
        // Shared RAM: cacheable (never device-ordered), RW, user, not exec.
        assert!(!flags.contains(MapFlags::NO_CACHE));
        assert!(flags.contains(MapFlags::READ | MapFlags::WRITE | MapFlags::USER));
        assert!(!flags.contains(MapFlags::EXEC));
    }

    // The window is bracketed by unmapped guard pages, like the single-block
    // path, so an over-run faults instead of reaching a neighbour.
    let leading = VirtAddr::new(base - PAGE_SIZE as u64);
    let trailing = VirtAddr::new(base + 3 * PAGE_SIZE as u64);
    assert!(space.translate(Page::from_addr(leading).unwrap()).is_none());
    assert!(space
        .translate(Page::from_addr(trailing).unwrap())
        .is_none());

    // Releasing by base tears down every data page; the frames belong to the
    // registry, so only the mapping goes.
    win.unmap_at(&mut space, region.virt())
        .expect("unmap by base");
    assert_eq!(win.live(), 0);
    assert_eq!(space.mapped_pages(), 0);
}

#[test]
fn window_map_cacheable_chunks_rejects_a_bad_chunk_list() {
    let mut space = borrowed_space();
    let mut win = window(16);
    // Empty list.
    assert_eq!(
        win.map_cacheable_chunks_into(&mut space, &[]).err(),
        Some(MmioError::InvalidRegion)
    );
    // A zero-length chunk.
    assert_eq!(
        win.map_cacheable_chunks_into(&mut space, &[(0x1000_0000, 0)])
            .err(),
        Some(MmioError::InvalidRegion)
    );
    // A misaligned chunk base.
    assert_eq!(
        win.map_cacheable_chunks_into(&mut space, &[(0x1000_0800, 1)])
            .err(),
        Some(MmioError::InvalidRegion)
    );
    // Every refusal left the space and mapper untouched (fail closed).
    assert_eq!(space.mapped_pages(), 0);
    assert_eq!(win.live(), 0);
}
