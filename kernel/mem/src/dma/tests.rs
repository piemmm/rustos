//! Unit tests for the per-process-heap DMA allocator.
//!
//! Every invariant called out in the module-level documentation has a
//! matching test below. The tests run entirely on the host:
//! [`HostPageTable`] stands in for an architecture page-table type,
//! and a freshly-constructed [`FrameAllocator`] over a small synthetic
//! memory map supplies the backing frames.

use super::*;
use crate::bootinfo::{BootMemoryMap, MemoryRegion, RegionKind};
use crate::frame::{FrameAllocator, PAGE_SIZE};
use crate::vmm::{AddressSpace, HostPageTable, VirtAddr};

/// Build a synthetic memory map of `usable_pages` pages of usable RAM
/// starting at frame 16 (leaving the low frames free for hypothetical
/// reserved regions, mirroring `slab.rs`' test style).
fn small_map(usable_pages: usize) -> BootMemoryMap {
    let mut m = BootMemoryMap::new();
    m.push(MemoryRegion {
        kind: RegionKind::Usable,
        start: PhysAddr::new(PAGE_SIZE as u64 * 16),
        length: (PAGE_SIZE * usable_pages) as u64,
    });
    m
}

/// Fresh frame allocator that the test's `DmaPool` will borrow from.
fn fresh_frames(usable_pages: usize) -> FrameAllocator {
    FrameAllocator::new(&small_map(usable_pages)).expect("frame allocator")
}

/// Construct a pool with `capacity_pages` virtual slots and at least
/// `capacity_pages * 2` worth of physical frames so OOM is the
/// virtual-window allocator's responsibility, not the frame allocator's.
fn pool_with_capacity(
    frames: &FrameAllocator,
    capacity_pages: usize,
) -> DmaPool<'_, HostPageTable> {
    DmaPool::new(
        AddressSpace::new(HostPageTable::new()),
        VirtAddr::new(0x1000_0000),
        capacity_pages,
        frames,
    )
    .expect("pool constructs")
}

#[test]
fn new_rejects_zero_capacity() {
    let frames = fresh_frames(4);
    let err = DmaPool::new(
        AddressSpace::new(HostPageTable::new()),
        VirtAddr::new(0x1000_0000),
        0,
        &frames,
    );
    assert_eq!(err.err(), Some(DmaError::InvalidPoolConfig));
}

#[test]
fn new_rejects_misaligned_base() {
    let frames = fresh_frames(4);
    let err = DmaPool::new(
        AddressSpace::new(HostPageTable::new()),
        VirtAddr::new(0x1000_0001),
        4,
        &frames,
    );
    assert_eq!(err.err(), Some(DmaError::InvalidPoolConfig));
}

#[test]
fn alloc_zero_rejected() {
    let frames = fresh_frames(16);
    let mut pool = pool_with_capacity(&frames, 8);
    assert_eq!(pool.alloc(0).err(), Some(DmaError::ZeroSize));
}

#[test]
fn alloc_returns_page_aligned_virt_and_phys() {
    let frames = fresh_frames(16);
    let mut pool = pool_with_capacity(&frames, 8);
    let buf = pool.alloc(1).expect("alloc one byte");
    assert!(buf.virt().is_page_aligned());
    assert_eq!(buf.phys().as_u64() % (PAGE_SIZE as u64), 0);
    // One-byte request rounds up to one page.
    assert_eq!(buf.len(), PAGE_SIZE);
}

#[test]
fn alloc_rounds_up_to_next_power_of_two_pages() {
    let frames = fresh_frames(64);
    let mut pool = pool_with_capacity(&frames, 32);
    // 5 KiB ⇒ 2 pages of data needed, which is already a power of two.
    let buf = pool.alloc(5 * 1024).expect("alloc 5 KiB");
    assert_eq!(buf.len(), 2 * PAGE_SIZE);
    // 9 KiB ⇒ 3 pages needed, rounded up to 4.
    let buf2 = pool.alloc(9 * 1024).expect("alloc 9 KiB");
    assert_eq!(buf2.len(), 4 * PAGE_SIZE);
}

