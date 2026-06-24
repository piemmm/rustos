//! Map a validated `rxe` load image into an [`AddressSpace`].
//!
//! The load-time policy — W^X segments, mandatory PIE, the CFI
//! type-tag, and the KASLR bias — is enforced by `rustos_abi::rxe` when a
//! [`LoadImage`] is parsed. This module is the kernel-side step that
//! consumes a *already-validated* image and materialises it: every segment
//! page is mapped at its KASLR-relocated address with the page permissions
//! its [`RxePermission`] dictates.
//!
//! W^X holds twice over: the loader never constructs a writable-and-
//! executable [`MapFlags`] (a [`RxePermission`] cannot be both), and the
//! underlying [`PageTable`] backend independently rejects any such combination. Frame allocation is injected as a closure so
//! this module stays free of any particular allocator and is fully
//! host-testable.

use rustos_abi::rxe::{LoadImage, RxeError, RxePermission, RXE_PAGE_SIZE};

use crate::frame::{Frame, PAGE_SIZE};
use crate::vmm::{AddressSpace, MapFlags, Page, PageTable, PageTableError, VirtAddr};

/// The `rxe` page size must match the kernel frame size; the per-page
/// mapping loop below relies on the two being identical.
const _: () = assert!(RXE_PAGE_SIZE == PAGE_SIZE as u64);

/// Why mapping an `rxe` load image failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LoadError {
    /// A relocated address computation overflowed the address space.
    Layout(RxeError),
    /// The page table refused a mapping (e.g. an address already mapped).
    PageTable(PageTableError),
    /// The injected frame allocator ran out of frames.
    OutOfFrames,
}

impl From<RxeError> for LoadError {
    fn from(e: RxeError) -> Self {
        Self::Layout(e)
    }
}

impl From<PageTableError> for LoadError {
    fn from(e: PageTableError) -> Self {
        Self::PageTable(e)
    }
}

/// The [`MapFlags`] a segment with permission `perm` is mapped with.
///
/// Always user-accessible and readable; execute and write are added per
/// the permission. The write-and-execute combination is unrepresentable
/// because [`RxePermission`] cannot be both (the W^X invariant).
#[must_use]
pub fn map_flags_for(perm: RxePermission) -> MapFlags {
    let base = MapFlags::READ | MapFlags::USER;
    match perm {
        RxePermission::ReadOnly => base,
        RxePermission::ReadExecute => base | MapFlags::EXEC,
        RxePermission::ReadWrite => base | MapFlags::WRITE,
    }
}

/// Map every page of every segment of `image` into `space`, relocated by
/// the KASLR `bias`, and return the relocated entry point.
///
/// `alloc_frame` is called once per mapped page; returning `None` aborts
/// the load with [`LoadError::OutOfFrames`]. Each segment is mapped with
/// the [`MapFlags`] from [`map_flags_for`].
///
/// # Errors
///
/// * [`LoadError::Layout`] if a relocated address overflows.
/// * [`LoadError::OutOfFrames`] if `alloc_frame` is exhausted.
/// * [`LoadError::PageTable`] if the underlying table refuses a mapping.
pub fn map_image<P, A>(
    space: &mut AddressSpace<P>,
    image: &LoadImage,
    bias: u64,
    mut alloc_frame: A,
) -> Result<u64, LoadError>
where
    P: PageTable,
    A: FnMut() -> Option<Frame>,
{
    for segment in image.segments() {
        let base = segment.relocated_vaddr(bias)?;
        let flags = map_flags_for(segment.permission);
        map_region(
            space,
            base,
            segment.page_count(),
            flags,
            &mut alloc_frame,
            |_page_index, _frame| Ok::<(), LoadError>(()),
        )?;
    }
    Ok(image.relocated_entry(bias)?)
}

