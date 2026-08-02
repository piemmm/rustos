//! The `ramzip` handoff and VM escalation ordering
//! (`plans/SMARTRAM.md` SMART2, `plans/SWAPSWAPSWAP.md` section 6).
//!
//! The band vocabulary itself, the watermarks, the gauge, and the
//! per-class [`shrink_target`](tairix_reclaim::shrink_target) ordering
//! are the shared model in `tairix_reclaim`: one definition the kernel
//! and a userland desktop session both classify against. This module
//! adds the two decisions that need the kernel's own anonymous-memory
//! tier and can therefore live nowhere else:
//!
//! - [`ramzip_handoff`] fixes the ordering `plans/SWAPSWAPSWAP.md`
//!   requires — cold anonymous pages are compressed only from moderate
//!   pressure onward, and at moderate only once clean and transform
//!   cache have been reclaimed, because reconstructable clean cache is
//!   always cheaper than encrypted compressed anonymous storage.
//! - [`escalation`] is the deterministic next step when reclaim cannot
//!   help.
//!
//! It also binds the physical [`FrameAllocator`] to the shared gauge as
//! its production [`FreeMemorySource`]: the free-frame count is the
//! authoritative "how much RAM is left" figure the whole model folds
//! into bands.

use tairix_reclaim::{FreeMemorySource, PressureBand};

use crate::frame::{FrameAllocator, PAGE_SIZE};

impl FreeMemorySource for FrameAllocator {
    fn free_bytes(&self) -> usize {
        self.free_frames().saturating_mul(PAGE_SIZE)
    }

    fn total_bytes(&self) -> usize {
        // Usable frames, not the bitmap's address-space extent: holes
        // and reserved regions are not memory pressure can spend.
        self.usable_frames().saturating_mul(PAGE_SIZE)
    }
}

/// Whether `ramzip` may compress cold anonymous pages at `band`, given
/// the clean-file plus transform cache bytes still resident.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RamzipHandoff {
    /// Compression is not the next step: either pressure is too
    /// shallow for `ramzip` activity, or cheaper clean/transform cache
    /// must be reclaimed first.
    HoldCompression,
    /// Cold eligible anonymous pages may be compressed, under the
    /// reserve, cap, and fail-closed rules `plans/SWAPSWAPSWAP.md`
    /// owns.
    CompressColdAnonymous,
}

/// The handoff ordering to `ramzip` (`plans/SWAPSWAPSWAP.md` section
/// 6): no compression at normal or mild pressure; at moderate pressure
/// compression starts only once clean file and transform cache have
/// been reclaimed (reconstructable clean cache is cheaper than
/// encrypted compressed anonymous storage); at severe pressure
/// `ramzip` owns cold-anonymous policy regardless of cache residue; at
/// critical pressure speculative work stops and [`escalation`] owns
/// the next step.
///
/// This is the integration seam `plans/SWAPSWAPSWAP.md` SWAP3 binds
/// to; until the `ramzip` store lands, the ordering is enforced
/// against the caches alone (their shrink targets already reach zero
/// before this gate opens).
#[must_use]
pub const fn ramzip_handoff(band: PressureBand, clean_and_transform_bytes: usize) -> RamzipHandoff {
    match band {
        PressureBand::Normal | PressureBand::Mild | PressureBand::Critical => {
            RamzipHandoff::HoldCompression
        }
        PressureBand::Moderate => {
            if clean_and_transform_bytes == 0 {
                RamzipHandoff::CompressColdAnonymous
            } else {
                RamzipHandoff::HoldCompression
            }
        }
        PressureBand::Severe => RamzipHandoff::CompressColdAnonymous,
    }
}

