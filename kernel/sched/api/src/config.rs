//! Static scheduler configuration shared across policies.

/// Static configuration for a scheduler.
///
/// The configuration is consumed at construction and is never mutated; all
/// limits are therefore enforceable without locks. Defaults are tuned for
/// kernel use (`AGENTS.md` §5 — security defaults): bounded queues, a
/// short quantum, and a frequent priority boost.
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
    /// Tick interval at which every non-exited task is promoted back to
    /// [`crate::Priority::High`]. Bounds the worst-case starvation
    /// latency to this many ticks
    /// (`docs/src/architecture/scheduler.md` §"Starvation freedom").
    pub boost_interval_ticks: u64,
}

impl SchedulerConfig {
    /// Reasonable defaults for host tests and small embedded ports.
    ///
    /// Production ports are expected to override these after measuring
    /// their timer resolution and workload mix.
    #[must_use]
    pub const fn defaults_for(cpus: u32) -> Self {
        Self {
            cpus,
            queue_capacity_per_band: 16_384,
            yields_before_demotion: 1,
            boost_interval_ticks: 256,
        }
    }
}
