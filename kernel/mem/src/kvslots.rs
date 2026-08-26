//! Heap-free page-slot allocator for the kernel remap window.
//!
//! The growable kernel heap assembles each fresh region out of scattered
//! physical chunks mapped into one virtually-contiguous run of the kernel
//! remap window ([`crate::kvmap`]). Choosing that run is a placement
//! decision, and it carries one constraint the sibling placement allocators
//! do not: it runs *inside* the kernel heap's own growth source, under that
//! heap's non-reentrant lock, so it must not allocate from it. [`crate::anon_window::AnonWindowMap`] keeps its bookkeeping in
//! `BTreeMap`s and would deadlock here; this allocator is its heap-free
//! counterpart.
//!
//! # Structure
//!
//! One address-sorted boundary-tag list covers `[0, cursor)` exactly and
//! without gaps, each entry recording a run of slots and whether it is
//! live; untouched space above the cursor needs no entry at all. Allocation
//! is first-fit over the free entries, then the cursor; release flips an
//! *exactly matching* live entry free, coalesces it with free neighbours,
//! and retracts the cursor when the result reaches it — so a heap that
//! grows and fully drains leaves the window pristine, holding no
//! bookkeeping.
//!
//! The entry records are drawn from the physical [`FrameAllocator`] and
//! reached through the direct map — the same trick the frame allocator uses
//! to stay heap-independent. Record storage is therefore bounded by the
//! number of live plus freed runs, never by the window's page count (a
//! per-page bitmap would cap the window or waste memory on a large
//! machine), and it grows a frame at a time on demand rather than sitting
//! behind a hand-picked ceiling.
//!
//! Both operations walk the entry list, so they cost O(live + freed runs).
//! That is never the dominant term for the one caller: every page of a
//! reserved run costs a page-table walk to map, and the growth granule is
//! several pages, so the mapping work a grow performs already exceeds the
//! list walk that chose its address.

use core::ptr::NonNull;

use crate::frame::{Frame, FrameAllocator, PAGE_SIZE};
use crate::phys::PhysMap;

/// Why a slot reservation or release was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotError {
    /// The request was for zero pages.
    ZeroLength,
    /// No free entry and no remaining space above the cursor can satisfy
    /// the request: the window's address space is exhausted.
    WindowExhausted,
    /// An entry record could not be created because the frame allocator is
    /// exhausted or the frame it offered lies outside the direct map.
    RecordsExhausted,
    /// The released run is not a live run this allocator handed out with
    /// exactly that extent.
    NotReserved,
}

/// One boundary-tag entry, or one link in the record arena's frame chain.
///
/// The two uses share a layout deliberately: a drawn frame spends its first
/// record linking itself into the arena chain (carrying the frame's
/// physical address in `start`), and the rest of the frame is carved into
/// records the entry list uses. One type, one size class, no second
/// allocator.
#[repr(C)]
struct Node {
    /// First page slot of the run — or, for an arena chain link, the index
    /// of the frame this record lives in.
    start: usize,
    /// Page count of the run. Unused in an arena chain link.
    len: usize,
    /// Next record in whichever list this one is currently on.
    next: Option<NonNull<Node>>,
    /// `true` while the run is handed out. Unused in an arena chain link.
    live: bool,
}

/// Records that fit in one frame.
const RECORDS_PER_FRAME: usize = PAGE_SIZE / core::mem::size_of::<Node>();

/// Frame-backed record source for the entry list.
///
/// Draws one physical frame at a time through the kernel commit path,
/// reaches it via the direct map, and carves it into [`Node`] records: the
/// first links the frame into `chain` (so [`Drop`] can hand every frame
/// back), the remainder are bump-allocated and then recycled through
/// `free`. No global-heap allocation on any path.
struct RecordArena {
    frames: &'static FrameAllocator,
    phys: &'static (dyn PhysMap + Sync),
    /// Frames drawn so far, linked through each frame's first record.
    chain: Option<NonNull<Node>>,
    /// Recycled records available for immediate reuse.
    free: Option<NonNull<Node>>,
    /// Next never-yet-used record in the current frame.
    bump: Option<NonNull<Node>>,
    /// How many records remain from `bump` onward, inclusive.
    bump_left: usize,
}