#[test]
fn alloc_too_large_returns_size_unsupported() {
    let frames = fresh_frames(16);
    let mut pool = pool_with_capacity(&frames, 32);
    // (1 << MAX_ORDER) + 1 pages forces order = MAX_ORDER + 1.
    let too_big = (1usize << (MAX_ORDER + 1)) * PAGE_SIZE;
    assert_eq!(pool.alloc(too_big).err(), Some(DmaError::SizeUnsupported));
}

#[test]
fn alloc_capacity_exhausted_returns_oom() {
    // 4 slots ⇒ at most one 1-page allocation (1 + 1 + 1 guards = 3,
    // leaving 1 slack), a second one would need 3 more slots: OOM.
    let frames = fresh_frames(8);
    let mut pool = pool_with_capacity(&frames, 4);
    let _first = pool.alloc(PAGE_SIZE).expect("first allocation succeeds");
    assert_eq!(
        pool.alloc(PAGE_SIZE).err(),
        Some(DmaError::Alloc(AllocError::OutOfMemory))
    );
}

#[test]
fn free_unknown_buffer_rejected() {
    let frames = fresh_frames(8);
    let mut pool = pool_with_capacity(&frames, 4);
    let bogus = DmaBuffer {
        virt: VirtAddr::new(0xDEAD_0000),
        phys: PhysAddr::new(0),
        len: PAGE_SIZE,
    };
    assert_eq!(pool.free(bogus).err(), Some(DmaError::UnknownBuffer));
}

#[test]
fn double_free_rejected() {
    let frames = fresh_frames(8);
    let mut pool = pool_with_capacity(&frames, 4);
    let buf = pool.alloc(PAGE_SIZE).expect("alloc");
    pool.free(buf).expect("first free");
    assert_eq!(pool.free(buf).err(), Some(DmaError::UnknownBuffer));
}

#[test]
fn alloc_returns_zero_initialised_bytes() {
    let frames = fresh_frames(16);
    let mut pool = pool_with_capacity(&frames, 8);
    let buf = pool.alloc(PAGE_SIZE).expect("alloc");
    assert!(pool.bytes(buf).unwrap().iter().all(|&b| b == 0));
}

#[test]
fn reuse_after_free_sees_zeroed_buffer() {
    // The core security invariant of `AGENTS.md` §4:
    // "Zero-on-free for any allocation that ever held credentials".
    // After freeing a buffer that held a sentinel, the next allocation
    // that lands on the same region must observe zeros, not the sentinel.
    let frames = fresh_frames(16);
    let mut pool = pool_with_capacity(&frames, 8);
    let buf = pool.alloc(PAGE_SIZE).expect("alloc");
    // Write a distinctive sentinel into the data.
    for b in pool.bytes_mut(buf).unwrap().iter_mut() {
        *b = 0xA5;
    }
    pool.free(buf).expect("free");
    // Allocate again. Because the pool is mostly empty the new
    // allocation will land at the same slot.
    let buf2 = pool.alloc(PAGE_SIZE).expect("re-alloc");
    assert_eq!(buf2.virt(), buf.virt(), "test relies on slot reuse");
    assert!(
        pool.bytes(buf2).unwrap().iter().all(|&b| b == 0),
        "freed bytes must not leak into the next allocation"
    );
}

#[test]
fn allocations_have_distinct_phys_addresses() {
    let frames = fresh_frames(16);
    let mut pool = pool_with_capacity(&frames, 16);
    let a = pool.alloc(PAGE_SIZE).expect("a");
    let b = pool.alloc(PAGE_SIZE).expect("b");
    assert_ne!(a.phys(), b.phys());
    assert_ne!(a.virt(), b.virt());
}

