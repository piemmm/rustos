//! The single scheduler selection point (`AGENTS.md` §17.1).
//!
//! `kernel/core` is the *only* kernel crate (besides `kernel/sched/*`)
//! permitted to name a concrete scheduler policy. It selects exactly one
//! implementation at build time via a `scheduler-*` Cargo feature and
//! re-exports it as [`Scheduler`]; the rest of `kernel/core` — and every
//! other kernel crate — works in terms of this alias and the
//! [`rustos_kernel_sched_api`] contract, never a concrete policy crate.
//!
//! Adding a policy means adding a sibling `kernel/sched/<impl>` crate and
//! a `scheduler-<impl>` feature here; the `compile_error!` guards below
//! enforce that exactly one is active per image.

// Re-export the policy-neutral vocabulary so the rest of `kernel/core`
// names one canonical definition (`AGENTS.md` §2.2).
pub(crate) use rustos_kernel_sched_api::{CpuId, SchedError, SchedulerArch, SchedulerConfig};

/// The concrete scheduler policy selected for this image.
#[cfg(feature = "scheduler-mlfq")]
pub(crate) use rustos_kernel_sched_mlfq::Scheduler;

// Exactly one `scheduler-*` feature must be active (§17.1). With a single
// policy in tree today this reduces to "the one feature is on"; as more
// policies land, add a `scheduler-<impl>` arm above and a mutual-exclusion
// guard below.
#[cfg(not(feature = "scheduler-mlfq"))]
compile_error!(
    "kernel/core: no scheduler policy selected — enable exactly one \
     `scheduler-*` feature (e.g. `scheduler-mlfq`) per image (AGENTS.md §17.1)"
);
