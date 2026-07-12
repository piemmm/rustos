//! Unit tests for the per-process-heap DMA allocator.
//!
//! Every invariant called out in the module-level documentation has a
//! matching test below. The tests run entirely on the host:
//! [`HostPageTable`] stands in for an architecture page-table type, a
//! freshly-constructed [`FrameAllocator`] over a small synthetic
//! memory map supplies the backing frames, and a [`SimPhysMap`] stands
//! in for physical RAM so the bytes a test writes "as the device"
//! alias the bytes the pool hands the driver.

use super::*;
use crate::bootinfo::{BootMemoryMap, MemoryRegion, RegionKind};
use crate::frame::{FrameAllocator, PAGE_SIZE};
use crate::phys::SimPhysMap;
use crate::vmm::{AddressSpace, HostPageTable, VirtAddr};
use core::cell::Cell;

/// Physical base of the usable RAM region in the synthetic map. Frame
/// 16 leaves the low frames free for hypothetical reserved regions,
/// mirroring `slab.rs`' test style.
const RAM_BASE: u64 = PAGE_SIZE as u64 * 16;

/// Build a synthetic memory map of `usable_pages` pages of usable RAM
/// starting at [`RAM_BASE`].
fn small_map(usable_pages: usize) -> BootMemoryMap {
    let mut m = BootMemoryMap::new();
    m.push(MemoryRegion {
        kind: RegionKind::Usable,
        start: PhysAddr::new(RAM_BASE),
        length: (PAGE_SIZE * usable_pages) as u64,
    });
    m
}

/// Fresh frame allocator that the test's `DmaPool` will borrow from.
fn fresh_frames(usable_pages: usize) -> FrameAllocator {
    FrameAllocator::new(&small_map(usable_pages)).expect("frame allocator")
}

/// Simulated physical RAM covering the usable region the frame
/// allocator hands out, so `phys.translate` resolves every frame.
fn fresh_sim(usable_pages: usize) -> SimPhysMap {
    SimPhysMap::new(PhysAddr::new(RAM_BASE), usable_pages * PAGE_SIZE)
}

/// Construct a pool with `capacity_pages` virtual slots.
fn pool_with_capacity<'a>(
    frames: &'a FrameAllocator,
    sim: &'a SimPhysMap,
    capacity_pages: usize,
) -> DmaPool<'a, HostPageTable> {
    DmaPool::new(
        AddressSpace::new(HostPageTable::new()),
        VirtAddr::new(0x1000_0000),
        capacity_pages,
        frames,
        sim,
    )
    .expect("pool constructs")
}

struct RecordingPhysMap<'a> {
    inner: &'a SimPhysMap,
    calls: Cell<usize>,
    last_phys: Cell<u64>,
    last_len: Cell<usize>,
}

impl<'a> RecordingPhysMap<'a> {
    fn new(inner: &'a SimPhysMap) -> Self {
        Self {
            inner,
            calls: Cell::new(0),
            last_phys: Cell::new(0),
            last_len: Cell::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.get()
    }

    fn last_phys(&self) -> u64 {
        self.last_phys.get()
    }

    fn last_len(&self) -> usize {
        self.last_len.get()
    }
}

impl PhysMap for RecordingPhysMap<'_> {
    fn translate(&self, phys: PhysAddr, len: usize) -> Option<core::ptr::NonNull<u8>> {
        self.inner.translate(phys, len)
    }

    fn clean_invalidate(&self, phys: PhysAddr, len: usize) {
        self.calls.set(self.calls.get() + 1);
        self.last_phys.set(phys.as_u64());
        self.last_len.set(len);
    }
}

#[test]
fn new_rejects_zero_capacity() {
    let frames = fresh_frames(4);
    let sim = fresh_sim(4);
    let err = DmaPool::new(
        AddressSpace::new(HostPageTable::new()),
        VirtAddr::new(0x1000_0000),
        0,
        &frames,
        &sim,
    );
    assert_eq!(err.err(), Some(DmaError::InvalidPoolConfig));
}

#[test]
fn new_rejects_misaligned_base() {
    let frames = fresh_frames(4);
    let sim = fresh_sim(4);
    let err = DmaPool::new(
        AddressSpace::new(HostPageTable::new()),
        VirtAddr::new(0x1000_0001),
        4,
        &frames,
        &sim,
    );
    assert_eq!(err.err(), Some(DmaError::InvalidPoolConfig));
}

#[test]
fn alloc_zero_rejected() {
    let frames = fresh_frames(16);
    let sim = fresh_sim(16);
    let mut pool = pool_with_capacity(&frames, &sim, 8);
    assert_eq!(pool.alloc(0).err(), Some(DmaError::ZeroSize));
}

