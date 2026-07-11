//! Build a freshly spawned process's user address space from a validated
//! `rxe` load image.
//!
//! [`crate::loader::map_image`] maps a validated [`LoadImage`]'s segment pages
//! but copies no bytes; this module is the kernel-side step that materialises
//! a *runnable* process: it
//!
//! 1. maps every segment page (R/RX/RW + USER) **and** fills it with the
//!    segment's file content, zeroing the BSS tail beyond `file_size`;
//! 2. allocates and maps a zeroed user stack (U|R|W);
//! 3. serialises the [`rustos_abi::process`] startup-vector block (arguments,
//!    environment, and the stack-canary seed) and writes it into the
//!    new address space (U|R|W).
//!
//! The result is a [`ProcessImage`] — the entry point, the initial user stack
//! pointer, and the user address of the startup block — i.e. exactly the
//! register state an Arch HAL "enter U-mode/EL0" primitive consumes.
//!
//! # W^X and content copy
//!
//! A read-execute code page must hold its bytes before it is ever run, yet it
//! must never be user-writable. The fill therefore writes
//! through the kernel's [`PhysMap`] directly to the freshly allocated frame —
//! a *kernel-side* physical write that does not depend on the page's user
//! permission — rather than through [`crate::uaccess::copy_out`] (which, by
//! contract, refuses a non-writable user page). The page is still mapped
//! R/RX/RW in user space, never RWX.
//!
//! # Capability checks and audit
//!
//! This module is the architecture-neutral *memory mechanism* only. The
//! capability gate that authorises a spawn and the `lib/log` audit record for
//! it belong to the higher-level spawn path (the spawn syscall / loader
//! service) that calls this builder; keeping them there preserves the
//! layering (`kernel/mem` does not depend on `lib/log` or the security
//! policy). Every entry here is fail-closed: a malformed input yields a
//! [`SpawnError`] and the partially built address space is discarded by the
//! caller.
//!
//! # Host-testability
//!
//! With `HostPageTable` + `SimPhysMap` the whole builder runs on a developer
//! workstation: the test backs frames with a simulated physical window, runs
//! the builder, then reads the user pages back through [`crate::uaccess`] and
//! re-parses the startup block.

use rustos_abi::process;
use rustos_abi::rxe::{LoadImage, RxeError};
use rustos_abi::Errno;

use crate::frame::{Frame, PAGE_SIZE};
use crate::loader::{map_flags_for, map_region, LoadError};
use crate::phys::PhysMap;
use crate::ptr::slice_within;
use crate::vmm::{AddressSpace, MapFlags, PageTable};

/// Where, and how large, a spawned process's initial user stack is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserStack {
    /// Page-aligned user virtual address of the lowest stack byte.
    pub base: u64,
    /// Number of 4 KiB pages; must be at least one.
    pub page_count: u64,
}

/// Where one spawned image's user stack and startup-vector block live,
/// derived from the image by [`derive_user_layout`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserLayout {
    /// Page-aligned user virtual address of the lowest stack page.
    pub stack_base: u64,
    /// Page-aligned user virtual address of the startup-vector block.
    pub block_base: u64,
}

/// Derive the stack and startup-vector-block placement for one spawned
/// image: the stack's lowest page sits one unmapped guard page above the
/// image's highest mapped page, and the block one unmapped guard page
/// above the stack top, so an overrun off the image or the stack faults
/// on the gap instead of silently corrupting its neighbour.
///
/// The placement scales with the image instead of capping it at a fixed
/// slot — a fixed stack offset silently collides with any program whose
/// mapped image outgrows it, refusing the spawn the moment a binary
/// crosses the boundary (the capacity-ceiling defect this derivation
/// replaces).
///
/// `ceiling` is the first user address the layout must not reach (the
/// caller's lowest reserved window above the image region). The block's
/// ABI-bounded worst case ([`process::PROCESS_START_MAX_TOTAL_LEN`]) is
/// reserved below it, so no argument vector can collide with the window.
///
/// Fails closed with `None` — the caller refuses the spawn — when
/// `stack_pages` is zero, the layout cannot fit below `ceiling`, or an
/// address computation overflows.
#[must_use]
pub fn derive_user_layout(
    image: &LoadImage,
    bias: u64,
    stack_pages: u64,
    ceiling: u64,
) -> Option<UserLayout> {
    let page = PAGE_SIZE as u64;
    if stack_pages == 0 {
        return None;
    }
    // Highest relocated one-past-the-end address across the image's mapped
    // pages. Segment virtual addresses are page-aligned and `page_count`
    // rounds `mem_size` up, so the top stays page-aligned.
    let mut image_top = bias;
    for segment in image.segments() {
        let base = segment.relocated_vaddr(bias).ok()?;
        let bytes = segment.page_count().checked_mul(page)?;
        image_top = image_top.max(base.checked_add(bytes)?);
    }
    let stack_base = image_top.checked_add(page)?;
    let stack_top = stack_base.checked_add(stack_pages.checked_mul(page)?)?;
    let block_base = stack_top.checked_add(page)?;
    let block_reserve = process::PROCESS_START_MAX_TOTAL_LEN.checked_next_multiple_of(page)?;
    let layout_end = block_base.checked_add(block_reserve)?;
    (layout_end <= ceiling).then_some(UserLayout {
        stack_base,
        block_base,
    })
}

