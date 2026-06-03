//! Bounded copies between the kernel and a task's user address space.
//!
//! A syscall handler is handed a raw user pointer (`ptr`, `len`) and must
//! move bytes to or from that buffer without ever trusting it. This
//! module is the architecture-neutral half of the kernel's
//! `copy_from_user` / `copy_to_user` boundary (`AGENTS.md` §5.4 /
//! `tests/SECURITY.md` §5): it walks the caller's [`AddressSpace`] one
//! page at a time, proves each page is a legitimate *user* page with the
//! permission the direction requires, turns the backing frame into a
//! CPU-dereferenceable pointer through the kernel's [`PhysMap`], and
//! moves only the in-page byte span. Any failure stops the copy
//! fail-closed with a [`UaccessError`]; no partial result is exposed to
//! the caller, and a malformed pointer never causes the kernel to touch
//! memory the user does not own.
//!
//! # Why a page walk
//!
//! User memory is contiguous in the *virtual* address space but its
//! frames need not be contiguous in physical RAM. The copy therefore
//! visits each `[page_start, page_start + PAGE_SIZE)` window the range
//! touches, [`translate`](AddressSpace::translate)s it to its
//! `(Frame, MapFlags)`, and copies the slice of the user buffer that
//! falls inside that one page. The first page may begin at a non-zero
//! offset and the last may end before the page boundary.
//!
//! # Permission model
//!
//! Every page in range must carry [`MapFlags::USER`] — a user task can
//! never name a kernel-only page — and the permission the direction
//! needs: [`MapFlags::READ`] to copy *from* the user
//! ([`copy_in`]) and [`MapFlags::WRITE`] to copy *to* it
//! ([`copy_out`]). A page missing `USER` is rejected before a missing
//! data permission, so a kernel-pointer-confusion attempt is never
//! reported as a mere "not readable". This upholds the §19.2 W^X model:
//! `copy_out` refuses an executable-but-not-writable page rather than
//! letting the kernel scribble over code.
//!
//! # Host-testability
//!
//! With `kernel/mem`'s `HostPageTable` and `SimPhysMap` test doubles (both
//! gated behind `#[cfg(any(test, feature = "host-tests"))]`) the entire
//! facility runs on a developer workstation: the test maps user pages to
//! frames inside a simulated physical window, seeds or observes the bytes
//! through the very same [`PhysMap`] the copy uses, and asserts the
//! cross-page, offset, and every fail-closed path (`AGENTS.md` §7).

use crate::frame::{PhysAddr, PAGE_SIZE};
use crate::phys::PhysMap;
use crate::vmm::{AddressSpace, MapFlags, Page, PageTableOps, VirtAddr};

/// Why a user-memory copy refused to proceed.
///
/// Each variant names one fail-closed reason (`AGENTS.md` §5.4). The
/// copy stops at the first failure having moved no observable bytes into
/// the caller's destination on the [`copy_in`] path; on the
/// [`copy_out`] path it may have written the pages it had already
/// validated, but it never writes past the first rejected page. Callers
/// map the variant onto their public `Errno`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum UaccessError {
    /// The user base pointer is null and the copy is non-empty.
    Null,
    /// `ptr + len` does not fit in the address space, or the length
    /// exceeds what a single span can describe (CWE-190).
    LengthOverflow,
    /// A page in the range has no mapping in the address space.
    NotMapped,
    /// A page in the range is mapped but is not user-accessible — the
    /// classic kernel-pointer-confusion vector (`AGENTS.md` §5).
    NotUser,
    /// `copy_in` was asked to read a user page that is not readable.
    NotReadable,
    /// `copy_out` was asked to write a user page that is not writable
    /// (e.g. read-only or executable — the §19.2 W^X guard).
    NotWritable,
    /// The backing frame is outside the kernel's direct physical map, so
    /// the kernel cannot reach its bytes.
    PhysUnmapped,
}