#[test]
fn frees_return_frames_to_the_allocator() {
    let frames = fresh_frames(16);
    let initial_free = frames.free_frames();
    let mut pool = pool_with_capacity(&frames, 16);
    let buf = pool.alloc(4 * PAGE_SIZE).expect("alloc 4 pages");
    assert!(frames.free_frames() < initial_free);
    pool.free(buf).expect("free");
    assert_eq!(frames.free_frames(), initial_free);
}

#[test]
fn address_space_records_one_mapping_per_data_page() {
    let frames = fresh_frames(16);
    let mut pool = pool_with_capacity(&frames, 16);
    let buf = pool.alloc(2 * PAGE_SIZE).expect("alloc 2 pages");
    assert_eq!(pool.address_space.mapped_pages(), 2);
    pool.free(buf).expect("free");
    assert_eq!(pool.address_space.mapped_pages(), 0);
}

#[test]
fn leading_guard_overrun_is_detected_at_free() {
    let frames = fresh_frames(16);
    let mut pool = pool_with_capacity(&frames, 8);
    let buf = pool.alloc(PAGE_SIZE).expect("alloc");
    // Smash one byte of the leading guard slot (which sits in
    // storage[0..PAGE_SIZE] because the allocation lands at slot 0
    // ⇒ guard at slot 0).
    let virt_off = usize::try_from(buf.virt().as_u64() - 0x1000_0000).expect("fits");
    let leading_guard_offset = virt_off - PAGE_SIZE;
    pool.poke_for_test(leading_guard_offset + 16, 0x00)
        .expect("poke");
    assert_eq!(pool.free(buf).err(), Some(DmaError::GuardViolation));
}

#[test]
fn trailing_guard_overrun_is_detected_at_free() {
    let frames = fresh_frames(16);
    let mut pool = pool_with_capacity(&frames, 8);
    let buf = pool.alloc(PAGE_SIZE).expect("alloc");
    let virt_off = usize::try_from(buf.virt().as_u64() - 0x1000_0000).expect("fits");
    let trailing_guard_offset = virt_off + buf.len();
    pool.poke_for_test(trailing_guard_offset + 32, 0x00)
        .expect("poke");
    assert_eq!(pool.free(buf).err(), Some(DmaError::GuardViolation));
}

#[test]
fn guard_violation_still_returns_frames() {
    // A buffer overrun is a bug, not a reason to leak physical
    // memory. After `free` reports the violation the frames must
    // nonetheless have made it back to the allocator.
    let frames = fresh_frames(16);
    let initial_free = frames.free_frames();
    let mut pool = pool_with_capacity(&frames, 8);
    let buf = pool.alloc(PAGE_SIZE).expect("alloc");
    let virt_off = usize::try_from(buf.virt().as_u64() - 0x1000_0000).expect("fits");
    pool.poke_for_test(virt_off + buf.len() + 1, 0x00)
        .expect("poke");
    assert!(matches!(pool.free(buf), Err(DmaError::GuardViolation)));
    assert_eq!(frames.free_frames(), initial_free);
}

#[test]
fn dma_buffer_is_not_empty() {
    let frames = fresh_frames(8);
    let mut pool = pool_with_capacity(&frames, 4);
    let buf = pool.alloc(PAGE_SIZE).expect("alloc");
    assert!(!buf.is_empty());
}

#[test]
fn display_messages_present() {
    extern crate std;
    use std::format;
    assert!(format!("{}", DmaError::ZeroSize).contains("zero"));
    assert!(format!("{}", DmaError::UnknownBuffer).contains("buffer"));
    assert!(format!("{}", DmaError::GuardViolation).contains("guard"));
    assert!(format!("{}", DmaError::InvalidPoolConfig).contains("config"));
    assert!(format!("{}", DmaError::SizeUnsupported).contains("max"));
    assert!(format!("{}", DmaError::Alloc(AllocError::OutOfMemory)).contains("alloc"));
}
