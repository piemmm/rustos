//! Core-clock frequency surface of the Arch HAL.
//!
//! The System Information API reports the *live* clock frequency of every
//! CPU — the "cpu MHz" a `/proc/cpuinfo`-style reader expects. Deriving it
//! honestly is subtle: the [`super::cpucycles`] counter is a **fixed-rate
//! reference** time base (`CNTVCT_EL0`, the `time` CSR, an Invariant TSC,
//! `performance.now()`), so its rate never tracks dynamic voltage/frequency
//! scaling and cannot answer "how fast is this core running *now*". The live
//! frequency must instead come from a counter that advances at the actual
//! core clock, and reading one is target-divergent and — on some ports —
//! needs privileged enabling, so it is a closed Arch HAL slice of its own.
//!
//! # Two counters, one ratio
//!
//! A core running at frequency `f` accrues `f · Δt` core-clock cycles over a
//! wall-clock span `Δt`, while the fixed reference counter accrues
//! `reference_hz · Δt` reference ticks over the same span. Their ratio is
//! independent of `Δt`, so the kernel needs no timed wait to sample it:
//!
//! ```text
//! f = Δcore · reference_hz / Δreference
//! ```
//!
//! This is the same principle Intel's `APERF`/`MPERF` feedback pair encodes,
//! generalised across the four Tier-1 ISAs. The kernel samples both counters
//! at a per-CPU periodic point it already takes (the preemption tick) and
//! divides; no busy-wait, no blocking read.
//!
//! # What lives here
//!
//! * [`CoreClock`] — the per-port handle: the core-clock counter, the
//!   reference counter and its frequency, a best-effort per-CPU
//!   [`CoreClock::enable`], and an honest [`CoreClock::support`] declaration.
//! * [`CoreClockSupport`] — the honest per-port position, mirroring
//!   [`super::cpufeatures::FeatureSupport`] and [`super::memtag`]: a port with
//!   no core-clock counter declares [`CoreClockSupport::Unsupported`] with a
//!   justification rather than fabricating a rate.
//! * [`conformance`] — the conformance vertical every port runs.

/// One port's honest position on whether it can measure the live core clock.
///
/// Mirrors [`super::cpufeatures::FeatureSupport`]: a port takes exactly one
/// honest position. [`CoreClockSupport::Unsupported`] is permitted only where
/// the silicon (or host environment) genuinely exposes no core-clock counter
/// — the wasm32 guest sees only `performance.now()` — and the payload records
/// why. [`CoreClockSupport::Pending`] is for a source the silicon has but a
/// not-yet-landed probe must wire up. Both payloads must be non-empty.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CoreClockSupport {
    /// The port drives a core-clock counter and reports a live frequency.
    Supported,
    /// The port has no core-clock counter. The payload is the justification.
    Unsupported(&'static str),
    /// The counter exists but is not wired up yet. The payload is the note.
    Pending(&'static str),
}

impl CoreClockSupport {
    /// `true` if this port drives a live core-clock counter.
    #[must_use]
    pub const fn is_supported(self) -> bool {
        matches!(self, Self::Supported)
    }

    /// `true` if release-ready: supported or a justified `Unsupported`. A
    /// `Pending` source is not release-ready.
    #[must_use]
    pub const fn is_release_ready(self) -> bool {
        matches!(self, Self::Supported | Self::Unsupported(_))
    }

    /// The explanatory note for a non-supported decision, or `None`.
    #[must_use]
    pub const fn detail(self) -> Option<&'static str> {
        match self {
            Self::Supported => None,
            Self::Unsupported(reason) | Self::Pending(reason) => Some(reason),
        }
    }
}

/// The live-core-frequency handle an architecture port exposes.
///
/// The kernel's per-CPU frequency estimator reads [`Self::core_cycles`] and
/// [`Self::reference_cycles`] together at a periodic point and divides their
/// deltas, scaled by [`Self::reference_hz`], to report the live clock. A port
/// that cannot measure a core clock declares it through [`Self::support`] and
/// its reads fail closed to `0` — the estimator then reports no frequency
/// rather than a fabricated one.
///
/// Implementations must be [`Send`] + [`Sync`]: the kernel reaches the handle
/// from every CPU.
pub trait CoreClock: Send + Sync {
    /// Enable the calling CPU's core-clock counter.
    ///
    /// Best-effort and idempotent: the kernel calls it on each CPU as it
    /// comes up. A port whose [`Self::support`] is not
    /// [`CoreClockSupport::Supported`] must make this a no-op (it touches no
    /// register it has declared absent). A port that needs no enabling (the
    /// counter is always live) may also make it a no-op.
    fn enable(&self);

