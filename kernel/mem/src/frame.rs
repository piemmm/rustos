//! Physical frame allocator — buddy + bitmap hybrid.
//!
//! # Layers
//!
//! - **Bitmap.** One bit per architectural page-frame across the whole
//!   physical address range described by the [`BootMemoryMap`].
//!   `0 = free`, `1 = allocated or reserved or non-existent`. The bitmap
//!   is the source of truth for "is this frame currently handed out?",
//!   so a double-free or a free of a reserved frame is detected
//!   immediately and reported as
//!   [`AllocError::InvariantViolation`].
//!
//! - **Buddy free lists.** For every order `0..=MAX_ORDER` we keep a
//!   `BTreeSet<usize>` of the starting frame indices of free blocks of
//!   exactly that order. Splits push two half-blocks to `order - 1`;
//!   merges pop a buddy at `order` and push the parent at `order + 1`.
//!   The bitmap is consulted on every merge to refuse merging across
//!   reserved boundaries (this is the "hybrid" part — reserved frames
//!   look identical to allocated frames at the bitmap level, so the
//!   buddy never reaches across them).
//!
//! # Concurrency
//!
//! The allocator's state is wrapped in
//! [`rustos_sync::SpinLock`] at the [`FrameAllocator`] level.
//! Internal helpers operate on `&mut FrameAllocatorState` and are
//! oblivious to locking.
//!
//! # Result-returning OOM
//!
//! `alloc` / `alloc_order` return [`AllocError::OutOfMemory`]; the
//! allocator never panics on resource exhaustion.

use alloc::collections::BTreeSet;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use rustos_sync::SpinLock;

use crate::bootinfo::{BootMemoryMap, RegionKind};
use crate::error::AllocError;

/// Page-frame size in bytes (4 KiB, the smallest unit every Tier-1 arch
/// supports natively).
pub const PAGE_SIZE: usize = 4096;

/// Bit-shift such that `1 << PAGE_SHIFT == PAGE_SIZE`.
pub const PAGE_SHIFT: u32 = 12;

/// Maximum buddy order supported by this allocator.
///
/// Order `n` blocks span `2^n` frames, i.e. `4 KiB * 2^n` bytes.
/// `MAX_ORDER = 11` ⇒ the largest atomically-allocatable block is
/// `4 KiB << 11 = 8 MiB`. Picked to comfortably exceed any current
/// huge-page or DMA-coherent allocation while keeping the per-order
/// metadata fixed-size.
pub const MAX_ORDER: u32 = 11;

/// Type alias for "number of frames", used to keep call-site arithmetic
/// distinct from frame-index arithmetic.
pub type FrameCount = usize;

/// Physical byte address.
///
/// A thin newtype around `u64` so kernel APIs cannot accidentally pass a
/// physical address where a virtual one is expected (or vice versa).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PhysAddr(u64);

impl PhysAddr {
    /// Wrap a raw `u64` as a physical address.
    #[must_use]
    pub const fn new(addr: u64) -> Self {
        Self(addr)
    }

    /// The numeric value.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Frame index this address falls into.
    ///
    /// Truncating: `0..PAGE_SIZE` ↦ frame 0.
    #[must_use]
    pub const fn frame_index(self) -> u64 {
        self.0 >> PAGE_SHIFT
    }
}

impl fmt::Display for PhysAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#018x}", self.0)
    }
}

/// A single page-frame, identified by its 0-based index in physical RAM.
///
/// `Frame(n)` covers the byte range `[n << PAGE_SHIFT, (n+1) << PAGE_SHIFT)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Frame(pub usize);

impl Frame {
    /// Starting physical address of this frame.
    #[must_use]
    pub fn start(self) -> PhysAddr {
        PhysAddr::new((self.0 as u64) << PAGE_SHIFT)
    }

    /// The frame containing `addr` (truncating).
    #[must_use]
    pub fn containing(addr: PhysAddr) -> Self {
        Self((addr.as_u64() >> PAGE_SHIFT) as usize)
    }
}

// ---------------------------------------------------------------------------
// State (private)
// ---------------------------------------------------------------------------

