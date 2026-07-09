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
//! *live + freed* regions — a bump cursor plus a free-list of returned holes
//! — never by the page count of the window. A hand-picked per-page bitmap
//! (which would cap the window or waste memory on a large machine) is
//! deliberately avoided.

use alloc::collections::BTreeMap;

use crate::anon::AnonError;
use crate::frame::{PAGE_SHIFT, PAGE_SIZE};
use crate::vmm::VirtAddr;

/// Per-task placement allocator over the virtual window
/// `[base, base + capacity_pages * PAGE_SIZE)`.
///
/// Allocations are served from the free-list of previously released holes
/// (first-fit, split on a partial match) before the bump cursor advances, so
/// a steady map/unmap workload reuses address space rather than exhausting
/// the window. Both maps are keyed by slot index (pages above `base`); the
/// public surface speaks user virtual addresses.
pub struct AnonWindowMap {
    base: VirtAddr,
    capacity_pages: usize,
    /// Next never-yet-allocated slot (the bump cursor).
    next: usize,
    /// Released holes available for reuse: start slot -> page count.
    free: BTreeMap<usize, usize>,
    /// Live allocations: base virtual address -> page count.
    regions: BTreeMap<u64, usize>,
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
        // the top address are validated up front so `va_of_slot` can never
        // overflow (fail closed before any state).
        let span = (capacity_pages as u64)
            .checked_mul(PAGE_SIZE as u64)
            .ok_or(AnonError::Overflow)?;
        base.as_u64().checked_add(span).ok_or(AnonError::Overflow)?;
        Ok(Self {
            base,
            capacity_pages,
            next: 0,
            free: BTreeMap::new(),
            regions: BTreeMap::new(),
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
    /// * [`AnonError::Overflow`] if `page_count` does not fit `usize`.
    /// * [`AnonError::OutOfMemory`] if no free hole and no remaining bump
    ///   space can satisfy the request (the window is exhausted — a
    ///   deterministic, fail-closed refusal).
    pub fn allocate(&mut self, page_count: u64) -> Result<u64, AnonError> {
        if page_count == 0 {
            return Err(AnonError::ZeroLength);
        }
        let n = usize::try_from(page_count).map_err(|_| AnonError::Overflow)?;

        let slot = self.take_free_hole(n).map_or_else(
            || {
                // No reusable hole: advance the bump cursor, failing closed
                // when the window cannot hold the request.
                let end = self.next.checked_add(n).ok_or(AnonError::OutOfMemory)?;
                if end > self.capacity_pages {
                    return Err(AnonError::OutOfMemory);
                }
                let slot = self.next;
                self.next = end;
                Ok(slot)
            },
            Ok,
        )?;

        let base_va = self.va_of_slot(slot);
        self.regions.insert(base_va, n);
        Ok(base_va)
    }

    /// Release the range based at `base_va` previously returned by
    /// [`Self::allocate`], returning its slots to the free-list for reuse.
    ///
    /// `page_count` must equal the count the range was allocated with: a
    /// mismatch (or an unknown base) is rejected fail-closed and frees
    /// nothing (the range is not one this allocator
    /// handed out, so it never tears down a neighbour's slots).
    ///
    /// # Errors
    ///
    /// [`AnonError::NotMapped`] if `base_va` is not a live allocation of this
    /// window, or `page_count` does not match its recorded extent.
    pub fn release(&mut self, base_va: u64, page_count: u64) -> Result<(), AnonError> {
        // The match check is the same fail-closed test [`Self::validate`]
        // performs (one definition); only release mutates.
        self.validate(base_va, page_count)?;
        let n = usize::try_from(page_count).map_err(|_| AnonError::Overflow)?;
        self.regions.remove(&base_va);
        let slot = self.slot_of_va(base_va);
        self.free.insert(slot, n);
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
    /// `page_count` does not match its recorded extent.
    pub fn validate(&self, base_va: u64, page_count: u64) -> Result<(), AnonError> {
        let n = usize::try_from(page_count).map_err(|_| AnonError::Overflow)?;
        match self.regions.get(&base_va) {
            Some(&recorded) if recorded == n => Ok(()),
            _ => Err(AnonError::NotMapped),
        }
    }

    /// `true` iff `base_va` lies inside this window's virtual range — the
    /// test the live space uses to tell a non-`FIXED` (placed) `mem_map` base
    /// apart from a `FIXED` one the caller chose elsewhere.
    #[must_use]
    pub fn owns(&self, base_va: u64) -> bool {
        let top = self.base.as_u64() + (self.capacity_pages as u64) * PAGE_SIZE as u64;
        base_va >= self.base.as_u64() && base_va < top
    }

    /// `true` iff `va` (any address, not only a base) lies inside a *live*
    /// allocation of this window — the test the live space uses before
    /// backing a demand-paged fault, so an address in the window but outside
    /// every reserved region is refused rather than silently backed.
    #[must_use]
    pub fn covers(&self, va: u64) -> bool {
        let Some((&base, &pages)) = self.regions.range(..=va).next_back() else {
            return false;
        };
        // The base cannot overflow: the window's top was validated at
        // construction, so `base + pages * PAGE_SIZE` stays in range.
        va < base + (pages as u64) * PAGE_SIZE as u64
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

    // -----------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------

    /// First-fit: claim a free hole of at least `n` slots, splitting any
    /// remainder back onto the free-list, and return its start slot.
    fn take_free_hole(&mut self, n: usize) -> Option<usize> {
        let (&start, &len) = self.free.iter().find(|&(_, &len)| len >= n)?;
        self.free.remove(&start);
        if len > n {
            self.free.insert(start + n, len - n);
        }
        Some(start)
    }

    fn va_of_slot(&self, slot: usize) -> u64 {
        self.base.as_u64() + ((slot as u64) << PAGE_SHIFT)
    }

    fn slot_of_va(&self, base_va: u64) -> usize {
        ((base_va - self.base.as_u64()) >> PAGE_SHIFT) as usize
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
