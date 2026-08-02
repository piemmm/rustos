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
//!   doubly-linked list of free blocks of exactly that order, threaded
//!   through a per-frame `nodes` array indexed by starting frame. Splits
//!   push two half-blocks to `order - 1`; merges pop a buddy at `order`
//!   and push the parent at `order + 1`. The bitmap is consulted on
//!   every merge to refuse merging across reserved boundaries (this is
//!   the "hybrid" part — reserved frames look identical to allocated
//!   frames at the bitmap level, so the buddy never reaches across them).
//!
//! # No dependency on the kernel heap
//!
//! The per-order free lists are **intrusive**: their links live in the
//! `nodes`/`blk_order` arrays this allocator owns, both sized once from
//! the frame count at construction. Allocation and freeing therefore
//! touch no other allocator — critically, they never call the global
//! kernel heap. This is what lets the kernel heap grow by drawing frames
//! from here without re-entering itself (a heap that fed itself through
//! a page allocator whose free lists allocated *from that heap* would
//! deadlock under its own lock). The only heap use is the one-time
//! `nodes`/`blk_order`/`bitmap` construction, before the heap is under
//! load. Each free block occupies at most one node (its start frame),
//! so the per-frame overhead is a fixed `2 * usize + 1` byte — far
//! leaner than a per-frame descriptor, and proportional to the RAM the
//! machine actually has.
//!
//! # Concurrency
//!
//! The allocator's state is wrapped in
//! [`tairix_sync::SpinLock`] at the [`FrameAllocator`] level.
//! Internal helpers operate on `&mut FrameAllocatorState` and are
//! oblivious to locking.
//!
//! # Result-returning OOM
//!
//! `alloc` / `alloc_order` return [`AllocError::OutOfMemory`]; the
//! allocator never panics on resource exhaustion.

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use tairix_reclaim::RESERVE_DIVISOR;
use tairix_sync::SpinLock;

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
/// `MAX_ORDER = 13` ⇒ the largest atomically-allocatable block is
/// `4 KiB << 13 = 32 MiB`. This is the ceiling on a single contiguous
/// physical allocation, and — crucially — on the region the kernel heap's
/// frame-backed growth source can draw in one go (`crate` `kheap`), so it
/// must comfortably exceed the largest single contiguous allocation the
/// kernel ever makes. The binding case is a bundle `Run` image, read whole
/// into one heap buffer up to `BUNDLE_FILE_MAX` (16 MiB): the allocator's
/// per-block header and power-of-two growth granule round a full 16 MiB
/// request up to the next power of two (32 MiB), so 32 MiB is the smallest
/// order that guarantees such a load is served from — or grown to — the
/// heap rather than failing once the bootstrap arena fragments. A
/// compile-time assertion in `kernel/core` keeps this coupling honest.
pub const MAX_ORDER: u32 = 13;

// [`RESERVE_DIVISOR`](tairix_reclaim::RESERVE_DIVISOR) is the one
// definition shared with the memory-pressure policy (`tairix_reclaim`),
// so the frame allocator's user-commit floor and the pressure band's
// critical floor can never diverge.

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

/// Sentinel for "no frame" in the intrusive free-list links and heads.
///
/// `usize::MAX` can never be a real frame index: valid indices are
/// `0..total_frames`, and `total_frames` is derived from a physical
/// address that fits `u64`, so the largest index is always strictly below
/// `usize::MAX` on every Tier-1 (64-bit) port.
const NIL: usize = usize::MAX;

/// Sentinel for "this frame is not a registered free-block head" in
/// [`FrameAllocatorState::blk_order`]. A real order is `0..=MAX_ORDER`
/// (≤ 13), so `0xFF` never collides.
const NOT_A_HEAD: u8 = 0xFF;

/// One entry of the intrusive per-order free list, indexed by the free
/// block's starting frame.
///
/// Only a block's *start* frame is ever a live node; the interior frames
/// of a block carry stale link values that are never read (the block is
/// found and unlinked by its start alone). Both links are [`NIL`] when
/// the frame is not the head/tail of its order's list.
#[derive(Clone, Copy)]
struct FrameNode {
    /// Next free block of the same order, or [`NIL`].
    next: usize,
    /// Previous free block of the same order, or [`NIL`].
    prev: usize,
}

impl FrameNode {
    const UNLINKED: Self = Self {
        next: NIL,
        prev: NIL,
    };
}

