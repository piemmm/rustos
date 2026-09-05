//! Per-task user-virtual-address placement allocator for **non-`FIXED`**
//! anonymous mappings (`plans/PI.md` P10 chunk 5d-0-ii (c); the
//! `plans/SPAWN.md` `SP5b` production `mem_map` placement).
//!
//! A `FIXED` [`mem_map`](crate::anon::map_anonymous) names its own base; a
//! non-`FIXED` `mem_map` asks the *kernel* to choose one. That placement
//! decision needs per-task bookkeeping — which user-virtual ranges are
//! already handed out — so a second non-`FIXED` request never overlaps the
//! first, the program image, its stack, or a granted device window. This
//! module owns exactly that decision and **nothing else**: it allocates and
//! releases page-aligned ranges inside one configured virtual window, and is
//! driven against a *borrowed* live [`AddressSpace`](crate::vmm::AddressSpace)
//! by the higher-level [`LiveSpace`](crate::live::LiveSpace), which composes
//! it with the already-audited [`map_anonymous`](crate::anon::map_anonymous)
//! mechanism — there is no second mapping path.
//!
//! It is the placement analogue of [`MmioWindowMap`](crate::mmio::MmioWindowMap):
//! both manage a per-task virtual window, but the MMIO mapper places a
//! *device's own* frames (and brackets each with guard pages), whereas this
//! only **chooses the base** for anonymous RAM the frame allocator backs.
//!
//! # Scalability
//!
//! The window is *address space*, not a physical resource: a large window
//! costs no RAM until the frame allocator backs a mapping (and that backing
//! fails closed as a deterministic OOM). So the window may be sized
//! generously, and the allocator's own memory is bounded by the number of
//! *live* regions — never by the page count of the window. A hand-picked
//! per-page bitmap (which would cap the window or waste memory on a large
//! machine) is deliberately avoided.

use core::ops::Range;

use tairix_collections::{RangeKey, RangeMap};

use crate::anon::AnonError;
use crate::frame::PAGE_SIZE;
use crate::vmm::VirtAddr;

/// Per-task placement allocator over the virtual window
/// `[base, base + capacity_pages * PAGE_SIZE)`.
///
/// The gaps between the ranges handed out *are* the free space, so a
/// released range is available again the moment its record leaves and two
/// released neighbours serve one larger request between them — there is no
/// second free-list to fall out of step with the first. Placement is
/// first-fit, so a steady map/unmap workload reuses address space rather
/// than exhausting the window.
pub struct AnonWindowMap {
    /// The virtual byte range placements are drawn from, validated whole at
    /// construction so no later arithmetic over it can overflow or wrap.
    window: Range<u64>,
    capacity_pages: usize,
    /// Live allocations, keyed by their user-virtual byte extent. Adjacent
    /// allocations stay distinct entries, so a release names exactly the
    /// range it was handed and can never take a neighbour's with it.
    regions: RangeMap<u64, ()>,
}

impl AnonWindowMap {
    /// Construct an allocator managing the virtual range
    /// `[base, base + capacity_pages * PAGE_SIZE)`.
    ///
    /// # Errors
    ///
    /// [`AnonError::ZeroLength`] if `capacity_pages == 0`,
    /// [`AnonError::Unaligned`] if `base` is not page-aligned, or
    /// [`AnonError::Overflow`] if the window's byte span or its top address
    /// overflows the address space.
    pub fn new(base: VirtAddr, capacity_pages: usize) -> Result<Self, AnonError> {
        if capacity_pages == 0 {
            return Err(AnonError::ZeroLength);
        }
        if !base.is_page_aligned() {
            return Err(AnonError::Unaligned);
        }
        // The window must fit in the address space: both the byte span and
        // the top address are validated up front so no later placement can
        // overflow (fail closed before any state).
        let span = u64::try_from(capacity_pages)
            .ok()
            .and_then(|pages| pages.checked_mul(PAGE_SIZE as u64))
            .ok_or(AnonError::Overflow)?;
        let top = base.as_u64().checked_add(span).ok_or(AnonError::Overflow)?;
        Ok(Self {
            window: base.as_u64()..top,
            capacity_pages,
            regions: RangeMap::new(),
        })
    }

    /// Reserve a `page_count`-page range and return its base user virtual
    /// address, **without** touching any page table.
    ///
    /// The caller maps the returned base through the audited
    /// [`map_anonymous`](crate::anon::map_anonymous) mechanism and, on a
    /// mapping failure, calls [`Self::release`] to give the range back.
    ///
    /// # Errors
    ///
    /// * [`AnonError::ZeroLength`] if `page_count == 0`.
    /// * [`AnonError::Overflow`] if the request's byte span does not fit the
    ///   address space.
    /// * [`AnonError::OutOfMemory`] if no gap in the window can satisfy the
    ///   request (the window is exhausted — a deterministic, fail-closed
    ///   refusal).
    pub fn allocate(&mut self, page_count: u64) -> Result<u64, AnonError> {
        if page_count == 0 {
            return Err(AnonError::ZeroLength);
        }
        let bytes = Self::bytes_of(page_count)?;
        let placed = self
            .regions
            .place(self.window.clone(), bytes, ())
            .ok_or(AnonError::OutOfMemory)?;
        Ok(placed.start)
    }