#[test]
fn alloc_returns_page_aligned_virt_and_phys() {
    let frames = fresh_frames(16);
    let sim = fresh_sim(16);
    let mut pool = pool_with_capacity(&frames, &sim, 8);
    let buf = pool.alloc(1).expect("alloc one byte");
    assert!(buf.virt().is_page_aligned());
    assert_eq!(buf.phys().as_u64() % (PAGE_SIZE as u64), 0);
    // One-byte request rounds up to one page.
    assert_eq!(buf.len(), PAGE_SIZE);
}

#[test]
fn alloc_rounds_up_to_next_power_of_two_pages() {
    let frames = fresh_frames(64);
    let sim = fresh_sim(64);
    let mut pool = pool_with_capacity(&frames, &sim, 32);
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
    let sim = fresh_sim(16);
    let mut pool = pool_with_capacity(&frames, &sim, 32);
    // (1 << MAX_ORDER) + 1 pages forces order = MAX_ORDER + 1.
    let too_big = (1usize << (MAX_ORDER + 1)) * PAGE_SIZE;
    assert_eq!(pool.alloc(too_big).err(), Some(DmaError::SizeUnsupported));
}

#[test]
fn alloc_capacity_exhausted_returns_oom() {
    // 4 slots ⇒ at most one 1-page allocation (1 + 1 + 1 guards = 3,
    // leaving 1 slack), a second one would need 3 more slots: OOM.
    let frames = fresh_frames(8);
    let sim = fresh_sim(8);
    let mut pool = pool_with_capacity(&frames, &sim, 4);
    let _first = pool.alloc(PAGE_SIZE).expect("first allocation succeeds");
    assert_eq!(
        pool.alloc(PAGE_SIZE).err(),
        Some(DmaError::Alloc(AllocError::OutOfMemory))
    );
}

#[test]
fn free_unknown_buffer_rejected() {
    let frames = fresh_frames(8);
    let sim = fresh_sim(8);
    let mut pool = pool_with_capacity(&frames, &sim, 4);
    let bogus = DmaBuffer {
        virt: VirtAddr::new(0xDEAD_0000),
        phys: PhysAddr::new(RAM_BASE),
        len: PAGE_SIZE,
    };
    assert_eq!(pool.free(bogus).err(), Some(DmaError::UnknownBuffer));
}

#[test]
fn double_free_rejected() {
    let frames = fresh_frames(8);
    let sim = fresh_sim(8);
    let mut pool = pool_with_capacity(&frames, &sim, 4);
    let buf = pool.alloc(PAGE_SIZE).expect("alloc");
    pool.free(buf).expect("first free");
    assert_eq!(pool.free(buf).err(), Some(DmaError::UnknownBuffer));
}

#[test]
fn alloc_returns_zero_initialised_bytes() {
    let frames = fresh_frames(16);
    let sim = fresh_sim(16);
    let mut pool = pool_with_capacity(&frames, &sim, 8);
    let buf = pool.alloc(PAGE_SIZE).expect("alloc");
    assert!(pool.bytes(buf).unwrap().iter().all(|&b| b == 0));
}

#[test]
fn cpu_view_aliases_device_physical_frame() {
    // The load-bearing hardware-realism invariant: the bytes the CPU
    // reads through the pool are the very frame the device is told to
    // DMA into (`buf.phys()`). Writing "as the device" through the
    // direct map must be observable through `bytes`, and vice versa.
    let frames = fresh_frames(16);
    let sim = fresh_sim(16);
    let mut pool = pool_with_capacity(&frames, &sim, 8);
    let buf = pool.alloc(PAGE_SIZE).expect("alloc");

    // Device → CPU: write through the same physical address the
    // descriptor would carry, observe it through `bytes`.
    let dev = sim.translate(buf.phys(), PAGE_SIZE).expect("device view");
    // SAFETY: `dev` names this buffer's frame in the simulator; the
    // pool holds the single live record, so the write aliases nothing.
    unsafe {
        dev.as_ptr().write(0x5A);
        dev.as_ptr().add(PAGE_SIZE - 1).write(0xC3);
    }
    assert_eq!(pool.bytes(buf).unwrap()[0], 0x5A);
    assert_eq!(pool.bytes(buf).unwrap()[PAGE_SIZE - 1], 0xC3);

    // CPU → device: write through the pool, observe at the physical
    // address.
    pool.bytes_mut(buf).unwrap()[1] = 0x99;
    // SAFETY: as above.
    assert_eq!(unsafe { dev.as_ptr().add(1).read() }, 0x99);

    pool.free(buf).expect("free");
}

