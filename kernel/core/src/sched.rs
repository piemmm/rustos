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
///
/// EEVDF (`kernel/sched/eevdf`) is the default; MLFQ
/// (`kernel/sched/mlfq`) is selectable with `--no-default-features
/// --features scheduler-mlfq`. Both implement the same
/// [`rustos_kernel_sched_api::SchedulerPolicy`] contract, so the rest of
/// `kernel/core` is agnostic to which one this alias resolves to.
#[cfg(feature = "scheduler-eevdf")]
pub(crate) use rustos_kernel_sched_eevdf::Scheduler;
#[cfg(all(feature = "scheduler-mlfq", not(feature = "scheduler-eevdf")))]
pub(crate) use rustos_kernel_sched_mlfq::Scheduler;

// Exactly one `scheduler-*` feature must be active (§17.1). The two
// guards below reject the "none selected" and "more than one selected"
// configurations at compile time; adding a policy adds a `scheduler-<impl>`
// arm above and extends the mutual-exclusion guard.
#[cfg(not(any(feature = "scheduler-eevdf", feature = "scheduler-mlfq")))]
compile_error!(
    "kernel/core: no scheduler policy selected — enable exactly one \
     `scheduler-*` feature (e.g. `scheduler-eevdf` or `scheduler-mlfq`) \
     per image (AGENTS.md §17.1)"
);
#[cfg(all(feature = "scheduler-eevdf", feature = "scheduler-mlfq"))]
compile_error!(
    "kernel/core: more than one scheduler policy selected — enable \
     exactly one `scheduler-*` feature per image; disable the default \
     with `--no-default-features` before selecting another (AGENTS.md §17.1)"
);
