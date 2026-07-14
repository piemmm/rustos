//! Growable kernel heap wiring.
//!
//! The kernel `#[global_allocator]` is a [`rustos_kalloc::FreeListAllocator`]
//! living in the bin crate over a small `.bss` bootstrap region. That
//! bootstrap covers early boot, before a physical frame allocator exists;
//! once one does, the boot path installs a growth source here so the heap
//! grows past its bootstrap region on demand and shrinks drained regions
//! back — the growable-capacity discipline the charter requires of every
//! resource ceiling, replacing the former fixed slab that a busy kernel
//! could exhaust into an allocation-failure panic.
//!
//! Two seams connect the bin's allocator to the kernel core:
//!
//! * [`register_global_heap`] — each arch bin publishes its
//!   `#[global_allocator]` here with one line before it calls `boot`, so
//!   the core can reach the same allocator instance without naming the
//!   bin.
//! * [`install_frame_heap_source`] — the boot path calls this once the
//!   frame allocator and the arch direct physical map both exist and are
//!   `'static`, wiring a frame-backed growth source into the registered heap.
//!
//! That source draws **physically contiguous** frames from the
//! frame allocator and hands the heap their direct-map addresses. Because
//! the frame allocator is heap-independent by construction, growth never
//! re-enters the heap's own lock. A bin that registers no heap, or a port
//! that wires no direct map, simply leaves the heap capped at its
//! bootstrap region (fail closed, never a panic).

use alloc::boxed::Box;
use core::sync::atomic::{AtomicPtr, Ordering};

use rustos_kalloc::{FreeListAllocator, HeapSource};
use rustos_kernel_mem::{Frame, FrameAllocator, PhysMap, MAX_ORDER, PAGE_SIZE};

/// Minimum growth granule, expressed as a frame order: 2^4 frames = 64 KiB.
///
/// A miss draws at least this much even for a small allocation, so a burst
/// of small allocations does not force a fresh frame draw each time
/// (amortised growth); the remainder stays as free holes the next
/// allocation reuses, and a wholly-drained region is returned intact.
const MIN_GROW_ORDER: u32 = 4;

/// The registered bin `#[global_allocator]`, or null before a bin
/// publishes one. Set once per boot by [`register_global_heap`].
static GLOBAL_HEAP: AtomicPtr<FreeListAllocator> = AtomicPtr::new(core::ptr::null_mut());

/// Publish the bin's `#[global_allocator]` so [`install_frame_heap_source`]
/// can later wire the growth source into it.
///
/// Each arch bin calls this with its `&'static FreeListAllocator` before
/// entering `boot`. Idempotent-by-policy: the boot path registers exactly
/// one heap for the life of the binary; a second call simply retargets the
/// slot.
pub fn register_global_heap(heap: &'static FreeListAllocator) {
    GLOBAL_HEAP.store(
        (heap as *const FreeListAllocator).cast_mut(),
        Ordering::Release,
    );
}

/// Borrow the registered heap, or `None` when no bin published one (a host
/// harness, or an early call before registration).
fn global_heap() -> Option<&'static FreeListAllocator> {
    // SAFETY: the pointer is only ever set by `register_global_heap` from a
    // `&'static FreeListAllocator`, so a non-null value is a valid `'static`
    // reference for the life of the binary; null means none was registered.
    unsafe { GLOBAL_HEAP.load(Ordering::Acquire).as_ref() }
}

/// The production kernel-heap growth source: physically contiguous frames
/// from the frame allocator, reached through the kernel direct map.
struct FrameHeapSource {
    frames: &'static FrameAllocator,
    physmap: &'static (dyn PhysMap + Sync),
}

/// The frame order that covers `len` bytes, floored at [`MIN_GROW_ORDER`],
/// or `None` when it would exceed the frame allocator's largest contiguous
/// block ([`MAX_ORDER`]). A grown region is always exactly `2^order`
/// frames, so [`HeapSource::shrink`] recovers the order from the length.
fn order_for(len: usize) -> Option<u32> {
    let pages = len.div_ceil(PAGE_SIZE).max(1);
    let order = pages
        .next_power_of_two()
        .trailing_zeros()
        .max(MIN_GROW_ORDER);
    if order > MAX_ORDER {
        None
    } else {
        Some(order)
    }
}

impl HeapSource for FrameHeapSource {
    fn grow(&self, min_len: usize) -> Option<(*mut u8, usize)> {
        let order = order_for(min_len)?;
        let frame = self.frames.alloc_order(order).ok()?;
        let len = (1usize << order) * PAGE_SIZE;
        let phys = frame.start();
        if let Some(ptr) = self.physmap.translate(phys, len) {
            Some((ptr.as_ptr(), len))
        } else {
            // The direct map does not cover this frame: hand it straight back
            // and fail closed rather than returning an unreachable chunk the
            // heap would then dereference.
            let _ = self.frames.free_order(frame, order);
            None
        }
    }

