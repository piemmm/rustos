//! Demand-paged, read-only file-backed user memory over a *live*
//! [`AddressSpace`] — the mechanism half of the `file_map` syscall.
//!
//! A file mapping reserves user *address space* at map time and backs it one
//! page at a time from the kernel's fault path: [`map_file_page`] turns one
//! faulting page resident (fresh frame, zeroed, filled with the file bytes,
//! mapped [`FILE_FLAGS`] — read-only, never writable or executable), and
//! [`unmap_file_region`] tears a region down **sparsely**, reclaiming only
//! the pages a fault ever made resident (a never-touched hole is legal and
//! costs nothing). Both are architecture-neutral mechanism only: which
//! region a virtual address belongs to, the file it names, the identity the
//! page is read under, and the accounting all belong to the caller
//! (`kernel/core`'s file-mapping region table over
//! [`LiveSpace`](crate::live::LiveSpace)).
//!
//! The sibling of [`crate::anon`]: same frame discipline (zero before the
//! mapping is visible, zero on free), same fail-closed shape, different
//! backing (file bytes instead of zeroes) and different lifetime (sparse,
//! fault-driven) — kept separate so neither path carries the other's rules.

use crate::anon::{page_at, zero_frame, AnonError};
use crate::frame::{Frame, PAGE_SIZE};
use crate::phys::PhysMap;
use crate::ptr::slice_within;
use crate::vmm::{AddressSpace, MapFlags, PageTable, PageTableError};

/// The single permission set file-backed user memory is mapped with:
/// user-accessible and readable — **never** writable (the file is not
/// written through the mapping) and **never** executable (W^X). Writing it
/// once here makes a writable or executable file mapping unrepresentable.
pub const FILE_FLAGS: MapFlags = MapFlags::READ.union(MapFlags::USER);

/// Make the single faulting page at `va` resident: allocate a frame, zero
/// it, copy `contents` to its start (the tail past `contents.len()` stays
/// zero — the end-of-file straddle), and map it [`FILE_FLAGS`] at `va`.
///
/// `contents` carries at most one page of file bytes; a short slice is the
/// page that straddles end-of-file, and an empty slice is rejected (a page
/// wholly past end-of-file is never backed — the caller refuses the fault
/// instead). The frame never becomes user-visible before it carries exactly
/// the bytes the caller supplied, and on any failure it is scrubbed and
/// returned to `free_frame`, leaving `space` unchanged (fail closed).
///
/// # Errors
///
/// * [`AnonError::ZeroLength`] if `contents` is empty.
/// * [`AnonError::Overflow`] if `contents` exceeds one page.
/// * [`AnonError::Unaligned`] if `va` is not page-aligned.
/// * [`AnonError::OutOfMemory`] if `alloc_frame` is exhausted.
/// * [`AnonError::PhysUnmapped`] if the frame cannot be reached to fill it.
/// * [`AnonError::Map`] if the page table refuses the mapping (e.g. the
///   address is already mapped — a fault raced a concurrent resolution).
pub fn map_file_page<P, A, F>(
    space: &mut AddressSpace<P>,
    physmap: &dyn PhysMap,
    va: u64,
    contents: &[u8],
    mut alloc_frame: A,
    mut free_frame: F,
) -> Result<(), AnonError>
where
    P: PageTable,
    A: FnMut() -> Option<Frame>,
    F: FnMut(Frame),
{
    if contents.is_empty() {
        return Err(AnonError::ZeroLength);
    }
    if contents.len() > PAGE_SIZE {
        return Err(AnonError::Overflow);
    }
    let page = page_at(va, 0)?;
    let Some(frame) = alloc_frame() else {
        return Err(AnonError::OutOfMemory);
    };
    // Zero first so the tail past `contents` never carries stale bytes, then
    // fill; the frame is not yet mapped anywhere, so nothing user-visible
    // exists until the map below succeeds.
    if let Err(err) = zero_frame(physmap, frame) {
        free_frame(frame);
        return Err(err);
    }
    if let Err(err) = fill_frame(physmap, frame, contents) {
        // The frame may hold a partial copy of file bytes: scrub before it
        // returns to the allocator, exactly as on the free path.
        let _ = zero_frame(physmap, frame);
        free_frame(frame);
        return Err(err);
    }
    if let Err(err) = space.map(page, frame, FILE_FLAGS) {
        let _ = zero_frame(physmap, frame);
        free_frame(frame);
        return Err(map_errno(err));
    }
    Ok(())
}