impl RecordArena {
    const fn new(frames: &'static FrameAllocator, phys: &'static (dyn PhysMap + Sync)) -> Self {
        Self {
            frames,
            phys,
            chain: None,
            free: None,
            bump: None,
            bump_left: 0,
        }
    }

    /// Take a record, drawing a fresh frame when neither the recycled list
    /// nor the current frame can supply one.
    fn take(&mut self) -> Result<NonNull<Node>, SlotError> {
        if let Some(node) = self.free {
            // SAFETY: `free` holds only records this arena carved out of a
            // frame it owns and that no other list currently names.
            self.free = unsafe { node.as_ref().next };
            return Ok(node);
        }
        if self.bump_left == 0 {
            self.grow()?;
        }
        let Some(node) = self.bump else {
            return Err(SlotError::RecordsExhausted);
        };
        self.bump_left -= 1;
        self.bump = if self.bump_left == 0 {
            None
        } else {
            // SAFETY: `grow` proved the whole frame is direct-mapped and
            // sized it in whole records, so advancing one record while
            // `bump_left` remain stays inside that frame.
            Some(unsafe { NonNull::new_unchecked(node.as_ptr().add(1)) })
        };
        Ok(node)
    }

    /// Return a record for reuse.
    fn give(&mut self, mut node: NonNull<Node>) {
        // SAFETY: `node` came from `take`, so it names a record in a frame
        // this arena owns, and the caller has unlinked it from every list.
        unsafe {
            node.as_mut().next = self.free;
        }
        self.free = Some(node);
    }

    /// Draw one frame and carve it into records.
    fn grow(&mut self) -> Result<(), SlotError> {
        // The kernel commit path, never the reserve-gated user one: this
        // runs on the heap-growth path, which must make progress under user
        // memory pressure.
        let frame = self
            .frames
            .alloc()
            .map_err(|_| SlotError::RecordsExhausted)?;
        let Some(ptr) = self.phys.translate(frame.start(), PAGE_SIZE) else {
            // Outside the direct map: hand it straight back rather than
            // fabricate a pointer. The frame was just drawn, so the free
            // cannot legitimately fail.
            let _ = self.frames.free(frame);
            return Err(SlotError::RecordsExhausted);
        };
        // A page-aligned physical address translates to a page-aligned
        // direct-map pointer, which is aligned for `Node`; the lint cannot
        // see that invariant.
        #[allow(clippy::cast_ptr_alignment)]
        let base = ptr.as_ptr().cast::<Node>();
        // SAFETY: `translate` proved the whole 4 KiB frame is mapped and
        // writable, the frame was just handed out so nothing else names it,
        // the pointer is aligned for `Node` (above), and
        // `RECORDS_PER_FRAME` records fit by construction. The first record
        // becomes the chain link naming the frame it lives in; the rest are
        // handed out by `take`.
        unsafe {
            base.write(Node {
                start: frame.0,
                len: 0,
                next: self.chain,
                live: false,
            });
            self.chain = Some(NonNull::new_unchecked(base));
            self.bump = Some(NonNull::new_unchecked(base.add(1)));
        }
        self.bump_left = RECORDS_PER_FRAME - 1;
        Ok(())
    }

    /// Frames currently held for record storage.
    fn frames_held(&self) -> usize {
        let mut count = 0;
        let mut cur = self.chain;
        while let Some(node) = cur {
            count += 1;
            // SAFETY: `chain` links records this arena wrote.
            cur = unsafe { node.as_ref().next };
        }
        count
    }
}

