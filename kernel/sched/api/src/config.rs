//! Static scheduler configuration shared across policies.

use tairix_arch_api::timer::DEFAULT_PREEMPT_QUANTUM_HZ;

/// Static configuration for a scheduler.
///
/// The configuration is consumed at construction and is never mutated; all
/// limits are therefore enforceable without locks. Defaults are tuned for
/// kernel use (security defaults): bounded queues and a frequent priority
/// boost. Every interval is counted in scheduling quanta, so one value means
/// the same span on every port whatever its tick
/// ([`crate::SchedulerArch::quantum_ticks`]).
#[derive(Copy, Clone, Debug)]
pub struct SchedulerConfig {
    /// Number of CPUs the scheduler will manage. Must equal the count
    /// reported by the underlying [`crate::SchedulerArch`].
    pub cpus: u32,
    /// Per-band queue capacity. Must be a power of two ≥ 2.
    pub queue_capacity_per_band: usize,
    /// Number of `Yield`s permitted at a single priority before MLFQ
    /// demotion kicks in. A value of `1` matches the classical MLFQ
    /// description; larger values make demotion gentler.
    pub yields_before_demotion: u64,
    /// Quanta between promotions of every non-exited task back to
    /// [`crate::Priority::High`], bounding MLFQ's worst-case starvation
    /// latency to this many quanta
    /// (`docs/src/architecture/scheduler.md` §"Starvation freedom").
    pub boost_interval_quanta: u64,
}

impl SchedulerConfig {
    /// Defaults for every port and for host tests.
    ///
    /// The boost period is a second's worth of quanta at the shared default
    /// quantum rate: the classical MLFQ boost (Arpaci-Dusseau, *Operating
    /// Systems: Three Easy Pieces*, ch. 8).
    #[must_use]
    pub const fn defaults_for(cpus: u32) -> Self {
        Self {
            cpus,
            queue_capacity_per_band: 16_384,
            yields_before_demotion: 1,
            boost_interval_quanta: DEFAULT_PREEMPT_QUANTUM_HZ,
        }
    }
}