/// Copy `dst.len()` bytes *from* the user buffer at `uaddr` in `space`
/// into the kernel slice `dst`.
///
/// Every page the user range touches must be mapped
/// [`USER`](MapFlags::USER) and [`READ`](MapFlags::READ)able. On success
/// `dst` holds the user bytes; on failure `dst` is left in an unspecified
/// state and the caller discards it (no partial user data is treated as
/// valid).
///
/// # Errors
///
/// A [`UaccessError`] naming the first invariant the range breaks.
pub fn copy_in<P: PageTableOps>(
    space: &AddressSpace<P>,
    physmap: &dyn PhysMap,
    uaddr: VirtAddr,
    dst: &mut [u8],
) -> Result<(), UaccessError> {
    let dst_base = dst.as_mut_ptr();
    walk(
        space,
        physmap,
        uaddr,
        dst.len(),
        MapFlags::READ,
        UaccessError::NotReadable,
        |user_ptr, buf_off, span| {
            // SAFETY: `walk` proves `user_ptr` is valid for `span`
            // readable bytes (the page is mapped USER|READ and the
            // `PhysMap` covers `[phys, phys + span)`). `buf_off + span`
            // is within `dst` because the per-span lengths sum to
            // `dst.len()` and each `buf_off` is the running prefix. The
            // kernel `dst` allocation and the user frame are distinct
            // regions, but `copy` (memmove semantics) is sound even if
            // they were not.
            unsafe {
                let into = dst_base.add(buf_off);
                core::ptr::copy(user_ptr.cast_const(), into, span);
            }
        },
    )
}

/// Copy `src.len()` bytes *to* the user buffer at `uaddr` in `space` from
/// the kernel slice `src`.
///
/// Every page the user range touches must be mapped
/// [`USER`](MapFlags::USER) and [`WRITE`](MapFlags::WRITE)able; a
/// read-only or executable page is refused (`AGENTS.md` §19.2 W^X).
///
/// # Errors
///
/// A [`UaccessError`] naming the first invariant the range breaks.
pub fn copy_out<P: PageTableOps>(
    space: &AddressSpace<P>,
    physmap: &dyn PhysMap,
    uaddr: VirtAddr,
    src: &[u8],
) -> Result<(), UaccessError> {
    let src_base = src.as_ptr();
    walk(
        space,
        physmap,
        uaddr,
        src.len(),
        MapFlags::WRITE,
        UaccessError::NotWritable,
        |user_ptr, buf_off, span| {
            // SAFETY: `walk` proves `user_ptr` is valid for `span`
            // writable bytes (the page is mapped USER|WRITE and the
            // `PhysMap` covers `[phys, phys + span)`). `buf_off + span`
            // is within `src` because the per-span lengths sum to
            // `src.len()`. `copy` tolerates overlap; the regions are in
            // practice distinct frames.
            unsafe {
                let from = src_base.add(buf_off);
                core::ptr::copy(from, user_ptr, span);
            }
        },
    )
}

