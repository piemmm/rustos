//! The kernel cross-process shared-memory region registry (`plans/USB.md`).
//!
//! A shared-memory region is a block of kernel-owned RAM two cooperating
//! processes map to exchange bulk data without a kernel copy (the USB
//! request-block data buffer). This registry owns the *policy* over those
//! regions: the kernel-allocated unforgeable region id, the backing block
//! set (each block's physical base and buddy order — one block for a small
//! region, several mapped into one contiguous window for a large one), the
//! reference count across the owner and
//! every grantee mapping, and the per-task list of live mappings used to
//! release the right region on `shm_unmap` and to reclaim a task's mappings
//! on exit or driver-unload teardown.
//!
//! The *mechanism* (allocate + zero + map + free frames) lives behind the
//! [`SharedMemFacility`] the caller passes in — the syscall handler hands its
//! own boot-installed producer, and the driver-unload teardown hands the same
//! one threaded through the spawn context — so the registry holds no global
//! producer of its own. Scrubbing through the kernel direct map (the facility's
//! job) lets the reclaim path free a region's frames even from a task whose
//! teardown is driven by the device manager rather than the region's owner.
//!
//! Like [`crate::callreg`] the region/mapping bookkeeping is global pure data
//! behind a [`SpinLock`] (never a `static mut`), because the syscall handlers
//! and the exit / driver-unload reclaim paths reach it from different call
//! sites and neither owns the other. Every operation fails closed.

use alloc::vec::Vec;
use core::ptr::NonNull;

use tairix_abi::Errno;
use tairix_collections::HashMap;
use tairix_hash::BuildFastHash;
use tairix_kernel_mem::{DmaCustodian, SharedMemory, PAGE_SIZE};
use tairix_kernel_sec::ProcessId;
use tairix_sync::SpinLock;

use crate::devres::{SharedChunk, SharedMemFacility};

/// One live shared-memory region.
struct Region {
    /// The region's physically-contiguous backing blocks: one for a small
    /// region, several for one larger than the single-block ceiling. Handed
    /// to the facility to map (into one contiguous window) and to free.
    chunks: Vec<SharedChunk>,
    /// Region size in whole pages (the sum of the chunks' pages).
    pages: u64,
    /// Live mappings of the region (owner map + every grantee map). The
    /// region's frames are freed when this reaches zero.
    refs: usize,
    /// Set for a region a DMA master may reach.
    dma: Option<DmaRegion>,
}

impl Region {
    fn memory(&self) -> SharedMemory {
        if self.dma.is_some() {
            SharedMemory::DmaCoherent
        } else {
            SharedMemory::Cacheable
        }
    }
}

/// Custody of a region a DMA master may reach (`plans/OPEN-DEFECTS.md` D167).
///
/// The region holds its node's custody binding for its whole life, so its
/// frames can reach the quarantine however long a grantee outlives its
/// creator.
#[derive(Clone, Copy)]
struct DmaRegion {
    custodian: DmaCustodian,
    /// The process that carved it: its driver, the one process that can have
    /// left the device running on it.
    creator: ProcessId,
    /// The creator ended still mapping it, so the device may still master it
    /// and its frames go to the quarantine rather than the allocator.
    orphaned: bool,
}

/// The registry state: the next id to mint, the live regions, and each
/// task's live `(base_va, region_id)` mappings.
///
/// Both maps are keyed on values the kernel assigns, never on caller input,
/// so the unkeyed hash is sound; and both grow fallibly, so a full kernel heap
/// refuses a syscall rather than aborting it.
struct State {
    next_id: u64,
    regions: HashMap<u64, Region, BuildFastHash>,
    mappings: HashMap<u64, Vec<(u64, u64)>, BuildFastHash>,
}

impl State {
    const fn new() -> Self {
        Self {
            next_id: 1,
            regions: HashMap::with_hasher(BuildFastHash::new()),
            mappings: HashMap::with_hasher(BuildFastHash::new()),
        }
    }

    /// Note that `process` maps region `id` at `base_va`.
    fn add_mapping(&mut self, process: u64, base_va: u64, id: u64) -> Result<(), Errno> {
        if let Some(list) = self.mappings.get_mut(&process) {
            list.try_reserve(1).map_err(|_| Errno::OutOfMemory)?;
            list.push((base_va, id));
            return Ok(());
        }
        let mut list = Vec::new();
        list.try_reserve_exact(1).map_err(|_| Errno::OutOfMemory)?;
        list.push((base_va, id));
        self.mappings
            .try_insert(process, list)
            .map_err(|_| Errno::OutOfMemory)?;
        Ok(())
    }
}

/// A copy of `chunks`, refused rather than aborting when the heap is full.
fn copy_chunks(chunks: &[SharedChunk]) -> Result<Vec<SharedChunk>, Errno> {
    let mut copy = Vec::new();
    copy.try_reserve_exact(chunks.len())
        .map_err(|_| Errno::OutOfMemory)?;
    copy.extend_from_slice(chunks);
    Ok(copy)
}

/// The global shared-region registry. Pure data behind a [`SpinLock`]; the
/// `mechanism` (the [`SharedMemFacility`]) is passed in by the caller, never
/// held here.
static REGIONS: SpinLock<State> = SpinLock::new(State::new());

/// Byte length of a `pages`-page region, saturating rather than truncating on
/// a 32-bit target (the value is advisory for the facility's `unmap_region`,
/// which releases by the recorded page count, so saturation cannot misrelease).
fn region_len_bytes(pages: u64) -> usize {
    usize::try_from(pages)
        .unwrap_or(usize::MAX)
        .saturating_mul(PAGE_SIZE)
}