#[test]
fn reuse_after_free_sees_zeroed_buffer() {
    // The core security invariant of:
    // "Zero-on-free for any allocation that ever held credentials".
    // After freeing a buffer that held a sentinel, the next allocation
    // that lands on the same region must observe zeros, not the sentinel.
    let frames = fresh_frames(16);
    let sim = fresh_sim(16);
    let mut pool = pool_with_capacity(&frames, &sim, 8);
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
fn free_zeroes_the_physical_frame() {
    // Zero-on-free must clear the *device-visible* frame, not a
    // disconnected copy. After `free` the simulated frame reads back
    // as zero even though we observe it through the device view.
    let frames = fresh_frames(16);
    let sim = fresh_sim(16);
    let mut pool = pool_with_capacity(&frames, &sim, 8);
    let buf = pool.alloc(PAGE_SIZE).expect("alloc");
    let phys = buf.phys();
    for b in pool.bytes_mut(buf).unwrap().iter_mut() {
        *b = 0xA5;
    }
    pool.free(buf).expect("free");
    let dev = sim.translate(phys, PAGE_SIZE).expect("device view");
    // SAFETY: the simulator outlives this body; the frame is no longer
    // handed to any live allocation.
    let view = unsafe { core::slice::from_raw_parts(dev.as_ptr(), PAGE_SIZE) };
    assert!(view.iter().all(|&b| b == 0), "free must zero the frame");
}

#[test]
fn alloc_cleans_direct_map_alias_after_zeroing() {
    let frames = fresh_frames(16);
    let sim = fresh_sim(16);
    let rec = RecordingPhysMap::new(&sim);
    let mut pool = DmaPool::new(
        AddressSpace::new(HostPageTable::new()),
        VirtAddr::new(0x1000_0000),
        8,
        &frames,
        &rec,
    )
    .expect("pool constructs");
    let buf = pool.alloc(PAGE_SIZE).expect("alloc");
    assert_eq!(rec.calls(), 1);
    assert_eq!(rec.last_phys(), buf.phys().as_u64());
    assert_eq!(rec.last_len(), PAGE_SIZE);
}

#[test]
fn free_cleans_direct_map_alias_after_zeroing() {
    let frames = fresh_frames(16);
    let sim = fresh_sim(16);
    let rec = RecordingPhysMap::new(&sim);
    let mut pool = DmaPool::new(
        AddressSpace::new(HostPageTable::new()),
        VirtAddr::new(0x1000_0000),
        8,
        &frames,
        &rec,
    )
    .expect("pool constructs");
    let buf = pool.alloc(PAGE_SIZE).expect("alloc");
    rec.calls.set(0);
    pool.free(buf).expect("free");
    assert_eq!(rec.calls(), 1);
    assert_eq!(rec.last_phys(), buf.phys().as_u64());
    assert_eq!(rec.last_len(), PAGE_SIZE);
}

#[test]
fn allocations_have_distinct_phys_addresses() {
    let frames = fresh_frames(16);
    let sim = fresh_sim(16);
    let mut pool = pool_with_capacity(&frames, &sim, 16);
    let a = pool.alloc(PAGE_SIZE).expect("a");
    let b = pool.alloc(PAGE_SIZE).expect("b");
    assert_ne!(a.phys(), b.phys());
    assert_ne!(a.virt(), b.virt());
}

#[test]
fn frees_return_frames_to_the_allocator() {
    let frames = fresh_frames(16);
    let sim = fresh_sim(16);
    let initial_free = frames.free_frames();
    let mut pool = pool_with_capacity(&frames, &sim, 16);
    let buf = pool.alloc(4 * PAGE_SIZE).expect("alloc 4 pages");
    assert!(frames.free_frames() < initial_free);
    pool.free(buf).expect("free");
    assert_eq!(frames.free_frames(), initial_free);
}

#[test]
fn address_space_records_one_mapping_per_data_page() {
    let frames = fresh_frames(16);
    let sim = fresh_sim(16);
    let mut pool = pool_with_capacity(&frames, &sim, 16);
    let buf = pool.alloc(2 * PAGE_SIZE).expect("alloc 2 pages");
    assert_eq!(pool.address_space.mapped_pages(), 2);
    pool.free(buf).expect("free");
    assert_eq!(pool.address_space.mapped_pages(), 0);
}

#[test]
fn guard_slots_are_left_unmapped() {
    // The guard-page mechanism: the leading and
    // trailing guard slots bracketing the data are never mapped in the
    // address space, so the MMU faults on a register-block over-run.
    let frames = fresh_frames(16);
    let sim = fresh_sim(16);
    let mut pool = pool_with_capacity(&frames, &sim, 8);
    let buf = pool.alloc(PAGE_SIZE).expect("alloc");
    // The data page lands at slot 1 (slot 0 is the leading guard); the
    // trailing guard is slot 2. Neither guard page is mapped.
    let data_virt = buf.virt().as_u64();
    let leading_guard = VirtAddr::new(data_virt - PAGE_SIZE as u64);
    let trailing_guard = VirtAddr::new(data_virt + buf.len() as u64);
    assert!(pool
        .address_space
        .translate(Page::from_addr(leading_guard).unwrap())
        .is_none());
    assert!(pool
        .address_space
        .translate(Page::from_addr(trailing_guard).unwrap())
        .is_none());
    // The data page itself *is* mapped.
    assert!(pool
        .address_space
        .translate(Page::from_addr(buf.virt()).unwrap())
        .is_some());
    pool.free(buf).expect("free");
}

#[test]
fn slot_base_points_at_live_data_bytes() {
    // `slot_base` hands out a raw `NonNull<u8>` to a buffer's data
    // frames so a user-space-driver host can construct an owned
    // `DmaSlab` without re-borrowing the pool. The pointer must
    // round-trip through `bytes_mut`'s slice view.
    let frames = fresh_frames(8);
    let sim = fresh_sim(8);
    let mut pool = pool_with_capacity(&frames, &sim, 4);
    let buf = pool.alloc(PAGE_SIZE).expect("alloc");
    let ptr = pool.slot_base(&buf).expect("slot_base");
    // Write through the slice view; observe through the raw ptr.
    let slice = pool.bytes_mut(buf).expect("bytes_mut");
    slice[0] = 0xAB;
    slice[PAGE_SIZE - 1] = 0xCD;
    // SAFETY: the slot bitmap proves no other reference covers
    // `[ptr, ptr + PAGE_SIZE)`; the slice borrow above has been
    // released for this read.
    let view = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), PAGE_SIZE) };
    assert_eq!(view[0], 0xAB);
    assert_eq!(view[PAGE_SIZE - 1], 0xCD);
    pool.free(buf).expect("free");
}

