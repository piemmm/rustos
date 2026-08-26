//! Single physical frames handed out as direct-mapped pages.
//!
//! The kernel heap's slab tier (`lib/kalloc`) wants one whole page per slab,
//! addressed by an ordinary pointer. That is exactly a frame plus the direct
//! physical map: no remap-window slot, no page-table work, and no
//! invalidation on either side — which is what lets a page-sized allocation
//! cost exactly one frame.
//!
//! # A page must be returnable before it is handed out
//!
//! A page comes back by *virtual* address only, so recovering its frame means
//! inverting the direct map ([`PhysMap::reverse`]). A map that cannot invert
//! would leak every page it ever supplied, so [`FramePages::alloc`] proves the
//! round trip before it hands one out and fails closed otherwise.

use core::ptr::NonNull;

use crate::frame::{Frame, FrameAllocator, PAGE_SIZE};
use crate::phys::PhysMap;

/// A supply of single frames, addressed through the kernel's direct physical
/// map.
///
/// Draws through the *kernel* commit path, so it may use the kernel reserve
/// and keeps making progress under user memory pressure — the same rule the
/// heap's region growth follows.
pub struct FramePages {
    frames: &'static FrameAllocator,
    phys: &'static (dyn PhysMap + Sync),
}

impl FramePages {
    /// Build a page supply over the kernel `frames` allocator, addressing
    /// frames through the direct map `phys`.
    #[must_use]
    pub fn new(frames: &'static FrameAllocator, phys: &'static (dyn PhysMap + Sync)) -> Self {
        Self { frames, phys }
    }

    /// Draw one page, or [`None`] when no frame is free or the direct map
    /// cannot address and re-invert it (fail closed, never a panic).
    ///
    /// The page's contents are whatever its last owner left; a caller that
    /// hands the bytes to another principal zeroes them itself, exactly as
    /// the frame allocator's other kernel-side consumers do.
    #[must_use]
    pub fn alloc(&self) -> Option<NonNull<u8>> {
        let frame = self.frames.alloc().ok()?;
        let phys = frame.start();
        let Some(page) = self.phys.translate(phys, PAGE_SIZE) else {
            // Outside the direct map: hand it back rather than fabricate a
            // pointer. The frame was just drawn, so the free cannot
            // legitimately fail and there is no recovery beyond declining.
            let _ = self.frames.free(frame);
            return None;
        };
        if self.phys.reverse(page.as_ptr() as usize) != Some(phys) {
            // A map that cannot invert its own translation would strand this
            // page on release, so refuse it now while the frame is still ours.
            let _ = self.frames.free(frame);
            return None;
        }
        Some(page)
    }

    /// Return a page previously drawn by [`Self::alloc`].
    ///
    /// A page this supply never issued names no frame it owns, so the
    /// allocator refuses it; there is no recovery beyond declining, so the
    /// result is dropped rather than raised.
    pub fn free(&self, page: NonNull<u8>) {
        let Some(phys) = self.phys.reverse(page.as_ptr() as usize) else {
            return;
        };
        let _ = self.frames.free(Frame::containing(phys));
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::bootinfo::{BootMemoryMap, MemoryRegion, RegionKind};
    use crate::frame::PhysAddr;
    use crate::phys::SimPhysMap;
    use alloc::boxed::Box;
    use alloc::vec::Vec;

    /// Physical base of the simulated RAM window. Non-zero so a stray
    /// `phys == 0` would show up as "outside the map".
    const RAM_BASE: u64 = 0x10_0000;
    const USABLE_PAGES: usize = 8;

    /// A direct map that translates but cannot invert — the default
    /// [`PhysMap::reverse`], which every non-linear map inherits.
    struct OneWayMap(SimPhysMap);

    impl PhysMap for OneWayMap {
        fn translate(&self, phys: PhysAddr, len: usize) -> Option<NonNull<u8>> {
            self.0.translate(phys, len)
        }

        fn clean_invalidate(&self, phys: PhysAddr, len: usize) {
            self.0.clean_invalidate(phys, len);
        }

        fn sync_instruction_cache(&self, phys: PhysAddr, len: usize) {
            self.0.sync_instruction_cache(phys, len);
        }
    }

    fn allocator(pages: usize) -> &'static FrameAllocator {
        let mut map = BootMemoryMap::new();
        map.push(MemoryRegion {
            start: PhysAddr::new(RAM_BASE),
            length: (pages * PAGE_SIZE) as u64,
            kind: RegionKind::Usable,
        });
        Box::leak(Box::new(FrameAllocator::new(&map).expect("allocator")))
    }