/// Walk `[uaddr, uaddr + len)` one page at a time, validating each page
/// and invoking `per_span` with `(user_ptr, buf_offset, span_len)` for
/// the byte run that falls inside that page.
///
/// `required` is the data permission the direction needs on top of
/// [`MapFlags::USER`]; `missing_perm` is the error returned when a page
/// is user-accessible but lacks it. The closure performs the actual
/// byte move; `walk` owns every bounds and permission check so the two
/// public entry points share exactly one validated traversal
/// (`AGENTS.md` §2.2).
fn walk<P, F>(
    space: &AddressSpace<P>,
    physmap: &dyn PhysMap,
    uaddr: VirtAddr,
    len: usize,
    required: MapFlags,
    missing_perm: UaccessError,
    mut per_span: F,
) -> Result<(), UaccessError>
where
    P: PageTableOps,
    F: FnMut(*mut u8, usize, usize),
{
    if len == 0 {
        return Ok(());
    }
    let base = uaddr.as_u64();
    if base == 0 {
        return Err(UaccessError::Null);
    }
    let len_u64 = u64::try_from(len).map_err(|_| UaccessError::LengthOverflow)?;
    let end = base
        .checked_add(len_u64)
        .ok_or(UaccessError::LengthOverflow)?;
    let page_size = PAGE_SIZE as u64;
    let page_mask = page_size - 1;

    let mut addr = base;
    let mut buf_off: usize = 0;
    while addr < end {
        let page_start = addr & !page_mask;
        // `page_start` is page-aligned by construction, so `Page::from_addr`
        // cannot report `Misaligned`; treat any error as "no mapping" to
        // stay total and fail-closed (`AGENTS.md` §2.9).
        let page =
            Page::from_addr(VirtAddr::new(page_start)).map_err(|_| UaccessError::NotMapped)?;
        let (frame, flags) = space.translate(page).ok_or(UaccessError::NotMapped)?;
        if !flags.contains(MapFlags::USER) {
            return Err(UaccessError::NotUser);
        }
        if !flags.contains(required) {
            return Err(missing_perm);
        }

        let offset_in_page = addr - page_start;
        let page_remaining = page_size - offset_in_page;
        let span_u64 = core::cmp::min(page_remaining, end - addr);
        let span = usize::try_from(span_u64).map_err(|_| UaccessError::LengthOverflow)?;

        let phys = frame
            .start()
            .as_u64()
            .checked_add(offset_in_page)
            .ok_or(UaccessError::PhysUnmapped)?;
        let ptr = physmap
            .translate(PhysAddr::new(phys), span)
            .ok_or(UaccessError::PhysUnmapped)?;

        per_span(ptr.as_ptr(), buf_off, span);

        buf_off += span;
        addr += span_u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Frame;
    use crate::phys::SimPhysMap;
    use crate::vmm::{AddressSpace, HostPageTable};

    extern crate std;
    use std::vec;
    use std::vec::Vec;

    // A simulated physical window covering frames 16..48 (128 KiB).
    const SIM_BASE_FRAME: usize = 16;
    const SIM_FRAMES: usize = 32;

    fn sim() -> SimPhysMap {
        SimPhysMap::new(
            PhysAddr::new((SIM_BASE_FRAME as u64) * PAGE_SIZE as u64),
            SIM_FRAMES * PAGE_SIZE,
        )
    }

    fn space_with(mappings: &[(u64, Frame, MapFlags)]) -> AddressSpace<HostPageTable> {
        let mut space = AddressSpace::new(HostPageTable::new());
        for &(vaddr, frame, flags) in mappings {
            let page = Page::from_addr(VirtAddr::new(vaddr)).expect("aligned test vaddr");
            space.map(page, frame, flags).expect("test mapping");
        }
        space
    }

    // Write `bytes` into the simulated physical RAM backing `frame` at
    // `offset_in_page`, so a `copy_in` reads them as user data.
    fn seed_frame(sim: &SimPhysMap, frame: Frame, offset_in_page: usize, bytes: &[u8]) {
        let phys = PhysAddr::new(frame.start().as_u64() + offset_in_page as u64);
        let ptr = sim.translate(phys, bytes.len()).expect("seed in window");
        // SAFETY: the window owns these bytes for the simulator's
        // lifetime and nothing else aliases them during the test.
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.as_ptr(), bytes.len());
        }
    }

    // Read `len` bytes of simulated physical RAM backing `frame`, so a
    // test can observe what `copy_out` wrote.
    fn read_frame(sim: &SimPhysMap, frame: Frame, offset_in_page: usize, len: usize) -> Vec<u8> {
        let phys = PhysAddr::new(frame.start().as_u64() + offset_in_page as u64);
        let ptr = sim.translate(phys, len).expect("read in window");
        let mut out = vec![0u8; len];
        // SAFETY: as above; the read stays inside the simulated frame.
        unsafe {
            core::ptr::copy_nonoverlapping(ptr.as_ptr(), out.as_mut_ptr(), len);
        }
        out
    }

    #[test]
    fn copy_in_reads_a_single_page() {
        let sim = sim();
        let frame = Frame(SIM_BASE_FRAME);
        let space = space_with(&[(0x4000, frame, MapFlags::READ | MapFlags::USER)]);
        let payload = [1u8, 2, 3, 4, 5, 6, 7, 8];
        seed_frame(&sim, frame, 0, &payload);

        let mut dst = [0u8; 8];
        copy_in(&space, &sim, VirtAddr::new(0x4000), &mut dst).expect("copy_in");
        assert_eq!(dst, payload);
    }

    #[test]
    fn copy_in_honours_offset_within_page() {
        let sim = sim();
        let frame = Frame(SIM_BASE_FRAME);
        let space = space_with(&[(0x4000, frame, MapFlags::READ | MapFlags::USER)]);
        seed_frame(&sim, frame, 0x100, &[0xAA, 0xBB, 0xCC]);

        let mut dst = [0u8; 3];
        copy_in(&space, &sim, VirtAddr::new(0x4000 + 0x100), &mut dst).expect("copy_in");
        assert_eq!(dst, [0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn copy_in_spans_two_pages() {
        let sim = sim();
        let f0 = Frame(SIM_BASE_FRAME);
        let f1 = Frame(SIM_BASE_FRAME + 1);
        let space = space_with(&[
            (0x4000, f0, MapFlags::READ | MapFlags::USER),
            (0x5000, f1, MapFlags::READ | MapFlags::USER),
        ]);
        // Last 4 bytes of page 0, first 4 of page 1.
        seed_frame(&sim, f0, PAGE_SIZE - 4, &[10, 11, 12, 13]);
        seed_frame(&sim, f1, 0, &[20, 21, 22, 23]);

        let mut dst = [0u8; 8];
        copy_in(
            &space,
            &sim,
            VirtAddr::new(0x4000 + PAGE_SIZE as u64 - 4),
            &mut dst,
        )
        .expect("copy_in");
        assert_eq!(dst, [10, 11, 12, 13, 20, 21, 22, 23]);
    }

    #[test]
    fn copy_out_writes_a_single_page() {
        let sim = sim();
        let frame = Frame(SIM_BASE_FRAME);
        let space = space_with(&[(0x4000, frame, MapFlags::WRITE | MapFlags::USER)]);

        let src = [9u8, 8, 7, 6];
        copy_out(&space, &sim, VirtAddr::new(0x4000), &src).expect("copy_out");
        assert_eq!(read_frame(&sim, frame, 0, 4), src);
    }

    #[test]
    fn copy_out_spans_two_pages() {
        let sim = sim();
        let f0 = Frame(SIM_BASE_FRAME);
        let f1 = Frame(SIM_BASE_FRAME + 1);
        let space = space_with(&[
            (0x4000, f0, MapFlags::WRITE | MapFlags::USER),
            (0x5000, f1, MapFlags::WRITE | MapFlags::USER),
        ]);
        let src = [1u8, 2, 3, 4, 5, 6];
        copy_out(
            &space,
            &sim,
            VirtAddr::new(0x4000 + PAGE_SIZE as u64 - 3),
            &src,
        )
        .expect("copy_out");
        assert_eq!(read_frame(&sim, f0, PAGE_SIZE - 3, 3), [1, 2, 3]);
        assert_eq!(read_frame(&sim, f1, 0, 3), [4, 5, 6]);
    }

    #[test]
    fn round_trip_out_then_in() {
        let sim = sim();
        let frame = Frame(SIM_BASE_FRAME);
        let space = space_with(&[(
            0x9000,
            frame,
            MapFlags::READ | MapFlags::WRITE | MapFlags::USER,
        )]);
        let src: Vec<u8> = (0u8..200).collect();
        copy_out(&space, &sim, VirtAddr::new(0x9000), &src).expect("copy_out");
        let mut dst = vec![0u8; src.len()];
        copy_in(&space, &sim, VirtAddr::new(0x9000), &mut dst).expect("copy_in");
        assert_eq!(dst, src);
    }

    #[test]
    fn zero_length_copy_touches_nothing_even_for_null() {
        let space = space_with(&[]);
        let sim = sim();
        let mut empty: [u8; 0] = [];
        copy_in(&space, &sim, VirtAddr::new(0), &mut empty).expect("empty copy_in");
        copy_out(&space, &sim, VirtAddr::new(0), &[]).expect("empty copy_out");
    }

    #[test]
    fn null_base_with_length_is_rejected() {
        let space = space_with(&[]);
        let sim = sim();
        let mut dst = [0u8; 4];
        assert_eq!(
            copy_in(&space, &sim, VirtAddr::new(0), &mut dst),
            Err(UaccessError::Null)
        );
    }

    #[test]
    fn unmapped_page_is_rejected() {
        let space = space_with(&[]);
        let sim = sim();
        let mut dst = [0u8; 4];
        assert_eq!(
            copy_in(&space, &sim, VirtAddr::new(0x4000), &mut dst),
            Err(UaccessError::NotMapped)
        );
    }

    #[test]
    fn kernel_only_page_is_rejected_before_data_permission() {
        let sim = sim();
        let frame = Frame(SIM_BASE_FRAME);
        // Readable but NOT user-accessible.
        let space = space_with(&[(0x4000, frame, MapFlags::READ)]);
        let mut dst = [0u8; 4];
        assert_eq!(
            copy_in(&space, &sim, VirtAddr::new(0x4000), &mut dst),
            Err(UaccessError::NotUser)
        );
    }

    #[test]
    fn copy_in_rejects_non_readable_user_page() {
        let sim = sim();
        let frame = Frame(SIM_BASE_FRAME);
        // User + write-only (no READ).
        let space = space_with(&[(0x4000, frame, MapFlags::WRITE | MapFlags::USER)]);
        let mut dst = [0u8; 4];
        assert_eq!(
            copy_in(&space, &sim, VirtAddr::new(0x4000), &mut dst),
            Err(UaccessError::NotReadable)
        );
    }

    #[test]
    fn copy_out_rejects_non_writable_user_page() {
        let sim = sim();
        let frame = Frame(SIM_BASE_FRAME);
        // User + read/execute (the W^X code case): not writable.
        let space = space_with(&[(
            0x4000,
            frame,
            MapFlags::READ | MapFlags::EXEC | MapFlags::USER,
        )]);
        assert_eq!(
            copy_out(&space, &sim, VirtAddr::new(0x4000), &[1, 2, 3]),
            Err(UaccessError::NotWritable)
        );
    }

    #[test]
    fn second_page_unmapped_is_rejected() {
        let sim = sim();
        let f0 = Frame(SIM_BASE_FRAME);
        // Only the first page is mapped; the span crosses into an
        // unmapped second page.
        let space = space_with(&[(0x4000, f0, MapFlags::READ | MapFlags::USER)]);
        let mut dst = [0u8; 8];
        assert_eq!(
            copy_in(
                &space,
                &sim,
                VirtAddr::new(0x4000 + PAGE_SIZE as u64 - 4),
                &mut dst,
            ),
            Err(UaccessError::NotMapped)
        );
    }

    #[test]
    fn length_that_overflows_address_space_is_rejected() {
        let sim = sim();
        let frame = Frame(SIM_BASE_FRAME);
        let space = space_with(&[(0x4000, frame, MapFlags::READ | MapFlags::USER)]);
        // A slice this long cannot exist on host, so synthesise the
        // overflow through the internal walk with a fake length: use the
        // public API with a base near u64::MAX is impossible to map, so
        // assert the overflow guard via a high base instead.
        let mut dst = [0u8; 4];
        // base + len wraps: pick a base whose page is unmapped anyway, so
        // we exercise the `checked_add` guard rather than a mapping.
        let res = copy_in(&space, &sim, VirtAddr::new(u64::MAX - 1), &mut dst);
        assert_eq!(res, Err(UaccessError::LengthOverflow));
    }

    #[test]
    fn frame_outside_direct_map_is_rejected() {
        // A frame whose physical address is below the simulated window:
        // the `PhysMap` cannot reach it.
        let sim = sim();
        let frame = Frame(0); // physical 0, outside [16*PAGE, 48*PAGE)
        let space = space_with(&[(0x4000, frame, MapFlags::READ | MapFlags::USER)]);
        let mut dst = [0u8; 4];
        assert_eq!(
            copy_in(&space, &sim, VirtAddr::new(0x4000), &mut dst),
            Err(UaccessError::PhysUnmapped)
        );
    }
}