#[test]
fn slot_base_rejects_unknown_buffer() {
    // After `free`, the descriptor is no longer live; `slot_base`
    // must refuse to lend a pointer to its (now-recycled) slots.
    let frames = fresh_frames(8);
    let sim = fresh_sim(8);
    let mut pool = pool_with_capacity(&frames, &sim, 4);
    let buf = pool.alloc(PAGE_SIZE).expect("alloc");
    pool.free(buf).expect("free");
    assert_eq!(pool.slot_base(&buf).err(), Some(DmaError::UnknownBuffer));
}

#[test]
fn free_at_releases_by_virtual_base_and_fails_closed_on_unknown_va() {
    // `free_at` is the syscall-side release: it keys on the CPU virtual base
    // alone (the driver holds no `DmaBuffer` across the trap). It must reclaim
    // exactly the carve at that base and fail closed on any other address.
    let frames = fresh_frames(16);
    let sim = fresh_sim(16);
    let initial_free = frames.free_frames();
    let mut pool = pool_with_capacity(&frames, &sim, 16);
    let buf = pool.alloc(2 * PAGE_SIZE).expect("alloc");
    let virt = buf.virt();
    assert!(frames.free_frames() < initial_free);
    // An address that is not the base of a live carve fails closed without
    // releasing anything (covers a forged, stale, or mid-buffer free).
    assert_eq!(
        pool.window.free_at(
            &mut pool.address_space,
            pool.frames,
            pool.phys,
            VirtAddr::new(virt.as_u64() + PAGE_SIZE as u64),
        ),
        Err(DmaError::UnknownBuffer)
    );
    assert_eq!(pool.live(), 1, "a bad free released nothing");
    // The matching base reclaims the carve.
    pool.window
        .free_at(&mut pool.address_space, pool.frames, pool.phys, virt)
        .expect("free by base");
    assert_eq!(pool.live(), 0);
    assert_eq!(frames.free_frames(), initial_free, "frames fully returned");
    // A second free of the same base is now unknown (no double-free).
    assert_eq!(
        pool.window
            .free_at(&mut pool.address_space, pool.frames, pool.phys, virt),
        Err(DmaError::UnknownBuffer)
    );
}

