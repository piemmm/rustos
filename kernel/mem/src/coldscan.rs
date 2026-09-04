//! Cold-page identification for the compressed-memory tier
//! (`plans/SWAPSWAPSWAP.md` section 5: "not recently accessed by the
//! page-replacement policy").
//!
//! Before the [`ramzip`](crate::ramzip) tier reclaims a task's anonymous
//! page it must know the page is *genuinely cold* — untouched by the task
//! for long enough that compressing it will not immediately fault it back
//! in (the thrash the tier is designed to avoid). This module is the
//! architecture-neutral page-replacement half of that decision: a
//! classic **second-chance (clock)** scan over a task's candidate pages,
//! driven by the per-page referenced bit the Arch HAL exposes
//! ([`AddressSpace::test_and_clear_accessed`](crate::vmm::AddressSpace::test_and_clear_accessed)).
//!
//! # The algorithm
//!
//! A [`ColdPageScanner`] keeps a **clock hand** — the page number to
//! resume from — so successive passes sweep the candidate set in a
//! rotating order rather than always re-examining the same low pages
//! (fairness; no page is starved of a second chance). For each candidate,
//! in clock order from the hand:
//!
//! * the page's referenced bit is **read and cleared** in one step;
//! * a page whose bit was **set** was touched since the previous pass —
//!   it gets a *second chance*: the bit is now cleared and the page is
//!   left mapped, so if the task keeps using it the next pass finds it hot
//!   again;
//! * a page whose bit was **clear** went untouched across a full pass — it
//!   is *cold* and is returned to the caller as a reclaim candidate.
//!
//! Clearing the bit also invalidates the page's TLB entry (the HAL
//! primitive does this), so the hardware — or the software
//! access-flag-fault path on ports without a hardware flag — re-sets it on
//! the next real access. This is the same approximation Linux's page
//! reclaim uses; it needs no per-page timestamp and no allocation on the
//! hot path beyond the returned candidate list.
//!
//! # Fail closed
//!
//! On a port whose [`AccessTracking`](tairix_arch_api::mmu::AccessTracking)
//! is not `Supported` the referenced bit does not exist, so the scan
//! **refuses to classify
//! any page cold** and returns [`ColdScanError::Unsupported`]. The tier
//! then simply does not reclaim running tasks on that port — reclaim is
//! safe by omission, never by guessing a page is unused (the charter's
//! fail-closed rule; a false-cold classification would evict a hot page
//! and cause the very thrash the tier avoids).

use alloc::vec::Vec;

use crate::vmm::{AddressSpace, Page, PageTable};

/// Why a [`ColdPageScanner::scan`] produced no verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ColdScanError {
    /// The address space's backend exposes no per-page referenced bit
    /// (a non-`Supported`
    /// [`AccessTracking`](tairix_arch_api::mmu::AccessTracking)), so no
    /// page can be shown to be cold. Fail closed: the caller reclaims
    /// nothing rather than guess.
    Unsupported,
}

/// A second-chance (clock) cold-page scanner bound to one address space's
/// candidate set across successive passes.
///
/// One scanner instance per address space: the [`Self::hand`] it carries
/// is the rotating clock position for *that* space, so the scan does not
/// keep re-examining the lowest-numbered pages while higher ones are never
/// given a second chance.
#[derive(Debug, Default, Clone, Copy)]
pub struct ColdPageScanner {
    /// Clock hand: the page number to resume the sweep from on the next
    /// [`Self::scan`]. Sweeps wrap around the candidate set from here.
    hand: u64,
}

impl ColdPageScanner {
    /// A fresh scanner with its clock hand at the start of the space.
    #[must_use]
    pub const fn new() -> Self {
        Self { hand: 0 }
    }

    /// The current clock-hand page number (diagnostic / test observer).
    #[must_use]
    pub const fn hand(&self) -> u64 {
        self.hand
    }