/// The register state a freshly built process image is entered with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessImage {
    /// Relocated entry-point virtual address.
    pub entry: u64,
    /// Exclusive top of the user stack — the initial stack pointer.
    pub stack_top: u64,
    /// User virtual address of the startup-vector block (the first argument
    /// register the entry trampoline receives).
    pub start_block: u64,
}

/// Why building a process image failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SpawnError {
    /// A page mapping step failed (see [`LoadError`]).
    Load(LoadError),
    /// A relocated address or size computation overflowed.
    Layout(RxeError),
    /// A segment's `[file_offset, file_offset + file_size)` range lies
    /// outside the supplied image bytes.
    SegmentContentOutOfRange,
    /// A region frame is not reachable through the kernel's direct map.
    PhysUnmapped,
    /// The user stack was requested with zero pages.
    EmptyStack,
    /// The stack base or startup-block base was not page-aligned.
    Misaligned,
    /// Serialising the startup-vector block failed against the frozen
    /// `abi-v1` limits (see [`rustos_abi::process::write_into`]).
    StartBlock(Errno),
}

impl From<LoadError> for SpawnError {
    fn from(e: LoadError) -> Self {
        Self::Load(e)
    }
}

impl From<RxeError> for SpawnError {
    fn from(e: RxeError) -> Self {
        Self::Layout(e)
    }
}

/// Convert a `u64` byte count to `usize`, mapping an overflow on a narrow
/// host word to a layout overflow (fail closed, never panic).
fn usize_or_overflow(value: u64) -> Result<usize, SpawnError> {
    usize::try_from(value).map_err(|_| SpawnError::Layout(RxeError::AddressOverflow))
}

/// The slice of `content` that belongs at the start of the region page whose
/// first byte is `start` bytes into the region; empty once `start` is past
/// the content (those pages are pure zero-fill — BSS or stack).
fn page_chunk(content: &[u8], start: usize) -> &[u8] {
    if start >= content.len() {
        return &[];
    }
    let end = start.saturating_add(PAGE_SIZE).min(content.len());
    &content[start..end]
}

/// Zero the freshly allocated `frame` and copy `content` (at most one page)
/// to the start of it, writing through the kernel's direct physical map.
///
/// This is a deliberate kernel-side physical write: it bypasses the
/// user-permission check of [`crate::uaccess::copy_out`] because a
/// read-execute code page must be initialised before it runs (see the
/// module-level "W^X and content copy" note).
fn fill_frame(physmap: &dyn PhysMap, frame: Frame, content: &[u8]) -> Result<(), SpawnError> {
    if content.len() > PAGE_SIZE {
        return Err(SpawnError::SegmentContentOutOfRange);
    }
    let ptr = physmap
        .translate(frame.start(), PAGE_SIZE)
        .ok_or(SpawnError::PhysUnmapped)?;
    // SAFETY: `physmap.translate` proved `ptr` is valid for `PAGE_SIZE` bytes
    // inside the kernel's direct map. The frame was just handed out by the
    // allocator and mapped into the new (not-yet-running) address space, so
    // nothing else aliases it; the page is fully initialised (zeroed, then the
    // content copied) before anything reads it. `slice_within` bounds the
    // window to exactly one page.
    let page = unsafe {
        slice_within(ptr.as_ptr(), PAGE_SIZE, 0, PAGE_SIZE).ok_or(SpawnError::PhysUnmapped)?
    };
    page.fill(0);
    page[..content.len()].copy_from_slice(content);
    Ok(())
}