/// Create a shared region of `pages` pages owned by `owner`, mapping it into
/// the owner's own live address space through `facility`. Returns the base
/// user virtual address and the kernel-minted region id.
///
/// # Errors
///
/// The facility error (frame exhaustion, oversize, no virtual slot). On a map
/// failure the freshly allocated frames are returned to the allocator, so a
/// failed create leaks nothing.
pub fn create(
    facility: &dyn SharedMemFacility,
    owner: ProcessId,
    pages: u64,
) -> Result<(u64, u64), Errno> {
    let chunks = facility.alloc_region(pages)?;
    let base_va = match facility.map_region(&chunks, SharedMemory::Cacheable) {
        Ok(va) => va,
        Err(err) => {
            facility.free_region(&chunks, SharedMemory::Cacheable);
            return Err(err);
        }
    };
    match record(owner, base_va, chunks, pages, None) {
        Ok(id) => Ok((base_va, id)),
        Err(chunks) => {
            let _ = facility.unmap_region(base_va, region_len_bytes(pages));
            facility.free_region(&chunks, SharedMemory::Cacheable);
            Err(Errno::OutOfMemory)
        }
    }
}

/// A DMA region [`create_dma`] carved and mapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmaRegionCreated {
    /// Base user virtual address of the creator's mapping.
    pub base_va: u64,
    /// The kernel-minted region id.
    pub id: u64,
    /// CPU-physical base of the region's one contiguous block.
    pub phys_base: u64,
    /// Pages the block spans: the request rounded up to one buddy block.
    pub pages: u64,
}

/// Create a region a DMA master may reach: one physically contiguous block of
/// at least `pages` pages below `addr_limit`, carved for `custodian`'s device,
/// owned by `owner` and mapped coherent into its live space.
///
/// The region binds its node's custody for its life. `owner`'s own unmap is
/// its word that the device is done with the region; should `owner` end
/// still mapping it, the frames pass to the quarantine when the last mapping
/// goes, because the device may still be mastering them.
///
/// # Errors
///
/// The custody's refusal to record the region ([`Errno::NotImplemented`]
/// where none is wired, [`Errno::OutOfMemory`] where it cannot record), or
/// the facility's carve or map error. A failed create leaves nothing
/// allocated or bound.
pub fn create_dma(
    facility: &dyn SharedMemFacility,
    owner: ProcessId,
    custodian: DmaCustodian,
    pages: u64,
    addr_limit: u64,
) -> Result<DmaRegionCreated, Errno> {
    custodian
        .custody
        .bind(custodian.node)
        .map_err(crate::live_producer::dma_errno)?;
    let unbind = || custodian.custody.unbind(custodian.node);
    let chunk = facility
        .alloc_dma_region(pages, addr_limit)
        .inspect_err(|_| unbind())?;
    let abandon = |chunks: &[SharedChunk]| {
        facility.free_region(chunks, SharedMemory::DmaCoherent);
        unbind();
    };
    let chunks = match copy_chunks(&[chunk]) {
        Ok(chunks) => chunks,
        Err(err) => {
            abandon(&[chunk]);
            return Err(err);
        }
    };
    let base_va = match facility.map_region(&chunks, SharedMemory::DmaCoherent) {
        Ok(va) => va,
        Err(err) => {
            abandon(&chunks);
            return Err(err);
        }
    };
    let dma = DmaRegion {
        custodian,
        creator: owner,
        orphaned: false,
    };
    let id = match record(owner, base_va, chunks, chunk.pages, Some(dma)) {
        Ok(id) => id,
        Err(chunks) => {
            let _ = facility.unmap_region(base_va, region_len_bytes(chunk.pages));
            abandon(&chunks);
            return Err(Errno::OutOfMemory);
        }
    };
    Ok(DmaRegionCreated {
        base_va,
        id,
        phys_base: chunk.phys_base,
        pages: chunk.pages,
    })
}

/// Record a freshly mapped region owned by `owner`, returning its new id, or
/// handing `chunks` back untouched for the caller to free when the registry
/// cannot grow.
fn record(
    owner: ProcessId,
    base_va: u64,
    chunks: Vec<SharedChunk>,
    pages: u64,
    dma: Option<DmaRegion>,
) -> Result<u64, Vec<SharedChunk>> {
    let mut state = REGIONS.lock();
    let id = state.next_id;
    // The entry goes in holding no chunks, so a refused insert drops nothing
    // the caller must still free.
    let entry = Region {
        chunks: Vec::new(),
        pages,
        refs: 1,
        dma,
    };
    if state.regions.try_insert(id, entry).is_err() {
        return Err(chunks);
    }
    if state.add_mapping(owner.0, base_va, id).is_err() {
        state.regions.remove(&id);
        return Err(chunks);
    }
    let Some(region) = state.regions.get_mut(&id) else {
        return Err(chunks);
    };
    region.chunks = chunks;
    state.next_id = state.next_id.wrapping_add(1);
    Ok(id)
}