    /// The calling CPU's core-clock cycle count — a counter that advances at
    /// the *actual*, DVFS-varying core frequency.
    ///
    /// Must be non-decreasing on a single core for the duration of one
    /// measurement. Returns `0` when [`Self::support`] is not
    /// [`CoreClockSupport::Supported`] (fail closed — never a fabricated
    /// value).
    fn core_cycles(&self) -> u64;

    /// The calling CPU's fixed-rate **reference** counter — the ratio
    /// denominator, advancing at [`Self::reference_hz`] regardless of the
    /// core clock.
    ///
    /// Must be non-decreasing on a single core. Returns `0` when the port is
    /// not [`CoreClockSupport::Supported`].
    fn reference_cycles(&self) -> u64;

    /// The fixed frequency of [`Self::reference_cycles`], in Hz.
    ///
    /// Returns `0` when unknown or unsupported; the estimator treats a `0`
    /// reference frequency as "cannot measure" and reports no live frequency
    /// (fail closed — never a divide-by-zero, never a guessed rate).
    fn reference_hz(&self) -> u64;

    /// The port's honest declaration of whether it drives a core-clock
    /// counter. Must carry a non-empty justification for any non-supported
    /// position (checked by [`conformance::run_all`]).
    fn support(&self) -> CoreClockSupport;
}

/// Compute a live core frequency in Hz from two counter deltas.
///
/// `delta_core` core-clock cycles elapsed while `delta_reference` reference
/// ticks elapsed, the reference advancing at `reference_hz`. Returns `None`
/// when the sample is unusable — a zero reference frequency, a zero or
/// backward reference delta — so the caller reports no frequency rather than
/// a fabricated or divide-by-zero value (fail closed). Uses 128-bit
/// intermediate arithmetic so a large `delta_core · reference_hz` product
/// cannot overflow before the divide.
#[must_use]
pub fn frequency_hz(delta_core: u64, delta_reference: u64, reference_hz: u64) -> Option<u64> {
    if reference_hz == 0 || delta_reference == 0 {
        return None;
    }
    let numerator = u128::from(delta_core) * u128::from(reference_hz);
    let hz = numerator / u128::from(delta_reference);
    Some(u64::try_from(hz).unwrap_or(u64::MAX))
}

/// The core-clock conformance vertical.
///
/// Every architecture port runs [`conformance::run_all`] against its
/// [`CoreClock`] handle. Portable and host-run, exactly like the
/// [`super::cpucycles`] vertical.
pub mod conformance {
    use super::{frequency_hz, CoreClock};

    /// Run the entire core-clock conformance suite against `port`.
    ///
    /// # Panics
    ///
    /// Panics (failing the test) if any required property does not hold: a
    /// non-supported position carries an empty justification; a supported
    /// port's counters run backwards across a short window or report a zero
    /// reference frequency; or a non-supported port fabricates a non-zero
    /// counter reading.
    pub fn run_all<C: CoreClock + ?Sized>(port: &C) {
        support_is_honest(port);
        if port.support().is_supported() {
            counters_are_non_decreasing(port);
            reference_frequency_is_known(port);
        } else {
            unsupported_reads_are_zero(port);
        }
        // The ratio helper's fail-closed contract holds regardless of port.
        assert_eq!(frequency_hz(1_000, 0, 1_000_000_000), None);
        assert_eq!(frequency_hz(1_000, 1_000, 0), None);
    }

    /// Every non-supported position carries a non-empty justification.
    fn support_is_honest<C: CoreClock + ?Sized>(port: &C) {
        if let Some(reason) = port.support().detail() {
            assert!(
                !reason.trim().is_empty(),
                "a non-supported core-clock position must justify itself"
            );
        }
    }

    /// A supported port's core and reference counters never go backwards
    /// across a short window of reads on one core.
    fn counters_are_non_decreasing<C: CoreClock + ?Sized>(port: &C) {
        let mut last_core = port.core_cycles();
        let mut last_ref = port.reference_cycles();
        for _ in 0..64 {
            core::hint::spin_loop();
            let core = port.core_cycles();
            let reference = port.reference_cycles();
            assert!(
                core >= last_core,
                "core_cycles went backwards: {core} < {last_core}"
            );
            assert!(
                reference >= last_ref,
                "reference_cycles went backwards: {reference} < {last_ref}"
            );
            last_core = core;
            last_ref = reference;
        }
    }

    /// A supported port reports a non-zero reference frequency — the ratio is
    /// meaningless without it.
    fn reference_frequency_is_known<C: CoreClock + ?Sized>(port: &C) {
        assert!(
            port.reference_hz() != 0,
            "a supported core-clock port must report a non-zero reference frequency"
        );
    }

