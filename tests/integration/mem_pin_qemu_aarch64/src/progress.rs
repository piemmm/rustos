//! Progress-based give-up policy for the four-vCPU migration driver.
//!
//! The migration controller spins on the boot CPU orchestrating a child that
//! runs on the *other* CPUs. How many controller iterations elapse per unit of
//! child progress is decided entirely by how the host schedules the guest's
//! vCPU threads against one another, so a give-up budget counted in *total*
//! controller iterations measures the controller's spin rate, not the child's
//! progress: a busier or more unevenly co-scheduled host burns the budget
//! before a perfectly healthy child finishes, fabricating a deadlock a quiet
//! host would never see. `StallGuard` instead counts only *consecutive*
//! iterations in which the workload made no forward progress at all. Any sign
//! the child is still advancing resets the count, so the controller waits as
//! long as the workload genuinely needs — independent of host load — and only
//! a child that has truly stopped advancing for the whole window is declared
//! wedged (fail-loud, never a silent give-up).

/// Consecutive-no-progress give-up counter (see the module docs).
pub(crate) struct StallGuard {
    /// Number of back-to-back no-progress observations that means wedged.
    limit: u64,
    /// No-progress observations since the last one that made progress.
    idle: u64,
}

impl StallGuard {
    /// A guard that declares a deadlock after `limit` consecutive no-progress
    /// observations. `limit` is clamped to at least one so a zero can never
    /// make the guard fire before any work has even been attempted.
    pub(crate) const fn new(limit: u64) -> Self {
        Self {
            limit: if limit == 0 { 1 } else { limit },
            idle: 0,
        }
    }

    /// Record one controller iteration. `progressed` is whether the workload
    /// advanced this iteration. Returns `true` once `limit` consecutive
    /// no-progress iterations have elapsed — the point at which the workload
    /// is genuinely wedged and the driver must give up. A single progressing
    /// iteration resets the run, so a slow-but-live workload never trips it.
    pub(crate) fn observe(&mut self, progressed: bool) -> bool {
        if progressed {
            self.idle = 0;
            false
        } else {
            self.idle += 1;
            self.idle >= self.limit
        }
    }
}

#[cfg(test)]
mod tests {
    use super::StallGuard;

    #[test]
    fn a_perpetually_progressing_workload_never_gives_up() {
        let mut guard = StallGuard::new(4);
        // Far more iterations than the limit: because every one makes
        // progress, the guard must never fire, no matter how long it runs.
        for _ in 0..10_000 {
            assert!(!guard.observe(true));
        }
    }

    #[test]
    fn only_a_full_run_of_no_progress_gives_up() {
        let mut guard = StallGuard::new(3);
        assert!(!guard.observe(false));
        assert!(!guard.observe(false));
        // The third consecutive no-progress observation reaches the limit.
        assert!(guard.observe(false));
    }

    #[test]
    fn any_progress_resets_the_stall_run() {
        let mut guard = StallGuard::new(3);
        assert!(!guard.observe(false));
        assert!(!guard.observe(false));
        // One progressing iteration clears the run, so it now takes another
        // full `limit` of no-progress observations to trip — proving the
        // give-up measures a *sustained* stall, not a total count.
        assert!(!guard.observe(true));
        assert!(!guard.observe(false));
        assert!(!guard.observe(false));
        assert!(guard.observe(false));
    }

    #[test]
    fn a_zero_limit_is_clamped_to_one() {
        let mut guard = StallGuard::new(0);
        // Clamped to one: the very first no-progress observation fires, and a
        // progressing one never does.
        assert!(!guard.observe(true));
        assert!(guard.observe(false));
    }
}