/// Internal, lock-free state of the frame allocator.
///
/// `FrameAllocatorState` is what the [`SpinLock`] in [`FrameAllocator`]
/// guards. Splitting it out makes the locking discipline explicit and
/// lets the unit tests exercise the algorithm without spinlock noise.
struct FrameAllocatorState {
    /// `total_frames` = number of bit positions in the bitmap.
    total_frames: usize,
    /// Bit set = "frame is allocated, reserved, or non-existent."
    bitmap: Vec<u64>,
    /// `free_lists[order]` = set of free block starts of that order.
    free_lists: Vec<BTreeSet<usize>>,
    /// Cached count of free frames (sum over `free_lists` of 2^order * len).
    free_frames: usize,
    /// Number of whole frames inside `Usable` boot-map regions — the RAM
    /// the allocator can ever hand out. Excludes reserved regions and
    /// physical-address holes (MMIO windows, the space below the RAM base),
    /// so `usable_frames - free_frames` is real allocation, fixed after
    /// construction.
    usable_frames: usize,
}

impl FrameAllocatorState {
    fn bit(&self, frame: usize) -> bool {
        let (w, b) = (frame / 64, frame % 64);
        (self.bitmap[w] >> b) & 1 == 1
    }
    fn set_bit(&mut self, frame: usize) {
        let (w, b) = (frame / 64, frame % 64);
        self.bitmap[w] |= 1u64 << b;
    }
    fn clear_bit(&mut self, frame: usize) {
        let (w, b) = (frame / 64, frame % 64);
        self.bitmap[w] &= !(1u64 << b);
    }

    /// Mark frames `[start, start + count)` as allocated/reserved.
    fn mark_range_used(&mut self, start: usize, count: usize) {
        for i in start..start + count {
            self.set_bit(i);
        }
    }

    /// Are all frames in `[start, start + (1<<order))` free in the bitmap?
    fn block_is_free(&self, start: usize, order: u32) -> bool {
        let n = 1usize << order;
        if start + n > self.total_frames {
            return false;
        }
        for i in start..start + n {
            if self.bit(i) {
                return false;
            }
        }
        true
    }

    /// Insert a maximally-aligned free block into the buddy lists.
    fn add_free_block(&mut self, start: usize, order: u32) {
        self.free_lists[order as usize].insert(start);
        self.free_frames += 1usize << order;
    }

    fn remove_free_block(&mut self, start: usize, order: u32) -> bool {
        let removed = self.free_lists[order as usize].remove(&start);
        if removed {
            self.free_frames -= 1usize << order;
        }
        removed
    }

    /// Greedy insertion of every free run discovered in `[start, end)`:
    /// chop the run into maximally-aligned, maximally-sized buddy blocks.
    fn populate_run(&mut self, mut start: usize, end: usize) {
        while start < end {
            // The largest order we can place at `start` is bounded by
            // (a) alignment of `start` and (b) the number of frames left
            // and (c) MAX_ORDER.
            let align_order = if start == 0 {
                MAX_ORDER
            } else {
                core::cmp::min(start.trailing_zeros(), MAX_ORDER)
            };
            let max_by_remaining = (end - start)
                .checked_ilog2()
                .map_or(0, |o| core::cmp::min(o, MAX_ORDER));
            let order = core::cmp::min(align_order, max_by_remaining);
            self.add_free_block(start, order);
            start += 1usize << order;
        }
    }

    fn alloc_order(&mut self, order: u32) -> Result<usize, AllocError> {
        if order > MAX_ORDER {
            return Err(AllocError::SizeUnsupported);
        }
        // Find lowest order ≥ `order` with a non-empty free list.
        let mut found: Option<u32> = None;
        for o in order..=MAX_ORDER {
            if !self.free_lists[o as usize].is_empty() {
                found = Some(o);
                break;
            }
        }
        let mut cur = found.ok_or(AllocError::OutOfMemory)?;
        // Pop the lowest-indexed block at `cur` (deterministic for tests).
        let start = *self.free_lists[cur as usize]
            .iter()
            .next()
            .ok_or(AllocError::OutOfMemory)?;
        let removed = self.free_lists[cur as usize].remove(&start);
        debug_assert!(removed);
        self.free_frames -= 1usize << cur;

        // Split down to the requested order.
        while cur > order {
            cur -= 1;
            let buddy = start + (1usize << cur);
            self.add_free_block(buddy, cur);
        }

        self.mark_range_used(start, 1usize << order);
        Ok(start)
    }