    fn shrink(&self, base: *mut u8, len: usize) {
        // `grow` always returns a whole, power-of-two number of frames, so
        // the order recovers exactly from the length.
        debug_assert_eq!(len % PAGE_SIZE, 0);
        let order = (len / PAGE_SIZE).trailing_zeros();
        if let Some(phys) = self.physmap.reverse(base as usize) {
            // `free_order` fails closed on a bad frame/order; a genuinely
            // matched region always frees. A leak here is impossible on a
            // real direct map (`reverse` is the exact inverse of the
            // `translate` that produced `base`); a port with no invertible
            // map never installs a source that shrinks.
            let _ = self.frames.free_order(Frame::containing(phys), order);
        }
    }
}

/// Wire a frame-backed growth source over `frames` and `physmap` into the
/// registered kernel heap, so it can grow past its bootstrap region.
///
/// Called once from the boot path after the frame allocator and the arch
/// direct physical map both exist and are `'static`. A no-op when no bin
/// registered a heap (a host harness): the heap then stays capped at its
/// bootstrap region, fail closed.
pub fn install_frame_heap_source(
    frames: &'static FrameAllocator,
    physmap: &'static (dyn PhysMap + Sync),
) {
    let Some(heap) = global_heap() else {
        return;
    };
    let source: &'static FrameHeapSource = Box::leak(Box::new(FrameHeapSource { frames, physmap }));
    heap.install_source(source);
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_kernel_mem::{BootMemoryMap, MemoryRegion, PhysAddr, RegionKind, SimPhysMap};

    /// Round-trips growth and shrink through the real allocator stack: a
    /// tiny bootstrap heap backed by a [`FrameHeapSource`] over a
    /// [`FrameAllocator`] and a [`SimPhysMap`] standing in for the direct
    /// map. An allocation larger than the bootstrap region forces a frame
    /// draw; freeing it drains the grown region and hands the frames back.
    #[test]
    fn grows_from_frames_and_shrinks_back() {
        use core::alloc::{GlobalAlloc, Layout};

        // A simulated physical window at a page-aligned base, large enough
        // to satisfy several 64 KiB growth granules.
        let base = PhysAddr::new(0x10_0000);
        let win_bytes = 2 * 1024 * 1024;
        let sim: &'static SimPhysMap = Box::leak(Box::new(SimPhysMap::new(base, win_bytes)));

        // A frame allocator whose only usable RAM is that same window, so
        // every frame it hands out is reachable through `sim`.
        let mut map = BootMemoryMap::new();
        map.push(MemoryRegion {
            start: base,
            length: win_bytes as u64,
            kind: RegionKind::Usable,
        });
        let frames: &'static FrameAllocator =
            Box::leak(Box::new(FrameAllocator::new(&map).unwrap()));
        let free_before = frames.free_frames();

        // An empty bootstrap region so every allocation must grow. The base
        // is an `ALIGN`-aligned `u64` (never dereferenced because the length
        // is below one hole header, so no initial hole is planted).
        let boot: &'static mut u64 = Box::leak(Box::new(0u64));
        // SAFETY: `boot` is a unique `'static`, 8-byte-aligned pointer
        // exposed through no other allocator; a zero length means it is
        // never read or written. Host test.
        let heap: &'static FreeListAllocator = Box::leak(Box::new(unsafe {
            FreeListAllocator::new((boot as *mut u64).cast::<u8>(), 0)
        }));
        let source: &'static FrameHeapSource = Box::leak(Box::new(FrameHeapSource {
            frames,
            physmap: sim,
        }));
        heap.install_source(source);

        // Allocate 128 KiB — the empty bootstrap cannot satisfy it — forcing
        // a grow that draws frames.
        let layout = Layout::from_size_align(128 * 1024, 16).unwrap();
        // SAFETY: valid layout; the heap is the sole owner of its regions.
        let ptr = unsafe { heap.alloc(layout) };
        assert!(!ptr.is_null(), "grow satisfied the large allocation");
        assert!(
            frames.free_frames() < free_before,
            "growth drew frames from the allocator"
        );

        // Free it: the grown region drains and is handed back to the frame
        // allocator (shrink), restoring the free-frame count.
        // SAFETY: same pointer/layout the alloc returned.
        unsafe { heap.dealloc(ptr, layout) };
        assert_eq!(
            frames.free_frames(),
            free_before,
            "shrink returned every drawn frame"
        );
    }

    #[test]
    fn order_for_floors_and_caps() {
        assert_eq!(order_for(1), Some(MIN_GROW_ORDER));
        assert_eq!(order_for(64 * 1024), Some(MIN_GROW_ORDER));
        assert_eq!(order_for(128 * 1024), Some(5));
        // A request larger than the largest contiguous block fails closed.
        assert_eq!(order_for((1usize << (MAX_ORDER + 1)) * PAGE_SIZE), None);
    }
}