    fn supply(pages: usize) -> (&'static FramePages, &'static FrameAllocator) {
        let frames = allocator(pages);
        let sim: &'static SimPhysMap = Box::leak(Box::new(SimPhysMap::new(
            PhysAddr::new(RAM_BASE),
            pages * PAGE_SIZE,
        )));
        let supply: &'static FramePages = Box::leak(Box::new(FramePages::new(
            frames,
            sim as &'static (dyn PhysMap + Sync),
        )));
        (supply, frames)
    }

    #[test]
    fn a_page_costs_exactly_one_frame_and_comes_back() {
        let (supply, frames) = supply(USABLE_PAGES);
        let before = frames.free_frames();
        let page = supply.alloc().expect("a page");
        assert_eq!(
            page.as_ptr() as usize % PAGE_SIZE,
            0,
            "a page is granule-aligned"
        );
        assert_eq!(frames.free_frames(), before - 1, "exactly one frame drawn");

        // The whole page is writable through the direct map.
        // SAFETY: the supply owns the frame until it is returned, so nothing
        // else names these bytes.
        unsafe { core::ptr::write_bytes(page.as_ptr(), 0xA5, PAGE_SIZE) };

        supply.free(page);
        assert_eq!(frames.free_frames(), before, "the frame came back");
    }

    #[test]
    fn pages_are_disjoint_and_exhaustion_fails_closed() {
        let (supply, frames) = supply(USABLE_PAGES);
        let mut pages = Vec::new();
        while let Some(page) = supply.alloc() {
            pages.push(page.as_ptr() as usize);
        }
        assert!(!pages.is_empty(), "the pool serves at least one page");
        assert_eq!(frames.free_frames(), 0, "the pool is drained");

        let mut sorted = pages.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), pages.len(), "no frame backs two pages");

        // Exhausted: `None`, never a panic.
        assert!(supply.alloc().is_none());

        for page in pages {
            supply.free(NonNull::new(page as *mut u8).expect("non-null"));
        }
        assert!(frames.free_frames() > 0, "every page came back");
    }

    #[test]
    fn a_map_that_cannot_invert_is_refused_rather_than_leaked() {
        let frames = allocator(USABLE_PAGES);
        let one_way: &'static OneWayMap = Box::leak(Box::new(OneWayMap(SimPhysMap::new(
            PhysAddr::new(RAM_BASE),
            USABLE_PAGES * PAGE_SIZE,
        ))));
        let supply = FramePages::new(frames, one_way as &'static (dyn PhysMap + Sync));
        let before = frames.free_frames();
        assert!(
            supply.alloc().is_none(),
            "a page that could not be returned must not be handed out"
        );
        assert_eq!(
            frames.free_frames(),
            before,
            "the refused draw returned its frame"
        );
    }

    #[test]
    fn freeing_an_address_the_supply_never_issued_frees_nothing() {
        let (supply, frames) = supply(USABLE_PAGES);
        let page = supply.alloc().expect("a page");
        let before = frames.free_frames();
        // Well outside the direct map's window.
        supply.free(NonNull::new(PAGE_SIZE as *mut u8).expect("non-null"));
        assert_eq!(frames.free_frames(), before, "no frame was freed");
        supply.free(page);
    }
}
