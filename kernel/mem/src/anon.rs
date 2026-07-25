//! Anonymous user-memory map/unmap over a *live* [`AddressSpace`]
//! (`plans/SPAWN.md` `SP5b`).
//!
//! [`map_anonymous`] grows the caller's own address space with fresh,
//! zeroed `RW` pages; [`unmap_anonymous`] releases them, zeroing the frames
//! it reclaims. Both are the architecture-neutral mechanism only: the
//! capability posture (`mem_map` is unprivileged), the
//! placement of a non-`FIXED` region, and the per-task bookkeeping belong
//! to the higher-level producer that calls them, preserving the
//! layering (this module knows only [`AddressSpace`], a [`PhysMap`], and the
//! injected frame source/sink).
//!
//! # Binding invariants (the SP5 design note, `docs/src/architecture/memory.md` §7c)
//!
//! * **W^X, `RW` only.** A region is mapped
//!   [`READ`](MapFlags::READ) | [`WRITE`](MapFlags::WRITE) |
//!   [`USER`](MapFlags::USER) and **never** executable; [`ANON_FLAGS`] is
//!   the single flag set, so an `RWX` mapping is unrepresentable here.
//! * **Zero on map and on free.** Each frame is zeroed
//!   through the kernel direct map *before* the mapping is visible, and the
//!   frames [`unmap_anonymous`] reclaims are zeroed before they are returned
//!   to the allocator — no stale kernel or other-process bytes are ever
//!   exposed, and freed user secrets do not survive.
//! * **Deterministic OOM, fail-closed reclaim.** A
//!   frame exhaustion part-way through a map unwinds every page already
//!   mapped (unmapping and freeing its frame) before returning
//!   [`AnonError::OutOfMemory`], so a failed map leaves the address space
//!   exactly as it found it. [`unmap_anonymous`] validates the *whole* range
//!   is mapped before tearing any of it down, so a bad range fails closed
//!   with [`AnonError::NotMapped`] without a partial teardown.

use crate::frame::{Frame, PAGE_SIZE};
use crate::phys::PhysMap;
use crate::ptr::slice_within;
use crate::vmm::{AddressSpace, MapFlags, Page, PageTable, PageTableError, VirtAddr};

/// The single permission set anonymous user memory is mapped with:
/// user-accessible, readable, and writable — **never** executable
/// (W^X). Writing it once here makes an `RWX` anonymous
/// mapping unrepresentable.
pub const ANON_FLAGS: MapFlags = MapFlags::READ.union(MapFlags::WRITE).union(MapFlags::USER);

/// Why an anonymous map/unmap failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AnonError {
    /// `page_count` was zero — a region of no pages names nothing.
    ZeroLength,
    /// `base_va` was not page-aligned.
    Unaligned,
    /// A page address computation overflowed the address space.
    Overflow,
    /// No backing frame (or page-table frame) was available
    /// (deterministic OOM).
    OutOfMemory,
    /// A page in the range to unmap had no live mapping (fail closed — the range is not one the caller mapped).
    NotMapped,
    /// A frame is not reachable through the kernel's direct physical map,
    /// so it cannot be zeroed.
    PhysUnmapped,
    /// The page table refused a mapping for a reason other than exhaustion
    /// (e.g. the address is already mapped).
    Map(PageTableError),
}

/// The whole-page count needed to back `len` bytes, rounded up.
///
/// # Errors
///
/// [`AnonError::ZeroLength`] for `len == 0`; [`AnonError::Overflow`] if the
/// rounded-up byte count does not fit a `u64`.
pub fn page_count_for(len: usize) -> Result<u64, AnonError> {
    if len == 0 {
        return Err(AnonError::ZeroLength);
    }
    let len = u64::try_from(len).map_err(|_| AnonError::Overflow)?;
    Ok(len.div_ceil(PAGE_SIZE as u64))
}

/// The page-aligned virtual address of the `page_index`-th page above
/// `base_va`, or [`AnonError::Overflow`] if it would leave the address space.
/// Crate-visible so the demand-paged file-mapping engine
/// ([`crate::filemap`]) walks regions through the same one definition.
pub(crate) fn page_at(base_va: u64, page_index: u64) -> Result<Page, AnonError> {
    let offset = page_index
        .checked_mul(PAGE_SIZE as u64)
        .ok_or(AnonError::Overflow)?;
    let vaddr = base_va.checked_add(offset).ok_or(AnonError::Overflow)?;
    Page::from_addr(VirtAddr::new(vaddr)).map_err(|_| AnonError::Unaligned)
}