/// The compress-out reclaim batch, in pages, for one direct-reclaim
/// sweep at `band` (`plans/SWAPSWAPSWAP.md` section 6, section 11 —
/// bounded, benchmarked, never ABI).
///
/// This bounds the work one triggering event (a demand fault under
/// pressure) may spend scanning and compressing the faulting task's own
/// cold anonymous pages: a small, fixed batch keeps direct reclaim off
/// the critical path (the sweep is amortised across many faults, never a
/// single unbounded pass) while still relieving pressure in step with how
/// deep it is.
///
/// The batch is zero except where compression is actually the answer,
/// mirroring [`ramzip_handoff`]:
///
/// - **normal / mild** — compression never runs here (cheaper cache
///   reclaim and speculative-growth stops handle these bands), so the
///   batch is zero and the caller does no reclaim work at all.
/// - **moderate** — a modest batch: cold anonymous pages begin
///   compressing once the cheaper caches have drained.
/// - **severe** — a larger batch: reserves are close, so a triggering
///   fault reclaims harder, still bounded.
/// - **critical** — zero: speculative work stops and [`escalation`]
///   hands the next step to the VM policy (freeze / kill / clean OOM),
///   not to more compression.
///
/// Pure and deterministic for equal inputs.
#[must_use]
pub const fn ramzip_reclaim_batch(band: PressureBand) -> usize {
    match band {
        PressureBand::Normal | PressureBand::Mild | PressureBand::Critical => 0,
        PressureBand::Moderate => MODERATE_RECLAIM_BATCH_PAGES,
        PressureBand::Severe => SEVERE_RECLAIM_BATCH_PAGES,
    }
}

/// Direct-reclaim batch at moderate pressure: 32 pages (128 KiB with a
/// 4 KiB page). Small enough that the scan+compress cost is a negligible
/// addition to a fault that was already going to allocate, large enough
/// that a task steadily faulting under moderate pressure makes real
/// headroom over successive faults. Not ABI; a tuning constant validated
/// against the section 19 benchmarks.
const MODERATE_RECLAIM_BATCH_PAGES: usize = 32;

/// Direct-reclaim batch at severe pressure: 128 pages (512 KiB). Reserves
/// are close, so a triggering fault reclaims four times as hard as at
/// moderate — still a bounded pass, never an unbounded sweep. Not ABI.
const SEVERE_RECLAIM_BATCH_PAGES: usize = 128;

/// The deterministic next step when the VM must free memory.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum EscalationStep {
    /// No action is needed at this band.
    Hold,
    /// Shrink reclaimable caches to their band targets first.
    ReclaimCaches,
    /// Caches are drained: hand cold anonymous pages to `ramzip`
    /// under its own reserve and cap rules.
    HandOffToRamzip,
    /// Reclaim cannot help and compression is no longer the answer:
    /// escalate through the VM pressure policy (lower-tier swap where
    /// approved, freeze or kill selected tasks, or a clean OOM) —
    /// deterministically, never a panic or a retry loop.
    VmPolicy,
}