impl Drop for RecordArena {
    fn drop(&mut self) {
        // Hand every drawn frame back. In production the window lives for
        // the life of the image and this never runs; the host tests rely on
        // it to prove the arena leaks no frame.
        let mut cur = self.chain.take();
        while let Some(node) = cur {
            // SAFETY: each chain link is a record this arena wrote, whose
            // `start` names the frame holding it.
            let (index, next) = unsafe { (node.as_ref().start, node.as_ref().next) };
            cur = next;
            let _ = self.frames.free(Frame(index));
        }
    }
}

/// Page-slot allocator over a fixed-capacity window.
///
/// Slots are 0-based page indices into the window; the caller turns a slot
/// into a virtual address. See the module docs for the structure and its
/// cost.
pub struct SlotWindow {
    capacity: usize,
    /// Lowest slot never yet handed out; every slot below it belongs to
    /// exactly one entry.
    cursor: usize,
    /// Address-sorted boundary-tag list covering `[0, cursor)`.
    entries: Option<NonNull<Node>>,
    /// Slots currently reserved — the sum of every live entry.
    reserved: usize,
    arena: RecordArena,
}

// SAFETY: every `NonNull<Node>` a `SlotWindow` holds points into a frame
// its own arena drew and no other code names; the records are reachable
// only through this window's lists. The type exposes no interior
// mutability, so moving one between threads under external serialisation
// (the kernel heap's own lock, its only driver) carries the whole graph.
unsafe impl Send for SlotWindow {}

impl SlotWindow {
    /// Build an allocator over `capacity` page slots, drawing entry records
    /// from `frames` through the direct map `phys`.
    ///
    /// # Errors
    ///
    /// [`SlotError::ZeroLength`] when `capacity` is zero.
    pub const fn new(
        capacity: usize,
        frames: &'static FrameAllocator,
        phys: &'static (dyn PhysMap + Sync),
    ) -> Result<Self, SlotError> {
        if capacity == 0 {
            return Err(SlotError::ZeroLength);
        }
        Ok(Self {
            capacity,
            cursor: 0,
            entries: None,
            reserved: 0,
            arena: RecordArena::new(frames, phys),
        })
    }

    /// Reserve `pages` consecutive slots and return the first.
    ///
    /// Serves the first free entry large enough (splitting a longer one)
    /// before advancing the cursor, so a steady grow/shrink workload reuses
    /// address space instead of marching through the window.
    ///
    /// # Errors
    ///
    /// * [`SlotError::ZeroLength`] when `pages` is zero.
    /// * [`SlotError::WindowExhausted`] when neither a free entry nor the
    ///   space above the cursor can hold the request.
    /// * [`SlotError::RecordsExhausted`] when the entry the reservation
    ///   needs cannot be created. Nothing is reserved in any error case.
    pub fn allocate(&mut self, pages: usize) -> Result<usize, SlotError> {
        if pages == 0 {
            return Err(SlotError::ZeroLength);
        }

        // One walk finds both the first fitting free entry and the tail the
        // cursor path appends to.
        let mut prev: Option<NonNull<Node>> = None;
        let mut cur = self.entries;
        while let Some(mut node) = cur {
            // SAFETY: `node` is a live record in this window's entry list,
            // reachable only through `&mut self`.
            let (start, len, live, next) = unsafe {
                let entry = node.as_ref();
                (entry.start, entry.len, entry.live, entry.next)
            };
            if !live && len >= pages {
                if len == pages {
                    // SAFETY: as above — this window's own record.
                    unsafe {
                        node.as_mut().live = true;
                    }
                } else {
                    // Split: a fresh live entry takes the front and the
                    // existing free entry keeps the remainder. The record is
                    // drawn before any mutation, so exhaustion changes
                    // nothing.
                    let record = self.arena.take()?;
                    // SAFETY: `node` is this window's record; `record` was
                    // just drawn and is named by nothing else.
                    unsafe {
                        let entry = node.as_mut();
                        entry.start = start + pages;
                        entry.len = len - pages;
                        Self::write(record, start, pages, true, cur);
                    }
                    self.link(prev, Some(record));
                }
                self.reserved += pages;
                return Ok(start);
            }
            prev = cur;
            cur = next;
        }

        let end = self
            .cursor
            .checked_add(pages)
            .ok_or(SlotError::WindowExhausted)?;
        if end > self.capacity {
            return Err(SlotError::WindowExhausted);
        }
        let record = self.arena.take()?;
        let start = self.cursor;
        // SAFETY: `record` was just drawn and is named by nothing else.
        unsafe {
            Self::write(record, start, pages, true, None);
        }
        self.link(prev, Some(record));
        self.cursor = end;
        self.reserved += pages;
        Ok(start)
    }

