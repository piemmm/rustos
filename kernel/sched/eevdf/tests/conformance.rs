//! §17.1 acceptance test: the EEVDF policy must pass the shared
//! [`rustos_kernel_sched_api::conformance`] suite.
//!
//! Every concrete scheduler runs the same suite against itself; the
//! canonical copy in `kernel/sched/api/tests/conformance.rs` drives the
//! build-selected policy. This drives the EEVDF policy purely through the
//! [`SchedulerPolicy`](rustos_kernel_sched_api::SchedulerPolicy) trait —
//! exactly as the MLFQ sibling is exercised — proving the two policies
//! are interchangeable behind the contract.

use rustos_kernel_sched_api::conformance;
use rustos_kernel_sched_api::TestArch;
use rustos_kernel_sched_eevdf::Scheduler;

#[test]
fn eevdf_passes_scheduler_policy_conformance() {
    conformance::run_all::<Scheduler<TestArch>>();
}