/// Map and fill the pages of one region: `page_count` pages from `base_va`,
/// each mapped with `flags` and filled with the corresponding window of
/// `content` (zero-filled past the content's end).
fn map_and_fill<P, A>(
    space: &mut AddressSpace<P>,
    physmap: &dyn PhysMap,
    base_va: u64,
    page_count: u64,
    flags: MapFlags,
    content: &[u8],
    alloc_frame: &mut A,
) -> Result<(), SpawnError>
where
    P: PageTable,
    A: FnMut() -> Option<Frame>,
{
    let page = PAGE_SIZE as u64;
    map_region(
        space,
        base_va,
        page_count,
        flags,
        alloc_frame,
        |page_index, frame| {
            let start = usize_or_overflow(
                page_index
                    .checked_mul(page)
                    .ok_or(SpawnError::Layout(RxeError::AddressOverflow))?,
            )?;
            fill_frame(physmap, frame, page_chunk(content, start))
        },
    )
}

/// Build a runnable user address space for `image` in `space`.
///
/// Maps and fills every segment of `image` (relocated by `bias`), maps a
/// zeroed user stack described by `stack`, and writes the
/// [`rustos_abi::process`] startup-vector block for `args` / `env` (carrying
/// the `canary` seed) at `start_block_base`. `image_bytes` is the whole `rxe`
/// file the segments' `file_offset`s index into; `physmap` is the kernel's
/// direct physical map (so the builder can reach freshly allocated frames);
/// `alloc_frame` yields one frame per mapped page.
///
/// On success the returned [`ProcessImage`] carries the relocated entry point,
/// the initial user stack pointer, and the user address of the startup block.
///
/// # Errors
///
/// * [`SpawnError::EmptyStack`] if `stack.page_count == 0`.
/// * [`SpawnError::Misaligned`] if `stack.base` or `start_block_base` is not
///   page-aligned.
/// * [`SpawnError::SegmentContentOutOfRange`] if a segment's file range falls
///   outside `image_bytes`.
/// * [`SpawnError::StartBlock`] if the startup block exceeds the frozen
///   `abi-v1` limits.
/// * [`SpawnError::Load`] / [`SpawnError::Layout`] / [`SpawnError::PhysUnmapped`]
///   from the mapping and fill steps.
#[allow(clippy::too_many_arguments)]
pub fn build_process_image<P, A>(
    space: &mut AddressSpace<P>,
    physmap: &dyn PhysMap,
    image: &LoadImage,
    image_bytes: &[u8],
    bias: u64,
    stack: &UserStack,
    start_block_base: u64,
    args: &[&[u8]],
    env: &[&[u8]],
    canary: u64,
    mut alloc_frame: A,
) -> Result<ProcessImage, SpawnError>
where
    P: PageTable,
    A: FnMut() -> Option<Frame>,
{
    let page = PAGE_SIZE as u64;
    if stack.page_count == 0 {
        return Err(SpawnError::EmptyStack);
    }
    if stack.base % page != 0 || start_block_base % page != 0 {
        return Err(SpawnError::Misaligned);
    }

    // 1. Segments: map each page and fill it with file content (BSS zeroed).
    for segment in image.segments() {
        let seg_base = segment.relocated_vaddr(bias)?;
        let flags = map_flags_for(segment.permission);
        let file_off = usize_or_overflow(segment.file_offset)?;
        let file_size = usize_or_overflow(segment.file_size)?;
        let content_end = file_off
            .checked_add(file_size)
            .ok_or(SpawnError::SegmentContentOutOfRange)?;
        let content = image_bytes
            .get(file_off..content_end)
            .ok_or(SpawnError::SegmentContentOutOfRange)?;
        map_and_fill(
            space,
            physmap,
            seg_base,
            segment.page_count(),
            flags,
            content,
            &mut alloc_frame,
        )?;
    }

    // 2. User stack: zeroed U|R|W pages.
    let stack_flags = MapFlags::READ | MapFlags::WRITE | MapFlags::USER;
    map_and_fill(
        space,
        physmap,
        stack.base,
        stack.page_count,
        stack_flags,
        &[],
        &mut alloc_frame,
    )?;
    let stack_bytes = stack
        .page_count
        .checked_mul(page)
        .ok_or(SpawnError::Layout(RxeError::AddressOverflow))?;
    let stack_top = stack
        .base
        .checked_add(stack_bytes)
        .ok_or(SpawnError::Layout(RxeError::AddressOverflow))?;

    // 3. Startup-vector block: serialise then map+write into U|R|W pages.
    let block_len = process::encoded_len(args, env).map_err(SpawnError::StartBlock)?;
    let mut block = alloc::vec![0u8; block_len];
    process::write_into(&mut block, args, env, canary).map_err(SpawnError::StartBlock)?;
    let block_pages = (block_len as u64).div_ceil(page);
    let block_flags = MapFlags::READ | MapFlags::WRITE | MapFlags::USER;
    map_and_fill(
        space,
        physmap,
        start_block_base,
        block_pages,
        block_flags,
        &block,
        &mut alloc_frame,
    )?;

    let entry = image.relocated_entry(bias)?;
    Ok(ProcessImage {
        entry,
        stack_top,
        start_block: start_block_base,
    })
}