#[test]
fn allocate_all_dma_then_free_it_all_reclaims_fully_every_round() {
    // The device-buffer analogue of the kalloc reclamation property: claim
    // every carve the window can serve, release them all, and find the pool
    // empty and the frame allocator exactly as full as before — round after
    // round, with no leak or drift. A driver that runs for years issuing many
    // transfers depends on exactly this.
    let frames = fresh_frames(64);
    let sim = fresh_sim(64);
    let initial_free = frames.free_frames();
    // A window large enough that the frame allocator (not the virtual window)
    // is the binding limit on some rounds, exercising both exhaustion paths.
    let mut pool = pool_with_capacity(&frames, &sim, 64);

    let mut first_round: Option<usize> = None;
    for round in 0..6 {
        assert_eq!(pool.live(), 0, "round {round} starts empty");
        assert_eq!(
            frames.free_frames(),
            initial_free,
            "round {round} starts with every frame free"
        );
        // Claim single-page carves until either the frame allocator or the
        // virtual window is exhausted (a `Result` error, never a panic).
        let mut live = alloc::vec::Vec::new();
        while let Ok(buf) = pool.alloc(PAGE_SIZE) {
            live.push(buf);
        }
        let count = live.len();
        assert!(count > 0, "the pool must serve at least one carve");
        match first_round {
            None => first_round = Some(count),
            Some(expected) => assert_eq!(
                count, expected,
                "round {round} served {count} carves, expected {expected} — \
                 capacity must not drift"
            ),
        }
        // Release every carve by its virtual base, exactly as `dma_free` does.
        for buf in live.drain(..) {
            pool.window
                .free_at(&mut pool.address_space, pool.frames, pool.phys, buf.virt())
                .expect("free by base");
        }
        assert_eq!(pool.live(), 0, "round {round} reclaimed every carve");
        assert_eq!(
            frames.free_frames(),
            initial_free,
            "round {round} returned every frame to the allocator"
        );
        assert_eq!(
            pool.address_space.mapped_pages(),
            0,
            "round {round} left no data page mapped"
        );
    }
}

#[test]
fn a_full_span_window_serves_a_multi_device_enclosure_lazily() {
    // The Pi 4 defect: the per-task DMA window was a fixed 256-page
    // ceiling, and a 13-device USB enclosure — one ~68 KiB ring/buffer
    // region per attached device, each rounded to a 32-page buddy block
    // plus two guard slots — exhausted it mid-walk, so the whole port was
    // skipped. The window now spans its full reserved gigabyte with
    // lazily grown slot bookkeeping: every region allocates, and the
    // bookkeeping paid tracks the peak slots actually used, never the
    // span.
    const REGION_BYTES: usize = 17 * PAGE_SIZE; // rounds to a 32-page block
    const REGIONS: usize = 13;
    const SLOTS_PER_REGION: usize = 32 + 2;
    // The scenario genuinely exceeds the former fixed 256-slot ceiling.
    const _: () = assert!(REGIONS * SLOTS_PER_REGION > 256);
    let frames = fresh_frames(2048);
    let sim = fresh_sim(2048);
    let span_pages = (1usize << 30) / PAGE_SIZE;
    let mut pool = pool_with_capacity(&frames, &sim, span_pages);
    let mut bufs = alloc::vec::Vec::new();
    for _ in 0..REGIONS {
        bufs.push(pool.alloc(REGION_BYTES).expect("a device region allocates"));
    }
    assert!(
        pool.window.slot_used.len() <= REGIONS * SLOTS_PER_REGION,
        "bookkeeping covers only the slots actually reached, not the span"
    );
    for buf in bufs {
        pool.free(buf).expect("a device region frees");
    }
}

#[test]
fn dma_buffer_is_not_empty() {
    let frames = fresh_frames(8);
    let sim = fresh_sim(8);
    let mut pool = pool_with_capacity(&frames, &sim, 4);
    let buf = pool.alloc(PAGE_SIZE).expect("alloc");
    assert!(!buf.is_empty());
}

#[test]
fn display_messages_present() {
    extern crate std;
    use std::format;
    assert!(format!("{}", DmaError::ZeroSize).contains("zero"));
    assert!(format!("{}", DmaError::UnknownBuffer).contains("buffer"));
    assert!(format!("{}", DmaError::DirectMap).contains("direct"));
    assert!(format!("{}", DmaError::InvalidPoolConfig).contains("config"));
    assert!(format!("{}", DmaError::SizeUnsupported).contains("max"));
    assert!(format!("{}", DmaError::Alloc(AllocError::OutOfMemory)).contains("alloc"));
}