/// Map an existing region `id` into `process`'s own live address space,
/// returning its base user virtual address and the region's byte length
/// (the registry's own record — the size the caller may trust without
/// consulting the granting task).
///
/// # Errors
///
/// [`Errno::NotFound`] if the region was torn down, or the facility error.
pub fn map(
    facility: &dyn SharedMemFacility,
    process: ProcessId,
    id: u64,
) -> Result<(u64, usize), Errno> {
    // The mapping's reference is taken before its entries exist, so a
    // concurrent last unmap cannot free the frames under it.
    let (chunks, pages, memory) = {
        let mut state = REGIONS.lock();
        let region = state.regions.get_mut(&id).ok_or(Errno::NotFound)?;
        let chunks = copy_chunks(&region.chunks)?;
        region.refs += 1;
        (chunks, region.pages, region.memory())
    };
    let len = region_len_bytes(pages);
    let base_va = match facility.map_region(&chunks, memory) {
        Ok(va) => va,
        Err(err) => {
            release_ref(facility, id);
            return Err(err);
        }
    };
    let recorded = REGIONS.lock().add_mapping(process.0, base_va, id);
    if let Err(err) = recorded {
        let _ = facility.unmap_region(base_va, len);
        release_ref(facility, id);
        return Err(err);
    }
    Ok((base_va, len))
}

/// A shared mapping whose page-table entries are gone but whose reference is
/// still held, so the region's frames outlive every view the caller has yet
/// to withdraw them from. Dropping it releases the reference, freeing the
/// frames if it was the last.
#[must_use = "the reference is released when this is dropped"]
pub struct Unmapped<'f> {
    facility: &'f dyn SharedMemFacility,
    id: u64,
    len: usize,
}

impl Unmapped<'_> {
    /// The byte length released — the registry's own record of the region.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// `true` if the region is zero-length (never the case for a live
    /// region — creation requires at least one page).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for Unmapped<'_> {
    fn drop(&mut self) {
        release_ref(self.facility, self.id);
    }
}

/// Release `process`'s shared mapping based at `base`, tearing down its
/// page-table entries; the reference goes when the returned [`Unmapped`] is
/// dropped, and the region's frames are zeroed and freed at the last one.
///
/// The caller drops the region's pages from the process's address-space
/// snapshot first, so no copy can reach frames a release has freed.
///
/// # Errors
///
/// [`Errno::NotFound`] if `base` does not name a live shared mapping of
/// `process`, or the facility's unmap error (the reference is released
/// either way).
pub fn unmap(
    facility: &dyn SharedMemFacility,
    process: ProcessId,
    base: u64,
) -> Result<Unmapped<'_>, Errno> {
    // Find and remove the mapping record and recover its region's length
    // under the lock; the reference is held by the returned guard.
    let (id, len) = {
        let mut state = REGIONS.lock();
        let list = state.mappings.get_mut(&process.0).ok_or(Errno::NotFound)?;
        let pos = list
            .iter()
            .position(|&(b, _)| b == base)
            .ok_or(Errno::NotFound)?;
        let (_, id) = list.remove(pos);
        if list.is_empty() {
            state.mappings.remove(&process.0);
        }
        let region = state.regions.get(&id).ok_or(Errno::NotFound)?;
        (id, region_len_bytes(region.pages))
    };
    let unmapped = Unmapped { facility, id, len };
    facility.unmap_region(base, len)?;
    Ok(unmapped)
}

/// Drop one reference to region `id`, releasing its frames if this was the
/// last one: to the allocator, or to its node's quarantine for a DMA region
/// whose creator ended still mapping it. The shared release step behind
/// [`Unmapped`], [`reclaim_process`], and a [`KernelHold`] drop.
fn release_ref(facility: &dyn SharedMemFacility, id: u64) {
    let released = {
        let mut state = REGIONS.lock();
        let Some(region) = state.regions.get_mut(&id) else {
            return;
        };
        region.refs -= 1;
        if region.refs == 0 {
            state.regions.remove(&id)
        } else {
            None
        }
    };
    let Some(region) = released else {
        return;
    };
    match region.dma {
        None => facility.free_region(&region.chunks, SharedMemory::Cacheable),
        Some(dma) => {
            if dma.orphaned {
                facility.surrender_region(&region.chunks, &dma.custodian);
            } else {
                facility.free_region(&region.chunks, SharedMemory::DmaCoherent);
            }
            dma.custodian.custody.unbind(dma.custodian.node);
        }
    }
}