#[cfg(test)]
mod tests {
    use super::{build_process_image, derive_user_layout, ProcessImage, SpawnError, UserStack};
    use crate::frame::{Frame, PhysAddr, PAGE_SIZE};
    use crate::loader::LoadError;
    use crate::phys::SimPhysMap;
    use crate::uaccess::copy_in;
    use crate::vmm::{AddressSpace, HostPageTable, VirtAddr};

    use rustos_abi::process;
    use rustos_abi::rxe::{LoadHeader, RxePermission, Segment, LOAD_FLAG_PIE};
    use rustos_abi::{
        LoadImage, ProcessStart, ABI_VERSION_CURRENT, LOAD_MAGIC, SYSCALL_TABLE_HASH_LEN,
    };

    extern crate std;
    use std::vec;
    use std::vec::Vec;

    const TAG: [u8; SYSCALL_TABLE_HASH_LEN] = [0x33; SYSCALL_TABLE_HASH_LEN];

    // A simulated physical window covering frames 16..80 (256 KiB) — ample
    // for the segments, stack, and startup block any test below builds.
    const SIM_BASE_FRAME: usize = 16;
    const SIM_FRAMES: usize = 64;

    fn sim() -> SimPhysMap {
        SimPhysMap::new(
            PhysAddr::new((SIM_BASE_FRAME as u64) * PAGE_SIZE as u64),
            SIM_FRAMES * PAGE_SIZE,
        )
    }

    // Hand out consecutive frames inside the simulated window.
    fn frame_source() -> impl FnMut() -> Option<Frame> {
        let mut next = SIM_BASE_FRAME;
        move || {
            let f = Frame(next);
            next += 1;
            Some(f)
        }
    }

    struct SegSpec {
        vaddr: u64,
        file_size: u64,
        mem_size: u64,
        perm: RxePermission,
        content: Vec<u8>,
    }

    // Build a whole `rxe` file: header + segment table + each segment's file
    // content placed contiguously after the table.
    fn image_bytes(entry: u64, specs: &[SegSpec]) -> (Vec<u8>, Vec<u8>) {
        let table = LoadHeader::WIRE_LEN + specs.len() * Segment::WIRE_LEN;
        let header = LoadHeader {
            magic: LOAD_MAGIC,
            abi_version: ABI_VERSION_CURRENT,
            flags: LOAD_FLAG_PIE,
            segment_count: u16::try_from(specs.len()).unwrap(),
            needed_count: 0,
            entry,
            cfi_tag: TAG,
        };

        let mut content_blob = Vec::new();
        let mut segments = Vec::new();
        let mut file_offset = table as u64;
        for s in specs {
            segments.push(Segment {
                vaddr: s.vaddr,
                file_offset,
                file_size: s.file_size,
                mem_size: s.mem_size,
                permission: s.perm,
            });
            content_blob.extend_from_slice(&s.content);
            file_offset += s.file_size;
        }

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&header.to_le_bytes());
        for seg in &segments {
            bytes.extend_from_slice(&seg.to_le_bytes());
        }
        bytes.extend_from_slice(&content_blob);
        (bytes, content_blob)
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

