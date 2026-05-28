//! RustOS kernel SMP scheduler.
//!
//! This crate ships the architecture-neutral half of RustOS' scheduler
//! (Stage 2.3 of `PLAN.md`). It is intentionally landed *before* the
//! architecture ports (Stage 3) so they can plug in a real
//! [`SchedulerArch`] implementation without having to retrofit the
//! generic core (`AGENTS.md` §2.4 — no interface creep).
//!
//! # Surface
//!
//! | Type                            | Role                                              |
//! | ------------------------------- | ------------------------------------------------- |
//! | [`Scheduler<A>`]                | Owns per-CPU run queues; dispatches tasks.        |
//! | [`SchedulerConfig`]             | Static configuration (CPU count, quantum, etc.).  |
//! | [`SchedulerArch`]               | Architecture hook (IPIs, current-CPU, ticks).     |
//! | [`Priority`] / [`TaskAction`]   | Public lifecycle types.                           |
//! | [`runqueue::RunDeque`]          | Bounded SPMC work-stealing deque.                 |
//! | [`SchedError`] / [`SchedResult`] | Error surface.                                   |
//! | `TestArch` *(feature: `test-arch`)* | In-memory arch implementation for host tests. |
//!
//! # Design summary
//!
//! * **Per-CPU run queues, work-stealing across cores.** Each CPU owns
//!   one bounded [`runqueue::RunDeque`] per [`Priority`] band; idle CPUs
//!   steal from a pseudo-random victim. The deque is a bounded Chase–Lev
//!   variant — see `runqueue` for the citation and ordering rationale.
//! * **MLFQ priority + fairness.** Three bands (High / Normal / Low),
//!   demotion after `yields_before_demotion` voluntary yields, periodic
//!   global priority boost to guarantee starvation-freedom. The policy
//!   is the classical MLFQ as described in Arpaci-Dusseau, *Operating
//!   Systems: Three Easy Pieces*, ch. 8.
//! * **IPI-based preemption.** [`Scheduler::spawn`] and
//!   [`Scheduler::unpark`] always notify the home CPU via
//!   [`SchedulerArch::send_ipi`]; the arch port decides whether that
//!   triggers an immediate context switch or a deferred reschedule.
//! * **Cancellation-safe lifecycle.** [`Scheduler::park`],
//!   [`Scheduler::unpark`], and [`Scheduler::exit`] are safe to call from
//!   any context, including against tasks currently running on another
//!   CPU. Transitions are CAS-driven; the scheduler re-checks state
//!   after the body returns so an external `park` / `exit` always wins
//!   over the body's own next intent.
//! * **No global mutable state outside per-CPU areas.** The only shared
//!   structures are an `Arc<A: SchedulerArch>`, an `RwLock`-protected
//!   task registry (`BTreeMap<TaskId, …>`), and a `SpinLock`-protected
//!   overflow list for when a home CPU's queue is full. Each is justified
//!   in the doc comment of its owning field (`AGENTS.md` §4 / §1).
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

pub mod arch;
pub mod error;
pub mod runqueue;
pub mod scheduler;
pub mod task;

#[cfg(any(test, feature = "test-arch"))]
pub use arch::TestArch;
pub use arch::{CpuId, SchedulerArch};
pub use error::{SchedError, SchedResult};
pub use scheduler::{Scheduler, SchedulerConfig, StepOutcome};
pub use task::{Priority, TaskAction, TaskContext, TaskId, TaskState};