/// Zero `frame` through the kernel's direct physical map.
///
/// This is a deliberate kernel-side physical write — the user permission of
/// the page the frame is (or was) mapped at is irrelevant; the frame is
/// scrubbed at its physical address. Used both before a freshly allocated
/// frame becomes user-visible and as the zero-on-free scrub
/// (secret hygiene), mirroring [`crate::spawn`]'s
/// `fill_frame`. Crate-visible so the live-space teardown
/// ([`crate::live::LiveSpace`]) scrubs a dead task's remaining frames
/// through the same one definition.
pub(crate) fn zero_frame(physmap: &dyn PhysMap, frame: Frame) -> Result<(), AnonError> {
    let ptr = physmap
        .translate(frame.start(), PAGE_SIZE)
        .ok_or(AnonError::PhysUnmapped)?;
    // SAFETY: `physmap.translate` proved `ptr` is valid for `PAGE_SIZE` bytes
    // inside the kernel's direct map. On the map path the frame was just
    // handed out by the allocator and is not yet mapped into any address
    // space, so nothing aliases it; on the free path it has already been
    // unmapped from the (single, caller-owned) space, so likewise nothing
    // aliases it. `slice_within` bounds the window to exactly one page.
    let page = unsafe {
        slice_within(ptr.as_ptr(), PAGE_SIZE, 0, PAGE_SIZE).ok_or(AnonError::PhysUnmapped)?
    };
    // Clear through the self-optimising page-zero routine: on a core with a
    // block-zero instruction (`DC ZVA` / ERMS) this is the hardware path,
    // self-verified bit-identical to the byte fill before it could be
    // selected, and the portable byte fill everywhere else.
    tairix_pagezero::zero(page);
    Ok(())
}

/// Tear down the first `mapped` pages of the region based at `base_va`,
/// unmapping each, zeroing its frame, and returning it to `free_frame`.
///
/// Used to unwind a partially built region when a later page fails to map,
/// so a failed [`map_anonymous`] leaves the address space exactly as it found
/// it (fail-closed reclaim). Each page in `0..mapped` was
/// just mapped by this call, so an unmap of it cannot fail; a defensive
/// `Err` is swallowed here because there is no better recovery than freeing
/// every frame we still hold.
fn reclaim<P, F>(
    space: &mut AddressSpace<P>,
    physmap: &dyn PhysMap,
    base_va: u64,
    mapped: u64,
    free_frame: &mut F,
) where
    P: PageTable,
    F: FnMut(Frame),
{
    for page_index in 0..mapped {
        let Ok(page) = page_at(base_va, page_index) else {
            continue;
        };
        if let Ok(frame) = space.unmap(page) {
            let _ = zero_frame(physmap, frame);
            free_frame(frame);
        }
    }
}

/// Map `page_count` fresh, zeroed `RW` user pages at `base_va` into the
/// caller's own live `space` (`plans/SPAWN.md` `SP5b`).
///
/// Each page gets a frame from `alloc_frame`, which is zeroed through
/// `physmap` *before* it is mapped, so no stale bytes are ever user-visible. Pages are mapped [`ANON_FLAGS`] — `RW|USER`, never
/// executable (W^X).
///
/// The map is all-or-nothing: if any page cannot be backed or mapped, every
/// page already mapped by this call is unmapped, its frame zeroed and
/// returned to `free_frame`, and the original error is returned, leaving
/// `space` unchanged.
///
/// # Errors
///
/// * [`AnonError::ZeroLength`] if `page_count == 0`.
/// * [`AnonError::Unaligned`] if `base_va` is not page-aligned.
/// * [`AnonError::Overflow`] if a page address overflows the address space.
/// * [`AnonError::OutOfMemory`] if `alloc_frame` is exhausted.
/// * [`AnonError::PhysUnmapped`] if a frame cannot be reached to zero it.
/// * [`AnonError::Map`] if the page table refuses a mapping (e.g. the
///   address is already mapped).
pub fn map_anonymous<P, A, F>(
    space: &mut AddressSpace<P>,
    physmap: &dyn PhysMap,
    base_va: u64,
    page_count: u64,
    mut alloc_frame: A,
    mut free_frame: F,
) -> Result<(), AnonError>
where
    P: PageTable,
    A: FnMut() -> Option<Frame>,
    F: FnMut(Frame),
{
    if page_count == 0 {
        return Err(AnonError::ZeroLength);
    }
    if base_va % PAGE_SIZE as u64 != 0 {
        return Err(AnonError::Unaligned);
    }

    for page_index in 0..page_count {
        let page = match page_at(base_va, page_index) {
            Ok(page) => page,
            Err(err) => {
                reclaim(space, physmap, base_va, page_index, &mut free_frame);
                return Err(err);
            }
        };
        let Some(frame) = alloc_frame() else {
            reclaim(space, physmap, base_va, page_index, &mut free_frame);
            return Err(AnonError::OutOfMemory);
        };
        // Zero before the mapping is visible; on failure the frame is not
        // yet mapped, so free it directly and unwind the earlier pages.
        if let Err(err) = zero_frame(physmap, frame) {
            free_frame(frame);
            reclaim(space, physmap, base_va, page_index, &mut free_frame);
            return Err(err);
        }
        if let Err(err) = space.map(page, frame, ANON_FLAGS) {
            // The frame never became user-visible; reclaim it and unwind.
            let _ = zero_frame(physmap, frame);
            free_frame(frame);
            reclaim(space, physmap, base_va, page_index, &mut free_frame);
            return Err(map_errno(err));
        }
    }
    Ok(())
}