/// A **kernel** consumer's counted hold on a shared region: the region's
/// frames stay alive (and are reached through the kernel direct map, never
/// a user mapping) until the hold is dropped.
///
/// The first consumer is the runtime volume attach path's block client
/// (`plans/DEVICES.md` D3b), which drives a user-space block service
/// through the service's shared data window. Holding a reference here is
/// what makes the owner's exit safe: the owner's mappings are reclaimed,
/// but the frames are freed only when the kernel's hold also drops, so the
/// kernel never reads through a dangling window.
pub struct KernelHold {
    facility: &'static dyn SharedMemFacility,
    id: u64,
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: the hold owns a counted reference on kernel-owned, physically
// contiguous frames reached through the kernel direct map; the pointer
// stays valid for the hold's whole life on any CPU, and the hold hands the
// raw pointer out only through `as_ptr` (no references are formed here),
// so moving or sharing the handle across threads cannot create an aliasing
// or lifetime hazard the holder does not already manage.
unsafe impl Send for KernelHold {}
// SAFETY: as for `Send` — `&KernelHold` only exposes the raw base pointer
// and length.
unsafe impl Sync for KernelHold {}

impl KernelHold {
    /// Base of the region in kernel-reachable memory.
    #[must_use]
    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    /// Region length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// `true` if the region is zero-length (never the case for a live
    /// region — creation requires at least one page).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for KernelHold {
    fn drop(&mut self) {
        release_ref(self.facility, self.id);
    }
}

#[cfg(test)]
impl KernelHold {
    /// A hold over caller-owned test memory, bypassing the registry: the
    /// inert facility's release is a no-op for the unknown id, so dropping
    /// the hold never frees anything. The caller keeps the pointed-to
    /// buffer alive for the hold's life.
    pub(crate) fn for_test(ptr: NonNull<u8>, len: usize) -> Self {
        Self {
            facility: &crate::devres::NULL_SHARED_MEM_FACILITY,
            id: 0,
            ptr,
            len,
        }
    }
}

/// Take a kernel-side counted hold on region `id`, translating its frames
/// through the kernel direct map.
///
/// # Errors
///
/// [`Errno::NotFound`] if the region was torn down, or
/// [`Errno::NotImplemented`] for a DMA region, which the kernel cannot read
/// coherently, or when the facility cannot reach the frames (fail closed; the
/// reference is released again).
pub fn kernel_hold(facility: &'static dyn SharedMemFacility, id: u64) -> Result<KernelHold, Errno> {
    let (chunks, pages) = {
        let mut state = REGIONS.lock();
        let region = state.regions.get_mut(&id).ok_or(Errno::NotFound)?;
        // A hold reads through the cacheable direct map, which a device's
        // writes to a coherent region bypass.
        if region.dma.is_some() {
            return Err(Errno::NotImplemented);
        }
        let chunks = copy_chunks(&region.chunks)?;
        region.refs += 1;
        (chunks, region.pages)
    };
    let len = region_len_bytes(pages);
    // A multi-chunk region is not physically contiguous, so the facility
    // returns `None` and the hold fails closed (no kernel consumer maps one).
    if let Some(ptr) = facility.kernel_window(&chunks, len) {
        Ok(KernelHold {
            facility,
            id,
            ptr,
            len,
        })
    } else {
        release_ref(facility, id);
        Err(Errno::NotImplemented)
    }
}

/// Reclaim every shared mapping `process` held when it exits or is torn down,
/// dropping each reference and freeing any region whose last reference this
/// releases. Does **not** tear down page-table entries (the task's address
/// space is being destroyed). Idempotent.
///
/// A DMA region `process` carved and still maps is orphaned first, so its
/// frames reach the quarantine rather than the allocator. Returns the bytes
/// orphaned, which join the process's own quarantined carves in its audit
/// record.
#[must_use]
pub fn reclaim_process(facility: &dyn SharedMemFacility, process: ProcessId) -> u64 {
    let (ids, orphaned) = {
        let mut state = REGIONS.lock();
        let Some(list) = state.mappings.remove(&process.0) else {
            return 0;
        };
        let mut orphaned: u64 = 0;
        for &(_, id) in &list {
            let Some(region) = state.regions.get_mut(&id) else {
                continue;
            };
            let pages = region.pages;
            if let Some(dma) = region
                .dma
                .as_mut()
                .filter(|dma| dma.creator == process && !dma.orphaned)
            {
                dma.orphaned = true;
                orphaned = orphaned.saturating_add(pages.saturating_mul(PAGE_SIZE as u64));
            }
        }
        (list, orphaned)
    };
    for (_, id) in ids {
        release_ref(facility, id);
    }
    orphaned
}

/// Number of live regions. Diagnostic / test observer.
#[must_use]
pub fn live_regions() -> usize {
    REGIONS.lock().regions.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate std;
    use core::sync::atomic::{AtomicU64, Ordering};
    use std::boxed::Box;
    use std::sync::Mutex;
    use std::vec::Vec;

    /// A deterministic [`SharedMemFacility`] double: it hands out a distinct
    /// physical base per allocation, derives the mapped VA from it, and
    /// records every `unmap` / `free` so a test can assert the refcounted
    /// zero-on-free fires exactly once at the last reference. Per-test (passed
    /// in), so the global `REGIONS` is the only shared state and tests use
    /// distinct task ids to stay independent.
    struct FakeFacility {
        next_phys: AtomicU64,
        maps: Mutex<Vec<(u64, u64)>>,
        map_memory: Mutex<Vec<SharedMemory>>,
        unmaps: Mutex<Vec<(u64, usize)>>,
        frees: Mutex<Vec<(u64, u32, u64)>>,
        free_memory: Mutex<Vec<SharedMemory>>,
        surrendered: Mutex<Vec<(u64, u32, u64)>>,
        fail_dma_alloc: bool,
        fail_map: bool,
    }

    impl FakeFacility {
        fn new() -> Self {
            Self {
                // A high, page-aligned base unlikely to collide with anything
                // a test reasons about; each alloc bumps it.
                next_phys: AtomicU64::new(0x1_0000_0000),
                maps: Mutex::new(Vec::new()),
                map_memory: Mutex::new(Vec::new()),
                unmaps: Mutex::new(Vec::new()),
                frees: Mutex::new(Vec::new()),
                free_memory: Mutex::new(Vec::new()),
                surrendered: Mutex::new(Vec::new()),
                fail_dma_alloc: false,
                fail_map: false,
            }
        }
        fn va_for(phys: u64) -> u64 {
            0x9000_0000_0000 + phys
        }
    }

    impl SharedMemFacility for FakeFacility {
        fn alloc_region(&self, pages: u64) -> Result<Vec<SharedChunk>, Errno> {
            // One chunk covering the whole request (the small, single-block
            // case); `WindowFacility` overrides this to split into several.
            let phys = self.next_phys.fetch_add(0x10_0000, Ordering::Relaxed);
            Ok(alloc::vec![SharedChunk {
                phys_base: phys,
                order: 0,
                pages,
            }])
        }
        fn alloc_dma_region(&self, pages: u64, addr_limit: u64) -> Result<SharedChunk, Errno> {
            if self.fail_dma_alloc {
                return Err(Errno::OutOfMemory);
            }
            let phys = self.next_phys.fetch_add(0x10_0000, Ordering::Relaxed);
            if addr_limit != 0 && phys >= addr_limit {
                return Err(Errno::OutOfRange);
            }
            let pages = pages.next_power_of_two();
            Ok(SharedChunk {
                phys_base: phys,
                order: pages.trailing_zeros(),
                pages,
            })
        }
        fn map_region(&self, chunks: &[SharedChunk], memory: SharedMemory) -> Result<u64, Errno> {
            if self.fail_map {
                return Err(Errno::OutOfMemory);
            }
            // Record the first chunk's base and the total page count, so a
            // test can assert the mapped extent without knowing the split.
            let total: u64 = chunks.iter().map(|c| c.pages).sum();
            let first = chunks[0].phys_base;
            self.maps.lock().unwrap().push((first, total));
            self.map_memory.lock().unwrap().push(memory);
            Ok(Self::va_for(first))
        }
        fn unmap_region(&self, base: u64, len: usize) -> Result<(), Errno> {
            self.unmaps.lock().unwrap().push((base, len));
            Ok(())
        }
        fn free_region(&self, chunks: &[SharedChunk], memory: SharedMemory) {
            for c in chunks {
                self.frees
                    .lock()
                    .unwrap()
                    .push((c.phys_base, c.order, c.pages));
            }
            self.free_memory.lock().unwrap().push(memory);
        }
        fn surrender_region(&self, chunks: &[SharedChunk], custodian: &DmaCustodian) {
            for c in chunks {
                custodian.custody.hold(
                    custodian.node,
                    custodian.generation,
                    tairix_kernel_mem::DmaBlock {
                        frame: tairix_kernel_mem::Frame::containing(
                            tairix_kernel_mem::PhysAddr::new(c.phys_base),
                        ),
                        order: c.order,
                    },
                );
                self.surrendered
                    .lock()
                    .unwrap()
                    .push((c.phys_base, c.order, c.pages));
            }
        }
    }

    #[test]
    fn create_maps_and_grants_an_owner_reference() {
        let fac = FakeFacility::new();
        let owner = ProcessId(0x5_0001);
        let (va, id) = create(&fac, owner, 2).expect("create");
        // The owner's mapping was created and the VA flows back from the
        // facility.
        assert_eq!(fac.maps.lock().unwrap().len(), 1);
        assert_eq!(fac.maps.lock().unwrap()[0].1, 2, "two pages mapped");
        assert_eq!(va, FakeFacility::va_for(fac.maps.lock().unwrap()[0].0));
        // Cleanup: the owner releases its only reference, freeing the region.
        drop(unmap(&fac, owner, va).expect("unmap"));
        assert_eq!(fac.frees.lock().unwrap().len(), 1, "freed at last ref");
        // The id is unforgeable to a later map once the region is gone.
        assert_eq!(map(&fac, owner, id), Err(Errno::NotFound));
    }

    #[test]
    fn region_frees_only_when_the_last_reference_is_released() {
        let fac = FakeFacility::new();
        let owner = ProcessId(0x5_0002);
        let grantee = ProcessId(0x5_0003);
        let (owner_va, id) = create(&fac, owner, 1).expect("create");
        // A grantee maps the same region: refs = 2. The reported length is
        // the registry's own record of the one-page region, never a claim.
        let (grantee_va, grantee_len) = map(&fac, grantee, id).expect("grantee maps");
        assert_eq!(grantee_len, PAGE_SIZE);
        assert_eq!(fac.maps.lock().unwrap().len(), 2);

        // The owner releases first: ref drops to 1, the frames are NOT freed
        // while the grantee still maps them (no use-after-free).
        drop(unmap(&fac, owner, owner_va).expect("owner unmap"));
        assert!(
            fac.frees.lock().unwrap().is_empty(),
            "not freed while a grantee still maps the region"
        );
        // The grantee releases last: now the region's frames are scrubbed and
        // freed exactly once.
        drop(unmap(&fac, grantee, grantee_va).expect("grantee unmap"));
        assert_eq!(fac.frees.lock().unwrap().len(), 1, "freed at last ref");
        assert_eq!(
            fac.unmaps.lock().unwrap().len(),
            2,
            "both mappings torn down"
        );
    }

    /// A facility on which the owner's last unmap lands while a grantee's map
    /// is installing its entries: the interleaving two CPUs can produce.
    struct UnmapDuringMap {
        inner: FakeFacility,
        owner: ProcessId,
        owner_va: AtomicU64,
    }

    impl SharedMemFacility for UnmapDuringMap {
        fn alloc_region(&self, pages: u64) -> Result<Vec<SharedChunk>, Errno> {
            self.inner.alloc_region(pages)
        }
        fn map_region(&self, chunks: &[SharedChunk], memory: SharedMemory) -> Result<u64, Errno> {
            let owner_va = self.owner_va.swap(0, Ordering::Relaxed);
            if owner_va != 0 {
                drop(unmap(self, self.owner, owner_va).expect("the owner's last unmap"));
            }
            self.inner.map_region(chunks, memory)
        }
        fn unmap_region(&self, base: u64, len: usize) -> Result<(), Errno> {
            self.inner.unmap_region(base, len)
        }
        fn free_region(&self, chunks: &[SharedChunk], memory: SharedMemory) {
            self.inner.free_region(chunks, memory);
        }
    }

    #[test]
    fn a_last_unmap_racing_a_map_cannot_free_the_frames_under_it() {
        let fac = UnmapDuringMap {
            inner: FakeFacility::new(),
            owner: ProcessId(0x5_0010),
            owner_va: AtomicU64::new(0),
        };
        let grantee = ProcessId(0x5_0011);
        let (owner_va, id) = create(&fac, fac.owner, 1).expect("create");
        fac.owner_va.store(owner_va, Ordering::Relaxed);

        let (grantee_va, _) = map(&fac, grantee, id).expect("the grantee maps");
        assert!(
            fac.inner.frees.lock().unwrap().is_empty(),
            "freed while the grantee's entries were being installed"
        );
        drop(unmap(&fac, grantee, grantee_va).expect("grantee unmap"));
        assert_eq!(fac.inner.frees.lock().unwrap().len(), 1);
    }

    #[test]
    fn an_unmapped_region_is_freed_only_when_its_reference_goes() {
        let fac = FakeFacility::new();
        let owner = ProcessId(0x5_0012);
        let (va, _) = create(&fac, owner, 1).expect("create");
        let unmapped = unmap(&fac, owner, va).expect("unmap");
        assert_eq!(unmapped.len(), PAGE_SIZE);
        assert_eq!(fac.unmaps.lock().unwrap().len(), 1, "entries torn down");
        assert!(
            fac.frees.lock().unwrap().is_empty(),
            "freed before the caller withdrew its view"
        );
        drop(unmapped);
        assert_eq!(fac.frees.lock().unwrap().len(), 1);
    }

    #[test]
    fn reclaim_process_drops_references_and_frees_at_zero() {
        let fac = FakeFacility::new();
        let owner = ProcessId(0x5_0004);
        let grantee = ProcessId(0x5_0005);
        let (_owner_va, id) = create(&fac, owner, 1).expect("create");
        let (_grantee_va, _) = map(&fac, grantee, id).expect("grantee maps");

        // Reclaiming the grantee (e.g. a class driver unloaded on hot-removal)
        // drops its reference but does not free the region — the owner still
        // holds it.
        assert_eq!(reclaim_process(&fac, grantee), 0);
        assert!(fac.frees.lock().unwrap().is_empty());
        // Reclaiming the owner drops the last reference: the region is freed.
        assert_eq!(reclaim_process(&fac, owner), 0);
        assert_eq!(fac.frees.lock().unwrap().len(), 1, "freed at last ref");
        // Reclaiming a task with no mappings is a benign no-op (idempotent).
        assert_eq!(reclaim_process(&fac, owner), 0);
        assert_eq!(fac.frees.lock().unwrap().len(), 1);
    }

    /// A facility double whose `kernel_window` serves a real buffer, so the
    /// kernel-hold path can be exercised host-side.
    struct WindowFacility {
        inner: FakeFacility,
        window: Mutex<Vec<u8>>,
    }

    impl SharedMemFacility for WindowFacility {
        fn alloc_region(&self, pages: u64) -> Result<Vec<SharedChunk>, Errno> {
            // One single-page chunk per page, so a `pages > 1` region is a
            // genuine multi-chunk region (the kernel-hold fail-closed case).
            let mut chunks = Vec::new();
            for _ in 0..pages {
                let phys = self.inner.next_phys.fetch_add(0x1000, Ordering::Relaxed);
                chunks.push(SharedChunk {
                    phys_base: phys,
                    order: 0,
                    pages: 1,
                });
            }
            Ok(chunks)
        }
        fn map_region(&self, chunks: &[SharedChunk], memory: SharedMemory) -> Result<u64, Errno> {
            self.inner.map_region(chunks, memory)
        }
        fn unmap_region(&self, base: u64, len: usize) -> Result<(), Errno> {
            self.inner.unmap_region(base, len)
        }
        fn free_region(&self, chunks: &[SharedChunk], memory: SharedMemory) {
            self.inner.free_region(chunks, memory);
        }
        fn alloc_dma_region(&self, pages: u64, addr_limit: u64) -> Result<SharedChunk, Errno> {
            self.inner.alloc_dma_region(pages, addr_limit)
        }
        fn kernel_window(&self, chunks: &[SharedChunk], len: usize) -> Option<NonNull<u8>> {
            // Only a single-chunk (physically contiguous) region is reachable
            // as one kernel window; a multi-chunk region fails closed.
            if chunks.len() != 1 {
                return None;
            }
            let mut window = self.window.lock().unwrap();
            if window.len() < len {
                window.resize(len, 0);
            }
            NonNull::new(window.as_mut_ptr())
        }
    }

    #[test]
    fn kernel_hold_keeps_the_region_alive_past_the_owner() {
        let fac: &'static WindowFacility = Box::leak(Box::new(WindowFacility {
            inner: FakeFacility::new(),
            window: Mutex::new(Vec::new()),
        }));
        let owner = ProcessId(0x5_0007);
        let (_va, id) = create(fac, owner, 1).expect("create");
        let hold = kernel_hold(fac, id).expect("kernel hold");
        assert_eq!(hold.len(), PAGE_SIZE);
        assert!(!hold.is_empty());
        assert!(!hold.as_ptr().is_null());

        // The owner exits: its mapping is reclaimed, but the kernel's hold
        // keeps the frames alive.
        assert_eq!(reclaim_process(fac, owner), 0);
        assert!(fac.inner.frees.lock().unwrap().is_empty());
        // Dropping the hold releases the last reference and frees exactly
        // once.
        drop(hold);
        assert_eq!(fac.inner.frees.lock().unwrap().len(), 1);
        // The region is gone: a later hold fails closed.
        assert_eq!(kernel_hold(fac, id).err(), Some(Errno::NotFound));
    }

    #[test]
    fn kernel_hold_fails_closed_for_a_multi_chunk_region() {
        let fac: &'static WindowFacility = Box::leak(Box::new(WindowFacility {
            inner: FakeFacility::new(),
            window: Mutex::new(Vec::new()),
        }));
        let owner = ProcessId(0x5_0009);
        // A two-page region is two single-page chunks: not physically
        // contiguous, so no single kernel window can span it and the hold
        // fails closed rather than fabricating a contiguous view.
        let (va, id) = create(fac, owner, 2).expect("create");
        assert_eq!(kernel_hold(fac, id).err(), Some(Errno::NotImplemented));
        // The failed hold released its extra reference: the owner's unmap
        // still frees the region exactly once (both chunks).
        drop(unmap(fac, owner, va).expect("unmap"));
        assert_eq!(
            fac.inner.frees.lock().unwrap().len(),
            2,
            "both chunks freed at the last reference"
        );
    }

    #[test]
    fn kernel_hold_fails_closed_when_the_kernel_cannot_reach_the_frames() {
        // `FakeFacility` inherits the fail-closed default `kernel_window`.
        let fac: &'static FakeFacility = Box::leak(Box::new(FakeFacility::new()));
        let owner = ProcessId(0x5_0008);
        let (va, id) = create(fac, owner, 1).expect("create");
        assert_eq!(kernel_hold(fac, id).err(), Some(Errno::NotImplemented));
        // The failed hold released its reference: the owner's unmap still
        // frees exactly once.
        drop(unmap(fac, owner, va).expect("unmap"));
        assert_eq!(fac.frees.lock().unwrap().len(), 1);
    }

    /// A custody recording every binding, surrender and release, so a test can
    /// assert a DMA region holds its node's record for exactly its life.
    #[derive(Default)]
    struct FakeCustody {
        binds: Mutex<Vec<u32>>,
        unbinds: Mutex<Vec<u32>>,
        held: Mutex<Vec<(u32, u64, u64)>>,
        refuse_bind: bool,
    }

    impl tairix_kernel_mem::DmaCustody for FakeCustody {
        fn bind(&self, node: u32) -> Result<(), tairix_kernel_mem::DmaError> {
            if self.refuse_bind {
                return Err(tairix_kernel_mem::DmaError::Alloc(
                    tairix_kernel_mem::AllocError::OutOfMemory,
                ));
            }
            self.binds.lock().unwrap().push(node);
            Ok(())
        }
        fn hold(&self, node: u32, generation: u64, block: tairix_kernel_mem::DmaBlock) {
            self.held
                .lock()
                .unwrap()
                .push((node, generation, block.frame.start().as_u64()));
        }
        fn unbind(&self, node: u32) {
            self.unbinds.lock().unwrap().push(node);
        }
    }

    fn custodian(custody: &'static FakeCustody) -> DmaCustodian {
        DmaCustodian {
            node: 7,
            generation: 3,
            custody,
        }
    }

    fn leaked_custody() -> &'static FakeCustody {
        Box::leak(Box::new(FakeCustody::default()))
    }

    #[test]
    fn a_dma_region_is_coherent_everywhere_and_frees_once_its_creator_let_go() {
        let fac = FakeFacility::new();
        let custody = leaked_custody();
        let creator = ProcessId(0x5_0101);
        let consumer = ProcessId(0x5_0102);
        let made = create_dma(&fac, creator, custodian(custody), 3, 0).expect("carves");
        // Three pages round up to the four-page block the carve holds.
        assert_eq!(fac.maps.lock().unwrap()[0], (made.phys_base, 4));
        let (consumer_va, len) = map(&fac, consumer, made.id).expect("consumer maps");
        assert_eq!(len, 4 * PAGE_SIZE);
        assert_eq!(
            *fac.map_memory.lock().unwrap(),
            [SharedMemory::DmaCoherent, SharedMemory::DmaCoherent]
        );
        assert_eq!(*custody.binds.lock().unwrap(), [7]);

        // The creator releases its own mapping while alive — its claim that
        // the device is stopped — so the last release frees normally.
        drop(unmap(&fac, creator, made.base_va).expect("creator unmaps"));
        assert!(fac.frees.lock().unwrap().is_empty());
        drop(unmap(&fac, consumer, consumer_va).expect("consumer unmaps"));
        assert_eq!(*fac.frees.lock().unwrap(), [(made.phys_base, 2, 4)]);
        assert_eq!(
            *fac.free_memory.lock().unwrap(),
            [SharedMemory::DmaCoherent]
        );
        assert!(fac.surrendered.lock().unwrap().is_empty());
        assert_eq!(*custody.unbinds.lock().unwrap(), [7]);
    }

    #[test]
    fn a_dma_region_whose_creator_died_holding_it_reaches_the_quarantine() {
        let fac = FakeFacility::new();
        let custody = leaked_custody();
        let creator = ProcessId(0x5_0103);
        let consumer = ProcessId(0x5_0104);
        let made = create_dma(&fac, creator, custodian(custody), 4, 0).expect("carves");
        let (consumer_va, _) = map(&fac, consumer, made.id).expect("consumer maps");

        // The creator dies still mapping it: the device may still master the
        // block, so the bytes are reported and nothing is freed yet.
        assert_eq!(reclaim_process(&fac, creator), 4 * PAGE_SIZE as u64);
        assert!(fac.frees.lock().unwrap().is_empty());
        assert!(custody.unbinds.lock().unwrap().is_empty());

        // The consumer's release is the last: the block goes to the node's
        // quarantine under the dead driver's generation, never the allocator.
        drop(unmap(&fac, consumer, consumer_va).expect("consumer unmaps"));
        assert!(fac.frees.lock().unwrap().is_empty());
        assert_eq!(*custody.held.lock().unwrap(), [(7, 3, made.phys_base)]);
        assert_eq!(*custody.unbinds.lock().unwrap(), [7]);
    }

    #[test]
    fn a_dma_region_its_dead_creator_last_mapped_is_quarantined_at_once() {
        let fac = FakeFacility::new();
        let custody = leaked_custody();
        let creator = ProcessId(0x5_0105);
        let made = create_dma(&fac, creator, custodian(custody), 1, 0).expect("carves");
        assert_eq!(reclaim_process(&fac, creator), PAGE_SIZE as u64);
        assert_eq!(*custody.held.lock().unwrap(), [(7, 3, made.phys_base)]);
        assert_eq!(*custody.unbinds.lock().unwrap(), [7]);
        assert!(fac.frees.lock().unwrap().is_empty());
        assert_eq!(map(&fac, creator, made.id), Err(Errno::NotFound));
    }

    #[test]
    fn kernel_hold_refuses_a_dma_region() {
        let fac: &'static WindowFacility = Box::leak(Box::new(WindowFacility {
            inner: FakeFacility::new(),
            window: Mutex::new(Vec::new()),
        }));
        let creator = ProcessId(0x5_0108);
        let made = create_dma(fac, creator, custodian(leaked_custody()), 1, 0).expect("carves");
        assert_eq!(kernel_hold(fac, made.id).err(), Some(Errno::NotImplemented));
        // The refusal took no reference: the creator's unmap is the last.
        drop(unmap(fac, creator, made.base_va).expect("creator unmaps"));
        assert_eq!(fac.inner.frees.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_grantee_ending_never_orphans_a_dma_region() {
        let fac = FakeFacility::new();
        let custody = leaked_custody();
        let creator = ProcessId(0x5_0106);
        let consumer = ProcessId(0x5_0107);
        let made = create_dma(&fac, creator, custodian(custody), 1, 0).expect("carves");
        map(&fac, consumer, made.id).expect("consumer maps");
        assert_eq!(reclaim_process(&fac, consumer), 0);
        drop(unmap(&fac, creator, made.base_va).expect("creator unmaps"));
        assert_eq!(fac.frees.lock().unwrap().len(), 1);
        assert!(custody.held.lock().unwrap().is_empty());
    }

    #[test]
    fn a_failed_dma_create_leaves_nothing_bound_or_allocated() {
        let refusing: &'static FakeCustody = Box::leak(Box::new(FakeCustody {
            refuse_bind: true,
            ..FakeCustody::default()
        }));
        let fac = FakeFacility::new();
        assert_eq!(
            create_dma(&fac, ProcessId(0x5_0108), custodian(refusing), 1, 0),
            Err(Errno::OutOfMemory)
        );
        assert!(fac.maps.lock().unwrap().is_empty());

        let custody = leaked_custody();
        let failing_alloc = FakeFacility {
            fail_dma_alloc: true,
            ..FakeFacility::new()
        };
        assert_eq!(
            create_dma(
                &failing_alloc,
                ProcessId(0x5_0109),
                custodian(custody),
                1,
                0
            ),
            Err(Errno::OutOfMemory)
        );
        let failing_map = FakeFacility {
            fail_map: true,
            ..FakeFacility::new()
        };
        assert_eq!(
            create_dma(&failing_map, ProcessId(0x5_010A), custodian(custody), 1, 0),
            Err(Errno::OutOfMemory)
        );
        assert_eq!(failing_map.frees.lock().unwrap().len(), 1);
        // A block past the device's reach is refused, and bound nothing.
        let fac = FakeFacility::new();
        assert_eq!(
            create_dma(&fac, ProcessId(0x5_010B), custodian(custody), 1, 0x1000),
            Err(Errno::OutOfRange)
        );
        assert_eq!(*custody.binds.lock().unwrap(), [7, 7, 7]);
        assert_eq!(*custody.unbinds.lock().unwrap(), [7, 7, 7]);
    }

    #[test]
    fn map_and_unmap_fail_closed_for_unknown_ids() {
        let fac = FakeFacility::new();
        let process = ProcessId(0x5_0006);
        // A region id that was never created.
        assert_eq!(map(&fac, process, 0xDEAD_BEEF), Err(Errno::NotFound));
        // A base VA the process never mapped.
        assert_eq!(unmap(&fac, process, 0x1234).err(), Some(Errno::NotFound));
        // Neither touched the facility's free path.
        assert!(fac.frees.lock().unwrap().is_empty());
    }
}
