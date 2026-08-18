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

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::ptr::NonNull;

use tairix_abi::Errno;
use tairix_kernel_mem::PAGE_SIZE;
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
}

/// The registry state: the next id to mint, the live regions, and each
/// task's live `(base_va, region_id)` mappings.
struct State {
    next_id: u64,
    regions: BTreeMap<u64, Region>,
    mappings: BTreeMap<u64, Vec<(u64, u64)>>,
}

impl State {
    const fn new() -> Self {
        Self {
            next_id: 1,
            regions: BTreeMap::new(),
            mappings: BTreeMap::new(),
        }
    }
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
    let base_va = match facility.map_region(&chunks) {
        Ok(va) => va,
        Err(err) => {
            facility.free_region(&chunks);
            return Err(err);
        }
    };
    let mut state = REGIONS.lock();
    let id = state.next_id;
    state.next_id = state.next_id.wrapping_add(1);
    state.regions.insert(
        id,
        Region {
            chunks,
            pages,
            refs: 1,
        },
    );
    state
        .mappings
        .entry(owner.0)
        .or_default()
        .push((base_va, id));
    Ok((base_va, id))
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
    // Snapshot the backing under the lock; map outside it.
    let (chunks, pages) = {
        let state = REGIONS.lock();
        let region = state.regions.get(&id).ok_or(Errno::NotFound)?;
        (region.chunks.clone(), region.pages)
    };
    let base_va = facility.map_region(&chunks)?;
    let mut state = REGIONS.lock();
    // The region could have been torn down between the two locks only if its
    // last reference dropped; but `process` holds the grant and no mapping was
    // dropped here, so re-checking keeps the accounting honest fail-closed.
    let Some(region) = state.regions.get_mut(&id) else {
        drop(state);
        let _ = facility.unmap_region(base_va, region_len_bytes(pages));
        return Err(Errno::NotFound);
    };
    region.refs += 1;
    state
        .mappings
        .entry(process.0)
        .or_default()
        .push((base_va, id));
    Ok((base_va, region_len_bytes(pages)))
}

/// Release `process`'s shared mapping based at `base`, tearing down its
/// page-table entries and dropping its reference to the region; the region's
/// frames are zeroed and freed when its last reference is released.
///
/// Reports the byte length released — the registry's own record of the
/// region, which the teardown already reads — so the caller can drop exactly
/// those pages from the process's address-space snapshot.
///
/// # Errors
///
/// [`Errno::NotFound`] if `base` does not name a live shared mapping of
/// `process`.
pub fn unmap(
    facility: &dyn SharedMemFacility,
    process: ProcessId,
    base: u64,
) -> Result<usize, Errno> {
    // Find and remove the mapping record and recover its region's length
    // under the lock; the reference itself is dropped through the shared
    // release step below, outside it.
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
    // Tear down the caller's page-table entries (outside the registry
    // lock), then drop the mapping's reference — freeing the frames if it
    // was the last one.
    let unmap = facility.unmap_region(base, len);
    release_ref(facility, id);
    unmap.map(|()| len)
}