/// Release the `page_count`-page **demand-paged** region based at `base_va`
/// from the caller's own live `space`, zeroing every resident frame before
/// returning it to `free_frame` (zero on free).
///
/// Anonymous regions are reserved by address space and backed one zeroed
/// page at a time by the fault path, so a region is **sparsely resident**:
/// this walks the range, reclaims every page that is resident, and skips
/// the ones that never faulted in. It does *not* fail on an unbacked page —
/// the caller validates that `(base_va, page_count)` names a region it
/// reserved (the registry's `anon_region_exact` and, for a placed base, the
/// per-task window record), so an unbacked page here is an untouched
/// reservation page, not a bad range.
///
/// # Errors
///
/// * [`AnonError::ZeroLength`] if `page_count == 0`.
/// * [`AnonError::Unaligned`] if `base_va` is not page-aligned.
/// * [`AnonError::Overflow`] if a page address overflows the address space.
/// * [`AnonError::PhysUnmapped`] if a reclaimed frame cannot be reached to
///   zero it (the frame is still freed; the error is reported).
pub fn unmap_anonymous<P, F>(
    space: &mut AddressSpace<P>,
    physmap: &dyn PhysMap,
    base_va: u64,
    page_count: u64,
    mut free_frame: F,
) -> Result<(), AnonError>
where
    P: PageTable,
    F: FnMut(Frame),
{
    if page_count == 0 {
        return Err(AnonError::ZeroLength);
    }
    if base_va % PAGE_SIZE as u64 != 0 {
        return Err(AnonError::Unaligned);
    }

    // A demand-paged region is sparsely resident: only the pages that
    // actually faulted in hold a frame. Tear down every page that *is*
    // resident (zeroing its reclaimed frame — secret hygiene) and skip the
    // ones that never faulted; the caller has already validated that
    // `(base_va, page_count)` names a region it reserved, so an unbacked
    // page here is an untouched reservation page, not an error. A
    // `PhysUnmapped` scrub failure is recorded but never leaks a frame (it
    // is freed regardless).
    let mut first_err = None;
    for page_index in 0..page_count {
        let page = page_at(base_va, page_index)?;
        if space.translate(page).is_none() {
            // Never faulted in — nothing to reclaim for this page.
            continue;
        }
        match space.unmap(page) {
            Ok(frame) => {
                if let Err(err) = zero_frame(physmap, frame) {
                    first_err.get_or_insert(err);
                }
                free_frame(frame);
            }
            Err(err) => {
                first_err.get_or_insert(map_errno(err));
            }
        }
    }
    match first_err {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Map a [`PageTableError`] onto an [`AnonError`], folding an allocator
/// exhaustion onto [`AnonError::OutOfMemory`] so callers see one OOM type.
fn map_errno(err: PageTableError) -> AnonError {
    match err {
        PageTableError::AllocFailed(_) => AnonError::OutOfMemory,
        other => AnonError::Map(other),
    }
}

#[cfg(test)]
mod tests {
    use super::{map_anonymous, page_count_for, unmap_anonymous, AnonError, ANON_FLAGS, PAGE_SIZE};
    use crate::frame::{Frame, PhysAddr};
    use crate::phys::{PhysMap, SimPhysMap};
    use crate::uaccess::copy_in;
    use crate::vmm::{AddressSpace, HostPageTable, MapFlags, Page, VirtAddr};

    extern crate std;
    use core::cell::{Cell, RefCell};
    use std::vec;
    use std::vec::Vec;

    // A simulated physical window covering frames 16..80 (256 KiB) — ample
    // for every region a test below maps.
    const SIM_BASE_FRAME: usize = 16;
    const SIM_FRAMES: usize = 64;

    fn sim() -> SimPhysMap {
        SimPhysMap::new(
            PhysAddr::new((SIM_BASE_FRAME as u64) * PAGE_SIZE as u64),
            SIM_FRAMES * PAGE_SIZE,
        )
    }

    /// A frame vendor over the simulated window: hands out consecutive frames
    /// up to `limit`, then `None`, and records every freed frame so a test
    /// can assert the fail-closed reclaim returned them all. Interior
    /// mutability lets the same vendor back both the `alloc` and `free`
    /// closures `map_anonymous` borrows simultaneously.
    struct Frames {
        next: Cell<usize>,
        limit: usize,
        freed: RefCell<Vec<Frame>>,
    }

    impl Frames {
        fn new(limit: usize) -> Self {
            Self {
                next: Cell::new(SIM_BASE_FRAME),
                limit: SIM_BASE_FRAME + limit,
                freed: RefCell::new(Vec::new()),
            }
        }

        fn alloc(&self) -> Option<Frame> {
            let n = self.next.get();
            if n >= self.limit {
                return None;
            }
            self.next.set(n + 1);
            Some(Frame(n))
        }

        fn free(&self, f: Frame) {
            self.freed.borrow_mut().push(f);
        }

        fn freed_len(&self) -> usize {
            self.freed.borrow().len()
        }
    }

    fn host_space() -> AddressSpace<HostPageTable> {
        AddressSpace::new(HostPageTable::new())
    }

    // Read `len` bytes of user memory at `addr` back through the uaccess
    // boundary (the anonymous pages are READ|USER, so this succeeds).
    fn read_user(
        space: &AddressSpace<HostPageTable>,
        sim: &SimPhysMap,
        addr: u64,
        len: usize,
    ) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        copy_in(space, sim, VirtAddr::new(addr), &mut buf).expect("readable user range");
        buf
    }

    #[test]
    fn page_count_rounds_up_and_rejects_zero() {
        assert_eq!(page_count_for(1), Ok(1));
        assert_eq!(page_count_for(PAGE_SIZE), Ok(1));
        assert_eq!(page_count_for(PAGE_SIZE + 1), Ok(2));
        assert_eq!(page_count_for(0), Err(AnonError::ZeroLength));
    }

    #[test]
    fn anon_flags_are_rw_user_never_exec() {
        assert!(ANON_FLAGS.contains(MapFlags::READ));
        assert!(ANON_FLAGS.contains(MapFlags::WRITE));
        assert!(ANON_FLAGS.contains(MapFlags::USER));
        assert!(!ANON_FLAGS.contains(MapFlags::EXEC));
    }

    #[test]
    fn maps_zeroed_rw_user_pages() {
        let mut space = host_space();
        let sim = sim();
        let frames = Frames::new(8);
        let base = 0x4000;
        map_anonymous(
            &mut space,
            &sim,
            base,
            3,
            || frames.alloc(),
            |f| frames.free(f),
        )
        .expect("map");

        assert_eq!(space.mapped_pages(), 3);
        for i in 0..3u64 {
            let page = Page::from_addr(VirtAddr::new(base + i * PAGE_SIZE as u64)).unwrap();
            let (_, flags) = space.translate(page).expect("mapped");
            assert_eq!(flags, ANON_FLAGS);
        }
        // The whole region reads back as zero.
        let bytes = read_user(&space, &sim, base, 3 * PAGE_SIZE);
        assert!(bytes.iter().all(|&b| b == 0));
    }

    #[test]
    fn unmap_zeroes_and_frees_every_frame() {
        let mut space = host_space();
        let sim = sim();
        let frames = Frames::new(8);
        let base = 0x8000;
        map_anonymous(
            &mut space,
            &sim,
            base,
            2,
            || frames.alloc(),
            |f| frames.free(f),
        )
        .expect("map");

        // Dirty the region through the kernel direct map so we can prove the
        // zero-on-free scrub actually happened.
        let dirty = [0xABu8; PAGE_SIZE];
        for i in 0..2u64 {
            let phys = PhysAddr::new((SIM_BASE_FRAME as u64 + i) * PAGE_SIZE as u64);
            let ptr = sim.translate(phys, PAGE_SIZE).unwrap();
            // SAFETY: the sim window backs `phys` for one page; the test owns it.
            unsafe { core::ptr::copy_nonoverlapping(dirty.as_ptr(), ptr.as_ptr(), PAGE_SIZE) };
        }

        unmap_anonymous(&mut space, &sim, base, 2, |f| frames.free(f)).expect("unmap");

        assert_eq!(space.mapped_pages(), 0);
        assert_eq!(frames.freed_len(), 2);
        // Each reclaimed frame was zeroed on free.
        for i in 0..2u64 {
            let phys = PhysAddr::new((SIM_BASE_FRAME as u64 + i) * PAGE_SIZE as u64);
            let ptr = sim.translate(phys, PAGE_SIZE).unwrap();
            // SAFETY: as above; reading one page the test owns.
            let slice = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), PAGE_SIZE) };
            assert!(slice.iter().all(|&b| b == 0));
        }
    }

    #[test]
    fn out_of_memory_unwinds_to_leave_space_unchanged() {
        let mut space = host_space();
        let sim = sim();
        // Only two frames available, but ask for three pages.
        let frames = Frames::new(2);
        let base = 0x1_0000;
        let err = map_anonymous(
            &mut space,
            &sim,
            base,
            3,
            || frames.alloc(),
            |f| frames.free(f),
        )
        .unwrap_err();

        assert_eq!(err, AnonError::OutOfMemory);
        // Nothing left mapped, and both handed-out frames were reclaimed.
        assert_eq!(space.mapped_pages(), 0);
        assert_eq!(frames.freed_len(), 2);
    }

    #[test]
    fn already_mapped_region_unwinds() {
        let mut space = host_space();
        let sim = sim();
        let frames = Frames::new(8);
        let base = 0x2_0000;
        map_anonymous(
            &mut space,
            &sim,
            base,
            2,
            || frames.alloc(),
            |f| frames.free(f),
        )
        .expect("first map");
        let freed_before = frames.freed_len();

        // A second map that overlaps the first page must fail and unwind any
        // page it managed to add, leaving the first region intact.
        let err = map_anonymous(
            &mut space,
            &sim,
            base,
            2,
            || frames.alloc(),
            |f| frames.free(f),
        )
        .unwrap_err();

        assert!(matches!(err, AnonError::Map(_)));
        // The original two pages are still mapped; the failed call left none.
        assert_eq!(space.mapped_pages(), 2);
        assert_eq!(frames.freed_len(), freed_before + 1);
    }

    #[test]
    fn unmap_tolerates_sparsely_resident_pages() {
        let mut space = host_space();
        let sim = sim();
        let frames = Frames::new(8);
        let base = 0x3_0000;
        // A demand-paged region: only the first page ever faulted in, so
        // only it is resident. Releasing the whole two-page reservation must
        // reclaim the resident page and skip the never-touched one, not fail.
        map_anonymous(
            &mut space,
            &sim,
            base,
            1,
            || frames.alloc(),
            |f| frames.free(f),
        )
        .expect("map");

        unmap_anonymous(&mut space, &sim, base, 2, |f| frames.free(f)).expect("sparse unmap");
        // The one resident page was torn down and its frame reclaimed; the
        // unbacked page was skipped without error.
        assert_eq!(space.mapped_pages(), 0);
        assert_eq!(frames.freed_len(), 1);
    }

    #[test]
    fn rejects_zero_length_and_misaligned_base() {
        let mut space = host_space();
        let sim = sim();
        let frames = Frames::new(8);
        assert_eq!(
            map_anonymous(&mut space, &sim, 0x4000, 0, || frames.alloc(), |_| {}),
            Err(AnonError::ZeroLength)
        );
        assert_eq!(
            map_anonymous(&mut space, &sim, 0x4001, 1, || frames.alloc(), |_| {}),
            Err(AnonError::Unaligned)
        );
        assert_eq!(
            unmap_anonymous(&mut space, &sim, 0x4001, 1, |_| {}),
            Err(AnonError::Unaligned)
        );
    }
}