    /// Release the live run `[slot, slot + pages)`.
    ///
    /// The extent must match a reservation exactly: a partial or unknown
    /// run frees nothing, so a miscounted caller can never hand a live
    /// region's address space back for reuse.
    ///
    /// # Errors
    ///
    /// * [`SlotError::ZeroLength`] when `pages` is zero.
    /// * [`SlotError::NotReserved`] when no live entry matches exactly.
    pub fn release(&mut self, slot: usize, pages: usize) -> Result<(), SlotError> {
        if pages == 0 {
            return Err(SlotError::ZeroLength);
        }

        let mut prev_prev: Option<NonNull<Node>> = None;
        let mut prev: Option<NonNull<Node>> = None;
        let mut cur = self.entries;
        while let Some(node) = cur {
            // SAFETY: `node` is a live record in this window's entry list.
            let (start, len, live, next) = unsafe {
                let entry = node.as_ref();
                (entry.start, entry.len, entry.live, entry.next)
            };
            if start == slot {
                if !live || len != pages {
                    return Err(SlotError::NotReserved);
                }
                break;
            }
            if start > slot {
                return Err(SlotError::NotReserved);
            }
            prev_prev = prev;
            prev = cur;
            cur = next;
        }
        let Some(mut node) = cur else {
            return Err(SlotError::NotReserved);
        };

        self.reserved -= pages;
        // SAFETY: `node` is this window's live record for the matched run.
        unsafe {
            node.as_mut().live = false;
        }

        // Absorb a free successor.
        // SAFETY: reading this window's own records.
        let next = unsafe { node.as_ref().next };
        if let Some(successor) = next {
            // SAFETY: `successor` is the next record in this window's list.
            let (s_len, s_live, s_next) = unsafe {
                let entry = successor.as_ref();
                (entry.len, entry.live, entry.next)
            };
            if !s_live {
                // SAFETY: both records belong to this window and the
                // successor is being unlinked here.
                unsafe {
                    let entry = node.as_mut();
                    entry.len += s_len;
                    entry.next = s_next;
                }
                self.arena.give(successor);
            }
        }

        // Merge into a free predecessor, which then becomes the entry.
        let mut owner_prev = prev;
        if let Some(mut predecessor) = prev {
            // SAFETY: `predecessor` is this window's preceding record.
            let p_live = unsafe { predecessor.as_ref().live };
            if !p_live {
                // SAFETY: both records belong to this window and `node` is
                // being unlinked into its predecessor here.
                unsafe {
                    let merged = node.as_ref();
                    let entry = predecessor.as_mut();
                    entry.len += merged.len;
                    entry.next = merged.next;
                }
                self.arena.give(node);
                node = predecessor;
                owner_prev = prev_prev;
            }
        }

        // A free tail entry is not bookkeeping worth keeping: retract the
        // cursor over it. The entry below a free tail is always live (two
        // adjacent free entries would have merged), so the invariant that
        // the list covers `[0, cursor)` holds afterwards.
        // SAFETY: reading this window's own record.
        let (start, is_tail) = unsafe {
            let entry = node.as_ref();
            (entry.start, entry.next.is_none())
        };
        if is_tail {
            self.cursor = start;
            self.link(owner_prev, None);
            self.arena.give(node);
        }
        Ok(())
    }

    /// Total slots the window spans.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Slots currently reserved by live runs.
    #[must_use]
    pub const fn reserved(&self) -> usize {
        self.reserved
    }

