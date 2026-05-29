//! Unit tests for the MMIO register-window mapper.
//!
//! These exercise the architecture-neutral mapping logic against the
//! `HostPageTable` test double and the `Vec<u8>` window backing, so
//! the same code paths that run on hardware are validated on the host.

use super::*;
use crate::frame::PAGE_SIZE;
use crate::vmm::{AddressSpace, HostPageTable, MapFlags, Page, PageTableError, VirtAddr};

/// Build a fresh mapper with a `capacity_pages`-page virtual window
/// anchored at a fixed, page-aligned base.
fn fresh(capacity_pages: usize) -> MmioMap<HostPageTable> {
    MmioMap::new(
        AddressSpace::new(HostPageTable::new()),
        VirtAddr::new(0x4000_0000),
        capacity_pages,
    )
    .expect("mapper constructs")
}

/// Read a little-endian `u32` through the region's base pointer +
/// offset. Byte-wise so the helper makes no alignment assumption
/// about the host backing (the production `RegisterWindow` asserts
/// the alignment contract instead).
fn read_u32(map: &MmioMap<HostPageTable>, region: &MmioRegion, offset: usize) -> u32 {
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
fn write_u32(map: &MmioMap<HostPageTable>, region: &MmioRegion, offset: usize, value: u32) {
    let base = map.region_base(region).expect("live region");
    for (i, b) in value.to_le_bytes().iter().enumerate() {
        // SAFETY: as in `read_u32`.
        unsafe { base.as_ptr().add(offset + i).write(*b) };
    }
}

#[test]
fn new_rejects_invalid_config() {
    let r = MmioMap::new(
        AddressSpace::new(HostPageTable::new()),
        VirtAddr::new(0x4000_0000),
        0,
    );
    assert_eq!(r.err(), Some(MmioError::InvalidMapConfig));
    let r = MmioMap::new(
        AddressSpace::new(HostPageTable::new()),
        VirtAddr::new(0x4000_0001),
        4,
    );
    assert_eq!(r.err(), Some(MmioError::InvalidMapConfig));
}

#[test]
fn map_then_round_trip_register() {
    let mut map = fresh(16);
    let region = map.map(0xFEBD_0000, 0x1000).expect("page-aligned BAR maps");
    assert_eq!(region.phys(), 0xFEBD_0000);
    assert_eq!(region.len(), 0x1000);
    // A freshly mapped window reads as zero.
    assert_eq!(read_u32(&map, &region, 0), 0);
    write_u32(&map, &region, 0x10, 0xCAFE_F00D);
    assert_eq!(read_u32(&map, &region, 0x10), 0xCAFE_F00D);
    // One data page mapped (plus two unmapped guard slots).
    assert_eq!(map.mapped_pages(), 1);
    assert_eq!(map.live(), 1);
}

#[test]
fn map_preserves_within_page_offset() {
    let mut map = fresh(16);
    // A BAR whose base sits 0x40 bytes into its page.
    let phys = 0xFEBD_0040;
    let region = map.map(phys, 0x20).expect("sub-page region maps");
    assert_eq!(region.phys(), phys);
    // The window virtual base carries the same within-page offset.
    assert_eq!((region.virt().as_u64() & 0xFFF), 0x40);
    write_u32(&map, &region, 0, 0x1234_5678);
    assert_eq!(read_u32(&map, &region, 0), 0x1234_5678);
    // Spanning a single page is enough.
    assert_eq!(map.mapped_pages(), 1);
}

#[test]
fn region_spanning_two_pages_maps_two_frames() {
    let mut map = fresh(16);
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
    let mut map = fresh(32);
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
    let mut map = fresh(8);
    assert_eq!(map.map(0xFEBD_0000, 0), Err(MmioError::InvalidRegion));
}

#[test]
fn physical_overflow_is_invalid() {
    let mut map = fresh(8);
    assert_eq!(map.map(u64::MAX - 1, 0x10), Err(MmioError::InvalidRegion));
}

#[test]
fn exhausted_virtual_window_reports_no_space() {
    // Capacity 4 pages: a 0x1000 region needs 1 data + 2 guard = 3
    // slots; a second identical request cannot fit.
    let mut map = fresh(4);
    let _a = map.map(0xFEBD_0000, 0x1000).expect("first fits");
    assert_eq!(map.map(0xFEBE_0000, 0x1000), Err(MmioError::NoVirtualSpace));
}

#[test]
fn unmap_releases_slots_and_frames() {
    let mut map = fresh(16);
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
    let mut map = fresh(16);
    let region = map.map(0xFEBD_0000, 0x1000).expect("maps");
    map.unmap(region).expect("first unmap");
    // A second unmap of the same region is a double-free.
    assert_eq!(map.unmap(region), Err(MmioError::UnknownRegion));
}

#[test]
fn region_base_after_unmap_is_unknown() {
    let mut map = fresh(16);
    let region = map.map(0xFEBD_0000, 0x1000).expect("maps");
    map.unmap(region).expect("unmap");
    assert_eq!(
        map.region_base(&region).err(),
        Some(MmioError::UnknownRegion)
    );
}

#[test]
fn leading_guard_overrun_is_detected() {
    let mut map = fresh(16);
    let region = map.map(0xFEBD_0000, 0x1000).expect("maps");
    // The data page sits at slot 1 (slot 0 is the leading guard).
    // Disturb the last byte of the leading guard slot, simulating a
    // backward over-run.
    map.poke_for_test(PAGE_SIZE - 1, 0x00).expect("in window");
    assert_eq!(map.unmap(region), Err(MmioError::GuardViolation));
    // Even on a guard violation the mapping is still torn down.
    assert_eq!(map.live(), 0);
    assert_eq!(map.mapped_pages(), 0);
}

#[test]
fn trailing_guard_overrun_is_detected() {
    let mut map = fresh(16);
    let region = map.map(0xFEBD_0000, 0x1000).expect("maps");
    // Data page is slot 1; trailing guard is slot 2. Disturb its
    // first byte (a forward over-run).
    map.poke_for_test(2 * PAGE_SIZE, 0x00).expect("in window");
    assert_eq!(map.unmap(region), Err(MmioError::GuardViolation));
}

#[test]
fn mapped_frame_matches_physical_base() {
    let mut map = fresh(16);
    let phys = 0xFEBD_0000u64;
    let region = map.map(phys, 0x1000).expect("maps");
    // The data page's virtual address must translate to the device's
    // physical frame in the address space.
    let page =
        Page::from_addr(VirtAddr::new(region.virt().as_u64() & !0xFFF)).expect("aligned page");
    let (frame, flags) = map
        .address_space
        .translate(page)
        .expect("data page is mapped");
    assert_eq!(frame.start().as_u64(), phys);
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
    assert!(!format!("{}", MmioError::GuardViolation).is_empty());
    assert!(!format!("{}", MmioError::InvalidMapConfig).is_empty());
    assert!(!format!("{}", MmioError::PageTable(PageTableError::NotMapped)).is_empty());
}
