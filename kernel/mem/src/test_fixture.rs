//! `'static` RAM backing for host tests, accountable to the UB oracle.
//!
//! [`FramePages`](crate::framepages::FramePages),
//! [`FrameTableSource`](crate::pagetables::FrameTableSource),
//! [`SlotWindow`](crate::kvslots::SlotWindow) and
//! [`LiveSpace`](crate::live::LiveSpace) all borrow `&'static` because
//! production holds those pieces in kernel globals, so a test cannot own them
//! on its stack. A leaked `Box` is indistinguishable from a real leak to the
//! interpreter, so each piece lives in a cell that stays reachable for the
//! whole run instead.

use tairix_sync::Once;

use crate::bootinfo::{BootMemoryMap, MemoryRegion, RegionKind};
use crate::frame::{FrameAllocator, PhysAddr, PAGE_SIZE};
use crate::phys::SimPhysMap;

/// A frame pool and the simulated direct map that addresses exactly its
/// RAM.
///
/// Reach one through [`frame_backing!`], which gives every test site a cell
/// of its own so no two concurrently-running tests share a pool.
pub(crate) struct FrameBacking {
    frames: Once<FrameAllocator>,
    sim: Once<SimPhysMap>,
}

impl FrameBacking {
    pub(crate) const fn new() -> Self {
        Self {
            frames: Once::new(),
            sim: Once::new(),
        }
    }

    /// `pages` frames of usable RAM based at `base`, addressed by a
    /// direct map over exactly those bytes.
    pub(crate) fn build(
        &'static self,
        base: u64,
        pages: usize,
    ) -> (&'static FrameAllocator, &'static SimPhysMap) {
        let mut map = BootMemoryMap::new();
        map.push(MemoryRegion {
            start: PhysAddr::new(base),
            length: (pages * PAGE_SIZE) as u64,
            kind: RegionKind::Usable,
        });
        let frames = self
            .frames
            .call_once_infallible(|| FrameAllocator::new(&map).expect("allocator over the window"))
            .expect("a fresh cell");
        let sim = self
            .sim
            .call_once_infallible(|| SimPhysMap::new(PhysAddr::new(base), pages * PAGE_SIZE))
            .expect("a fresh cell");
        (frames, sim)
    }
}

/// Build a [`FrameBacking`] in a cell of this expansion's own.
macro_rules! frame_backing {
    ($base:expr, $pages:expr) => {{
        static CELL: crate::test_fixture::FrameBacking = crate::test_fixture::FrameBacking::new();
        CELL.build($base, $pages)
    }};
}

pub(crate) use frame_backing;
