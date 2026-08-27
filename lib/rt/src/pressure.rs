//! The process's view of system memory pressure (`plans/SMARTRAM.md`
//! SMART5).
//!
//! A program that holds reclaimable memory — rasterised desktop assets,
//! glyph atlases, decoded resources — is expected to give it back as the
//! machine tightens, in the same order and at the same bands as the
//! kernel's own caches. It cannot measure free memory itself (free
//! frames, watermarks, and the reserve floor are kernel state, and
//! rightly unreadable), so it is *told* the band and stores it here.
//!
//! This module holds exactly one thing: the process-wide
//! [`ReportedPressure`] gauge. It is the process's single answer to "how
//! tight is memory", so every cache in the process shrinks together
//! instead of each holding its own opinion.
//!
//! # How the band gets here
//!
//! The runtime deliberately does **not** fetch the band itself. Reading
//! it means talking to the System Information service, which needs an
//! endpoint and a transport the runtime has no business choosing for a
//! program. The owning program does that — parking on a
//! [`WaitSourceKind::MemoryPressure`](tairix_abi::WaitSourceKind::MemoryPressure)
//! wait-set member, reading the ungated
//! [`SysinfoQueryId::MEMORY_PRESSURE_BAND`](tairix_abi::SysinfoQueryId::MEMORY_PRESSURE_BAND)
//! query, and calling [`report`] — and every cache in the process then
//! sees the new band through [`gauge`]. Event-driven throughout: no
//! program polls for this.
//!
//! # Before the first report
//!
//! The gauge answers [`PressureBand::Critical`] until told otherwise, so
//! a process that never wires the band up admits nothing to its caches
//! and simply builds every value on demand. Unknown fails closed:
//! rendering uncached is slower, but guessing "plenty of memory" on a
//! machine that is starving is a defect.

use tairix_reclaim::{PressureBand, ReportedPressure};

/// The one gauge every reclaimable cache in this process consults.
static PROCESS_PRESSURE: ReportedPressure = ReportedPressure::unknown();

/// The process-wide memory-pressure gauge.
///
/// Hand this to every [`ReclaimCache`](tairix_reclaim::ReclaimCache) the
/// process builds, so they all shrink on the same band at the same
/// moment.
#[must_use]
pub fn gauge() -> &'static ReportedPressure {
    &PROCESS_PRESSURE
}

/// Publish the band the kernel reported, returning whether it differed
/// from the band already held.
///
/// A caller shrinks its caches on `true` and does nothing on `false`, so
/// a spurious wake costs one atomic and no eviction work.
pub fn report(band: PressureBand) -> bool {
    PROCESS_PRESSURE.report(band)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_reclaim::{CacheBudget, PressureGauge, ReclaimClass};

    #[test]
    fn an_unreported_band_admits_nothing() {
        // The process gauge is shared, so this asserts the *type's*
        // start state rather than mutating the process-wide one.
        let fresh = ReportedPressure::unknown();
        assert_eq!(fresh.band(), PressureBand::Critical);
        let budget = CacheBudget::from_ceiling(1 << 20);
        for class in ReclaimClass::ALL {
            assert!(!fresh.growth_permitted(class, budget, 1), "{class:?}");
        }
    }

    #[test]
    fn a_report_is_only_a_change_the_first_time() {
        let fresh = ReportedPressure::unknown();
        assert!(fresh.report(PressureBand::Normal));
        assert!(!fresh.report(PressureBand::Normal));
        assert!(fresh.report(PressureBand::Mild));
    }

    #[test]
    fn a_wire_depth_this_build_does_not_know_is_read_as_the_deepest_band() {
        // The wire decode already refuses an out-of-range depth, so this
        // is the second line of defence: a depth that somehow arrives
        // unrecognised is read as critical (shrink everything), never as
        // normal (grow freely).
        assert_eq!(PressureBand::from_depth(5), PressureBand::Critical);
        assert_eq!(PressureBand::from_depth(u8::MAX), PressureBand::Critical);
    }
}