    /// A non-supported port fabricates no counter reading.
    fn unsupported_reads_are_zero<C: CoreClock + ?Sized>(port: &C) {
        assert_eq!(
            port.core_cycles(),
            0,
            "a non-supported core-clock port must report a zero core-cycle count"
        );
        assert_eq!(
            port.reference_cycles(),
            0,
            "a non-supported core-clock port must report a zero reference count"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicU64, Ordering};

    /// A supported stub: a core counter advancing three ticks per read over
    /// a reference advancing one, i.e. a nominal 3× the reference frequency.
    #[derive(Default)]
    struct SupportedStub {
        core: AtomicU64,
        reference: AtomicU64,
    }

    impl CoreClock for SupportedStub {
        fn enable(&self) {}
        fn core_cycles(&self) -> u64 {
            self.core.fetch_add(3, Ordering::Relaxed) + 3
        }
        fn reference_cycles(&self) -> u64 {
            self.reference.fetch_add(1, Ordering::Relaxed) + 1
        }
        fn reference_hz(&self) -> u64 {
            1_000_000_000
        }
        fn support(&self) -> CoreClockSupport {
            CoreClockSupport::Supported
        }
    }

    /// The honest host/wasm stub: no core-clock counter, reads are zero.
    struct UnsupportedStub;

    impl CoreClock for UnsupportedStub {
        fn enable(&self) {}
        fn core_cycles(&self) -> u64 {
            0
        }
        fn reference_cycles(&self) -> u64 {
            0
        }
        fn reference_hz(&self) -> u64 {
            0
        }
        fn support(&self) -> CoreClockSupport {
            CoreClockSupport::Unsupported("host build exposes no core-clock counter")
        }
    }

    #[test]
    fn support_helpers() {
        assert!(CoreClockSupport::Supported.is_supported());
        assert!(CoreClockSupport::Supported.is_release_ready());
        assert!(!CoreClockSupport::Pending("later").is_release_ready());
        assert!(CoreClockSupport::Unsupported("why").is_release_ready());
        assert_eq!(CoreClockSupport::Supported.detail(), None);
        assert_eq!(CoreClockSupport::Unsupported("why").detail(), Some("why"));
    }

    #[test]
    fn frequency_ratio_scales_by_reference() {
        // 3 core cycles per reference tick at a 1 GHz reference → 3 GHz.
        assert_eq!(frequency_hz(3, 1, 1_000_000_000), Some(3_000_000_000));
        // Fail closed on unusable samples.
        assert_eq!(frequency_hz(3, 0, 1_000_000_000), None);
        assert_eq!(frequency_hz(3, 1, 0), None);
    }

    #[test]
    fn frequency_ratio_does_not_overflow() {
        // A huge core delta and reference frequency must not overflow the
        // 128-bit intermediate before the divide.
        assert_eq!(
            frequency_hz(u64::MAX, u64::MAX, 1_000_000_000),
            Some(1_000_000_000)
        );
    }

    #[test]
    fn conformance_accepts_a_supported_port() {
        let port = SupportedStub::default();
        conformance::run_all(&port);
        let dynamic: &dyn CoreClock = &port;
        conformance::run_all(dynamic);
    }

    #[test]
    fn conformance_accepts_an_unsupported_port() {
        conformance::run_all(&UnsupportedStub);
    }

    #[test]
    #[should_panic(expected = "must report a zero core-cycle count")]
    fn conformance_rejects_a_fabricated_reading() {
        struct Liar;
        impl CoreClock for Liar {
            fn enable(&self) {}
            fn core_cycles(&self) -> u64 {
                999
            }
            fn reference_cycles(&self) -> u64 {
                0
            }
            fn reference_hz(&self) -> u64 {
                0
            }
            fn support(&self) -> CoreClockSupport {
                CoreClockSupport::Unsupported("claims none yet reports a count")
            }
        }
        conformance::run_all(&Liar);
    }

    #[test]
    #[should_panic(expected = "must justify itself")]
    fn conformance_rejects_an_unjustified_position() {
        struct Blank;
        impl CoreClock for Blank {
            fn enable(&self) {}
            fn core_cycles(&self) -> u64 {
                0
            }
            fn reference_cycles(&self) -> u64 {
                0
            }
            fn reference_hz(&self) -> u64 {
                0
            }
            fn support(&self) -> CoreClockSupport {
                CoreClockSupport::Unsupported("  ")
            }
        }
        conformance::run_all(&Blank);
    }
}
