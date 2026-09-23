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

use alloc::vec::Vec;

use tairix_sync::{Once, SpinLock};

use crate::bootinfo::{BootMemoryMap, MemoryRegion, RegionKind};
use crate::dma::{DmaBlock, DmaCustody, DmaError};
use crate::error::AllocError;
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

/// What a [`RecordingCustody`] was handed.
pub(crate) struct CustodyRecord {
    /// Spaces bound and not yet unbound.
    pub(crate) bound: usize,
    /// Every `bind` accepted.
    pub(crate) binds: usize,
    /// Every block held, with the node and generation it came under.
    pub(crate) held: Vec<(u32, u64, DmaBlock)>,
}

/// A [`DmaCustody`] that records what it is given, optionally refusing every
/// binding.
pub(crate) struct RecordingCustody {
    refuse_bind: bool,
    record: SpinLock<CustodyRecord>,
}

impl RecordingCustody {
    pub(crate) const fn new(refuse_bind: bool) -> Self {
        Self {
            refuse_bind,
            record: SpinLock::new(CustodyRecord {
                bound: 0,
                binds: 0,
                held: Vec::new(),
            }),
        }
    }

    /// Run `f` over the record.
    pub(crate) fn with<R>(&self, f: impl FnOnce(&CustodyRecord) -> R) -> R {
        f(&self.record.lock())
    }
}

impl DmaCustody for RecordingCustody {
    fn bind(&self, _node: u32) -> Result<(), DmaError> {
        if self.refuse_bind {
            return Err(DmaError::Alloc(AllocError::OutOfMemory));
        }
        let mut record = self.record.lock();
        record.bound += 1;
        record.binds += 1;
        Ok(())
    }

    fn hold(&self, node: u32, generation: u64, block: DmaBlock) {
        self.record.lock().held.push((node, generation, block));
    }

    fn unbind(&self, _node: u32) {
        let mut record = self.record.lock();
        record.bound -= 1;
    }
}

/// A [`RecordingCustody`] in a cell of this expansion's own; `refusing`
/// builds one that refuses every binding.
macro_rules! custody {
    () => {{
        static CUSTODY: crate::test_fixture::RecordingCustody =
            crate::test_fixture::RecordingCustody::new(false);
        &CUSTODY
    }};
    (refusing) => {{
        static CUSTODY: crate::test_fixture::RecordingCustody =
            crate::test_fixture::RecordingCustody::new(true);
        &CUSTODY
    }};
}

pub(crate) use custody;