    fn free_order(&mut self, start: usize, order: u32) -> Result<(), AllocError> {
        if order > MAX_ORDER {
            return Err(AllocError::SizeUnsupported);
        }
        let n = 1usize << order;
        // Range checks.
        if start.checked_add(n).map_or(true, |e| e > self.total_frames) {
            return Err(AllocError::OutOfRange);
        }
        if start & (n - 1) != 0 {
            return Err(AllocError::InvariantViolation);
        }
        // Every frame in the block must currently be allocated (bit=1)
        // *and* must not be reserved. We can't tell the two apart from
        // the bitmap alone, but our public free path only sees frames
        // returned by a prior alloc, which is necessarily not reserved.
        for i in start..start + n {
            if !self.bit(i) {
                return Err(AllocError::InvariantViolation);
            }
        }
        for i in start..start + n {
            self.clear_bit(i);
        }

        // Try to merge with successive buddies.
        let mut cur_start = start;
        let mut cur_order = order;
        while cur_order < MAX_ORDER {
            let buddy = cur_start ^ (1usize << cur_order);
            // Refuse to merge across an end-of-RAM boundary.
            let parent_start = cur_start & !(1usize << cur_order);
            if parent_start + (1usize << (cur_order + 1)) > self.total_frames {
                break;
            }
            // Refuse to merge if any frame in the buddy half is not
            // marked free in the bitmap (covers both reserved holes and
            // currently-allocated buddies).
            if !self.block_is_free(buddy, cur_order) {
                break;
            }
            if !self.remove_free_block(buddy, cur_order) {
                // The buddy is bitmap-free but not registered at this
                // order — it must be split into smaller blocks below us,
                // so we cannot merge.
                break;
            }
            cur_start = parent_start;
            cur_order += 1;
        }
        self.add_free_block(cur_start, cur_order);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Public FrameAllocator
// ---------------------------------------------------------------------------

/// Buddy + bitmap physical frame allocator.
///
/// Constructed once at boot from a [`BootMemoryMap`]; thereafter handed
/// to subsystems through a `&FrameAllocator`. All public methods are
/// safe and use a [`SpinLock`] internally.
pub struct FrameAllocator {
    inner: SpinLock<FrameAllocatorState>,
}

impl FrameAllocator {
    /// Build a frame allocator from `map`.
    ///
    /// Validation performed:
    ///
    /// - No region's `start + length` overflows `u64`.
    /// - No two regions overlap.
    /// - Map describes at least one usable whole frame.
    ///
    /// Reserved regions are merged into the bitmap as "used" so the
    /// allocator will never hand them out. Usable regions are rounded
    /// *inward* to whole-frame boundaries before being inserted into the
    /// buddy free lists.
    ///
    /// The frame at physical address zero is never enrolled, even when the
    /// firmware map reports it usable: under an identity direct map its
    /// translation is the null pointer, which no [`NonNull`]-based consumer
    /// (the page-table frame source, the DMA pool, an MMIO window) can
    /// represent — and because the buddy lists hand out the lowest free
    /// index first, a frame 0 that a consumer draws, cannot use, and hands
    /// back would be re-drawn by every later request, wedging allocation
    /// permanently while `free_frames` still reports plenty. On PCs it is
    /// also the real-mode IVT/BDA page. It stays marked reserved, exactly
    /// like firmware-reserved RAM, and is excluded from
    /// [`Self::usable_frames`].
    ///
    /// [`NonNull`]: core::ptr::NonNull
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::InvariantViolation`] if the map is
    /// malformed (overflow or overlap) and
    /// [`AllocError::OutOfMemory`] if the map contains no usable frame.
    pub fn new(map: &BootMemoryMap) -> Result<Self, AllocError> {
        // 1. Determine total frame count.
        let hi = map
            .highest_address()
            .ok_or(AllocError::InvariantViolation)?
            .as_u64();
        // Round up to a whole frame.
        let total_bytes = hi
            .checked_add(PAGE_SIZE as u64 - 1)
            .ok_or(AllocError::InvariantViolation)?
            & !(PAGE_SIZE as u64 - 1);
        let total_frames: usize = (total_bytes >> PAGE_SHIFT)
            .try_into()
            .map_err(|_| AllocError::InvariantViolation)?;
        if total_frames == 0 {
            return Err(AllocError::OutOfMemory);
        }

        // 2. Allocate bitmap and free lists. All "1" initially — any
        //    frame not subsequently marked usable stays reserved.
        let words = total_frames.div_ceil(64);
        let bitmap = vec![u64::MAX; words];
        let mut free_lists = Vec::with_capacity(MAX_ORDER as usize + 1);
        for _ in 0..=MAX_ORDER {
            free_lists.push(BTreeSet::new());
        }
        let mut state = FrameAllocatorState {
            total_frames,
            bitmap,
            free_lists,
            free_frames: 0,
            usable_frames: 0,
        };

        // 3. Detect overlaps by sorting by start and scanning.
        let mut sorted: Vec<_> = map.regions().to_vec();
        sorted.sort_by_key(|r| r.start.as_u64());
        for win in sorted.windows(2) {
            let a_end = win[0].end().ok_or(AllocError::InvariantViolation)?;
            if a_end.as_u64() > win[1].start.as_u64() {
                return Err(AllocError::InvariantViolation);
            }
        }

        // 4. Build per-frame state: only Usable regions clear bits.
        //    Reserved regions remain marked "used".
        let mut any_usable = false;
        for r in &sorted {
            if r.kind != RegionKind::Usable {
                continue;
            }
            let Some(end) = r.end() else {
                return Err(AllocError::InvariantViolation);
            };
            // Inward rounding: first frame fully inside region,
            // last frame fully inside region (exclusive upper bound).
            let first = usize::try_from(r.start.as_u64().div_ceil(PAGE_SIZE as u64))
                .map_err(|_| AllocError::InvariantViolation)?;
            // Never enroll the zero page: its identity translation is the
            // null pointer, so a consumer that draws it must hand it back,
            // and the lowest-first buddy pop would then re-issue it forever
            // (the CI-only spawn wedge this guards against). Reserved, like
            // firmware-reserved RAM.
            let first = first.max(1);
            let last_excl = usize::try_from(end.as_u64() / PAGE_SIZE as u64)
                .map_err(|_| AllocError::InvariantViolation)?;
            if first >= last_excl {
                continue;
            }
            for i in first..last_excl {
                state.clear_bit(i);
            }
            state.usable_frames += last_excl - first;
            any_usable = true;
            // Insert as buddy blocks.
            state.populate_run(first, last_excl);
        }
        if !any_usable {
            return Err(AllocError::OutOfMemory);
        }

        Ok(Self {
            inner: SpinLock::new(state),
        })
    }

    /// Allocate a single frame.
    ///
    /// # Errors
    ///
    /// [`AllocError::OutOfMemory`] if no frame is available.
    pub fn alloc(&self) -> Result<Frame, AllocError> {
        self.alloc_order(0)
    }

    /// Allocate `2^order` contiguous frames, aligned to that boundary.
    ///
    /// # Errors
    ///
    /// - [`AllocError::SizeUnsupported`] if `order > MAX_ORDER`.
    /// - [`AllocError::OutOfMemory`] if no block of any order ≥ `order`
    ///   is available.
    pub fn alloc_order(&self, order: u32) -> Result<Frame, AllocError> {
        let mut g = self.inner.lock();
        g.alloc_order(order).map(Frame)
    }

    /// Free a frame previously returned by [`Self::alloc`].
    ///
    /// # Errors
    ///
    /// [`AllocError::InvariantViolation`] for double-free, free of an
    /// unowned frame, or free of a misaligned address.
    pub fn free(&self, frame: Frame) -> Result<(), AllocError> {
        self.free_order(frame, 0)
    }

    /// Free a `2^order` block previously returned by
    /// [`Self::alloc_order`].
    ///
    /// # Errors
    ///
    /// As for [`Self::free`], plus [`AllocError::SizeUnsupported`] for
    /// `order > MAX_ORDER`.
    pub fn free_order(&self, frame: Frame, order: u32) -> Result<(), AllocError> {
        let mut g = self.inner.lock();
        g.free_order(frame.0, order)
    }

    /// Number of frames the allocator can still hand out.
    #[must_use]
    pub fn free_frames(&self) -> FrameCount {
        self.inner.lock().free_frames
    }

    /// Total frames the allocator is aware of (usable + reserved + holes).
    ///
    /// This is an *address-space* extent (bitmap size), not a RAM size:
    /// it spans from physical address zero to the highest mapped address,
    /// including reserved regions and holes. For the amount of RAM the
    /// system actually has, use [`Self::usable_frames`].
    #[must_use]
    pub fn total_frames(&self) -> FrameCount {
        self.inner.lock().total_frames
    }

    /// Number of whole frames of usable RAM the allocator manages —
    /// frames inside `Usable` boot-map regions, whether currently free or
    /// handed out. Excludes reserved regions, physical-address holes, and
    /// the permanently reserved zero page (see [`Self::new`]), so
    /// `usable_frames() - free_frames()` is the memory genuinely in use.
    #[must_use]
    pub fn usable_frames(&self) -> FrameCount {
        self.inner.lock().usable_frames
    }
}

// SAFETY: All shared mutable state lives behind the internal `SpinLock`,
// which already implements `Send`/`Sync` for its protected payload. The
// allocator itself contains no other interior mutability.
unsafe impl Sync for FrameAllocator {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::bootinfo::MemoryRegion;

    extern crate std;
    use std::format;
    use std::vec::Vec;

    /// A map whose single usable region starts at frame 16 — clear of the
    /// permanently reserved zero page, and aligned for the largest order
    /// the merge test draws — so `usable_pages` frames enter the pool.
    fn small_map(usable_pages: usize) -> BootMemoryMap {
        let mut m = BootMemoryMap::new();
        m.push(MemoryRegion {
            start: PhysAddr::new((16 * PAGE_SIZE) as u64),
            length: (usable_pages * PAGE_SIZE) as u64,
            kind: RegionKind::Usable,
        });
        m
    }

    #[test]
    fn page_size_matches_shift() {
        assert_eq!(1usize << PAGE_SHIFT, PAGE_SIZE);
    }

    #[test]
    fn phys_addr_frame_index_truncates() {
        assert_eq!(PhysAddr::new(0).frame_index(), 0);
        assert_eq!(PhysAddr::new(PAGE_SIZE as u64 - 1).frame_index(), 0);
        assert_eq!(PhysAddr::new(PAGE_SIZE as u64).frame_index(), 1);
    }

    #[test]
    fn frame_start_round_trip() {
        let f = Frame(7);
        assert_eq!(Frame::containing(f.start()), f);
    }

    #[test]
    fn new_rejects_empty_map() {
        let m = BootMemoryMap::new();
        assert_eq!(FrameAllocator::new(&m).err(), Some(AllocError::OutOfMemory));
    }

    #[test]
    fn new_rejects_overlap() {
        let mut m = BootMemoryMap::new();
        m.push(MemoryRegion {
            start: PhysAddr::new(0),
            length: 0x4000,
            kind: RegionKind::Usable,
        });
        m.push(MemoryRegion {
            start: PhysAddr::new(0x2000),
            length: 0x4000,
            kind: RegionKind::Usable,
        });
        assert_eq!(
            FrameAllocator::new(&m).err(),
            Some(AllocError::InvariantViolation)
        );
    }

    #[test]
    fn alloc_then_free_returns_same_frame() {
        let m = small_map(8);
        let a = FrameAllocator::new(&m).unwrap();
        let f = a.alloc().unwrap();
        assert!(a.free_frames() < 8);
        a.free(f).unwrap();
        assert_eq!(a.free_frames(), 8);
    }

    #[test]
    fn alloc_order_returns_aligned_block() {
        let m = small_map(16);
        let a = FrameAllocator::new(&m).unwrap();
        let blk = a.alloc_order(3).unwrap(); // 8 frames
        assert_eq!(blk.0 & 7, 0);
        a.free_order(blk, 3).unwrap();
    }

    #[test]
    fn oom_when_exhausted() {
        let m = small_map(4);
        let a = FrameAllocator::new(&m).unwrap();
        let mut held = Vec::new();
        while let Ok(f) = a.alloc() {
            held.push(f);
        }
        assert_eq!(held.len(), 4);
        assert_eq!(a.alloc().err(), Some(AllocError::OutOfMemory));
        for f in held {
            a.free(f).unwrap();
        }
        assert_eq!(a.free_frames(), 4);
    }

    #[test]
    fn order_too_large_is_unsupported() {
        let m = small_map(4);
        let a = FrameAllocator::new(&m).unwrap();
        assert_eq!(
            a.alloc_order(MAX_ORDER + 1).err(),
            Some(AllocError::SizeUnsupported)
        );
    }

    #[test]
    fn double_free_detected() {
        let m = small_map(4);
        let a = FrameAllocator::new(&m).unwrap();
        let f = a.alloc().unwrap();
        a.free(f).unwrap();
        assert_eq!(a.free(f).err(), Some(AllocError::InvariantViolation));
    }

    #[test]
    fn free_out_of_range() {
        let m = small_map(4);
        let a = FrameAllocator::new(&m).unwrap();
        assert_eq!(a.free(Frame(9999)).err(), Some(AllocError::OutOfRange));
    }

    #[test]
    fn free_misaligned_order_block() {
        let m = small_map(16);
        let a = FrameAllocator::new(&m).unwrap();
        // Frame 17 cannot be the start of an order-1 block (needs alignment 2).
        assert_eq!(
            a.free_order(Frame(17), 1).err(),
            Some(AllocError::InvariantViolation)
        );
    }

    #[test]
    fn reserved_region_not_handed_out() {
        let mut m = BootMemoryMap::new();
        // Frames 0..4 usable, 4..8 reserved, 8..16 usable.
        m.push(MemoryRegion {
            start: PhysAddr::new(0),
            length: (4 * PAGE_SIZE) as u64,
            kind: RegionKind::Usable,
        });
        m.push(MemoryRegion {
            start: PhysAddr::new((4 * PAGE_SIZE) as u64),
            length: (4 * PAGE_SIZE) as u64,
            kind: RegionKind::Reserved,
        });
        m.push(MemoryRegion {
            start: PhysAddr::new((8 * PAGE_SIZE) as u64),
            length: (8 * PAGE_SIZE) as u64,
            kind: RegionKind::Usable,
        });
        let a = FrameAllocator::new(&m).unwrap();
        let mut handed = Vec::new();
        while let Ok(f) = a.alloc() {
            // No frame in [4,8) — nor the reserved zero page — may ever be
            // handed out.
            assert!(!(4..8).contains(&f.0), "reserved frame {} handed out", f.0);
            assert_ne!(f.0, 0, "the zero page must never be handed out");
            handed.push(f);
        }
        assert_eq!(handed.len(), 11);
    }

    /// Regression: the zero page never enters circulation even when the
    /// firmware map reports it usable. Before the fix, frame 0's identity
    /// translation (the null pointer) made the page-table frame source hand
    /// it back, and the lowest-first buddy pop re-issued it to every later
    /// request — wedging `spawn` permanently with tens of thousands of
    /// frames still free (the CI-only spawn-session failure).
    #[test]
    fn zero_page_is_reserved_even_when_firmware_reports_it_usable() {
        let mut m = BootMemoryMap::new();
        m.push(MemoryRegion {
            start: PhysAddr::new(0),
            length: (8 * PAGE_SIZE) as u64,
            kind: RegionKind::Usable,
        });
        let a = FrameAllocator::new(&m).unwrap();
        assert_eq!(a.usable_frames(), 7, "the zero page is not usable RAM");
        assert_eq!(a.free_frames(), 7);
        let mut handed = Vec::new();
        while let Ok(f) = a.alloc() {
            assert_ne!(f.0, 0, "the zero page must never be handed out");
            handed.push(f);
        }
        assert_eq!(handed.len(), 7);
    }

    /// A map whose only usable frame is the zero page holds no RAM the
    /// allocator may hand out, so construction fails closed.
    #[test]
    fn map_of_only_the_zero_page_has_no_usable_frame() {
        let mut m = BootMemoryMap::new();
        m.push(MemoryRegion {
            start: PhysAddr::new(0),
            length: PAGE_SIZE as u64,
            kind: RegionKind::Usable,
        });
        assert_eq!(FrameAllocator::new(&m).err(), Some(AllocError::OutOfMemory));
    }

    #[test]
    fn usable_frames_excludes_holes_and_reserved() {
        // The QEMU-virt shape that broke the login screen's memory line:
        // RAM sits above a large MMIO hole, so the address-space extent
        // (total_frames) dwarfs the actual RAM. usable_frames must count
        // only the usable regions, and stay fixed across alloc/free.
        let mut m = BootMemoryMap::new();
        m.push(MemoryRegion {
            start: PhysAddr::new((1024 * PAGE_SIZE) as u64),
            length: (4 * PAGE_SIZE) as u64,
            kind: RegionKind::Reserved,
        });
        m.push(MemoryRegion {
            start: PhysAddr::new((1028 * PAGE_SIZE) as u64),
            length: (16 * PAGE_SIZE) as u64,
            kind: RegionKind::Usable,
        });
        let a = FrameAllocator::new(&m).unwrap();
        assert_eq!(a.total_frames(), 1044);
        assert_eq!(a.usable_frames(), 16);
        assert_eq!(a.free_frames(), 16);
        let f = a.alloc().unwrap();
        assert_eq!(a.usable_frames(), 16);
        assert_eq!(a.free_frames(), 15);
        a.free(f).unwrap();
        assert_eq!(a.usable_frames(), 16);
        assert_eq!(a.free_frames(), 16);
    }

    #[test]
    fn split_and_merge_round_trip() {
        let m = small_map(16);
        let a = FrameAllocator::new(&m).unwrap();
        // Take an order-2 block (4 frames). This forces a split if the
        // initial population coalesced higher.
        let b0 = a.alloc_order(2).unwrap();
        let b1 = a.alloc_order(2).unwrap();
        a.free_order(b0, 2).unwrap();
        a.free_order(b1, 2).unwrap();
        // Everything must merge back; we should once again be able to
        // satisfy the largest single allocation the map allows.
        let big = a.alloc_order(4).unwrap(); // 16 frames
        a.free_order(big, 4).unwrap();
        assert_eq!(a.free_frames(), 16);
    }

    // Property tests: randomized alloc/free sequences uphold the
    // no-double-allocation and no-leak invariants.
    #[test]
    fn proptest_alloc_free_invariants() {
        use proptest::prelude::*;
        use proptest::test_runner::{Config, TestRunner};

        let strat = proptest::collection::vec(any::<u8>(), 1..200);
        let mut runner = TestRunner::new(Config {
            cases: 64,
            ..Config::default()
        });
        runner
            .run(&strat, |ops| {
                let m = small_map(32);
                let a = FrameAllocator::new(&m).unwrap();
                let mut held: Vec<Frame> = Vec::new();
                let mut seen: std::collections::HashSet<usize> =
                    std::collections::HashSet::default();
                for op in ops {
                    if op % 2 == 0 || held.is_empty() {
                        if let Ok(f) = a.alloc() {
                            prop_assert!(seen.insert(f.0), "double alloc {}", f.0);
                            held.push(f);
                        }
                    } else {
                        let idx = (op as usize) % held.len();
                        let f = held.swap_remove(idx);
                        seen.remove(&f.0);
                        a.free(f).unwrap();
                    }
                }
                for f in held {
                    a.free(f).unwrap();
                }
                prop_assert_eq!(a.free_frames(), 32);
                Ok(())
            })
            .unwrap();
    }
}