    /// Lowest slot never yet handed out.
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// Frames the entry-record arena currently holds.
    #[must_use]
    pub fn record_frames(&self) -> usize {
        self.arena.frames_held()
    }

    /// `(live, free)` entry counts. A fully drained window has neither.
    #[must_use]
    pub fn entry_counts(&self) -> (usize, usize) {
        let mut live = 0;
        let mut free = 0;
        let mut cur = self.entries;
        while let Some(node) = cur {
            // SAFETY: `entries` links records this window owns.
            let entry = unsafe { node.as_ref() };
            if entry.live {
                live += 1;
            } else {
                free += 1;
            }
            cur = entry.next;
        }
        (live, free)
    }

    /// Point `prev`'s successor (or the list head) at `target`.
    fn link(&mut self, prev: Option<NonNull<Node>>, target: Option<NonNull<Node>>) {
        match prev {
            // SAFETY: `p` is a live record in this window's entry list.
            Some(mut p) => unsafe {
                p.as_mut().next = target;
            },
            None => self.entries = target,
        }
    }

    /// Initialise a freshly drawn record.
    ///
    /// # Safety
    ///
    /// `record` must be a record the caller has exclusive use of and that
    /// no list currently names.
    unsafe fn write(
        mut record: NonNull<Node>,
        start: usize,
        len: usize,
        live: bool,
        next: Option<NonNull<Node>>,
    ) {
        // SAFETY: the caller guarantees exclusive use of `record`.
        unsafe {
            *record.as_mut() = Node {
                start,
                len,
                next,
                live,
            };
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::bootinfo::{BootMemoryMap, MemoryRegion, RegionKind};
    use crate::frame::PhysAddr;
    use crate::phys::SimPhysMap;
    use alloc::boxed::Box;

    const RAM_BASE: u64 = 0x40_0000;

    /// Leak a frame allocator plus a matching simulated direct map over
    /// `pages` of RAM — the host stand-in for the kernel globals these are
    /// in production.
    fn backing(pages: usize) -> (&'static FrameAllocator, &'static (dyn PhysMap + Sync)) {
        let mut map = BootMemoryMap::new();
        map.push(MemoryRegion {
            start: PhysAddr::new(RAM_BASE),
            length: (pages * PAGE_SIZE) as u64,
            kind: RegionKind::Usable,
        });
        let frames: &'static FrameAllocator =
            Box::leak(Box::new(FrameAllocator::new(&map).expect("allocator")));
        let sim: &'static SimPhysMap = Box::leak(Box::new(SimPhysMap::new(
            PhysAddr::new(RAM_BASE),
            pages * PAGE_SIZE,
        )));
        (frames, sim)
    }

    fn window(capacity: usize) -> (SlotWindow, &'static FrameAllocator) {
        let (frames, sim) = backing(64);
        (
            SlotWindow::new(capacity, frames, sim).expect("non-zero capacity"),
            frames,
        )
    }

    #[test]
    fn new_refuses_an_empty_window() {
        let (frames, sim) = backing(4);
        assert_eq!(
            SlotWindow::new(0, frames, sim).err(),
            Some(SlotError::ZeroLength)
        );
    }

    #[test]
    fn allocations_advance_the_cursor_without_overlapping() {
        let (mut w, _frames) = window(32);
        assert_eq!(w.allocate(4), Ok(0));
        assert_eq!(w.allocate(8), Ok(4));
        assert_eq!(w.allocate(1), Ok(12));
        assert_eq!(w.reserved(), 13);
        assert_eq!(w.cursor(), 13);
        assert_eq!(w.entry_counts(), (3, 0));
    }

    #[test]
    fn allocate_refuses_zero_and_fails_closed_when_exhausted() {
        let (mut w, _frames) = window(8);
        assert_eq!(w.allocate(0), Err(SlotError::ZeroLength));
        assert_eq!(w.allocate(8), Ok(0));
        assert_eq!(w.allocate(1), Err(SlotError::WindowExhausted));
        assert_eq!(w.reserved(), 8);
    }

    #[test]
    fn allocate_refuses_a_request_larger_than_the_window() {
        let (mut w, _frames) = window(4);
        assert_eq!(w.allocate(5), Err(SlotError::WindowExhausted));
        assert_eq!(w.allocate(usize::MAX), Err(SlotError::WindowExhausted));
        assert_eq!(w.reserved(), 0);
    }

    #[test]
    fn a_released_middle_run_is_reused_first_fit_and_split() {
        let (mut w, _frames) = window(32);
        let a = w.allocate(4).expect("fits");
        let b = w.allocate(8).expect("fits");
        let _c = w.allocate(4).expect("fits");
        w.release(b, 8).expect("live run");
        assert_eq!(w.entry_counts(), (2, 1));

        // First fit takes the freed run and splits it.
        assert_eq!(w.allocate(3), Ok(b));
        assert_eq!(w.allocate(5), Ok(b + 3));
        assert_eq!(
            w.entry_counts(),
            (4, 0),
            "the free run was consumed exactly"
        );
        assert_eq!(a, 0);
        assert_eq!(w.cursor(), 16, "reuse did not advance the cursor");
    }

    #[test]
    fn releasing_around_a_free_run_coalesces_both_sides() {
        let (mut w, _frames) = window(64);
        let a = w.allocate(4).expect("fits");
        let b = w.allocate(4).expect("fits");
        let c = w.allocate(4).expect("fits");
        let _tail = w.allocate(4).expect("fits");

        w.release(a, 4).expect("live");
        w.release(c, 4).expect("live");
        assert_eq!(w.entry_counts(), (2, 2));
        // The middle run bridges the two free runs into one 12-slot run.
        w.release(b, 4).expect("live");
        assert_eq!(w.entry_counts(), (1, 1));
        assert_eq!(w.allocate(12), Ok(a), "the merged run serves 12 slots");
    }

    #[test]
    fn draining_the_window_retracts_the_cursor_and_frees_every_record() {
        let (mut w, frames) = window(64);
        let free_before = frames.free_frames();
        let a = w.allocate(6).expect("fits");
        let b = w.allocate(6).expect("fits");
        assert_eq!(w.record_frames(), 1, "one frame backs the entry records");

        // Release out of order so the free run has to be coalesced.
        w.release(a, 6).expect("live");
        assert_eq!(w.entry_counts(), (1, 1));
        w.release(b, 6).expect("live");
        assert_eq!(w.entry_counts(), (0, 0), "coalescing reached the cursor");
        assert_eq!(w.reserved(), 0);
        assert_eq!(w.cursor(), 0);
        // The whole window is available again from slot 0.
        assert_eq!(w.allocate(64), Ok(0));

        drop(w);
        assert_eq!(
            frames.free_frames(),
            free_before,
            "the record arena returned every frame it drew"
        );
    }

    #[test]
    fn releasing_the_tail_retracts_the_cursor_over_it() {
        let (mut w, _frames) = window(64);
        let a = w.allocate(4).expect("fits");
        let b = w.allocate(4).expect("fits");
        w.release(b, 4).expect("live");
        assert_eq!(w.cursor(), 4, "the tail run gave its slots straight back");
        assert_eq!(w.entry_counts(), (1, 0));
        w.release(a, 4).expect("live");
        assert_eq!(w.cursor(), 0);
        assert_eq!(w.entry_counts(), (0, 0));
    }

    #[test]
    fn release_rejects_an_unknown_partial_or_repeated_run() {
        let (mut w, _frames) = window(32);
        let a = w.allocate(4).expect("fits");
        let _b = w.allocate(4).expect("fits");
        assert_eq!(w.release(a, 8), Err(SlotError::NotReserved), "too long");
        assert_eq!(w.release(a, 2), Err(SlotError::NotReserved), "too short");
        assert_eq!(w.release(a + 1, 3), Err(SlotError::NotReserved), "interior");
        assert_eq!(w.release(16, 1), Err(SlotError::NotReserved), "never given");
        assert_eq!(w.release(a, 0), Err(SlotError::ZeroLength));
        assert_eq!(w.reserved(), 8, "a refused release frees nothing");

        w.release(a, 4).expect("the matching release succeeds");
        assert_eq!(
            w.release(a, 4),
            Err(SlotError::NotReserved),
            "a double release is refused"
        );
        assert_eq!(w.reserved(), 4);
    }

    #[test]
    fn entry_records_are_recycled_rather_than_redrawn() {
        let (mut w, frames) = window(4096);
        let mut live = [0usize; 8];
        for slot in &mut live {
            *slot = w.allocate(2).expect("fits");
        }
        // Free every other run so each becomes its own free entry.
        for slot in live.iter().step_by(2) {
            w.release(*slot, 2).expect("live");
        }
        assert_eq!(w.entry_counts(), (4, 4));
        let frames_after = frames.free_frames();

        // Refilling recycles the records; no fresh frame is drawn.
        for _ in 0..4 {
            assert!(w.allocate(2).is_ok());
        }
        assert_eq!(w.entry_counts(), (8, 0));
        assert_eq!(frames.free_frames(), frames_after);
        assert_eq!(w.record_frames(), 1);
    }

    #[test]
    fn the_record_arena_grows_past_one_frame() {
        // Every run needs its own record, so reserve more runs than a
        // single frame's records can describe.
        let runs = RECORDS_PER_FRAME + 4;
        let (frames, sim) = backing(16);
        let mut w = SlotWindow::new(runs, frames, sim).expect("window");
        for _ in 0..runs {
            w.allocate(1).expect("fits");
        }
        assert_eq!(w.entry_counts(), (runs, 0));
        assert!(
            w.record_frames() >= 2,
            "the arena drew a second frame rather than capping at one"
        );
    }

    /// Reserve 4-slot runs until the record supply is exhausted, with
    /// physical RAM drained so the arena cannot draw another frame.
    fn exhaust_records(w: &mut SlotWindow, frames: &'static FrameAllocator) {
        w.allocate(4).expect("the first run draws the record frame");
        while frames.alloc().is_ok() {}
        for _ in 0..=RECORDS_PER_FRAME {
            if w.allocate(4).is_err() {
                return;
            }
        }
        unreachable!("a single record frame cannot describe more runs than it holds");
    }

    #[test]
    fn an_allocation_that_cannot_draw_a_record_fails_closed() {
        // A window far larger than the record supply, so the refusal is
        // about records and not about running out of address space.
        let (frames, sim) = backing(2);
        let mut w = SlotWindow::new(8 * RECORDS_PER_FRAME, frames, sim).expect("window");
        exhaust_records(&mut w, frames);

        let reserved = w.reserved();
        assert_eq!(w.allocate(4), Err(SlotError::RecordsExhausted));
        assert_eq!(w.reserved(), reserved, "a refused allocation reserves none");
        assert_eq!(
            w.allocate(4),
            Err(SlotError::RecordsExhausted),
            "still fails closed"
        );
        // Releasing the tail recycles its record, so the window works again.
        w.release(reserved - 4, 4).expect("live");
        assert!(w.allocate(4).is_ok());
    }

    #[test]
    fn a_split_that_cannot_draw_a_record_leaves_the_free_run_intact() {
        let (frames, sim) = backing(2);
        let mut w = SlotWindow::new(8 * RECORDS_PER_FRAME, frames, sim).expect("window");
        exhaust_records(&mut w, frames);

        // Free a run between two live ones: no coalescing, so no record is
        // recycled, and the free run is longer than the next request.
        w.release(4, 4).expect("live");
        let counts = w.entry_counts();
        assert_eq!(
            w.allocate(2),
            Err(SlotError::RecordsExhausted),
            "the split needs a record the arena cannot supply"
        );
        assert_eq!(
            w.entry_counts(),
            counts,
            "the refused split changed no entry"
        );
        // The whole free run is still servable without a split.
        assert_eq!(w.allocate(4), Ok(4));
    }
}