/// The escalation order when reclaim cannot help
/// (`plans/SMARTRAM.md` section 7, `plans/SWAPSWAPSWAP.md` section 6):
/// reclaimable cache is always the first answer while any remains;
/// with the caches drained, moderate and severe pressure hand off to
/// `ramzip`, and critical pressure escalates to the VM policy.
///
/// Pure and deterministic for equal inputs — two callers observing the
/// same band and residue compute the same step.
#[must_use]
pub const fn escalation(band: PressureBand, reclaimable_bytes: usize) -> EscalationStep {
    match band {
        PressureBand::Normal => EscalationStep::Hold,
        PressureBand::Mild => {
            if reclaimable_bytes > 0 {
                EscalationStep::ReclaimCaches
            } else {
                EscalationStep::Hold
            }
        }
        PressureBand::Moderate | PressureBand::Severe => {
            if reclaimable_bytes > 0 {
                EscalationStep::ReclaimCaches
            } else {
                EscalationStep::HandOffToRamzip
            }
        }
        PressureBand::Critical => {
            if reclaimable_bytes > 0 {
                EscalationStep::ReclaimCaches
            } else {
                EscalationStep::VmPolicy
            }
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn ramzip_compression_waits_for_clean_and_transform_reclaim() {
        // No compression at all while pressure is shallow.
        assert_eq!(
            ramzip_handoff(PressureBand::Normal, 0),
            RamzipHandoff::HoldCompression
        );
        assert_eq!(
            ramzip_handoff(PressureBand::Mild, 0),
            RamzipHandoff::HoldCompression
        );
        // At moderate pressure the cheaper caches must drain first.
        assert_eq!(
            ramzip_handoff(PressureBand::Moderate, 4096),
            RamzipHandoff::HoldCompression
        );
        assert_eq!(
            ramzip_handoff(PressureBand::Moderate, 0),
            RamzipHandoff::CompressColdAnonymous
        );
        // At severe pressure ramzip owns cold-anonymous policy.
        assert_eq!(
            ramzip_handoff(PressureBand::Severe, 4096),
            RamzipHandoff::CompressColdAnonymous
        );
        // At critical pressure speculative work stops; escalation owns
        // the next step.
        assert_eq!(
            ramzip_handoff(PressureBand::Critical, 0),
            RamzipHandoff::HoldCompression
        );
    }

    #[test]
    fn the_reclaim_batch_is_bounded_and_matches_the_handoff_bands() {
        // Compression never runs at normal/mild/critical, so the batch is
        // zero there — the caller does no reclaim work at all.
        assert_eq!(ramzip_reclaim_batch(PressureBand::Normal), 0);
        assert_eq!(ramzip_reclaim_batch(PressureBand::Mild), 0);
        assert_eq!(ramzip_reclaim_batch(PressureBand::Critical), 0);
        // Compression runs at moderate and severe; severe reclaims harder.
        let moderate = ramzip_reclaim_batch(PressureBand::Moderate);
        let severe = ramzip_reclaim_batch(PressureBand::Severe);
        assert!(moderate > 0);
        assert!(severe > moderate);
        // A batch is only ever non-zero where the handoff opens compression.
        for band in PressureBand::ALL {
            let opens = matches!(band, PressureBand::Moderate | PressureBand::Severe);
            assert_eq!(ramzip_reclaim_batch(band) > 0, opens, "{band:?}");
        }
    }

    #[test]
    fn escalation_is_deterministic_and_prefers_cache_reclaim() {
        assert_eq!(escalation(PressureBand::Normal, 0), EscalationStep::Hold);
        assert_eq!(
            escalation(PressureBand::Mild, 4096),
            EscalationStep::ReclaimCaches
        );
        assert_eq!(escalation(PressureBand::Mild, 0), EscalationStep::Hold);
        assert_eq!(
            escalation(PressureBand::Moderate, 4096),
            EscalationStep::ReclaimCaches
        );
        assert_eq!(
            escalation(PressureBand::Moderate, 0),
            EscalationStep::HandOffToRamzip
        );
        assert_eq!(
            escalation(PressureBand::Severe, 0),
            EscalationStep::HandOffToRamzip
        );
        assert_eq!(
            escalation(PressureBand::Critical, 4096),
            EscalationStep::ReclaimCaches
        );
        assert_eq!(
            escalation(PressureBand::Critical, 0),
            EscalationStep::VmPolicy
        );
    }

    #[test]
    fn the_frame_allocator_is_an_honest_source() {
        use crate::bootinfo::{BootMemoryMap, MemoryRegion, RegionKind};
        use crate::frame::PhysAddr;

        // Based at frame 1: the zero page is permanently reserved, so a
        // base-0 region would enroll one frame fewer than it spans.
        let mut map = BootMemoryMap::new();
        map.push(MemoryRegion {
            start: PhysAddr::new(PAGE_SIZE as u64),
            length: (64 * PAGE_SIZE) as u64,
            kind: RegionKind::Usable,
        });
        let allocator = FrameAllocator::new(&map).expect("allocator over a usable map");
        let total = FreeMemorySource::total_bytes(&allocator);
        let before = FreeMemorySource::free_bytes(&allocator);
        assert_eq!(total, 64 * PAGE_SIZE);
        assert!(before <= total);
        let frame = allocator.alloc().expect("one frame");
        let after = FreeMemorySource::free_bytes(&allocator);
        assert_eq!(after + PAGE_SIZE, before);
        allocator.free(frame).expect("free the frame");
        assert_eq!(FreeMemorySource::free_bytes(&allocator), before);
    }
}
