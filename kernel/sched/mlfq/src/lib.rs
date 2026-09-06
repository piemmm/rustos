//! TAIRiX MLFQ scheduler policy.
//!
//! This `kernel/sched/mlfq` crate is one concrete implementation of the
//! [`SchedulerPolicy`] contract defined in `kernel/sched/api`. It is a sibling of any other policy crate (e.g. a
//! future `kernel/sched/eevdf` or `kernel/sched/rt`); adding a policy
//! means adding a sibling crate, never editing this one.
//!
//! # Design summary
//!
//! * **Per-CPU run queues, work-stealing across cores.** Each CPU owns
//!   one bounded `RunDeque` per [`Priority`] band; idle CPUs steal from a
//!   victim chosen by the shared per-CPU `StealScan`
//!   (`kernel/sched/api`). The deque is a bounded Chase–Lev variant — see
//!   `runqueue` for the citation and ordering rationale.
//! * **MLFQ priority + fairness.** Three bands (High / Normal / Low),
//!   demotion after `yields_before_demotion` voluntary yields, and an
//!   anti-starvation global priority boost every `boost_interval_ticks`.
//!   The policy is the classical MLFQ as described in Arpaci-Dusseau,
//!   *Operating Systems: Three Easy Pieces*, ch. 8.
//! * **Tickless boost cadence (carve-out).** TAIRiX is tickless: there is no global fixed-frequency timer. The
//!   boost interval is far longer than one scheduling quantum and there is
//!   only one per-CPU timer, so the boost rides the **on-demand preemption
//!   one-shots** the kernel already arms whenever a CPU is *contended*
//!   (exactly when starvation is possible). [`Scheduler::step`] — and with
//!   it the internal `maybe_priority_boost` — runs at that quantum cadence,
//!   so the boost fires once `boost_interval_ticks` elapse; a CPU running a
//!   sole runnable task disarms (no starvation, no boost needed) and takes
//!   no ticks. No global periodic tick is reintroduced for the boost.
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
// The `loom` model-checking build compiles only the interleaving harnesses,
// so items only production reaches read as dead there.
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
// it private (minimal public surface).
#[cfg(loom)]
pub mod runqueue;
#[cfg(not(loom))]
mod runqueue;

// Re-export the contract vocabulary so existing call sites
// (`tairix_kernel_sched_mlfq::{Scheduler, Priority, …}`) keep resolving to
// the single canonical definitions in `kernel/sched/api` (no duplication).
pub use tairix_kernel_sched_api::{
    choose_task_id, CoreClass, CpuId, ExitDisposition, Priority, SchedClass, SchedError,
    SchedResult, SchedulerArch, SchedulerConfig, SchedulerPolicy, StepOutcome, TaskAction,
    TaskContext, TaskId, TaskState,
};

#[cfg(any(test, feature = "test-arch"))]
pub use tairix_kernel_sched_api::TestArch;

pub use scheduler::Scheduler;
