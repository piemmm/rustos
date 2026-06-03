//! RustOS MLFQ scheduler policy.
//!
//! This `kernel/sched/mlfq` crate is one concrete implementation of the
//! [`SchedulerPolicy`] contract defined in `kernel/sched/api`
//! (`AGENTS.md` §17.1). It is a sibling of any other policy crate (e.g. a
//! future `kernel/sched/eevdf` or `kernel/sched/rt`); adding a policy
//! means adding a sibling crate, never editing this one (§17.1).
//!
//! # Design summary
//!
//! * **Per-CPU run queues, work-stealing across cores.** Each CPU owns
//!   one bounded `RunDeque` per [`Priority`] band; idle CPUs steal from a
//!   victim chosen by the project's shared non-cryptographic `FastRng`
//!   (`lib/rng`; `AGENTS.md` §2.2 — no second PRNG). The deque is a
//!   bounded Chase–Lev variant — see `runqueue` for the citation and
//!   ordering rationale.
//! * **MLFQ priority + fairness.** Three bands (High / Normal / Low),
//!   demotion after `yields_before_demotion` voluntary yields, periodic
//!   global priority boost to guarantee starvation-freedom. The policy
//!   is the classical MLFQ as described in Arpaci-Dusseau, *Operating
//!   Systems: Three Easy Pieces*, ch. 8.
//! * **IPI-based preemption.** [`Scheduler::spawn`] and
//!   [`Scheduler::unpark`] always notify the home CPU via
//!   [`SchedulerArch::send_ipi`]; the arch port decides whether that
//!   triggers an immediate context switch or a deferred reschedule.
//! * **Timer-driven preemption observation point.** The arch port's
//!   timer ISR calls [`Scheduler::on_timer_tick`] after acknowledging the
//!   device-level interrupt source; the scheduler bumps a per-CPU
//!   preemption counter and returns. It does not itself call
//!   [`Scheduler::step`] — the registry `RwLock` and the overflow
//!   `SpinLock` are forbidden from interrupt context by `lib/sync`, so the
//!   cooperative `step` loop driven from kernel-thread context is the only
//!   writer of run-queue state.
//! * **No global mutable state outside per-CPU areas.** The only shared
//!   structures are an `Arc<A: SchedulerArch>`, an `RwLock`-protected task
//!   registry (`BTreeMap<TaskId, …>`), and a `SpinLock`-protected overflow
//!   list for when a home CPU's queue is full.
//!
//! Full algorithmic detail, invariants, and a debugging guide live in
//! `docs/src/architecture/scheduler.md`.

#![no_std]
#![cfg_attr(loom, allow(dead_code))]

extern crate alloc;

// Host tests need `std` for `Arc<Mutex<_>>` and `thread::spawn` — the
// crate itself remains `no_std` for production builds.
#[cfg(test)]
extern crate std;

mod loom_compat;
mod scheduler;
mod task;

// `runqueue` is a crate-internal detail. It is exposed publicly *only*
// under `--cfg loom` so the model-checking harness in `tests/loom.rs`
// (a separate crate) can drive `RunDeque` directly; normal builds keep
// it private (`AGENTS.md` §8 — minimal public surface).
#[cfg(loom)]
pub mod runqueue;
#[cfg(not(loom))]
mod runqueue;

// Re-export the contract vocabulary so existing call sites
// (`rustos_kernel_sched_mlfq::{Scheduler, Priority, …}`) keep resolving to
// the single canonical definitions in `kernel/sched/api` (`AGENTS.md`
// §2.2 — no duplication).
pub use rustos_kernel_sched_api::{
    CoreClass, CpuId, Priority, SchedError, SchedResult, SchedulerArch, SchedulerConfig,
    SchedulerPolicy, StepOutcome, TaskAction, TaskContext, TaskId, TaskState,
};

#[cfg(any(test, feature = "test-arch"))]
pub use rustos_kernel_sched_api::TestArch;

pub use scheduler::Scheduler;