    /// Release the range based at `base_va` previously returned by
    /// [`Self::allocate`], making its address space available again.
    ///
    /// `page_count` must equal the count the range was allocated with: a
    /// mismatch (or an unknown base) is rejected fail-closed and frees
    /// nothing (the range is not one this allocator
    /// handed out, so it never tears down a neighbour's slots).
    ///
    /// # Errors
    ///
    /// [`AnonError::NotMapped`] if `base_va` is not a live allocation of this
    /// window, or `page_count` does not match its recorded extent, and
    /// [`AnonError::Overflow`] if that count's byte span does not fit the
    /// address space.
    pub fn release(&mut self, base_va: u64, page_count: u64) -> Result<(), AnonError> {
        // The match check is the same fail-closed test [`Self::validate`]
        // performs (one definition); only release mutates.
        self.validate(base_va, page_count)?;
        self.regions.remove(base_va);
        Ok(())
    }

    /// Confirm `base_va` is a live allocation of this window of exactly
    /// `page_count` pages, **without** mutating any state.
    ///
    /// The live space calls this before tearing a placed region's pages down,
    /// so a mismatched `(base, len)` fails closed *before* any page is
    /// unmapped; the matching [`Self::release`] then runs
    /// after the teardown and is guaranteed to match.
    ///
    /// # Errors
    ///
    /// [`AnonError::NotMapped`] if `base_va` is not a live allocation, or
    /// `page_count` does not match its recorded extent, and
    /// [`AnonError::Overflow`] if that count's byte span does not fit the
    /// address space.
    pub fn validate(&self, base_va: u64, page_count: u64) -> Result<(), AnonError> {
        let bytes = Self::bytes_of(page_count)?;
        match self.regions.get(base_va) {
            // A zero page count spans nothing, so it matches no live range.
            Some((held, ())) if held.end.distance_from(held.start) == bytes => Ok(()),
            _ => Err(AnonError::NotMapped),
        }
    }

    /// `true` iff `base_va` lies inside this window's virtual range — the
    /// test the live space uses to tell a non-`FIXED` (placed) `mem_map` base
    /// apart from a `FIXED` one the caller chose elsewhere.
    #[must_use]
    pub fn owns(&self, base_va: u64) -> bool {
        self.window.contains(&base_va)
    }

    /// `true` iff `va` (any address, not only a base) lies inside a *live*
    /// allocation of this window — the test the live space uses before
    /// backing a demand-paged fault, so an address in the window but outside
    /// every reserved region is refused rather than silently backed.
    #[must_use]
    pub fn covers(&self, va: u64) -> bool {
        self.regions.covering(va).is_some()
    }

    /// Number of live allocations.
    #[must_use]
    pub fn live(&self) -> usize {
        self.regions.len()
    }

    /// Total pages in the allocator's virtual window.
    #[must_use]
    pub fn capacity_pages(&self) -> usize {
        self.capacity_pages
    }

