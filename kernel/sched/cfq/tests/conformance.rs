//! acceptance test: the CFQ policy must pass the shared
//! [`tairix_kernel_sched_api::conformance`] suite.
//!
//! Every concrete scheduler runs the same suite against itself; the
//! canonical copy in `kernel/sched/api/tests/conformance.rs` drives the
//! build-selected policy. This drives the CFQ policy purely through the
//! [`SchedulerPolicy`](tairix_kernel_sched_api::SchedulerPolicy) trait —
//! exactly as the EEVDF and MLFQ siblings are exercised — proving the
//! policies are interchangeable behind the contract.

use tairix_kernel_sched_api::conformance;
use tairix_kernel_sched_api::TestArch;
use tairix_kernel_sched_cfq::Scheduler;

#[test]
fn cfq_passes_scheduler_policy_conformance() {
    conformance::run_all::<Scheduler<TestArch>>();
}
