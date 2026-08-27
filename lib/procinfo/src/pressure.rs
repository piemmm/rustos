//! Keeping a process's memory-pressure band current
//! (`plans/SMARTRAM.md` SMART5).
//!
//! A process that holds reclaimable memory sizes that memory against the band
//! the kernel publishes, and has no way to measure free memory itself. It
//! learns the band the one way a process may: the ungated
//! [`crate::kstats::memory_pressure_band`] query, woken by the
//! edge-triggered `WaitSourceKind::MemoryPressure` wait source.
//!
//! Arm the wake, read the band, publish it — the same three steps in every
//! program that caches anything, so they live here once rather than being
//! re-spelled per program. `lib/rt` owns the gauge itself and deliberately
//! does not fetch it (choosing a transport is not the runtime's business);
//! this crate already owns the System Information client, so it is where the
//! fetch belongs.
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

use crate::kstats::memory_pressure_band;
use crate::transport::Transport;

/// Read the published band over `transport` and publish it to `gauge`,
/// returning whether the band actually moved.
///
/// A refused or malformed read publishes nothing and reports `false`: the
/// gauge keeps the band it already had rather than assuming the machine is
/// comfortable, which costs cache hits and never correctness. A depth outside
/// the known set is one such malformed reply — the wire decode refuses it, so
/// an unrecognised band is never read as a guess in either direction.
///
/// The injectable form, so the policy is exercised against a fixture with no
/// service running; `refresh` is the process-wide binding of it.
pub fn refresh_into(transport: &dyn Transport, gauge: &ReportedPressure) -> bool {
    let Ok(reported) = memory_pressure_band(transport) else {
        return false;
    };
    gauge.report(PressureBand::from_depth(reported.band))
}

/// The production bindings: the process gauge `lib/rt` owns, read over the
/// `sysinfo-v1` endpoint.
#[cfg(all(freestanding, feature = "program"))]
mod program {
    use tairix_abi::{WaitSetOp, WaitSourceKind};

    use crate::client::IpcTransport;

    /// Re-read the band and publish it to this process's gauge, returning
    /// whether it moved.
    ///
    /// Call this on a [`watch`]-armed wake; a caller that holds caches
    /// enforces their new ceiling when this reports `true`. A caller merely
    /// priming a gauge before anything is cached has nothing to enforce and
    /// discards the answer.
    #[must_use]
    pub fn refresh() -> bool {
        super::refresh_into(&IpcTransport, tairix_rt::pressure::gauge())
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
            WaitSourceKind::MemoryPressure,
            0,
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
    use super::refresh_into;
    use crate::transport::Transport;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use tairix_abi::sysinfo::{MemoryPressureBand, SysinfoQueryId, SysinfoRequestHeader};
    use tairix_abi::Errno;
    use tairix_reclaim::{
        CacheBudget, PressureBand, PressureGauge, ReclaimClass, ReportedPressure,
    };

    /// A `sysinfod` stand-in answering the band query with one depth, or
    /// refusing it, and recording which query it was actually asked.
    struct Fixture {
        answer: Result<u8, Errno>,
        seen: RefCell<Vec<SysinfoQueryId>>,
    }

    impl Fixture {
        fn answering(depth: u8) -> Self {
            Self {
                answer: Ok(depth),
                seen: RefCell::new(Vec::new()),
            }
        }

        fn refusing(errno: Errno) -> Self {
            Self {
                answer: Err(errno),
                seen: RefCell::new(Vec::new()),
            }
        }
    }

    impl Transport for Fixture {
        fn query(&self, request: &[u8]) -> Result<Vec<u8>, Errno> {
            let header = SysinfoRequestHeader::from_bytes(request)?;
            self.seen.borrow_mut().push(header.query);
            let depth = self.answer?;
            Ok(MemoryPressureBand {
                band: depth,
                ..MemoryPressureBand::default()
            }
            .to_le_bytes()
            .to_vec())
        }
    }

    #[test]
    fn the_band_only_query_is_the_one_issued() {
        let fixture = Fixture::answering(PressureBand::Normal.depth());
        refresh_into(&fixture, &ReportedPressure::unknown());
        assert_eq!(
            fixture.seen.borrow().as_slice(),
            &[SysinfoQueryId::MEMORY_PRESSURE_BAND]
        );
    }

    #[test]
    fn a_reported_band_reaches_the_gauge_and_is_a_change_only_once() {
        let gauge = ReportedPressure::unknown();
        let fixture = Fixture::answering(PressureBand::Normal.depth());
        assert!(refresh_into(&fixture, &gauge));
        assert_eq!(gauge.band(), PressureBand::Normal);
        assert!(!refresh_into(&fixture, &gauge));
    }

    #[test]
    fn a_reported_normal_band_is_what_lets_a_cache_grow() {
        // The whole point of wiring the band up: an unreported gauge admits
        // nothing, so every cached value is rebuilt on every use.
        let gauge = ReportedPressure::unknown();
        let class = ReclaimClass::CleanFileData;
        let budget = CacheBudget::from_ceiling(1 << 20);
        assert!(!gauge.growth_permitted(class, budget, 1));
        assert!(refresh_into(
            &Fixture::answering(PressureBand::Normal.depth()),
            &gauge
        ));
        assert!(gauge.growth_permitted(class, budget, 1));
    }

    #[test]
    fn a_refused_read_leaves_the_band_alone() {
        let gauge = ReportedPressure::unknown();
        assert!(refresh_into(
            &Fixture::answering(PressureBand::Normal.depth()),
            &gauge
        ));
        assert!(!refresh_into(&Fixture::refusing(Errno::NotFound), &gauge));
        assert_eq!(gauge.band(), PressureBand::Normal);
    }

    #[test]
    fn a_tightening_band_is_reported_as_a_change_and_closes_growth() {
        let gauge = ReportedPressure::unknown();
        assert!(refresh_into(
            &Fixture::answering(PressureBand::Normal.depth()),
            &gauge
        ));
        assert!(refresh_into(
            &Fixture::answering(PressureBand::Severe.depth()),
            &gauge
        ));
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
        assert!(refresh_into(
            &Fixture::answering(PressureBand::Normal.depth()),
            &gauge
        ));
        assert!(!refresh_into(&Fixture::answering(u8::MAX), &gauge));
        assert_eq!(gauge.band(), PressureBand::Normal);
    }
}