    fn code_data_image() -> (
        Vec<u8>,
        ProcessImage,
        AddressSpace<HostPageTable>,
        SimPhysMap,
    ) {
        let code: Vec<u8> = (0u8..16).collect();
        let data: Vec<u8> = vec![0xAA; 8];
        let specs = [
            SegSpec {
                vaddr: 0x1000,
                file_size: code.len() as u64,
                mem_size: 0x1000,
                perm: RxePermission::ReadExecute,
                content: code.clone(),
            },
            SegSpec {
                vaddr: 0x2000,
                file_size: data.len() as u64,
                // Two pages: the second page is pure BSS (zero-fill).
                mem_size: 0x2000,
                perm: RxePermission::ReadWrite,
                content: data.clone(),
            },
        ];
        let (bytes, _) = image_bytes(0x1000, &specs);
        let image = LoadImage::parse(&bytes, &TAG).expect("valid image");

        let mut space = AddressSpace::new(HostPageTable::new());
        let sim = sim();
        let stack = UserStack {
            base: 0x10_0000,
            page_count: 2,
        };
        let img = build_process_image(
            &mut space,
            &sim,
            &image,
            &bytes,
            0,
            &stack,
            0x20_0000,
            &[b"prog", b"--x"],
            &[b"K=v"],
            0x0123_4567_89AB_CDEF,
            frame_source(),
        )
        .expect("build");
        (bytes, img, space, sim)
    }

    #[test]
    fn reports_entry_stack_top_and_block_address() {
        let (_bytes, img, _space, _sim) = code_data_image();
        assert_eq!(img.entry, 0x1000);
        assert_eq!(img.stack_top, 0x10_0000 + 2 * PAGE_SIZE as u64);
        assert_eq!(img.start_block, 0x20_0000);
    }

    #[test]
    fn fills_code_segment_with_content_then_zero() {
        let (_bytes, _img, space, sim) = code_data_image();
        let page = read_user(&space, &sim, 0x1000, PAGE_SIZE);
        let expected: Vec<u8> = (0u8..16).collect();
        assert_eq!(&page[..16], &expected[..]);
        assert!(page[16..].iter().all(|&b| b == 0));
    }

    #[test]
    fn zero_fills_bss_beyond_file_size_and_following_pages() {
        let (_bytes, _img, space, sim) = code_data_image();
        // First data page: 8 bytes of content, rest zero.
        let first = read_user(&space, &sim, 0x2000, PAGE_SIZE);
        assert_eq!(&first[..8], &[0xAA; 8]);
        assert!(first[8..].iter().all(|&b| b == 0));
        // Second data page is pure BSS.
        let second = read_user(&space, &sim, 0x3000, PAGE_SIZE);
        assert!(second.iter().all(|&b| b == 0));
    }

    #[test]
    fn user_stack_is_zeroed() {
        let (_bytes, _img, space, sim) = code_data_image();
        let stack = read_user(&space, &sim, 0x10_0000, 2 * PAGE_SIZE);
        assert!(stack.iter().all(|&b| b == 0));
    }

    #[test]
    fn startup_block_parses_back_to_args_env_and_canary() {
        let (_bytes, img, space, sim) = code_data_image();
        let len = process::encoded_len(&[b"prog", b"--x"], &[b"K=v"]).unwrap();
        let block = read_user(&space, &sim, img.start_block, len);
        let view = ProcessStart::parse(&block).expect("valid block");
        assert_eq!(view.arg_count(), 2);
        assert_eq!(view.env_count(), 1);
        assert_eq!(view.arg(0), Some(&b"prog"[..]));
        assert_eq!(view.arg(1), Some(&b"--x"[..]));
        assert_eq!(view.env(0), Some(&b"K=v"[..]));
        assert_eq!(view.canary(), 0x0123_4567_89AB_CDEF);
    }

    #[test]
    fn relocates_segments_entry_and_keeps_block_at_its_base() {
        let code: Vec<u8> = vec![0x90; 4];
        let specs = [SegSpec {
            vaddr: 0x1000,
            file_size: 4,
            mem_size: 0x1000,
            perm: RxePermission::ReadExecute,
            content: code.clone(),
        }];
        let (bytes, _) = image_bytes(0x1000, &specs);
        let image = LoadImage::parse(&bytes, &TAG).expect("valid");
        let mut space = AddressSpace::new(HostPageTable::new());
        let sim = sim();
        let bias = 0x40_0000;
        let stack = UserStack {
            base: 0x80_0000,
            page_count: 1,
        };
        let img = build_process_image(
            &mut space,
            &sim,
            &image,
            &bytes,
            bias,
            &stack,
            0x90_0000,
            &[],
            &[],
            7,
            frame_source(),
        )
        .expect("build");
        assert_eq!(img.entry, 0x1000 + bias);
        let page = read_user(&space, &sim, 0x1000 + bias, PAGE_SIZE);
        assert_eq!(&page[..4], &[0x90; 4]);
    }

