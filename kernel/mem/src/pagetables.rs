//! Allocator-backed page-table frame source (`plans/WIRING.md`
//! Stage W5b-3).
//!
//! A port's `AddressSpace` (`kernel/arch/<target>`) draws its root table
//! and every intermediate table through the Arch HAL
//! [`PageTableFrames`] seam. The boot/bootstrap implementation is the
//! static `PageTablePool` each port ships; this module is the
//! *production* implementation, backing those tables with the kernel's
//! physical [`FrameAllocator`] so a per-process address space's page
//! tables live in ordinary reclaimable RAM rather than a fixed-size
//! `.bss` pool.
//!
//! §17.4 forbids `kernel/arch/*` from naming `kernel/mem`, so the port
//! cannot reach the allocator directly; it names only the HAL trait.
//! `kernel/mem` *is* allowed to depend on `kernel/arch/api`, so the
//! adapter lives here and is handed to a port as a `&'static dyn
//! PageTableFrames` at the single `kernel/core` wiring point — the same
//! shape the scheduler and arch backends are selected with.
//!
//! # Physical ↔ virtual
//!
//! The allocator hands out a [`Frame`](crate::frame::Frame) by
//! *physical* address; a port
//! needs a CPU-dereferenceable view of that frame's 512 entries to build
//! a table. That translation is exactly the kernel's direct physical map
//! ([`crate::phys::PhysMap`]) the DMA and MMIO layers already use, so the
//! adapter routes through it rather than re-deriving a pointer
//! (`AGENTS.md` §2.2). A frame whose physical address is outside the
//! direct map is returned to the allocator and the request fails closed
//! (`AGENTS.md` §2.9), never synthesising a pointer of its own.

use rustos_arch_api::frames::{PageTableFrames, TableFrame, PAGE_TABLE_ENTRIES};

use crate::frame::{FrameAllocator, PhysAddr, PAGE_SIZE};
use crate::phys::PhysMap;

/// A [`PageTableFrames`] source backed by the kernel [`FrameAllocator`].
///
/// Each [`PageTableFrames::alloc_table`] draws one physical frame from
/// the allocator, maps it through the direct [`PhysMap`], zeroes it, and
/// hands the port both the physical address (for the parent PTE / root
/// register) and a `'static` mutable view of its entries.
///
/// Both references are `'static`: in production the [`FrameAllocator`]
/// and the direct map are kernel globals that live for the lifetime of
/// the image, so the frame's direct-map view is permanently valid. The
/// source is therefore stored behind a `&'static dyn PageTableFrames` by
/// the port, exactly like the static pool it replaces.
pub struct FrameTableSource {
    frames: &'static FrameAllocator,
    phys: &'static (dyn PhysMap + Sync),
}

impl FrameTableSource {
    /// Build a frame source over the kernel `frames` allocator, mapping
    /// freshly-allocated frames to CPU pointers through the direct map
    /// `phys`.
    ///
    /// `phys` is `Sync` because in production the one source is shared,
    /// immutably, by every CPU's spawn path (it lives behind a `'static`
    /// shared handle), so the kernel can cache a single `FrameTableSource`
    /// in a `static` (`AGENTS.md` §2.1). The kernel direct map
    /// ([`DirectPhysMap`](crate::DirectPhysMap)) is `Copy` plain data, so it
    /// satisfies the bound.
    #[must_use]
    pub fn new(frames: &'static FrameAllocator, phys: &'static (dyn PhysMap + Sync)) -> Self {
        Self { frames, phys }
    }
}

