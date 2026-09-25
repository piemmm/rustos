//! The shared recording [`LiveUserSpace`] double, for the host tests that
//! route through a live space without a real page table.

extern crate std;
use std::vec::Vec;

use tairix_kernel_mem::{
    AddressSpace, DmaCustodian, DmaMapping, FrozenAddressSpace, HostPageTable, LiveSpaceError,
    LiveUserSpace, SharedMemory,
};

/// A recording [`LiveUserSpace`] double: it logs each call and returns a
/// configurable result, so the producer's routing + error fold are
/// exercised without a real page table (the real [`LiveUserSpace`] is
/// covered in `kernel/mem`). `&mut self` methods mean plain fields
/// suffice — no interior mutability — so it stays `Send`.
#[derive(Default)]
pub(crate) struct FakeLive {
    pub(crate) anon_maps: Vec<(u64, u64)>,
    pub(crate) anon_placed: Vec<u64>,
    pub(crate) anon_reserves: Vec<u64>,
    pub(crate) anon_commits: Vec<u64>,
    pub(crate) anon_reserves_at: Vec<(u64, u64)>,
    pub(crate) anon_unmaps: Vec<(u64, u64)>,
    pub(crate) device_maps: Vec<(u64, usize)>,
    pub(crate) writeback_framebuffer_maps: Vec<(u64, usize)>,
    pub(crate) framebuffer_maps: Vec<(u64, usize)>,
    pub(crate) dma_allocs: Vec<(usize, u64, u32, u64)>,
    pub(crate) dma_frees: Vec<u64>,
    pub(crate) file_reserves: Vec<u64>,
    pub(crate) file_page_maps: Vec<(u64, usize)>,
    pub(crate) file_releases: Vec<(u64, u64)>,
    pub(crate) next: Option<LiveSpaceError>,
}

/// The physical base a DMA carve reports back from the fake, so the
/// producer test can assert the device address flows through unchanged.
pub(crate) const DMA_PHYS: u64 = 0x4001_0000;

/// The base a placed (non-`FIXED`) map reports back from the fake, so the
/// producer test can assert the returned value flows through unchanged.
pub(crate) const PLACED_BASE: u64 = 0xC000_0000;

/// The base a file-region reservation reports back from the fake, so the
/// producer test can assert the returned value flows through unchanged.
pub(crate) const FILE_BASE: u64 = 0xF000_0000;

/// The resident-page count a file-region release reports back from the
/// fake.
pub(crate) const FILE_RESIDENT: u64 = 3;

impl LiveUserSpace for FakeLive {
    fn map_anonymous(&mut self, base_va: u64, page_count: u64) -> Result<u64, LiveSpaceError> {
        self.anon_maps.push((base_va, page_count));
        match self.next.take() {
            Some(err) => Err(err),
            None => Ok(base_va),
        }
    }

    fn map_anonymous_placed(&mut self, page_count: u64) -> Result<u64, LiveSpaceError> {
        self.anon_placed.push(page_count);
        match self.next.take() {
            Some(err) => Err(err),
            None => Ok(PLACED_BASE),
        }
    }

    fn reserve_anonymous(&mut self, page_count: u64) -> Result<u64, LiveSpaceError> {
        self.anon_reserves.push(page_count);
        match self.next.take() {
            Some(err) => Err(err),
            None => Ok(PLACED_BASE),
        }
    }

    fn reserve_anonymous_growable(&mut self, page_count: u64) -> Result<u64, LiveSpaceError> {
        self.anon_reserves.push(page_count);
        match self.next.take() {
            Some(err) => Err(err),
            None => Ok(PLACED_BASE),
        }
    }