    /// Sweep `candidates` in clock order and return up to `want` pages that
    /// are cold (untouched since the previous pass), advancing the clock
    /// hand past the pages examined.
    ///
    /// `candidates` must be sorted ascending by page number (the order
    /// [`AddressSpace::live_pages`](crate::vmm::AddressSpace::live_pages)
    /// yields), so the clock hand selects a stable rotation point. The
    /// caller supplies only pages that are *eligible* to compress (cold
    /// anonymous user pages); this scanner decides only the "recently
    /// used?" question, never eligibility.
    ///
    /// Every examined page has its referenced bit read-and-cleared, giving
    /// a still-hot page its second chance whether or not `want` is reached,
    /// so a page the task keeps touching is never selected on the next
    /// pass either.
    ///
    /// # Errors
    ///
    /// [`ColdScanError::Unsupported`] if the backend exposes no referenced
    /// bit — the scan classifies nothing cold and the caller reclaims
    /// nothing (fail closed).
    pub fn scan<P: PageTable>(
        &mut self,
        space: &mut AddressSpace<P>,
        candidates: &[Page],
        want: usize,
    ) -> Result<Vec<Page>, ColdScanError> {
        if !space.access_tracking().is_supported() {
            return Err(ColdScanError::Unsupported);
        }
        debug_assert!(
            candidates.windows(2).all(|w| w[0].number() < w[1].number()),
            "coldscan candidates must be sorted ascending and unique"
        );
        let n = candidates.len();
        if want == 0 || n == 0 {
            return Ok(Vec::new());
        }

        // The rotation start: the first candidate at or after the clock
        // hand, wrapping to the front when the hand is past the last page.
        let start = candidates.partition_point(|p| p.number() < self.hand) % n;
        let mut cold = Vec::new();
        let mut last_examined = candidates[start].number();

        for step in 0..n {
            let page = candidates[(start + step) % n];
            last_examined = page.number();
            // Only an `Ok(false)` — the referenced bit was clear, so the
            // page went untouched across the last pass — makes a page cold.
            // Every other outcome leaves it unreclaimed (fail closed):
            //   * `Ok(true)` — touched since the last clear, so it gets its
            //     second chance (the bit is now cleared) and stays mapped;
            //   * `Err(NotMapped)` — raced out from under the scan by a
            //     concurrent unmap;
            //   * any other `Err` cannot arise here (the candidate came from
            //     `live_pages`, so it is page aligned, and the guard above
            //     proved the backend tracks access, ruling out
            //     `Misaligned`/`Unsupported`) — and a hypothetical backend
            //     defect must never cause a reclaim.
            if let Ok(false) = space.test_and_clear_accessed(page) {
                cold.push(page);
                if cold.len() >= want {
                    break;
                }
            }
        }

        // Resume the next sweep just past the last page examined, so the
        // clock keeps rotating rather than restarting at the front.
        self.hand = last_examined.wrapping_add(1);
        Ok(cold)
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::frame::{Frame, PhysAddr, PAGE_SIZE};
    use crate::vmm::{HostPageTable, MapFlags, VirtAddr};
    use alloc::vec;
    use tairix_arch_api::mmu::{AddressSpace as HalAddressSpace, MapError, PageFlags};
    use tairix_arch_api::tlb::TlbShootdown;

    /// User RW flags, the shape of an anonymous page.
    const RW_USER: MapFlags = MapFlags::READ.union(MapFlags::WRITE).union(MapFlags::USER);

    fn page(n: u64) -> Page {
        Page::from_addr(VirtAddr::new(n * PAGE_SIZE as u64)).expect("aligned")
    }

    fn frame(n: u64) -> Frame {
        Frame::containing(PhysAddr::new(0x10_0000 + n * PAGE_SIZE as u64))
    }

    /// Build a space with `count` mapped anonymous pages at page numbers
    /// `1..=count`, returning the space and the ascending candidate list.
    fn space_with(count: u64) -> (AddressSpace<HostPageTable>, Vec<Page>) {
        let mut space = AddressSpace::new(HostPageTable::new());
        let mut pages = Vec::new();
        for n in 1..=count {
            space.map(page(n), frame(n), RW_USER).expect("map");
            pages.push(page(n));
        }
        (space, pages)
    }

    #[test]
    fn all_untouched_pages_are_cold_up_to_the_budget() {
        let (mut space, pages) = space_with(6);
        let mut scanner = ColdPageScanner::new();
        // Nothing was ever accessed, so every page is cold; the budget
        // caps the return at 4.
        let cold = scanner.scan(&mut space, &pages, 4).expect("supported");
        assert_eq!(cold.len(), 4);
        assert_eq!(cold, vec![page(1), page(2), page(3), page(4)]);
    }

    #[test]
    fn a_referenced_page_gets_a_second_chance_and_is_not_cold() {
        let (mut space, pages) = space_with(4);
        // The task touched pages 2 and 3 since the last pass.
        space
            .table_mut_for_test()
            .mark_accessed(page(2).start().as_u64());
        space
            .table_mut_for_test()
            .mark_accessed(page(3).start().as_u64());
        let mut scanner = ColdPageScanner::new();
        let cold = scanner.scan(&mut space, &pages, 8).expect("supported");
        // Only the untouched pages are cold.
        assert_eq!(cold, vec![page(1), page(4)]);
    }

    #[test]
    fn the_second_chance_clears_the_bit_so_a_still_idle_page_is_cold_next_pass() {
        let (mut space, pages) = space_with(3);
        space
            .table_mut_for_test()
            .mark_accessed(page(2).start().as_u64());
        let mut scanner = ColdPageScanner::new();
        // First pass: page 2 was touched, gets a second chance (bit
        // cleared); pages 1 and 3 are cold.
        let first = scanner.scan(&mut space, &pages, 8).expect("supported");
        assert_eq!(first, vec![page(1), page(3)]);
        // Second pass, page 2 not touched again: now cold.
        let second = scanner.scan(&mut space, &pages, 8).expect("supported");
        assert_eq!(second, vec![page(1), page(2), page(3)]);
    }

    #[test]
    fn the_clock_hand_rotates_across_passes() {
        let (mut space, pages) = space_with(5);
        let mut scanner = ColdPageScanner::new();
        // Take two cold pages; hand advances past page 2.
        let first = scanner.scan(&mut space, &pages, 2).expect("supported");
        assert_eq!(first, vec![page(1), page(2)]);
        assert_eq!(scanner.hand(), page(2).number() + 1);
        // The next pass resumes at page 3, not page 1.
        let second = scanner.scan(&mut space, &pages, 2).expect("supported");
        assert_eq!(second, vec![page(3), page(4)]);
    }

    #[test]
    fn want_zero_returns_nothing_but_still_checks_support() {
        let (mut space, pages) = space_with(3);
        let mut scanner = ColdPageScanner::new();
        assert_eq!(scanner.scan(&mut space, &pages, 0), Ok(Vec::new()));
    }

    /// A backend that honestly reports no referenced bit: the scanner must
    /// fail closed rather than treat every page as cold.
    #[derive(Default)]
    struct NoTrackTable {
        entries: alloc::collections::BTreeMap<u64, (u64, PageFlags)>,
    }

    impl HalAddressSpace for NoTrackTable {
        fn map_page(&mut self, vaddr: u64, paddr: u64, flags: PageFlags) -> Result<(), MapError> {
            self.entries.insert(vaddr, (paddr, flags));
            Ok(())
        }
        fn translate(&self, vaddr: u64) -> Option<(u64, PageFlags)> {
            self.entries.get(&vaddr).copied()
        }
        fn unmap(&mut self, vaddr: u64) -> Result<u64, MapError> {
            self.entries
                .remove(&vaddr)
                .map(|(p, _)| p)
                .ok_or(MapError::NotMapped)
        }
        fn root_phys(&self) -> u64 {
            PAGE_SIZE as u64
        }
        // Uses the default `access_tracking` (Unsupported) and the default
        // fail-closed `test_and_clear_accessed`.
        unsafe fn activate(&self) {}
    }

    impl TlbShootdown for NoTrackTable {
        fn flush_page(&mut self, _vaddr: u64) {}
        fn flush_range(&mut self, _start_vaddr: u64, _page_count: usize) {}
    }

    #[test]
    fn a_backend_without_a_referenced_bit_fails_closed() {
        let mut space = AddressSpace::new(NoTrackTable::default());
        space.map(page(1), frame(1), RW_USER).expect("map");
        let mut scanner = ColdPageScanner::new();
        assert_eq!(
            scanner.scan(&mut space, &[page(1)], 4),
            Err(ColdScanError::Unsupported)
        );
    }

    #[test]
    fn an_empty_candidate_set_is_no_cold_pages() {
        let (mut space, _pages) = space_with(2);
        let mut scanner = ColdPageScanner::new();
        assert_eq!(scanner.scan(&mut space, &[], 4), Ok(Vec::new()));
    }
}
