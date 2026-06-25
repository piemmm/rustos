//! The kernel cross-process shared-memory region registry (`plans/USB.md`).
//!
//! A shared-memory region is a block of kernel-owned RAM two cooperating
//! processes map to exchange bulk data without a kernel copy (the USB
//! request-block data buffer). This registry owns the *policy* over those
//! regions: the kernel-allocated unforgeable region id, the backing block's
//! physical base and buddy order, the reference count across the owner and
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

use rustos_abi::Errno;
use rustos_kernel_mem::PAGE_SIZE;
use rustos_kernel_sec::TaskId;
use rustos_sync::SpinLock;

use crate::devres::SharedMemFacility;

/// One live shared-memory region.
struct Region {
    /// Physical base of the contiguous backing block.
    phys_base: u64,
    /// Buddy order of the backing block (handed back to free it).
    order: u32,
    /// Region size in whole pages.
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
    owner: TaskId,
    pages: u64,
) -> Result<(u64, u64), Errno> {
    let (phys_base, order) = facility.alloc_region(pages)?;
    let base_va = match facility.map_region(phys_base, pages) {
        Ok(va) => va,
        Err(err) => {
            facility.free_region(phys_base, order, pages);
            return Err(err);
        }
    };
    let mut state = REGIONS.lock();
    let id = state.next_id;
    state.next_id = state.next_id.wrapping_add(1);
    state.regions.insert(
        id,
        Region {
            phys_base,
            order,
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

/// Map an existing region `id` into `task`'s own live address space,
/// returning its base user virtual address.
///
/// # Errors
///
/// [`Errno::NotFound`] if the region was torn down, or the facility error.
pub fn map(facility: &dyn SharedMemFacility, task: TaskId, id: u64) -> Result<u64, Errno> {
    // Snapshot the backing under the lock; map outside it.
    let (phys_base, pages) = {
        let state = REGIONS.lock();
        let region = state.regions.get(&id).ok_or(Errno::NotFound)?;
        (region.phys_base, region.pages)
    };
    let base_va = facility.map_region(phys_base, pages)?;
    let mut state = REGIONS.lock();
    // The region could have been torn down between the two locks only if its
    // last reference dropped; but `task` holds the grant and no mapping was
    // dropped here, so re-checking keeps the accounting honest fail-closed.
    let Some(region) = state.regions.get_mut(&id) else {
        drop(state);
        let _ = facility.unmap_region(base_va, region_len_bytes(pages));
        return Err(Errno::NotFound);
    };
    region.refs += 1;
    state
        .mappings
        .entry(task.0)
        .or_default()
        .push((base_va, id));
    Ok(base_va)
}

/// Release `task`'s shared mapping based at `base`, tearing down its
/// page-table entries and dropping its reference to the region; the region's
/// frames are zeroed and freed when its last reference is released.
///
/// # Errors
///
/// [`Errno::NotFound`] if `base` does not name a live shared mapping of
/// `task`.
pub fn unmap(facility: &dyn SharedMemFacility, task: TaskId, base: u64) -> Result<(), Errno> {
    // Find and remove the mapping record, recover its region, and decide
    // whether this was the last reference - all under the lock.
    let (id, len, free) = {
        let mut state = REGIONS.lock();
        let list = state.mappings.get_mut(&task.0).ok_or(Errno::NotFound)?;
        let pos = list
            .iter()
            .position(|&(b, _)| b == base)
            .ok_or(Errno::NotFound)?;
        let (_, id) = list.remove(pos);
        if list.is_empty() {
            state.mappings.remove(&task.0);
        }
        let region = state.regions.get_mut(&id).ok_or(Errno::NotFound)?;
        let len = region_len_bytes(region.pages);
        region.refs -= 1;
        let free = if region.refs == 0 {
            let Region {
                phys_base,
                order,
                pages,
                ..
            } = *region;
            state.regions.remove(&id);
            Some((phys_base, order, pages))
        } else {
            None
        };
        (id, len, free)
    };
    let _ = id;
    // Tear down the caller's page-table entries (outside the registry lock).
    let unmap = facility.unmap_region(base, len);
    if let Some((phys_base, order, pages)) = free {
        facility.free_region(phys_base, order, pages);
    }
    unmap
}

/// Reclaim every shared mapping `task` held when it exits or is torn down,
/// dropping each reference and freeing any region whose last reference this
/// releases. Does **not** tear down page-table entries (the task's address
/// space is being destroyed). Idempotent.
pub fn reclaim_task(facility: &dyn SharedMemFacility, task: TaskId) {
    let frees: Vec<(u64, u32, u64)> = {
        let mut state = REGIONS.lock();
        let Some(list) = state.mappings.remove(&task.0) else {
            return;
        };
        let mut frees = Vec::new();
        for (_, id) in list {
            if let Some(region) = state.regions.get_mut(&id) {
                region.refs -= 1;
                if region.refs == 0 {
                    let Region {
                        phys_base,
                        order,
                        pages,
                        ..
                    } = *region;
                    state.regions.remove(&id);
                    frees.push((phys_base, order, pages));
                }
            }
        }
        frees
    };
    for (phys_base, order, pages) in frees {
        facility.free_region(phys_base, order, pages);
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
        fn alloc_region(&self, _pages: u64) -> Result<(u64, u32), Errno> {
            let phys = self.next_phys.fetch_add(0x10_0000, Ordering::Relaxed);
            Ok((phys, 0))
        }
        fn map_region(&self, phys_base: u64, pages: u64) -> Result<u64, Errno> {
            self.maps.lock().unwrap().push((phys_base, pages));
            Ok(Self::va_for(phys_base))
        }
        fn unmap_region(&self, base: u64, len: usize) -> Result<(), Errno> {
            self.unmaps.lock().unwrap().push((base, len));
            Ok(())
        }
        fn free_region(&self, phys_base: u64, order: u32, pages: u64) {
            self.frees.lock().unwrap().push((phys_base, order, pages));
        }
    }

    #[test]
    fn create_maps_and_grants_an_owner_reference() {
        let fac = FakeFacility::new();
        let owner = TaskId(0x5_0001);
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
        let owner = TaskId(0x5_0002);
        let grantee = TaskId(0x5_0003);
        let (owner_va, id) = create(&fac, owner, 1).expect("create");
        // A grantee maps the same region: refs = 2.
        let grantee_va = map(&fac, grantee, id).expect("grantee maps");
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
    fn reclaim_task_drops_references_and_frees_at_zero() {
        let fac = FakeFacility::new();
        let owner = TaskId(0x5_0004);
        let grantee = TaskId(0x5_0005);
        let (_owner_va, id) = create(&fac, owner, 1).expect("create");
        let _grantee_va = map(&fac, grantee, id).expect("grantee maps");

        // Reclaiming the grantee (e.g. a class driver unloaded on hot-removal)
        // drops its reference but does not free the region — the owner still
        // holds it.
        reclaim_task(&fac, grantee);
        assert!(fac.frees.lock().unwrap().is_empty());
        // Reclaiming the owner drops the last reference: the region is freed.
        reclaim_task(&fac, owner);
        assert_eq!(fac.frees.lock().unwrap().len(), 1, "freed at last ref");
        // Reclaiming a task with no mappings is a benign no-op (idempotent).
        reclaim_task(&fac, owner);
        assert_eq!(fac.frees.lock().unwrap().len(), 1);
    }

    #[test]
    fn map_and_unmap_fail_closed_for_unknown_ids() {
        let fac = FakeFacility::new();
        let task = TaskId(0x5_0006);
        // A region id that was never created.
        assert_eq!(map(&fac, task, 0xDEAD_BEEF), Err(Errno::NotFound));
        // A base VA the task never mapped.
        assert_eq!(unmap(&fac, task, 0x1234), Err(Errno::NotFound));
        // Neither touched the facility's free path.
        assert!(fac.frees.lock().unwrap().is_empty());
    }
}
