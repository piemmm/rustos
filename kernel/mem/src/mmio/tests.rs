//! Unit tests for the MMIO register-window mapper.
//!
//! These exercise the architecture-neutral mapping logic against the
//! `HostPageTable` test double and a [`SimPhysMap`] standing in for the
//! device's register block, so the same code paths that run on
//! hardware are validated on the host: the pointer a driver writes
//! through addresses the very (simulated) registers.

use super::*;
use crate::frame::{PhysAddr, PAGE_SIZE};
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
fn guard_slots_are_left_unmapped() {
    // Guard pages bracketing the register window are never mapped, so
    // a register-block over-run faults instead of reaching a
    // neighbouring device (`AGENTS.md` §4).
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