/// Drop one reference to region `id`, freeing its frames if this was the
/// last one. The shared release step behind [`unmap`], [`reclaim_process`],
/// and a [`KernelHold`] drop.
fn release_ref(facility: &dyn SharedMemFacility, id: u64) {
    let free = {
        let mut state = REGIONS.lock();
        let Some(region) = state.regions.get_mut(&id) else {
            return;
        };
        region.refs -= 1;
        let empty = region.refs == 0;
        if empty {
            // The `region` borrow ends above; removing the entry now yields
            // its chunk list to free outside the lock.
            state.regions.remove(&id).map(|region| region.chunks)
        } else {
            None
        }
    };
    if let Some(chunks) = free {
        facility.free_region(&chunks);
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
/// [`Errno::NotImplemented`] when the facility cannot reach the frames
/// (fail closed; the reference is released again).
pub fn kernel_hold(facility: &'static dyn SharedMemFacility, id: u64) -> Result<KernelHold, Errno> {
    let (chunks, pages) = {
        let mut state = REGIONS.lock();
        let region = state.regions.get_mut(&id).ok_or(Errno::NotFound)?;
        region.refs += 1;
        (region.chunks.clone(), region.pages)
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
pub fn reclaim_process(facility: &dyn SharedMemFacility, process: ProcessId) {
    let ids: Vec<u64> = {
        let mut state = REGIONS.lock();
        let Some(list) = state.mappings.remove(&process.0) else {
            return;
        };
        list.into_iter().map(|(_, id)| id).collect()
    };
    for id in ids {
        release_ref(facility, id);
    }
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
        unmaps: Mutex<Vec<(u64, usize)>>,
        frees: Mutex<Vec<(u64, u32, u64)>>,
    }

    impl FakeFacility {
        fn new() -> Self {
            Self {
                // A high, page-aligned base unlikely to collide with anything
                // a test reasons about; each alloc bumps it.
                next_phys: AtomicU64::new(0x1_0000_0000),
                maps: Mutex::new(Vec::new()),
                unmaps: Mutex::new(Vec::new()),
                frees: Mutex::new(Vec::new()),
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
        fn map_region(&self, chunks: &[SharedChunk]) -> Result<u64, Errno> {
            // Record the first chunk's base and the total page count, so a
            // test can assert the mapped extent without knowing the split.
            let total: u64 = chunks.iter().map(|c| c.pages).sum();
            let first = chunks[0].phys_base;
            self.maps.lock().unwrap().push((first, total));
            Ok(Self::va_for(first))
        }
        fn unmap_region(&self, base: u64, len: usize) -> Result<(), Errno> {
            self.unmaps.lock().unwrap().push((base, len));
            Ok(())
        }
        fn free_region(&self, chunks: &[SharedChunk]) {
            for c in chunks {
                self.frees
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
        unmap(&fac, owner, va).expect("unmap");
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
        unmap(&fac, owner, owner_va).expect("owner unmap");
        assert!(
            fac.frees.lock().unwrap().is_empty(),
            "not freed while a grantee still maps the region"
        );
        // The grantee releases last: now the region's frames are scrubbed and
        // freed exactly once.
        unmap(&fac, grantee, grantee_va).expect("grantee unmap");
        assert_eq!(fac.frees.lock().unwrap().len(), 1, "freed at last ref");
        assert_eq!(
            fac.unmaps.lock().unwrap().len(),
            2,
            "both mappings torn down"
        );
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
        reclaim_process(&fac, grantee);
        assert!(fac.frees.lock().unwrap().is_empty());
        // Reclaiming the owner drops the last reference: the region is freed.
        reclaim_process(&fac, owner);
        assert_eq!(fac.frees.lock().unwrap().len(), 1, "freed at last ref");
        // Reclaiming a task with no mappings is a benign no-op (idempotent).
        reclaim_process(&fac, owner);
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
        fn map_region(&self, chunks: &[SharedChunk]) -> Result<u64, Errno> {
            self.inner.map_region(chunks)
        }
        fn unmap_region(&self, base: u64, len: usize) -> Result<(), Errno> {
            self.inner.unmap_region(base, len)
        }
        fn free_region(&self, chunks: &[SharedChunk]) {
            self.inner.free_region(chunks);
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
        reclaim_process(fac, owner);
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
        unmap(fac, owner, va).expect("unmap");
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
        unmap(fac, owner, va).expect("unmap");
        assert_eq!(fac.frees.lock().unwrap().len(), 1);
    }

    #[test]
    fn map_and_unmap_fail_closed_for_unknown_ids() {
        let fac = FakeFacility::new();
        let process = ProcessId(0x5_0006);
        // A region id that was never created.
        assert_eq!(map(&fac, process, 0xDEAD_BEEF), Err(Errno::NotFound));
        // A base VA the process never mapped.
        assert_eq!(unmap(&fac, process, 0x1234), Err(Errno::NotFound));
        // Neither touched the facility's free path.
        assert!(fac.frees.lock().unwrap().is_empty());
    }
}