    /// `page_count` pages as a byte span, refusing one the address space
    /// cannot hold.
    fn bytes_of(page_count: u64) -> Result<u64, AnonError> {
        page_count
            .checked_mul(PAGE_SIZE as u64)
            .ok_or(AnonError::Overflow)
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    const WINDOW_BASE: u64 = 0x8000_0000;
    const WINDOW_PAGES: usize = 16;
    const PAGE: u64 = PAGE_SIZE as u64;

    fn window() -> AnonWindowMap {
        AnonWindowMap::new(VirtAddr::new(WINDOW_BASE), WINDOW_PAGES)
            .expect("a page-aligned, non-zero window is valid")
    }

    #[test]
    fn new_rejects_a_misaligned_or_empty_window() {
        // `AnonWindowMap` is deliberately not `Debug`/`PartialEq` (it carries
        // per-task maps), so assert on the error arm with `matches!`.
        assert!(matches!(
            AnonWindowMap::new(VirtAddr::new(WINDOW_BASE + 1), 4),
            Err(AnonError::Unaligned)
        ));
        assert!(matches!(
            AnonWindowMap::new(VirtAddr::new(WINDOW_BASE), 0),
            Err(AnonError::ZeroLength)
        ));
    }

    #[test]
    fn allocations_bump_upward_and_do_not_overlap() {
        let mut w = window();
        let a = w.allocate(2).expect("fits");
        let b = w.allocate(3).expect("fits");
        assert_eq!(a, WINDOW_BASE);
        assert_eq!(b, WINDOW_BASE + 2 * PAGE);
        assert_eq!(w.live(), 2);
    }

    #[test]
    fn allocate_rejects_zero_pages() {
        let mut w = window();
        assert_eq!(w.allocate(0), Err(AnonError::ZeroLength));
    }

    #[test]
    fn allocate_fails_closed_when_the_window_is_exhausted() {
        let mut w = window();
        w.allocate(WINDOW_PAGES as u64).expect("the whole window");
        assert_eq!(w.allocate(1), Err(AnonError::OutOfMemory));
    }

    #[test]
    fn release_returns_slots_for_reuse() {
        let mut w = window();
        // Fill the window, free the first allocation, and prove the freed
        // hole is reused rather than the (exhausted) bump cursor.
        let a = w.allocate(4).expect("fits");
        let _b = w.allocate(WINDOW_PAGES as u64 - 4).expect("rest");
        assert_eq!(w.allocate(1), Err(AnonError::OutOfMemory), "window full");
        w.release(a, 4).expect("a is live");
        let reused = w.allocate(2).expect("reuses the freed hole");
        assert_eq!(reused, a, "first-fit reuses the freed region's base");
        // The 4-page hole split: 2 pages remain free and serve the next ask.
        assert_eq!(w.allocate(2), Ok(a + 2 * PAGE));
    }

    #[test]
    fn two_released_neighbours_serve_one_request_between_them() {
        // The defect the free-list this replaces carried: two ranges released
        // side by side stayed two separate holes, so a request larger than
        // either was refused while the address space for it sat free.
        let mut w = window();
        let a = w.allocate(4).expect("fits");
        let b = w.allocate(4).expect("fits");
        w.allocate(WINDOW_PAGES as u64 - 8).expect("the rest");
        assert_eq!(b, a + 4 * PAGE);
        w.release(a, 4).expect("a is live");
        w.release(b, 4).expect("b is live");
        assert_eq!(w.live(), 1);
        assert_eq!(w.allocate(8), Ok(a), "the two ranges are now one gap");
    }

    #[test]
    fn a_placement_never_overlaps_a_live_neighbour() {
        // First-fit reuses the lowest gap, and a request too large for it
        // moves above the live range rather than into it.
        let mut w = window();
        let a = w.allocate(2).expect("fits");
        let b = w.allocate(2).expect("fits");
        let c = w.allocate(2).expect("fits");
        w.release(a, 2).expect("a is live");
        w.release(c, 2).expect("c is live");
        assert_eq!(w.allocate(3), Ok(c), "the 2-page hole cannot hold three");
        assert!(w.covers(b), "the neighbour range is untouched");
        assert_eq!(w.allocate(2), Ok(a), "the low hole still serves its size");
    }

    #[test]
    fn release_rejects_an_unknown_base_or_mismatched_count() {
        let mut w = window();
        let a = w.allocate(3).expect("fits");
        assert_eq!(w.release(a + PAGE, 3), Err(AnonError::NotMapped));
        assert_eq!(w.release(a, 2), Err(AnonError::NotMapped));
        // The live region is untouched by the refused releases.
        assert_eq!(w.live(), 1);
        w.release(a, 3).expect("the matching release succeeds");
        assert_eq!(w.live(), 0);
    }

    #[test]
    fn covers_names_only_live_regions_never_the_gaps() {
        let mut w = window();
        let a = w.allocate(2).expect("fits");
        let b = w.allocate(3).expect("fits");
        // Any byte inside a live region is covered, including mid-page.
        assert!(w.covers(a));
        assert!(w.covers(a + PAGE + 123));
        assert!(w.covers(b + 2 * PAGE));
        // The exclusive top of a region and the window's unreserved tail
        // are not (the fault path must refuse them).
        assert!(!w.covers(b + 3 * PAGE));
        assert!(!w.covers(WINDOW_BASE + (WINDOW_PAGES as u64 - 1) * PAGE));
        // A released region stops being covered.
        w.release(a, 2).expect("live");
        assert!(!w.covers(a + PAGE));
        assert!(w.covers(b), "the neighbour region is untouched");
        // An address below the window is never covered.
        assert!(!w.covers(WINDOW_BASE - 1));
    }

    #[test]
    fn owns_distinguishes_in_window_from_fixed_bases() {
        let w = window();
        assert!(w.owns(WINDOW_BASE));
        assert!(w.owns(WINDOW_BASE + (WINDOW_PAGES as u64 - 1) * PAGE));
        assert!(!w.owns(WINDOW_BASE - PAGE), "below the window");
        assert!(
            !w.owns(WINDOW_BASE + WINDOW_PAGES as u64 * PAGE),
            "at the exclusive top"
        );
        assert!(!w.owns(0x1000), "an unrelated FIXED base");
    }
}