    fn commit_anonymous(&mut self, page_count: u64) -> Result<(), LiveSpaceError> {
        self.anon_commits.push(page_count);
        match self.next.take() {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    fn reserve_anonymous_at(
        &mut self,
        base_va: u64,
        page_count: u64,
    ) -> Result<u64, LiveSpaceError> {
        self.anon_reserves_at.push((base_va, page_count));
        match self.next.take() {
            Some(err) => Err(err),
            None => Ok(base_va),
        }
    }

    fn unmap_anonymous(&mut self, base_va: u64, page_count: u64) -> Result<(), LiveSpaceError> {
        self.anon_unmaps.push((base_va, page_count));
        match self.next.take() {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    fn reserve_file_region(&mut self, page_count: u64) -> Result<u64, LiveSpaceError> {
        self.file_reserves.push(page_count);
        match self.next.take() {
            Some(err) => Err(err),
            None => Ok(FILE_BASE),
        }
    }

    fn map_file_page_at(&mut self, va: u64, contents: &[u8]) -> Result<(), LiveSpaceError> {
        self.file_page_maps.push((va, contents.len()));
        match self.next.take() {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    fn release_file_region(
        &mut self,
        base_va: u64,
        page_count: u64,
    ) -> Result<u64, LiveSpaceError> {
        self.file_releases.push((base_va, page_count));
        match self.next.take() {
            Some(err) => Err(err),
            None => Ok(FILE_RESIDENT),
        }
    }

    fn map_device_window(&mut self, phys_base: u64, len: usize) -> Result<u64, LiveSpaceError> {
        self.device_maps.push((phys_base, len));
        match self.next.take() {
            Some(err) => Err(err),
            None => Ok(0x9000_1000),
        }
    }

    fn map_framebuffer_window(
        &mut self,
        phys_base: u64,
        len: usize,
    ) -> Result<u64, LiveSpaceError> {
        self.framebuffer_maps.push((phys_base, len));
        match self.next.take() {
            Some(err) => Err(err),
            None => Ok(0x9000_2000),
        }
    }

    fn map_writeback_framebuffer_window(
        &mut self,
        phys_base: u64,
        len: usize,
    ) -> Result<u64, LiveSpaceError> {
        self.writeback_framebuffer_maps.push((phys_base, len));
        match self.next.take() {
            Some(err) => Err(err),
            None => Ok(0x9000_3000),
        }
    }

    fn retain_device_windows(
        &mut self,
        _keep: &mut dyn FnMut(u64, u64) -> bool,
        _unmapped: &mut dyn FnMut(u64, u64),
    ) -> Result<(), LiveSpaceError> {
        // The routing double models no page table, so it holds no window.
        self.next.take().map_or(Ok(()), Err)
    }

    fn translate_page(
        &self,
        _page: tairix_kernel_mem::Page,
    ) -> Option<(tairix_kernel_mem::Frame, tairix_kernel_mem::MapFlags)> {
        // The routing double models no page table; fault-resolution
        // translation is covered by the real `LiveSpace` in `kernel/mem`.
        None
    }

    fn alloc_dma(
        &mut self,
        len: usize,
        addr_limit: u64,
        custodian: DmaCustodian,
    ) -> Result<DmaMapping, LiveSpaceError> {
        self.dma_allocs
            .push((len, addr_limit, custodian.node, custodian.generation));
        match self.next.take() {
            Some(err) => Err(err),
            None => Ok(DmaMapping {
                cpu_va: 0xD000_2000,
                phys_base: DMA_PHYS,
                len: 2 * tairix_kernel_mem::PAGE_SIZE,
            }),
        }
    }

    fn free_dma(&mut self, cpu_va: u64) -> Result<usize, LiveSpaceError> {
        self.dma_frees.push(cpu_va);
        match self.next.take() {
            Some(err) => Err(err),
            None => Ok(tairix_kernel_mem::PAGE_SIZE),
        }
    }

    fn map_shared_chunks(
        &mut self,
        _chunks: &[(u64, u64)],
        _memory: SharedMemory,
    ) -> Result<u64, LiveSpaceError> {
        // The shared-memory producer's map/unmap routing is exercised at
        // the syscall-handler level and end-to-end in QEMU; this double
        // only satisfies the trait for the other producers' tests.
        match self.next.take() {
            Some(err) => Err(err),
            None => Ok(0x9000_5000),
        }
    }

    fn unmap_shared(&mut self, _base_va: u64, _len: usize) -> Result<(), LiveSpaceError> {
        match self.next.take() {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    fn freeze(&self) -> FrozenAddressSpace {
        // The producer-routing tests never inspect the snapshot; an empty
        // frozen space satisfies the trait. The re-freeze behaviour is
        // exercised end-to-end against a real `LiveSpace` in `aspace`.
        AddressSpace::new(HostPageTable::new()).freeze()
    }

    fn ramzip_fault_in(
        &mut self,
        _tier: &tairix_sync::SpinLock<tairix_kernel_mem::Ramzip>,
        _va: u64,
        _sink: &dyn tairix_log::Sink,
    ) -> tairix_kernel_mem::RamzipFaultOutcome {
        // The routing double models no page table and holds no tier; the
        // real compressed fault-in is exercised against `LiveSpace` in
        // `kernel/mem`. Fall through (no entry).
        tairix_kernel_mem::RamzipFaultOutcome::NoEntry
    }

    fn ramzip_reclaim(
        &mut self,
        _tier: &tairix_sync::SpinLock<tairix_kernel_mem::Ramzip>,
        _pressure: &tairix_reclaim::MemoryPressure,
        _reclaimable_residue: usize,
        _want: usize,
        _template: tairix_kernel_mem::PageCandidate,
        _sink: &dyn tairix_log::Sink,
    ) -> tairix_kernel_mem::RamzipReclaimSummary {
        // No candidates in the routing double; reclaim is exercised
        // against the real `LiveSpace`.
        tairix_kernel_mem::RamzipReclaimSummary::default()
    }

    fn ramzip_cluster(
        &mut self,
        _tier: &tairix_sync::SpinLock<tairix_kernel_mem::Ramzip>,
        _pressure: &tairix_reclaim::MemoryPressure,
        _va: u64,
        _sink: &dyn tairix_log::Sink,
    ) -> usize {
        // The routing double holds no tier; clustering is exercised
        // against the real `LiveSpace` in `kernel/mem`.
        0
    }

    fn ramzip_warm(
        &mut self,
        _tier: &tairix_sync::SpinLock<tairix_kernel_mem::Ramzip>,
        _pressure: &tairix_reclaim::MemoryPressure,
        _sink: &dyn tairix_log::Sink,
    ) -> usize {
        // As `ramzip_cluster`: warm-up is exercised against the real
        // `LiveSpace`.
        0
    }
}