    #[test]
    fn rejects_empty_stack() {
        let code: Vec<u8> = vec![0u8; 1];
        let specs = [SegSpec {
            vaddr: 0x1000,
            file_size: 1,
            mem_size: 0x1000,
            perm: RxePermission::ReadExecute,
            content: code,
        }];
        let (bytes, _) = image_bytes(0x1000, &specs);
        let image = LoadImage::parse(&bytes, &TAG).unwrap();
        let mut space = AddressSpace::new(HostPageTable::new());
        let sim = sim();
        let err = build_process_image(
            &mut space,
            &sim,
            &image,
            &bytes,
            0,
            &UserStack {
                base: 0x10_0000,
                page_count: 0,
            },
            0x20_0000,
            &[],
            &[],
            0,
            frame_source(),
        )
        .unwrap_err();
        assert_eq!(err, SpawnError::EmptyStack);
    }

    #[test]
    fn rejects_misaligned_block_base() {
        let code: Vec<u8> = vec![0u8; 1];
        let specs = [SegSpec {
            vaddr: 0x1000,
            file_size: 1,
            mem_size: 0x1000,
            perm: RxePermission::ReadExecute,
            content: code,
        }];
        let (bytes, _) = image_bytes(0x1000, &specs);
        let image = LoadImage::parse(&bytes, &TAG).unwrap();
        let mut space = AddressSpace::new(HostPageTable::new());
        let sim = sim();
        let err = build_process_image(
            &mut space,
            &sim,
            &image,
            &bytes,
            0,
            &UserStack {
                base: 0x10_0000,
                page_count: 1,
            },
            0x20_0001,
            &[],
            &[],
            0,
            frame_source(),
        )
        .unwrap_err();
        assert_eq!(err, SpawnError::Misaligned);
    }

    #[test]
    fn rejects_segment_content_outside_image() {
        // Hand-build an image whose segment file range runs past the bytes.
        let specs = [SegSpec {
            vaddr: 0x1000,
            file_size: 16,
            mem_size: 0x1000,
            perm: RxePermission::ReadExecute,
            content: vec![0u8; 16],
        }];
        let (mut bytes, _) = image_bytes(0x1000, &specs);
        let image = LoadImage::parse(&bytes, &TAG).unwrap();
        // Truncate the content so the declared file range no longer fits.
        bytes.truncate(bytes.len() - 4);
        let mut space = AddressSpace::new(HostPageTable::new());
        let sim = sim();
        let err = build_process_image(
            &mut space,
            &sim,
            &image,
            &bytes,
            0,
            &UserStack {
                base: 0x10_0000,
                page_count: 1,
            },
            0x20_0000,
            &[],
            &[],
            0,
            frame_source(),
        )
        .unwrap_err();
        assert_eq!(err, SpawnError::SegmentContentOutOfRange);
    }

    #[test]
    fn propagates_out_of_frames() {
        let code: Vec<u8> = vec![0u8; 1];
        let specs = [SegSpec {
            vaddr: 0x1000,
            file_size: 1,
            mem_size: 0x1000,
            perm: RxePermission::ReadExecute,
            content: code,
        }];
        let (bytes, _) = image_bytes(0x1000, &specs);
        let image = LoadImage::parse(&bytes, &TAG).unwrap();
        let mut space = AddressSpace::new(HostPageTable::new());
        let sim = sim();
        let err = build_process_image(
            &mut space,
            &sim,
            &image,
            &bytes,
            0,
            &UserStack {
                base: 0x10_0000,
                page_count: 1,
            },
            0x20_0000,
            &[],
            &[],
            0,
            || None,
        )
        .unwrap_err();
        assert_eq!(err, SpawnError::Load(LoadError::OutOfFrames));
    }

    // ---- derive_user_layout ------------------------------------------

    const LAYOUT_BIAS: u64 = 64 << 30;
    const LAYOUT_STACK_PAGES: u64 = 288;
    // The lowest reserved window above the image region (1 GiB up, the
    // production device-window offset).
    const LAYOUT_CEILING: u64 = LAYOUT_BIAS + 0x4000_0000;
    const PAGE: u64 = PAGE_SIZE as u64;