/// Sparsely release the `page_count`-page file-mapped region based at
/// `base_va`: every *resident* page is unmapped, its frame zeroed and
/// returned to `free_frame`, and every never-faulted hole is skipped.
/// Returns the number of pages that were resident.
///
/// The all-mapped precondition of [`crate::anon::unmap_anonymous`] is
/// deliberately absent: residency of a demand-paged region is fault
/// history, not caller knowledge. Confirming that `(base_va, page_count)`
/// names a region the caller owns is the caller's job (the region table),
/// done **before** this is called.
///
/// # Errors
///
/// * [`AnonError::ZeroLength`] if `page_count == 0`.
/// * [`AnonError::Unaligned`] if `base_va` is not page-aligned.
/// * [`AnonError::Overflow`] if a page address overflows the address space.
/// * [`AnonError::PhysUnmapped`] if a reclaimed frame cannot be reached to
///   zero it (the frame is still freed; the first such error is reported
///   after the whole region is torn down).
pub fn unmap_file_region<P, F>(
    space: &mut AddressSpace<P>,
    physmap: &dyn PhysMap,
    base_va: u64,
    page_count: u64,
    mut free_frame: F,
) -> Result<u64, AnonError>
where
    P: PageTable,
    F: FnMut(Frame),
{
    if page_count == 0 {
        return Err(AnonError::ZeroLength);
    }
    if !base_va.is_multiple_of(PAGE_SIZE as u64) {
        return Err(AnonError::Unaligned);
    }

    // Validate the extent up front so a range that leaves the address space
    // tears nothing down (fail closed before any state).
    page_at(base_va, page_count - 1)?;

    let mut resident = 0u64;
    let mut first_err = None;
    for page_index in 0..page_count {
        let page = page_at(base_va, page_index)?;
        if space.translate(page).is_none() {
            continue;
        }
        match space.unmap(page) {
            Ok(frame) => {
                resident += 1;
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
        None => Ok(resident),
    }
}

/// Copy `contents` to the start of `frame` through the kernel's direct
/// physical map. The frame was zeroed immediately before, so the tail past
/// `contents.len()` reads as zeroes.
fn fill_frame(physmap: &dyn PhysMap, frame: Frame, contents: &[u8]) -> Result<(), AnonError> {
    let ptr = physmap
        .translate(frame.start(), PAGE_SIZE)
        .ok_or(AnonError::PhysUnmapped)?;
    // SAFETY: `physmap.translate` proved `ptr` is valid for `PAGE_SIZE`
    // bytes inside the kernel's direct map, the frame was just handed out by
    // the allocator and is not yet mapped into any address space, so nothing
    // aliases it, and `slice_within` bounds the window to `contents.len()
    // <= PAGE_SIZE` bytes (checked by the caller).
    let window = unsafe {
        slice_within(ptr.as_ptr(), PAGE_SIZE, 0, contents.len()).ok_or(AnonError::PhysUnmapped)?
    };
    window.copy_from_slice(contents);
    Ok(())
}

/// Map a [`PageTableError`] onto an [`AnonError`], folding an allocator
/// exhaustion onto [`AnonError::OutOfMemory`] so callers see one OOM type
/// (the [`crate::anon`] convention).
fn map_errno(err: PageTableError) -> AnonError {
    match err {
        PageTableError::AllocFailed(_) => AnonError::OutOfMemory,
        other => AnonError::Map(other),
    }
}

#[cfg(test)]
mod tests {
    use super::{map_file_page, unmap_file_region, FILE_FLAGS};
    use crate::anon::AnonError;
    use crate::frame::{Frame, PhysAddr};
    use crate::phys::{PhysMap, SimPhysMap};
    use crate::uaccess::copy_in;
    use crate::vmm::{AddressSpace, HostPageTable, MapFlags, Page, VirtAddr};
    use crate::PAGE_SIZE;

    extern crate std;
    use core::cell::{Cell, RefCell};
    use std::vec;
    use std::vec::Vec;

    // A simulated physical window covering frames 16..48 (128 KiB) — ample
    // for every page a test below makes resident.
    const SIM_BASE_FRAME: usize = 16;
    const SIM_FRAMES: usize = 32;

    fn sim() -> SimPhysMap {
        SimPhysMap::new(
            PhysAddr::new((SIM_BASE_FRAME as u64) * PAGE_SIZE as u64),
            SIM_FRAMES * PAGE_SIZE,
        )
    }

    // A frame vendor over the simulated window (the `crate::anon` test
    // shape): consecutive frames up to `limit`, then `None`, recording every
    // freed frame so a test can assert the reclaim returned them all.
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

    fn resident(space: &AddressSpace<HostPageTable>, addr: u64) -> bool {
        space
            .translate(Page::from_addr(VirtAddr::new(addr)).unwrap())
            .is_some()
    }

    #[test]
    fn file_flags_are_read_user_never_write_or_exec() {
        assert!(FILE_FLAGS.contains(MapFlags::READ));
        assert!(FILE_FLAGS.contains(MapFlags::USER));
        assert!(!FILE_FLAGS.contains(MapFlags::WRITE));
        assert!(!FILE_FLAGS.contains(MapFlags::EXEC));
    }

    #[test]
    fn maps_one_read_only_page_carrying_the_contents() {
        let mut space = host_space();
        let sim = sim();
        let frames = Frames::new(4);
        let contents: Vec<u8> = (0..PAGE_SIZE)
            .map(|i| u8::try_from(i % 251).expect("bounded by the modulus"))
            .collect();
        map_file_page(
            &mut space,
            &sim,
            0x4000,
            &contents,
            || frames.alloc(),
            |f| frames.free(f),
        )
        .expect("map");
        assert_eq!(read_user(&space, &sim, 0x4000, PAGE_SIZE), contents);
        let (_, flags) = space
            .translate(Page::from_addr(VirtAddr::new(0x4000)).unwrap())
            .expect("mapped");
        assert!(flags.contains(MapFlags::READ));
        assert!(!flags.contains(MapFlags::WRITE));
        assert!(!flags.contains(MapFlags::EXEC));
    }

    #[test]
    fn a_short_tail_page_is_zero_filled_past_end_of_file() {
        let mut space = host_space();
        let sim = sim();
        let frames = Frames::new(4);
        map_file_page(
            &mut space,
            &sim,
            0x4000,
            &[0xAB; 7],
            || frames.alloc(),
            |f| frames.free(f),
        )
        .expect("map");
        let bytes = read_user(&space, &sim, 0x4000, PAGE_SIZE);
        assert_eq!(&bytes[..7], &[0xAB; 7]);
        assert!(bytes[7..].iter().all(|&b| b == 0));
    }

    #[test]
    fn empty_and_oversized_contents_are_rejected() {
        let mut space = host_space();
        let sim = sim();
        let frames = Frames::new(4);
        assert_eq!(
            map_file_page(
                &mut space,
                &sim,
                0x4000,
                &[],
                || frames.alloc(),
                |f| { frames.free(f) }
            ),
            Err(AnonError::ZeroLength)
        );
        let too_big = vec![0u8; PAGE_SIZE + 1];
        assert_eq!(
            map_file_page(
                &mut space,
                &sim,
                0x4000,
                &too_big,
                || frames.alloc(),
                |f| frames.free(f)
            ),
            Err(AnonError::Overflow)
        );
        assert!(!resident(&space, 0x4000));
    }

    #[test]
    fn a_misaligned_va_is_rejected_before_any_frame_is_drawn() {
        let mut space = host_space();
        let sim = sim();
        let frames = Frames::new(4);
        assert_eq!(
            map_file_page(
                &mut space,
                &sim,
                0x4001,
                &[1],
                || frames.alloc(),
                |f| { frames.free(f) }
            ),
            Err(AnonError::Unaligned)
        );
        assert_eq!(frames.next.get(), SIM_BASE_FRAME);
    }

    #[test]
    fn frame_exhaustion_fails_closed_as_oom() {
        let mut space = host_space();
        let sim = sim();
        let frames = Frames::new(0);
        assert_eq!(
            map_file_page(
                &mut space,
                &sim,
                0x4000,
                &[1],
                || frames.alloc(),
                |f| { frames.free(f) }
            ),
            Err(AnonError::OutOfMemory)
        );
    }

    #[test]
    fn an_already_mapped_page_is_refused_and_the_frame_reclaimed() {
        let mut space = host_space();
        let sim = sim();
        let frames = Frames::new(4);
        map_file_page(
            &mut space,
            &sim,
            0x4000,
            &[1],
            || frames.alloc(),
            |f| frames.free(f),
        )
        .expect("first map");
        let before = frames.freed_len();
        assert!(matches!(
            map_file_page(
                &mut space,
                &sim,
                0x4000,
                &[2],
                || frames.alloc(),
                |f| { frames.free(f) }
            ),
            Err(AnonError::Map(_))
        ));
        // The losing frame was scrubbed and returned, and the resident page
        // still carries the first mapping's bytes.
        assert_eq!(frames.freed_len(), before + 1);
        assert_eq!(read_user(&space, &sim, 0x4000, 1), vec![1]);
    }

    #[test]
    fn sparse_release_reclaims_only_resident_pages() {
        let mut space = host_space();
        let sim = sim();
        let frames = Frames::new(8);
        // A 5-page region with pages 1 and 3 resident (fault history).
        let base = 0x10000u64;
        for index in [1u64, 3] {
            map_file_page(
                &mut space,
                &sim,
                base + index * PAGE_SIZE as u64,
                &[u8::try_from(index).expect("a small test index")],
                || frames.alloc(),
                |f| frames.free(f),
            )
            .expect("map");
        }
        let resident_count =
            unmap_file_region(&mut space, &sim, base, 5, |f| frames.free(f)).expect("release");
        assert_eq!(resident_count, 2);
        assert_eq!(frames.freed_len(), 2);
        for index in 0..5u64 {
            assert!(!resident(&space, base + index * PAGE_SIZE as u64));
        }
    }

    #[test]
    fn releasing_a_wholly_untouched_region_frees_nothing() {
        let mut space = host_space();
        let sim = sim();
        let frames = Frames::new(4);
        let resident_count =
            unmap_file_region(&mut space, &sim, 0x10000, 8, |f| frames.free(f)).expect("release");
        assert_eq!(resident_count, 0);
        assert_eq!(frames.freed_len(), 0);
    }

    #[test]
    fn release_rejects_zero_length_and_misalignment() {
        let mut space = host_space();
        let sim = sim();
        let frames = Frames::new(4);
        assert_eq!(
            unmap_file_region(&mut space, &sim, 0x10000, 0, |f| frames.free(f)),
            Err(AnonError::ZeroLength)
        );
        assert_eq!(
            unmap_file_region(&mut space, &sim, 0x10001, 1, |f| frames.free(f)),
            Err(AnonError::Unaligned)
        );
    }

    #[test]
    fn released_frames_are_zeroed_before_reuse() {
        let mut space = host_space();
        let sim = sim();
        let frames = Frames::new(4);
        map_file_page(
            &mut space,
            &sim,
            0x4000,
            &[0xEE; 16],
            || frames.alloc(),
            |f| frames.free(f),
        )
        .expect("map");
        unmap_file_region(&mut space, &sim, 0x4000, 1, |f| frames.free(f)).expect("release");
        let frame = frames.freed.borrow()[0];
        let ptr = sim.translate(frame.start(), PAGE_SIZE).expect("in window");
        // SAFETY: the sim window owns the frame's bytes for PAGE_SIZE and
        // nothing else references them after the unmap.
        let bytes = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), PAGE_SIZE) };
        assert!(bytes.iter().all(|&b| b == 0));
    }
}