/// Internal, lock-free state of the frame allocator.
///
/// `FrameAllocatorState` is what the [`SpinLock`] in [`FrameAllocator`]
/// guards. Splitting it out makes the locking discipline explicit and
/// lets the unit tests exercise the algorithm without spinlock noise.
///
/// The per-order free lists are intrusive (see the module docs): a heap
/// allocation never occurs on the allocate/free paths, only on the
/// one-time construction of `bitmap`/`nodes`/`blk_order`.
struct FrameAllocatorState {
    /// Address-space extent, in frames: the frame count from physical zero
    /// to the highest mapped address. Reported by [`FrameAllocator::total_frames`]
    /// as a diagnostic; it is **not** the size of any per-frame array — the
    /// bitmap and the intrusive arrays are all sized to the *usable* span
    /// (`span`) and indexed from `base_frame`.
    total_frames: usize,
    /// Lowest usable frame index — the base every per-frame array
    /// (`bitmap`, `nodes`, `blk_order`) is indexed from. A frame `f` maps to
    /// slot `f - base_frame`. Sizing the arrays to the *usable* frame span
    /// rather than to the whole address-space extent keeps the per-frame
    /// bookkeeping proportional to the RAM the machine actually has, so a
    /// map whose usable window sits at a very high physical address does not
    /// cost arrays indexed from frame zero.
    base_frame: usize,
    /// Number of frames the base-relative arrays cover: `hi - base_frame`,
    /// where `hi` is one past the highest usable frame. A frame outside
    /// `[base_frame, base_frame + span)` is not represented and is treated
    /// as "used" by [`Self::bit`] (it is reserved, a hole, or non-existent).
    span: usize,
    /// Bit set = "frame is allocated, reserved, or non-existent", one bit
    /// per frame of the usable span (indexed by `frame - base_frame`).
    bitmap: Vec<u64>,
    /// Intrusive free-list links, one per frame of the usable span (indexed
    /// by `frame - base_frame`). Only a free block's start frame holds live
    /// links.
    nodes: Vec<FrameNode>,
    /// The order a free block starting at this frame is registered at, or
    /// [`NOT_A_HEAD`] (indexed by `frame - base_frame`). Lets an arbitrary
    /// buddy be found and unlinked in O(1) without scanning.
    blk_order: Vec<u8>,
    /// `free_heads[order]` = starting frame of the first free block of
    /// that order, or [`NIL`].
    free_heads: [usize; MAX_ORDER as usize + 1],
    /// Cached count of free frames (sum over the free lists of 2^order).
    free_frames: usize,
    /// Number of whole frames inside `Usable` boot-map regions — the RAM
    /// the allocator can ever hand out. Excludes reserved regions and
    /// physical-address holes (MMIO windows, the space below the RAM base),
    /// so `usable_frames - free_frames` is real allocation, fixed after
    /// construction.
    usable_frames: usize,
    /// Kernel reserve floor, in frames (`usable_frames / RESERVE_DIVISOR`).
    /// A *user* commit ([`FrameAllocator::alloc_user`] /
    /// [`FrameAllocator::alloc_order_user`]) is refused when it would drop
    /// `free_frames` to or below this, so the kernel always keeps headroom
    /// to make progress (heap growth, page-table build, fault service).
    /// Kernel-internal allocations draw the whole pool. Fixed after
    /// construction.
    reserve_frames: usize,
    /// Frames of anonymous/stack user memory that are *committed* (a
    /// successful `mem_map`/stack reservation promised the caller they can
    /// be touched) but not yet resident (never faulted in). Every such page
    /// is guaranteed a frame: a commit is admitted only while
    /// `free_frames >= reserve_frames + committed_frames + request`, and an
    /// eager user allocation may not draw the free pool below
    /// `reserve_frames + committed_frames`. The count therefore reserves
    /// physical headroom *up front* so a first touch of committed memory
    /// can never fail — the deterministic, no-overcommit refusal happens at
    /// reservation time (`Errno::OutOfMemory`) instead of as a fault-time
    /// kill. Decremented as a committed page faults in
    /// ([`FrameAllocator::alloc_user_committed`]) or its reservation is
    /// released ([`FrameAllocator::uncommit`]).
    committed_frames: usize,
}

impl FrameAllocatorState {
    /// The bitmap slot for `frame`, or `None` when `frame` lies outside the
    /// represented usable span `[base_frame, base_frame + span)`.
    #[inline]
    fn bit_slot(&self, frame: usize) -> Option<usize> {
        frame
            .checked_sub(self.base_frame)
            .filter(|&i| i < self.span)
    }

    /// Is `frame` marked used? A frame outside the usable span is not
    /// represented in the bitmap and is treated as used (reserved, a
    /// physical-address hole, or non-existent) — so a buddy probe that
    /// walks off either end of the usable window never merges across it.
    fn bit(&self, frame: usize) -> bool {
        match self.bit_slot(frame) {
            Some(i) => (self.bitmap[i / 64] >> (i % 64)) & 1 == 1,
            None => true,
        }
    }
    /// Mark a usable-span frame used. Only ever called for a frame the
    /// caller has already confirmed is inside the span (an allocated block).
    fn set_bit(&mut self, frame: usize) {
        let i = frame - self.base_frame;
        self.bitmap[i / 64] |= 1u64 << (i % 64);
    }
    /// Mark a usable-span frame free. Only ever called for a frame inside
    /// the span (a usable run at construction, or a freed allocation).
    fn clear_bit(&mut self, frame: usize) {
        let i = frame - self.base_frame;
        self.bitmap[i / 64] &= !(1u64 << (i % 64));
    }

    /// Mark frames `[start, start + count)` as allocated/reserved.
    fn mark_range_used(&mut self, start: usize, count: usize) {
        for i in start..start + count {
            self.set_bit(i);
        }
    }

    /// Are all frames in `[start, start + (1<<order))` free in the bitmap?
    ///
    /// A block that reaches outside the represented usable span is never
    /// "free": those frames are not backed by the bitmap and are treated as
    /// used, so a buddy merge can never fuse a usable block with a
    /// reserved/hole region beyond the window (`bit` returns `true` for
    /// them, but the span check short-circuits before indexing).
    fn block_is_free(&self, start: usize, order: u32) -> bool {
        let n = 1usize << order;
        let Some(rel) = start.checked_sub(self.base_frame) else {
            return false;
        };
        if rel.checked_add(n).is_none_or(|e| e > self.span) {
            return false;
        }
        for i in start..start + n {
            if self.bit(i) {
                return false;
            }
        }
        true
    }

    /// The compact-array slot for `frame`: its offset from `base_frame`.
    /// Every free-block start is a usable frame, so it is always
    /// `>= base_frame` and in range.
    #[inline]
    fn slot(&self, frame: usize) -> usize {
        frame - self.base_frame
    }

    /// Insert a maximally-aligned free block into the buddy lists.
    ///
    /// `start` must be an off-list, aligned block start of `order`; it
    /// becomes the new head of that order's intrusive list.
    fn add_free_block(&mut self, start: usize, order: u32) {
        let head = self.free_heads[order as usize];
        let s = self.slot(start);
        self.nodes[s] = FrameNode {
            next: head,
            prev: NIL,
        };
        if head != NIL {
            let h = self.slot(head);
            self.nodes[h].prev = start;
        }
        self.free_heads[order as usize] = start;
        // `order <= MAX_ORDER` (13) by construction, so it fits a `u8`.
        #[allow(clippy::cast_possible_truncation)]
        {
            self.blk_order[s] = order as u8;
        }
        self.free_frames += 1usize << order;
    }