/// Map `page_count` consecutive pages starting at `base_va`, allocating one
/// frame per page through `alloc_frame` and mapping it with `flags`, invoking
/// `per_page(page_index, frame)` immediately after each page is mapped.
///
/// This is the single page-mapping loop shared by [`map_image`] and the
/// process-image builder ([`crate::spawn`]); the per-page hook lets the
/// builder fill each freshly mapped frame with segment content, a zeroed
/// stack, or the startup-vector block without duplicating the mapping
/// arithmetic.
///
/// The error type `E` is generic so callers can thread their own richer
/// error (e.g. [`crate::spawn::SpawnError`]) through the `per_page` hook while
/// the mapping failures surface as [`LoadError`] converted via `E::from`.
///
/// # Errors
///
/// * [`LoadError::Layout`] (converted into `E`) if a page address overflows.
/// * [`LoadError::OutOfFrames`] (converted into `E`) if `alloc_frame` is
///   exhausted.
/// * [`LoadError::PageTable`] (converted into `E`) if the table refuses a
///   mapping.
/// * any error `per_page` returns.
pub(crate) fn map_region<P, A, F, E>(
    space: &mut AddressSpace<P>,
    base_va: u64,
    page_count: u64,
    flags: MapFlags,
    alloc_frame: &mut A,
    mut per_page: F,
) -> Result<(), E>
where
    P: PageTable,
    A: FnMut() -> Option<Frame>,
    F: FnMut(u64, Frame) -> Result<(), E>,
    E: From<LoadError>,
{
    for page_index in 0..page_count {
        let page_offset = page_index
            .checked_mul(RXE_PAGE_SIZE)
            .ok_or(LoadError::Layout(RxeError::AddressOverflow))
            .map_err(E::from)?;
        let vaddr = base_va
            .checked_add(page_offset)
            .ok_or(LoadError::Layout(RxeError::AddressOverflow))
            .map_err(E::from)?;
        let page = Page::from_addr(VirtAddr::new(vaddr))
            .map_err(LoadError::from)
            .map_err(E::from)?;
        let frame = alloc_frame()
            .ok_or(LoadError::OutOfFrames)
            .map_err(E::from)?;
        space
            .map(page, frame, flags)
            .map_err(LoadError::from)
            .map_err(E::from)?;
        per_page(page_index, frame)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{map_flags_for, map_image, LoadError};
    use crate::frame::Frame;
    use crate::vmm::{AddressSpace, HostPageTable, MapFlags, Page, VirtAddr};
    use rustos_abi::rxe::{LoadHeader, RxePermission, Segment, LOAD_FLAG_PIE};
    use rustos_abi::{LoadImage, ABI_VERSION_CURRENT, LOAD_MAGIC, SYSCALL_TABLE_HASH_LEN};

    const TAG: [u8; SYSCALL_TABLE_HASH_LEN] = [0x33; SYSCALL_TABLE_HASH_LEN];

    fn segment(vaddr: u64, pages: u64, perm: RxePermission) -> Segment {
        Segment {
            vaddr,
            file_offset: 0,
            file_size: pages * 0x1000,
            mem_size: pages * 0x1000,
            permission: perm,
        }
    }

    fn image_bytes(entry: u64, segments: &[Segment]) -> alloc::vec::Vec<u8> {
        let header = LoadHeader {
            magic: LOAD_MAGIC,
            abi_version: ABI_VERSION_CURRENT,
            flags: LOAD_FLAG_PIE,
            segment_count: u16::try_from(segments.len()).unwrap(),
            needed_count: 0,
            entry,
            cfi_tag: TAG,
        };
        let mut bytes = alloc::vec::Vec::new();
        bytes.extend_from_slice(&header.to_le_bytes());
        for s in segments {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        bytes
    }

    fn frame_source() -> impl FnMut() -> Option<Frame> {
        let mut next = 0usize;
        move || {
            next += 1;
            Some(Frame(next))
        }
    }

    #[test]
    fn maps_segments_with_relocated_addresses_and_permissions() {
        let segs = [
            segment(0x1000, 1, RxePermission::ReadExecute),
            segment(0x2000, 1, RxePermission::ReadWrite),
        ];
        let bytes = image_bytes(0x1000, &segs);
        let image = LoadImage::parse(&bytes, &TAG).expect("valid image");

        let mut space = AddressSpace::new(HostPageTable::new());
        let bias = 0x10_0000;
        let entry = map_image(&mut space, &image, bias, frame_source()).expect("mapped");

        assert_eq!(entry, 0x1000 + bias);
        assert_eq!(space.mapped_pages(), 2);

        let code = Page::from_addr(VirtAddr::new(0x1000 + bias)).unwrap();
        let (_, code_flags) = space.translate(code).expect("code mapped");
        assert_eq!(code_flags, MapFlags::READ | MapFlags::EXEC | MapFlags::USER);

        let data = Page::from_addr(VirtAddr::new(0x2000 + bias)).unwrap();
        let (_, data_flags) = space.translate(data).expect("data mapped");
        assert_eq!(
            data_flags,
            MapFlags::READ | MapFlags::WRITE | MapFlags::USER
        );
    }

    #[test]
    fn maps_every_page_of_a_multi_page_segment() {
        let segs = [segment(0x1000, 3, RxePermission::ReadExecute)];
        let bytes = image_bytes(0x1000, &segs);
        let image = LoadImage::parse(&bytes, &TAG).expect("valid image");

        let mut space = AddressSpace::new(HostPageTable::new());
        map_image(&mut space, &image, 0, frame_source()).expect("mapped");
        assert_eq!(space.mapped_pages(), 3);
    }

    #[test]
    fn reports_out_of_frames() {
        let segs = [segment(0x1000, 1, RxePermission::ReadExecute)];
        let bytes = image_bytes(0x1000, &segs);
        let image = LoadImage::parse(&bytes, &TAG).expect("valid image");

        let mut space = AddressSpace::new(HostPageTable::new());
        let err = map_image(&mut space, &image, 0, || None).unwrap_err();
        assert_eq!(err, LoadError::OutOfFrames);
    }

    #[test]
    fn map_flags_never_writable_and_executable() {
        for perm in [
            RxePermission::ReadOnly,
            RxePermission::ReadExecute,
            RxePermission::ReadWrite,
        ] {
            let flags = map_flags_for(perm);
            assert!(flags.contains(MapFlags::READ));
            assert!(flags.contains(MapFlags::USER));
            assert!(!(flags.contains(MapFlags::WRITE) && flags.contains(MapFlags::EXEC)));
        }
    }
}
