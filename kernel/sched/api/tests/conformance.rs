//! acceptance test: the in-tree concrete scheduler policy must pass
//! the shared [`tairix_kernel_sched_api::conformance`] suite.
//!
//! This is the canonical location the charter names ("the shared
//! conformance test suite in `kernel/sched/api/tests/`"). It links the
//! concrete `kernel/sched/mlfq` policy as a dev-dependency and drives it
//! purely through the [`SchedulerPolicy`](tairix_kernel_sched_api::SchedulerPolicy)
//! trait — exactly as a future EEVDF/RT policy would be exercised here.

use tairix_kernel_sched_api::conformance;
use tairix_kernel_sched_api::TestArch;
use tairix_kernel_sched_mlfq::Scheduler;

#[test]
fn mlfq_passes_scheduler_policy_conformance() {
    conformance::run_all::<Scheduler<TestArch>>();
}