    /// Unlink the free block starting at `start` from `order`'s list.
    ///
    /// Returns `false` (and changes nothing) when `start` is not a
    /// registered free-block head of exactly `order` — the caller then
    /// knows the buddy is split into smaller blocks and cannot merge.
    fn remove_free_block(&mut self, start: usize, order: u32) -> bool {
        // `start` may be any frame the merge logic probes, including one
        // below `base_frame`; resolve the slot fail-safe.
        let Some(s) = start.checked_sub(self.base_frame) else {
            return false;
        };
        if self.blk_order.get(s).copied().map(u32::from) != Some(order) {
            return false;
        }
        let FrameNode { next, prev } = self.nodes[s];
        if prev == NIL {
            self.free_heads[order as usize] = next;
        } else {
            let p = self.slot(prev);
            self.nodes[p].next = next;
        }
        if next != NIL {
            let n = self.slot(next);
            self.nodes[n].prev = prev;
        }
        self.nodes[s] = FrameNode::UNLINKED;
        self.blk_order[s] = NOT_A_HEAD;
        self.free_frames -= 1usize << order;
        true
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
            if self.free_heads[o as usize] != NIL {
                found = Some(o);
                break;
            }
        }
        let mut cur = found.ok_or(AllocError::OutOfMemory)?;
        // Pop the head block at `cur`. The intrusive list is LIFO, so the
        // head is the most recently freed/split block of this order —
        // deterministic, and O(1).
        let start = self.free_heads[cur as usize];
        debug_assert_ne!(start, NIL);
        let removed = self.remove_free_block(start, cur);
        debug_assert!(removed);

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
        // Range checks: a validly-freed block lies wholly inside the usable
        // span, since it was handed out from a usable run. Checking against
        // the span (not the address-space extent) keeps every subsequent
        // `clear_bit` in bounds.
        let Some(rel) = start.checked_sub(self.base_frame) else {
            return Err(AllocError::OutOfRange);
        };
        if rel.checked_add(n).is_none_or(|e| e > self.span) {
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
            // Refuse to merge across the end of the usable window.
            let parent_start = cur_start & !(1usize << cur_order);
            let past_end = parent_start
                .checked_sub(self.base_frame)
                .and_then(|p| p.checked_add(1usize << (cur_order + 1)))
                .is_none_or(|e| e > self.span);
            if past_end {
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

        // A `usize` frame count can never reach the `NIL` sentinel: valid
        // frame indices are `0..total_frames`, so the largest possible index
        // is `total_frames - 1 < usize::MAX == NIL`. `NIL` therefore never
        // collides with a real index for any representable `total_frames`,
        // and no explicit guard is needed here.

        // 2. Detect overlaps by sorting by start and scanning.
        let mut sorted: Vec<_> = map.regions().to_vec();
        sorted.sort_by_key(|r| r.start.as_u64());
        for win in sorted.windows(2) {
            let a_end = win[0].end().ok_or(AllocError::InvariantViolation)?;
            if a_end.as_u64() > win[1].start.as_u64() {
                return Err(AllocError::InvariantViolation);
            }
        }

        // 3. Pre-pass over the Usable regions: collect the inward-rounded
        //    frame runs (the zero page excluded) and the `[lo, hi)` span of
        //    usable frames. The intrusive free-list arrays are sized to that
        //    span and indexed from `lo`, so a usable window at a very high
        //    physical address costs an array proportional to the RAM it
        //    covers, not one indexed from frame zero.
        let mut runs: Vec<(usize, usize)> = Vec::new();
        let mut lo = usize::MAX;
        let mut hi = 0usize;
        for r in &sorted {
            if r.kind != RegionKind::Usable {
                continue;
            }
            let Some(end) = r.end() else {
                return Err(AllocError::InvariantViolation);
            };
            // Inward rounding: first frame fully inside region, last frame
            // fully inside region (exclusive upper bound).
            let first = usize::try_from(r.start.as_u64().div_ceil(PAGE_SIZE as u64))
                .map_err(|_| AllocError::InvariantViolation)?
                // Never enroll the zero page: its identity translation is
                // the null pointer, so a consumer that draws it must hand it
                // back, and a re-issue of it would wedge allocation forever
                // (the CI-only spawn wedge this guards against). Reserved,
                // like firmware-reserved RAM.
                .max(1);
            let last_excl = usize::try_from(end.as_u64() / PAGE_SIZE as u64)
                .map_err(|_| AllocError::InvariantViolation)?;
            if first >= last_excl {
                continue;
            }
            lo = lo.min(first);
            hi = hi.max(last_excl);
            runs.push((first, last_excl));
        }
        if runs.is_empty() {
            return Err(AllocError::OutOfMemory);
        }
        let base_frame = lo;
        let span = hi - lo;

        // 4. Allocate the per-frame metadata, all sized to the *usable*
        //    span and indexed from `base_frame`: the bitmap (all "1" — any
        //    frame not subsequently marked usable stays reserved), the
        //    intrusive free-list link array, and the per-block order tag.
        //    Sizing every one to the usable span rather than the
        //    address-space extent keeps the bookkeeping proportional to the
        //    RAM the machine actually has, so a usable window at a very high
        //    physical address (or a huge sparse map) costs metadata for the
        //    RAM it covers, not for the address range it sits in. These are
        //    the allocator's *only* heap use, taken once here at
        //    construction; the allocate/free paths touch no allocator but
        //    this one (see the module docs), so the kernel heap can later
        //    grow by drawing frames from here without re-entering itself.
        let words = span.div_ceil(64);
        let bitmap = vec![u64::MAX; words];
        let nodes = vec![FrameNode::UNLINKED; span];
        let blk_order = vec![NOT_A_HEAD; span];
        let mut state = FrameAllocatorState {
            total_frames,
            base_frame,
            span,
            bitmap,
            nodes,
            blk_order,
            free_heads: [NIL; MAX_ORDER as usize + 1],
            free_frames: 0,
            usable_frames: 0,
            reserve_frames: 0,
            committed_frames: 0,
        };

        // 5. Build per-frame state from the collected runs: clear the
        //    bitmap bit of every usable frame and insert the run as buddy
        //    blocks. Reserved frames keep their "used" bit.
        for &(first, last_excl) in &runs {
            for i in first..last_excl {
                state.clear_bit(i);
            }
            state.usable_frames += last_excl - first;
            state.populate_run(first, last_excl);
        }

        // 6. Derive the kernel reserve floor from the discovered RAM (never
        //    a fixed constant): a user commit may not draw the free pool to
        //    or below it, so the kernel keeps headroom to make progress on a
        //    machine a greedy userland is starving.
        state.reserve_frames = state.usable_frames / RESERVE_DIVISOR;

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
    /// This is the **kernel-internal** path: it draws the whole free pool,
    /// including the reserve, so the kernel can always make progress (grow
    /// its heap, build page tables, service a fault) even under user memory
    /// pressure. A *user* commit must use [`Self::alloc_order_user`].
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

    /// Allocate a single frame on behalf of **userland** (reserve-gated).
    ///
    /// # Errors
    ///
    /// [`AllocError::OutOfMemory`] if satisfying it would drop the free
    /// pool to or below the kernel reserve, or if no frame is available.
    pub fn alloc_user(&self) -> Result<Frame, AllocError> {
        self.alloc_order_user(0)
    }

    /// Allocate `2^order` contiguous frames on behalf of **userland**,
    /// refusing when the draw would drop the free pool to or below the
    /// kernel reserve ([`RESERVE_DIVISOR`]).
    ///
    /// A greedy user process therefore fails closed with
    /// [`AllocError::OutOfMemory`] while the kernel still has reserved
    /// headroom to grow its heap and make progress — rather than draining
    /// physical RAM to zero and wedging the kernel. Kernel-internal callers
    /// (including heap growth) use [`Self::alloc_order`] and may draw the
    /// reserve.
    ///
    /// # Errors
    ///
    /// - [`AllocError::SizeUnsupported`] if `order > MAX_ORDER`.
    /// - [`AllocError::OutOfMemory`] if the draw would breach the reserve,
    ///   or if no block of any order ≥ `order` is available.
    pub fn alloc_order_user(&self, order: u32) -> Result<Frame, AllocError> {
        if order > MAX_ORDER {
            return Err(AllocError::SizeUnsupported);
        }
        let n = 1usize << order;
        let mut g = self.inner.lock();
        // Reserve guard: refuse if the draw would leave the free pool at or
        // below the reserve *plus the frames already promised to committed
        // (reserved-but-not-yet-resident) user pages*. Checked before the
        // carve so this eager draw never even transiently dips into kernel
        // headroom or into the physical frames a prior `mem_map`/stack
        // reservation is guaranteeing — an eager user allocation can never
        // steal a committed page's frame, so a committed first touch can
        // never fail closed.
        let floor = g.reserve_frames.saturating_add(g.committed_frames);
        if g.free_frames < n || g.free_frames - n <= floor {
            return Err(AllocError::OutOfMemory);
        }
        g.alloc_order(order).map(Frame)
    }

    /// Reserve physical headroom for `pages` frames of anonymous/stack user
    /// memory that the caller promises the owning task it may later touch,
    /// **without** allocating any frame yet.
    ///
    /// This is the no-overcommit admission control for demand-paged user
    /// memory: the reservation is admitted only while the free pool can
    /// still hold every already-committed page, the kernel reserve, *and*
    /// this request at once (`free_frames >= reserve_frames +
    /// committed_frames + pages`). A first touch of a committed page is
    /// therefore guaranteed a frame ([`Self::alloc_user_committed`]) — the
    /// out-of-memory refusal happens here, deterministically, as a
    /// `Result`, never as a fault-time task kill under overcommit.
    ///
    /// The caller must later balance every admitted page with exactly one
    /// of a committed fault-in ([`Self::alloc_user_committed`], as the page
    /// becomes resident) or an [`Self::uncommit`] (as the reservation is
    /// released while still unbacked), so the count returns to zero when the
    /// region is gone.
    ///
    /// # Errors
    ///
    /// [`AllocError::OutOfMemory`] when the reservation cannot be admitted
    /// without breaching the kernel reserve or a prior commitment.
    pub fn commit(&self, pages: u64) -> Result<(), AllocError> {
        let Ok(pages) = usize::try_from(pages) else {
            // More pages than the address width can count is, a fortiori,
            // more than any machine's RAM: refuse closed.
            return Err(AllocError::OutOfMemory);
        };
        if pages == 0 {
            return Ok(());
        }
        let mut g = self.inner.lock();
        // Admit only while the free pool still covers the kernel reserve,
        // every already-committed page, and this request together. Computed
        // saturating so a momentarily tiny pool can never wrap into a
        // spurious admission.
        let floor = g.reserve_frames.saturating_add(g.committed_frames);
        let headroom = g.free_frames.saturating_sub(floor);
        if pages > headroom {
            return Err(AllocError::OutOfMemory);
        }
        g.committed_frames += pages;
        Ok(())
    }

    /// Release `pages` frames of a commitment made by [`Self::commit`] whose
    /// pages never became resident (an unbacked reservation being torn
    /// down). Saturating, so a double release or a miscount can never wrap
    /// the counter below zero and spuriously admit a later commit.
    pub fn uncommit(&self, pages: u64) {
        let pages = usize::try_from(pages).unwrap_or(usize::MAX);
        let mut g = self.inner.lock();
        g.committed_frames = g.committed_frames.saturating_sub(pages);
    }

    /// Allocate a single frame to make a previously [`committed`](Self::commit)
    /// user page resident (the demand-fault path of anonymous/stack memory).
    ///
    /// Unlike [`Self::alloc_user`] this draws the whole pool — the frame was
    /// already reserved by the matching [`Self::commit`], so the kernel
    /// reserve is not a barrier here — and it converts one committed page
    /// from reserved to resident (decrementing the committed count,
    /// [`Self::committed_frames`]). The
    /// commitment guarantees a frame is available, so this fails only on a
    /// genuine invariant breach.
    ///
    /// # Errors
    ///
    /// [`AllocError::OutOfMemory`] if no frame is available — which, given a
    /// matching prior commit, indicates the commitment invariant was
    /// violated rather than ordinary pressure; the caller still fails closed.
    pub fn alloc_user_committed(&self) -> Result<Frame, AllocError> {
        let mut g = self.inner.lock();
        let frame = g.alloc_order(0).map(Frame)?;
        g.committed_frames = g.committed_frames.saturating_sub(1);
        Ok(frame)
    }

    /// Return a frame taken by [`Self::alloc_user_committed`] **back to its
    /// committed-but-unbacked reservation** (the fault-in unwind path):
    /// free the frame and re-charge one committed page, so a first touch
    /// that allocated a frame but then failed to install its page-table
    /// entry leaves the commitment intact and the counter undrifted. The
    /// reservation is released later, exactly once, by [`Self::uncommit`]
    /// when the region is torn down.
    ///
    /// # Errors
    ///
    /// [`AllocError::InvariantViolation`] for a double-free or a free of an
    /// unowned/misaligned frame (as [`Self::free`]); the commitment is
    /// re-charged only on a successful free.
    pub fn free_committed(&self, frame: Frame) -> Result<(), AllocError> {
        let mut g = self.inner.lock();
        g.free_order(frame.0, 0)?;
        g.committed_frames += 1;
        Ok(())
    }

    /// Frames currently committed to demand-paged user memory but not yet
    /// resident — the reserved-but-untouched headroom
    /// [`Self::commit`]/[`Self::alloc_user_committed`]/[`Self::uncommit`]
    /// track. A diagnostic and the basis of the no-overcommit gate.
    #[must_use]
    pub fn committed_frames(&self) -> FrameCount {
        self.inner.lock().committed_frames
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

    /// Allocate `pages` frames as a *set* of physically-contiguous buddy
    /// chunks, each of order `≤` [`MAX_ORDER`], preferring the largest block
    /// that fits the remainder so a large request costs few chunks.
    ///
    /// This is the path a cross-process shared-memory region draws its
    /// backing from: a region larger than the single-block ceiling
    /// ([`MAX_ORDER`]) is satisfied by several blocks the caller then maps
    /// into one contiguous virtual window, so the region size is bounded by
    /// available RAM rather than a fixed order. A region that fits one block
    /// is returned as a single chunk (the common small case). Each chunk is
    /// returned as `(start_frame, order)` in allocation order.
    ///
    /// Like [`Self::alloc_order`] this is the kernel-internal path and may
    /// draw the reserve. When a block of the preferred order is unavailable
    /// the search steps down one order at a time before giving up, so a
    /// fragmented pool is still satisfied while enough total RAM is free.
    ///
    /// On any failure every chunk already taken is returned to the allocator
    /// before the error propagates, so a failed call leaks nothing and leaves
    /// the free pool unchanged.
    ///
    /// # Errors
    ///
    /// - [`AllocError::SizeUnsupported`] if `pages` is zero.
    /// - [`AllocError::OutOfMemory`] if the request cannot be satisfied even
    ///   after stepping down to single frames, or if the chunk-list
    ///   bookkeeping cannot be grown.
    pub fn alloc_chunks(&self, pages: u64) -> Result<Vec<(Frame, u32)>, AllocError> {
        if pages == 0 {
            return Err(AllocError::SizeUnsupported);
        }
        let mut out: Vec<(Frame, u32)> = Vec::new();
        let mut remaining = pages;
        while remaining > 0 {
            // Largest order whose block fits the remainder, capped at
            // MAX_ORDER. `remaining >= 1` (the loop guard), so `ilog2` is
            // well-defined (it is `floor(log2(remaining))`).
            let fit = remaining.ilog2();
            let mut order = core::cmp::min(fit, MAX_ORDER);
            let (frame, taken) = loop {
                match self.alloc_order(order) {
                    Ok(frame) => break (frame, order),
                    // No block of this order is free; the pool may be
                    // fragmented, so step down one size and retry before
                    // declaring the whole request out of memory.
                    Err(AllocError::OutOfMemory) if order > 0 => order -= 1,
                    Err(e) => {
                        for (f, o) in out.drain(..) {
                            let _ = self.free_order(f, o);
                        }
                        return Err(e);
                    }
                }
            };
            // Grow the chunk list fallibly before recording the block, so a
            // bookkeeping OOM returns every chunk (including this one) rather
            // than aborting.
            if out.try_reserve(1).is_err() {
                let _ = self.free_order(frame, taken);
                for (f, o) in out.drain(..) {
                    let _ = self.free_order(f, o);
                }
                return Err(AllocError::OutOfMemory);
            }
            out.push((frame, taken));
            remaining -= 1u64 << taken;
        }
        Ok(out)
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

    /// The kernel reserve floor, in frames: the free-pool level a user
    /// commit ([`Self::alloc_user`] / [`Self::alloc_order_user`]) may not
    /// draw to or below. Derived from discovered RAM
    /// (`usable_frames / RESERVE_DIVISOR`), fixed after construction.
    #[must_use]
    pub fn reserve_frames(&self) -> FrameCount {
        self.inner.lock().reserve_frames
    }

    /// Invoke `f(base, len)` once for every maximal run of currently-**free**
    /// physical frames, in ascending address order — `base` is the run's
    /// physical start address and `len` its byte length.
    ///
    /// This is the single authority on which physical RAM is safe to
    /// overwrite. A frame is reported only when the allocator has neither
    /// handed it out nor reserved it: every in-use frame (the kernel image
    /// and page tables, the heap, DMA buffers, driver and userland memory),
    /// every reserved region, and every physical-address hole is *excluded*,
    /// because the bitmap marks all of them used.
    ///
    /// The one caller is the pre-boot Supervisor's whole-RAM `memtest`
    /// takeover, which must test only free RAM: writing an in-use frame — a
    /// DMA buffer a device still maps non-cacheably, say — races its owner
    /// and can wedge the machine. Sweeping the free set is the honest
    /// maximum-safe coverage, exactly as a running memtest86 cannot test its
    /// own resident working set.
    ///
    /// `f` runs under the allocator lock, so it must not re-enter the
    /// allocator; it is expected only to copy each `(base, len)` into a
    /// caller-owned buffer.
    pub fn for_each_free_region(&self, mut f: impl FnMut(PhysAddr, u64)) {
        let state = self.inner.lock();
        let base = state.base_frame;
        let span = state.span;
        // Absolute start frame of the free run currently being accumulated.
        let mut run_start: Option<usize> = None;
        let mut rel = 0usize;
        while rel < span {
            let word = state.bitmap[rel / 64];
            let end_rel = (rel + 64).min(span);
            if word == 0 {
                // Whole word free: extend (or open) the current run and skip
                // the rest of the word in one step.
                if run_start.is_none() {
                    run_start = Some(base + rel);
                }
            } else if word == u64::MAX {
                // Whole word used: close any open run at the word boundary.
                if let Some(s) = run_start.take() {
                    emit_free_run(&mut f, s, base + rel);
                }
            } else {
                // Mixed word: decide each frame individually.
                for r in rel..end_rel {
                    if (word >> (r % 64)) & 1 == 1 {
                        if let Some(s) = run_start.take() {
                            emit_free_run(&mut f, s, base + r);
                        }
                    } else if run_start.is_none() {
                        run_start = Some(base + r);
                    }
                }
            }
            rel = end_rel;
        }
        if let Some(s) = run_start {
            emit_free_run(&mut f, s, base + span);
        }
    }
}

/// Report the free frame run `[start_frame, end_frame)` to `f` as a physical
/// `(base, byte_len)` pair.
fn emit_free_run<F: FnMut(PhysAddr, u64)>(f: &mut F, start_frame: usize, end_frame: usize) {
    let base = PhysAddr::new((start_frame as u64) << PAGE_SHIFT);
    let len = ((end_frame - start_frame) as u64) << PAGE_SHIFT;
    f(base, len);
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
    fn for_each_free_region_reports_only_currently_free_frames() {
        let m = small_map(8);
        let a = FrameAllocator::new(&m).unwrap();
        let base = (16 * PAGE_SIZE) as u64;
        let p = PAGE_SIZE as u64;

        // Every frame free: one contiguous run over the whole region.
        let mut runs = Vec::new();
        a.for_each_free_region(|b, l| runs.push((b.as_u64(), l)));
        assert_eq!(runs, std::vec![(base, 8 * p)]);

        // Hand out every frame, then return two non-adjacent ones (by address)
        // so the free set is two isolated single-frame runs and everything
        // handed out is excluded.
        let f: Vec<_> = (0..8).map(|_| a.alloc().unwrap()).collect();
        for &k in &[1u64, 5] {
            let target = base + k * p;
            let fr = *f.iter().find(|fr| fr.start().as_u64() == target).unwrap();
            a.free(fr).unwrap();
        }
        runs.clear();
        a.for_each_free_region(|b, l| runs.push((b.as_u64(), l)));
        assert_eq!(runs, std::vec![(base + p, p), (base + 5 * p, p)]);
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

    /// Total frames across a chunk list.
    fn chunk_frames(chunks: &[(Frame, u32)]) -> u64 {
        chunks.iter().map(|&(_, o)| 1u64 << o).sum()
    }

    #[test]
    fn alloc_chunks_rejects_zero() {
        let m = small_map(4);
        let a = FrameAllocator::new(&m).unwrap();
        assert_eq!(a.alloc_chunks(0).err(), Some(AllocError::SizeUnsupported));
    }

    #[test]
    fn alloc_chunks_of_a_power_of_two_is_a_single_block() {
        let m = small_map(16);
        let a = FrameAllocator::new(&m).unwrap();
        // Four frames is one order-2 block: the common small case stays a
        // single chunk (so the USB URB-buffer path is unaffected).
        let chunks = a.alloc_chunks(4).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].1, 2);
        assert_eq!(a.free_frames(), 16 - 4);
        for (f, o) in chunks {
            a.free_order(f, o).unwrap();
        }
        assert_eq!(a.free_frames(), 16);
    }

    #[test]
    fn alloc_chunks_splits_a_non_power_of_two_into_descending_blocks() {
        let m = small_map(16);
        let a = FrameAllocator::new(&m).unwrap();
        // Six frames = one order-2 block (4) + one order-1 block (2), largest
        // first.
        let chunks = a.alloc_chunks(6).unwrap();
        assert_eq!(chunk_frames(&chunks), 6);
        assert_eq!(chunks.iter().map(|&(_, o)| o).collect::<Vec<_>>(), [2, 1]);
        assert_eq!(a.free_frames(), 16 - 6);
        for (f, o) in chunks {
            a.free_order(f, o).unwrap();
        }
        assert_eq!(a.free_frames(), 16);
    }

    #[test]
    fn alloc_chunks_spans_more_than_one_max_order_block() {
        // A request larger than a single order-`MAX_ORDER` block must be
        // satisfied by several blocks — the whole point of the chunked
        // backing that removes the shared-region ceiling. Sized relative to
        // `MAX_ORDER` so it stays a genuine multi-block request whatever the
        // order ceiling is: one-and-a-half of the largest block.
        let max_block: usize = 1 << MAX_ORDER;
        let want_pages = max_block + max_block / 2;
        let total = want_pages + 100;
        let m = small_map(total);
        let a = FrameAllocator::new(&m).unwrap();
        let want = want_pages as u64;
        let chunks = a.alloc_chunks(want).unwrap();
        assert!(chunks.len() >= 2, "must span multiple blocks");
        assert!(chunks.iter().all(|&(_, o)| o <= MAX_ORDER));
        assert_eq!(chunk_frames(&chunks), want);
        assert_eq!(a.free_frames(), total - want_pages);
        for (f, o) in chunks {
            a.free_order(f, o).unwrap();
        }
        assert_eq!(a.free_frames(), total);
    }

    #[test]
    fn alloc_chunks_frees_every_block_on_exhaustion() {
        let m = small_map(4);
        let a = FrameAllocator::new(&m).unwrap();
        // Nine frames cannot be satisfied by four: the partial progress (an
        // order-2 block over the whole pool) is returned and the call fails
        // closed, leaving the pool exactly as it was found.
        assert_eq!(a.alloc_chunks(9).err(), Some(AllocError::OutOfMemory));
        assert_eq!(a.free_frames(), 4);
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
    fn usable_window_at_a_high_base_uses_span_sized_metadata() {
        // Regression: the intrusive free-list arrays are indexed from the
        // lowest usable frame, not from frame zero, so a usable window at a
        // high physical base costs metadata proportional to the window, not
        // to its address. A region of 32 frames based ~4 GiB up must build,
        // allocate, split, merge, and account exactly like a low-based one —
        // this exercises the `base_frame` offset (`slot`) on every path.
        let base = 1_000_000usize; // ~3.8 GiB up, far from frame 0
        let mut m = BootMemoryMap::new();
        m.push(MemoryRegion {
            start: PhysAddr::new((base * PAGE_SIZE) as u64),
            length: (32 * PAGE_SIZE) as u64,
            kind: RegionKind::Usable,
        });
        let a = FrameAllocator::new(&m).unwrap();
        assert_eq!(a.usable_frames(), 32);
        assert_eq!(a.free_frames(), 32);

        // Order-2 blocks split from the population, then merge back.
        let b0 = a.alloc_order(2).unwrap();
        let b1 = a.alloc_order(2).unwrap();
        assert!(
            b0.0 >= base && b1.0 >= base,
            "blocks lie in the high window"
        );
        assert_eq!(b0.0 & 3, 0, "order-2 block is 4-frame aligned");
        a.free_order(b0, 2).unwrap();
        a.free_order(b1, 2).unwrap();
        assert_eq!(a.free_frames(), 32);

        // Drain to single frames (exercises the slot offset per frame),
        // then free them all back and coalesce to the largest block.
        let mut held = Vec::new();
        while let Ok(f) = a.alloc() {
            assert!(f.0 >= base && f.0 < base + 32);
            held.push(f);
        }
        assert_eq!(held.len(), 32);
        for f in held {
            a.free(f).unwrap();
        }
        assert_eq!(a.free_frames(), 32);
        let big = a.alloc_order(5).unwrap(); // 32 frames
        assert_eq!(big.0, base, "the whole window coalesced back");
        a.free_order(big, 5).unwrap();
        assert_eq!(a.free_frames(), 32);
    }

    #[test]
    fn alloc_user_stops_at_reserve_but_kernel_may_draw_it() {
        // Enough usable RAM for a non-zero reserve
        // (`usable / RESERVE_DIVISOR`); `small_map` bases the window clear of
        // the zero page, so all `usable` frames are usable.
        let usable = 4 * RESERVE_DIVISOR;
        let m = small_map(usable);
        let a = FrameAllocator::new(&m).unwrap();
        let reserve = a.reserve_frames();
        assert_eq!(reserve, usable / RESERVE_DIVISOR);
        assert!(reserve > 0, "test needs a non-zero reserve");

        // User commits succeed until one more would drop the free pool to or
        // below the reserve; the last success leaves exactly `reserve + 1`.
        let mut held = Vec::new();
        while let Ok(f) = a.alloc_user() {
            held.push(f);
        }
        assert_eq!(a.free_frames(), reserve + 1);
        assert_eq!(a.alloc_user().err(), Some(AllocError::OutOfMemory));

        // The kernel-internal path may draw into the reserve, all the way to
        // exhaustion — the kernel always keeps the ability to make progress.
        while let Ok(f) = a.alloc() {
            held.push(f);
        }
        assert_eq!(a.free_frames(), 0);

        for f in held {
            a.free(f).unwrap();
        }
        assert_eq!(a.free_frames(), usable);
    }

    #[test]
    fn commit_reserves_headroom_and_refuses_the_overcommit() {
        // A commit is admitted only while the free pool can still hold the
        // kernel reserve, every prior commitment, and this request at once.
        let usable = 4 * RESERVE_DIVISOR;
        let m = small_map(usable);
        let a = FrameAllocator::new(&m).unwrap();
        let reserve = a.reserve_frames();
        assert!(reserve > 0, "test needs a non-zero reserve");

        // The whole non-reserve pool may be committed, one page at a time,
        // but not one page more: the last admissible commit leaves exactly
        // `reserve` frames of guaranteed kernel headroom uncommitted.
        let committable = usable - reserve;
        for _ in 0..committable {
            a.commit(1).expect("within the no-overcommit budget");
        }
        assert_eq!(a.committed_frames(), committable);
        assert_eq!(
            a.commit(1).err(),
            Some(AllocError::OutOfMemory),
            "one page past the budget is refused as a Result, not overcommitted"
        );
        // A single bulk request for the whole budget behaves identically.
        a.uncommit(committable as u64);
        assert_eq!(a.committed_frames(), 0);
        a.commit(committable as u64)
            .expect("bulk commit of the budget");
        assert_eq!(
            a.commit(1).err(),
            Some(AllocError::OutOfMemory),
            "the budget is the budget however it is split"
        );
    }

    #[test]
    fn committed_pages_are_guaranteed_a_frame_and_eager_draws_respect_them() {
        // Every committed page can always be made resident, and an eager
        // user draw can never steal a committed page's reserved frame.
        let usable = 4 * RESERVE_DIVISOR;
        let m = small_map(usable);
        let a = FrameAllocator::new(&m).unwrap();
        let reserve = a.reserve_frames();
        let committable = usable - reserve;

        // Commit the entire non-reserve budget up front.
        a.commit(committable as u64).expect("commit the budget");
        // An eager user allocation must now be refused outright: every
        // non-reserve frame is promised to a committed page.
        assert_eq!(
            a.alloc_user().err(),
            Some(AllocError::OutOfMemory),
            "an eager draw cannot dip into committed headroom"
        );
        // Yet every committed page can still be faulted in — the frames were
        // reserved for exactly this — until the commitment is exhausted.
        let mut held = Vec::new();
        for _ in 0..committable {
            held.push(
                a.alloc_user_committed()
                    .expect("committed page is guaranteed a frame"),
            );
        }
        assert_eq!(
            a.committed_frames(),
            0,
            "every commitment converted to residency"
        );
        assert_eq!(a.free_frames(), reserve, "only the kernel reserve remains");

        for f in held {
            a.free(f).unwrap();
        }
        assert_eq!(a.free_frames(), usable);
    }

    #[test]
    fn uncommit_is_saturating_and_releases_headroom() {
        let usable = 4 * RESERVE_DIVISOR;
        let m = small_map(usable);
        let a = FrameAllocator::new(&m).unwrap();
        a.commit(3).expect("commit three");
        assert_eq!(a.committed_frames(), 3);
        // An over-release cannot wrap the counter below zero.
        a.uncommit(10);
        assert_eq!(a.committed_frames(), 0);
        // A zero-page commit is a trivial success and changes nothing.
        a.commit(0).expect("zero commit");
        assert_eq!(a.committed_frames(), 0);
    }

    #[test]
    fn high_base_map_bitmap_is_proportional_to_ram_not_address() {
        // Regression: the bitmap, like the intrusive arrays, is sized to the
        // usable span and indexed from `base_frame`. A small usable window
        // sitting ~128 TiB up (huge-address territory) must cost bitmap
        // metadata proportional to the window (one word for 64 frames), not
        // to the physical address it sits at — indexed from frame zero that
        // bitmap alone would be ~4 GiB.
        let base = 128usize * 1024 * 1024 * 1024 * 1024 / PAGE_SIZE; // 128 TiB
        let frames = 64usize;
        let mut m = BootMemoryMap::new();
        m.push(MemoryRegion {
            start: PhysAddr::new((base * PAGE_SIZE) as u64),
            length: (frames * PAGE_SIZE) as u64,
            kind: RegionKind::Usable,
        });
        let a = FrameAllocator::new(&m).unwrap();
        assert_eq!(a.usable_frames(), frames);
        assert_eq!(a.free_frames(), frames);

        // Span-sized: 64 frames → one 64-bit word. Address-extent sizing
        // would be hundreds of millions of words.
        let words = a.inner.lock().bitmap.len();
        assert_eq!(words, frames.div_ceil(64));

        // `total_frames` still reports the address-space extent (a
        // diagnostic), proving the small bitmap is not merely a small map.
        assert!(a.total_frames() >= base);

        // Alloc/free still works correctly in the high window.
        let f = a.alloc().unwrap();
        assert!(f.0 >= base && f.0 < base + frames);
        a.free(f).unwrap();
        assert_eq!(a.free_frames(), frames);
        let big = a.alloc_order(6).unwrap(); // 64 frames
        assert_eq!(big.0, base, "the whole window coalesced back");
        a.free_order(big, 6).unwrap();
        assert_eq!(a.free_frames(), frames);
    }

    #[test]
    fn intrusive_list_unlinks_a_middle_block_on_merge() {
        // Regression for the heap-independent intrusive free lists: a buddy
        // merge must be able to unlink a free block that is *not* the head
        // of its order's list (the doubly-linked `prev`/`next` splice). Set
        // up three order-0 free blocks, then free a fourth whose buddy is
        // the middle one, forcing a middle-of-list removal, and confirm the
        // whole window merges back to a single largest block with no leak
        // or double-count.
        let m = small_map(16);
        let a = FrameAllocator::new(&m).unwrap();
        // Drain to individual frames so every order-0 block is on the list.
        let mut frames = Vec::new();
        while let Ok(f) = a.alloc() {
            frames.push(f);
        }
        assert_eq!(frames.len(), 16);
        // Free odd-indexed frames first, then even ones: when an even
        // frame is later freed its odd buddy is already an interior list
        // node, so the merge must splice it out of the middle of the
        // order-0 list.
        for f in frames.iter().filter(|f| f.0 % 2 == 1) {
            a.free(*f).unwrap();
        }
        for f in frames.iter().filter(|f| f.0 % 2 == 0) {
            a.free(*f).unwrap();
        }
        assert_eq!(a.free_frames(), 16);
        // Fully coalesced: the largest single block the map allows is
        // available again.
        let big = a.alloc_order(4).unwrap();
        assert_eq!(big.0 & 15, 0, "order-4 block is 16-frame aligned");
        a.free_order(big, 4).unwrap();
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
