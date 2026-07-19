//! TAIRiX scheduler contract.
//!
//! This `kernel/sched/api` crate (`SchedApi` in the layering) holds
//! the architecture-neutral *contract* every TAIRiX scheduler obeys:
//!
//! * the [`SchedulerPolicy`] trait — task admission, dispatch, yield,
//!   block/wake, priority/quantum accounting, and the SMP hooks;
//! * the policy-neutral lifecycle vocabulary ([`Priority`], [`TaskState`],
//!   [`TaskAction`], [`TaskContext`], [`TaskId`], [`SchedError`],
//!   [`StepOutcome`], [`SchedulerConfig`]);
//! * the re-exported scheduler-facing Arch HAL surface ([`CpuId`],
//!   [`SchedulerArch`]) and the host [`TestArch`] double; and
//! * the shared `conformance` suite (feature `conformance`) every
//!   concrete scheduler must pass.
//!
//! Concrete policies live in sibling `kernel/sched/<impl>` crates (e.g.
//! `kernel/sched/mlfq`) and implement [`SchedulerPolicy`]. Per only
//! `kernel/core` and `kernel/sched/*` may name a concrete scheduler type;
//! every other crate depends on this contract.

#![no_std]

extern crate alloc;

pub mod arch;
pub mod config;
pub mod conformance;
pub mod error;
pub mod outcome;
pub mod policy;
pub mod task;

#[cfg(any(test, feature = "test-arch"))]
pub use arch::TestArch;
pub use arch::{CoreClass, CpuId, SchedulerArch};
pub use config::SchedulerConfig;
pub use error::{SchedError, SchedResult};
pub use outcome::{ExitDisposition, StepOutcome};
pub use policy::SchedulerPolicy;
pub use task::{Priority, SchedClass, TaskAction, TaskContext, TaskId, TaskState};
