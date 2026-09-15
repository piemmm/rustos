//! Keeping a process's memory-pressure band current
//! (`plans/SMARTRAM.md` SMART5, `plans/NOTICE.md`).
//!
//! A process that holds reclaimable memory sizes that memory against the band
//! the kernel publishes, and has no way to measure free memory itself. It
//! learns the band through the `MemoryPressure` system notice: the
//! edge-triggered `WaitSourceKind::SystemNotice` member says the band moved,
//! and `notice_read` answers with the depth it moved to.
//!
//! Arm the wake, read the band, publish it — the same three steps in every
//! program that caches anything, so they live here once rather than being
//! re-spelled per program. `lib/rt` owns the gauge itself and deliberately
//! does not fetch it (choosing what to do about a band is not the runtime's
//! business).
//!
//! # The read is a syscall, not a service call
//!
//! The band arrives from the kernel directly. It has to: the wake lands on the
//! loop that owes the user a frame, and an IPC round trip to the System
//! Information service there is exactly the blocking I/O an interactive
//! surface may not perform. The gated
//! [`crate::kstats::memory_pressure_band`] query remains what a *monitor*
//! reads to display the band; a cache holder converges on the notice.
//!
//! # A gauge nobody reports to admits nothing
//!
//! [`ReportedPressure`] starts at [`PressureBand::Critical`] so a process that
//! never learns the band cannot grow a cache on a machine that may be
//! starving. That default is only safe while it is *transient*: a program that
//! never arms the wake leaves every cache in the process permanently unable to
//! retain anything, turning each cached value into a fresh rebuild — for a
//! glyph, a whole IPC round trip per character drawn. Any program with an
//! event loop and a cache calls `watch` once and `refresh` on the wake.

use tairix_reclaim::{PressureBand, ReportedPressure};

/// Publish band `depth` to `gauge`, returning whether the band actually moved.
///
/// A depth outside the known set publishes nothing and reports `false`: the
/// gauge keeps the band it already had rather than assuming the machine is
/// comfortable, which costs cache hits and never correctness. An unrecognised
/// band is never read as a guess in either direction.
///
/// The injectable form, so the policy is exercised without a kernel; the
/// `program::refresh` binding of it is what a freestanding program calls.
#[must_use]
pub fn publish_depth(depth: u8, gauge: &ReportedPressure) -> bool {
    let Some(band) = PressureBand::from_known_depth(depth) else {
        return false;
    };
    gauge.report(band)
}

/// The production bindings: the process gauge `lib/rt` owns, read from the
/// kernel's own notice topic.
#[cfg(all(freestanding, feature = "program"))]
mod program {
    use tairix_abi::notice::{Notice, NoticeTopic, NOTICE_PAYLOAD_MAX};
    use tairix_abi::{WaitSetOp, WaitSourceKind};

    /// Re-read the band and publish it to this process's gauge, returning
    /// whether it moved.
    ///
    /// Call this on a [`watch`]-armed wake; a caller that holds caches
    /// enforces their new ceiling when this reports `true`. A caller merely
    /// priming a gauge before anything is cached has nothing to enforce and
    /// discards the answer.
    ///
    /// A refused or malformed read publishes nothing and reports `false`,
    /// leaving the gauge on the band it already had.
    #[must_use]
    pub fn refresh() -> bool {
        let mut buf = [0u8; NOTICE_PAYLOAD_MAX];
        let read = tairix_rt::notice_read(NoticeTopic::MemoryPressure, &mut buf);
        if read < 0 {
            return false;
        }
        // Narrowed rather than cast: `usize` is 32 bits on a wasm32 build, so
        // a cast would truncate a length the kernel reported.
        let Ok(read) = usize::try_from(read) else {
            return false;
        };
        let Some(bytes) = buf.get(..read) else {
            return false;
        };
        let Ok(Notice::MemoryPressure { band }) =
            Notice::decode(NoticeTopic::MemoryPressure, bytes)
        else {
            return false;
        };
        super::publish_depth(band, tairix_rt::pressure::gauge())
    }

    /// Add the memory-pressure wake to `set` under `token` and prime the
    /// process gauge with the band in force now, reporting whether the wake
    /// was armed.
    ///
    /// Both halves are one call because neither works alone: the wake reports
    /// only *changes*, so without the priming read the gauge would sit on its
    /// fail-closed unknown band until the machine happened to move, and
    /// without the wake the primed band would go stale the moment it did.
    ///
    /// `false` means the kernel refused the member — the caller has no wake
    /// source and treats that as the start-up failure it is.
    #[must_use]
    pub fn watch(set: u64, token: u64) -> bool {
        if tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::SystemNotice,
            u64::from(NoticeTopic::MemoryPressure.as_u32()),
            token,
        ) != 0
        {
            return false;
        }
        let _ = refresh();
        true
    }
}

#[cfg(all(freestanding, feature = "program"))]
pub use program::{refresh, watch};

#[cfg(test)]
mod tests {
    use super::publish_depth;
    use tairix_reclaim::{
        CacheBudget, PressureBand, PressureGauge, ReclaimClass, ReportedPressure,
    };

    #[test]
    fn a_reported_band_reaches_the_gauge_and_is_a_change_only_once() {
        let gauge = ReportedPressure::unknown();
        assert!(publish_depth(PressureBand::Normal.depth(), &gauge));
        assert_eq!(gauge.band(), PressureBand::Normal);
        assert!(!publish_depth(PressureBand::Normal.depth(), &gauge));
    }

    #[test]
    fn a_reported_normal_band_is_what_lets_a_cache_grow() {
        // The whole point of wiring the band up: an unreported gauge admits
        // nothing, so every cached value is rebuilt on every use.
        let gauge = ReportedPressure::unknown();
        let class = ReclaimClass::CleanFileData;
        let budget = CacheBudget::from_ceiling(1 << 20);
        assert!(!gauge.growth_permitted(class, budget, 1));
        assert!(publish_depth(PressureBand::Normal.depth(), &gauge));
        assert!(gauge.growth_permitted(class, budget, 1));
    }

    #[test]
    fn a_tightening_band_is_reported_as_a_change_and_closes_growth() {
        let gauge = ReportedPressure::unknown();
        assert!(publish_depth(PressureBand::Normal.depth(), &gauge));
        assert!(publish_depth(PressureBand::Severe.depth(), &gauge));
        assert_eq!(gauge.band(), PressureBand::Severe);
        // Severe takes every class to zero, so nothing is admitted at all.
        for class in ReclaimClass::ALL {
            assert!(
                !gauge.growth_permitted(class, CacheBudget::from_ceiling(1 << 20), 1),
                "{class:?}"
            );
        }
    }

    #[test]
    fn a_depth_this_build_does_not_know_is_refused_not_guessed() {
        let gauge = ReportedPressure::unknown();
        assert!(publish_depth(PressureBand::Normal.depth(), &gauge));
        assert!(!publish_depth(u8::MAX, &gauge));
        assert_eq!(gauge.band(), PressureBand::Normal);
        // Not clamped to critical either: an unknown depth must not pin
        // every cache in the process shut.
        assert!(!publish_depth(PressureBand::Critical.depth() + 1, &gauge));
        assert_eq!(gauge.band(), PressureBand::Normal);
    }

    #[test]
    fn every_known_band_round_trips_through_its_depth() {
        let gauge = ReportedPressure::unknown();
        for band in PressureBand::ALL {
            let _ = publish_depth(band.depth(), &gauge);
            assert_eq!(gauge.band(), band);
        }
    }
}
