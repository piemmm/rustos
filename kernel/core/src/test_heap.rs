//! Host-test kernel heap for [`crate::BootInfo`].
//!
//! `BootInfo` requires the binary's `#[global_allocator]` so the boot path
//! can wire the growth source into it and report its live size, which is
//! what stops a bin from booting with an ungrowable heap. A host harness has
//! no such allocator — its global allocator is the platform's — so it hands
//! over a standalone one here instead. The host boot path never allocates
//! from it (the host `KernelArch` doubles reserve no remap window, so no
//! growth source is installed), but a test may, to move the figure the
//! System Information API reports.
//!
//! Gated with the other host-boot fixtures so it never links into a
//! production build.

use core::alloc::Layout;

use tairix_abi::PAGE_SIZE;
use tairix_kalloc::FreeListAllocator;

/// Bytes of the arena [`leak_heap`] hands each allocator.
///
/// Several pages, and page-aligned below, so both of the allocator's tiers
/// work: the slab carves a granule-aligned page out of the arena through the
/// byte tier, which a single-page arena cannot spare.
const ARENA_BYTES: usize = 16 * PAGE_SIZE;

/// Leak a kernel heap for a host `BootInfo`, mirroring the bin-crate
/// `static ALLOCATOR` convention.
///
/// Returns `None` when the arena allocation fails, so a caller fails rather
/// than fabricating a heap over a null base.
#[must_use]
pub fn leak_heap() -> Option<&'static FreeListAllocator> {
    let layout = Layout::from_size_align(ARENA_BYTES, PAGE_SIZE).ok()?;
    // SAFETY: `ARENA_BYTES` is non-zero and `PAGE_SIZE` is a power of two, so
    // the layout has a non-zero size and a valid alignment.
    let arena = unsafe { alloc::alloc::alloc_zeroed(layout) };
    if arena.is_null() {
        return None;
    }
    // SAFETY: `arena` is `ARENA_BYTES` writable bytes, aligned well beyond
    // two machine words, never freed (the allocation is leaked), and handed
    // to this allocator alone.
    let heap = unsafe { FreeListAllocator::new(arena, ARENA_BYTES) };
    Some(alloc::boxed::Box::leak(alloc::boxed::Box::new(heap)))
}