impl PageTableFrames for FrameTableSource {
    fn alloc_table(&self) -> Option<TableFrame> {
        // Deterministic OOM: a full allocator returns `None`, never a
        // panic (`AGENTS.md` §4).
        let frame = self.frames.alloc().ok()?;
        let phys = frame.start().as_u64();

        let Some(ptr) = self.phys.translate(PhysAddr::new(phys), PAGE_SIZE) else {
            // The frame is outside the direct map: hand it back and fail
            // closed rather than fabricating a pointer (`AGENTS.md` §2.9).
            // A best-effort free is correct here — the frame was just
            // allocated, so the matching free cannot legitimately fail.
            let _ = self.frames.free(frame);
            return None;
        };

        let raw = ptr.as_ptr();
        // A page-aligned physical address maps to a page-aligned (hence
        // `u64`-aligned) direct-map pointer; a misaligned one is a broken
        // `PhysMap` and must never be reinterpreted as a table.
        debug_assert_eq!(
            raw.align_offset(core::mem::align_of::<[u64; PAGE_TABLE_ENTRIES]>()),
            0,
            "direct-map pointer for a page-aligned frame must be table-aligned"
        );
        // SAFETY: `frame` was just handed out by the allocator, so no
        // other live reference names it; `translate` proved the whole
        // 4 KiB frame lies in the direct map, and the page-aligned
        // physical address maps to a pointer aligned for `[u64; 512]`
        // (asserted above). We mint a single `&'static mut` to it (the
        // direct map is permanent in production) and immediately zero it
        // so the port receives a clean table (the allocator does not
        // guarantee zeroed frames). The `cast_ptr_alignment` lint flags
        // the `u8`→`[u64; 512]` widening, which the page alignment makes
        // sound.
        #[allow(clippy::cast_ptr_alignment)]
        let entries: &'static mut [u64; PAGE_TABLE_ENTRIES] =
            unsafe { &mut *raw.cast::<[u64; PAGE_TABLE_ENTRIES]>() };
        entries.fill(0);

        Some(TableFrame { phys, entries })
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::bootinfo::{BootMemoryMap, MemoryRegion, RegionKind};
    use crate::frame::FrameCount;
    use crate::phys::SimPhysMap;
    use alloc::boxed::Box;
    use rustos_arch_api::frames::conformance;

    /// Physical base of the simulated RAM window. Non-zero so a stray
    /// `phys == 0` would be caught as "outside the map".
    const RAM_BASE: u64 = 0x10_0000;
    const USABLE_PAGES: usize = 8;

    /// Leak a frame allocator + a direct map over `pages` of simulated
    /// RAM based at [`RAM_BASE`], returning both as `'static`. Leaking is
    /// the host stand-in for the kernel globals these are in production.
    fn fresh_source() -> (&'static FrameTableSource, &'static FrameAllocator) {
        let mut map = BootMemoryMap::new();
        map.push(MemoryRegion {
            start: PhysAddr::new(RAM_BASE),
            length: (USABLE_PAGES * PAGE_SIZE) as u64,
            kind: RegionKind::Usable,
        });
        let frames: &'static FrameAllocator =
            Box::leak(Box::new(FrameAllocator::new(&map).expect("allocator")));
        let sim: &'static SimPhysMap = Box::leak(Box::new(SimPhysMap::new(
            PhysAddr::new(RAM_BASE),
            USABLE_PAGES * PAGE_SIZE,
        )));
        let source: &'static FrameTableSource = Box::leak(Box::new(FrameTableSource::new(
            frames,
            sim as &'static (dyn PhysMap + Sync),
        )));
        (source, frames)
    }

    /// The production source is shared, immutably, by every CPU's spawn
    /// path, so it must be `Sync` to live behind a `'static` cache
    /// (`AGENTS.md` §2.1 / §24.1). Asserting it here keeps the `phys: &dyn
    /// PhysMap + Sync` bound from silently regressing.
    #[test]
    fn frame_table_source_is_sync() {
        fn assert_sync<T: Sync>() {}
        assert_sync::<FrameTableSource>();
    }

    #[test]
    fn alloc_table_draws_a_zeroed_frame_from_the_allocator() {
        let (source, frames) = fresh_source();
        let before: FrameCount = frames.free_frames();

        let table = source.alloc_table().expect("a frame");
        assert_eq!(table.phys & 0xFFF, 0, "frame is page-aligned");
        assert!(
            table.phys >= RAM_BASE,
            "frame comes from the allocator's RAM window"
        );
        assert!(table.entries.iter().all(|&e| e == 0), "frame is zeroed");
        assert_eq!(
            frames.free_frames(),
            before - 1,
            "exactly one frame left the allocator"
        );
    }

    #[test]
    fn passes_frames_conformance_over_the_allocator() {
        let (source, _frames) = fresh_source();
        // The allocator can hand out every usable page before failing
        // closed, so the conformance capacity is the usable-page count.
        conformance::run_all(source, USABLE_PAGES);
    }

    #[test]
    fn distinct_tables_do_not_alias() {
        let (source, _frames) = fresh_source();
        let a = source.alloc_table().expect("first");
        let a_phys = a.phys;
        a.entries[0] = 0xA5A5_A5A5;
        let b = source.alloc_table().expect("second");
        assert_ne!(a_phys, b.phys, "frames are physically distinct");
        assert_eq!(b.entries[0], 0, "the second frame is independent");
    }

    #[test]
    fn exhausted_allocator_fails_closed() {
        let (source, _frames) = fresh_source();
        for _ in 0..USABLE_PAGES {
            assert!(source.alloc_table().is_some());
        }
        assert!(
            source.alloc_table().is_none(),
            "an exhausted allocator yields None, never a panic"
        );
    }
}