    /// A zero-content image whose segments are `(vaddr, pages)`; the first
    /// segment is executable so it can carry the entry point.
    fn layout_bytes(segs: &[(u64, u64)]) -> Vec<u8> {
        let specs: Vec<SegSpec> = segs
            .iter()
            .enumerate()
            .map(|(i, &(vaddr, pages))| SegSpec {
                vaddr,
                file_size: 0,
                mem_size: pages * PAGE,
                perm: if i == 0 {
                    RxePermission::ReadExecute
                } else {
                    RxePermission::ReadOnly
                },
                content: Vec::new(),
            })
            .collect();
        image_bytes(segs[0].0, &specs).0
    }

    #[test]
    fn layout_places_stack_and_block_above_the_image_with_guard_gaps() {
        // The regression shape: a BSS page at 1 MiB above the bias,
        // exactly where a fixed 1 MiB stack offset used to place the
        // stack's first page and refuse the spawn on the collision.
        let bytes = layout_bytes(&[(0x1_0000, 2), (0x10_0000, 1)]);
        let image = LoadImage::parse(&bytes, &TAG).unwrap();

        let layout = derive_user_layout(&image, LAYOUT_BIAS, LAYOUT_STACK_PAGES, LAYOUT_CEILING)
            .expect("layout fits");

        let image_top = LAYOUT_BIAS + 0x10_0000 + PAGE;
        assert_eq!(layout.stack_base, image_top + PAGE);
        let stack_top = layout.stack_base + LAYOUT_STACK_PAGES * PAGE;
        assert_eq!(layout.block_base, stack_top + PAGE);
        assert_eq!(layout.stack_base % PAGE, 0);
        assert_eq!(layout.block_base % PAGE, 0);
    }

    #[test]
    fn layout_scales_with_the_image_instead_of_capping_it() {
        let small_bytes = layout_bytes(&[(0x1_0000, 1)]);
        let small_image = LoadImage::parse(&small_bytes, &TAG).unwrap();
        let large_bytes = layout_bytes(&[(0x1_0000, 4096)]);
        let large_image = LoadImage::parse(&large_bytes, &TAG).unwrap();

        let small = derive_user_layout(
            &small_image,
            LAYOUT_BIAS,
            LAYOUT_STACK_PAGES,
            LAYOUT_CEILING,
        )
        .expect("small image fits");
        let large = derive_user_layout(
            &large_image,
            LAYOUT_BIAS,
            LAYOUT_STACK_PAGES,
            LAYOUT_CEILING,
        )
        .expect("large image fits");

        assert!(small.stack_base < large.stack_base);
        assert!(large.stack_base >= LAYOUT_BIAS + 0x1_0000 + 4096 * PAGE + PAGE);
    }

    #[test]
    fn layout_reaching_the_ceiling_is_refused() {
        // A (hostile or corrupt) image whose mapped top leaves no room for
        // the stack and the ABI-bounded startup block below the ceiling
        // fails closed.
        let pages = (LAYOUT_CEILING - LAYOUT_BIAS) / PAGE - LAYOUT_STACK_PAGES;
        let bytes = layout_bytes(&[(0, pages)]);
        let image = LoadImage::parse(&bytes, &TAG).unwrap();

        assert!(
            derive_user_layout(&image, LAYOUT_BIAS, LAYOUT_STACK_PAGES, LAYOUT_CEILING).is_none()
        );
    }

    #[test]
    fn layout_address_overflow_is_refused() {
        // The segment decodes on its own, but adding the bias overflows;
        // the derivation fails closed rather than wrapping.
        let bytes = layout_bytes(&[((u64::MAX - (1 << 35)) & !(PAGE - 1), 1)]);
        let image = LoadImage::parse(&bytes, &TAG).unwrap();

        assert!(
            derive_user_layout(&image, LAYOUT_BIAS, LAYOUT_STACK_PAGES, LAYOUT_CEILING).is_none()
        );
    }

    #[test]
    fn layout_with_zero_stack_pages_is_refused() {
        let bytes = layout_bytes(&[(0x1_0000, 1)]);
        let image = LoadImage::parse(&bytes, &TAG).unwrap();

        assert!(derive_user_layout(&image, LAYOUT_BIAS, 0, LAYOUT_CEILING).is_none());
    }
}
