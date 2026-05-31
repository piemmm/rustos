//! RustOS EEVDF scheduler policy.
//!
//! This `kernel/sched/eevdf` crate is one concrete implementation of the
//! [`SchedulerPolicy`] contract defined in `kernel/sched/api`
//! (`AGENTS.md` §17.1). It is a sibling of the `kernel/sched/mlfq` policy
//! crate; adding or replacing a policy means adding a sibling crate,
//! never editing another one (§17.1, §2.2 carve-out — parallel policy
//! implementations are deliberate, not duplication).
//!
//! # Design summary
//!
//! * **Fully tickless EEVDF.** Dispatch is *Earliest Eligible Virtual
//!   Deadline First* (Stoica & Abdel-Wahab, 1995). Each task carries a
//!   virtual *eligible* time `ve` and a virtual *deadline* `vd = ve +
//!   request/weight`. A task is eligible once its CPU's virtual time `V`
//!   reaches `ve`; among eligible tasks the earliest `vd` runs. Fairness,
//!   eligibility, and preemption are driven **entirely by virtual time
//!   advanced as work is dispatched** — never by a periodic timer tick.
//!   [`Scheduler::on_timer_tick`] is a pure observation counter; no
//!   scheduling decision reads it. This is the property that makes the
//!   policy tickless: on a real port the timer can run in one-shot /
//!   `NO_HZ` mode, programmed only for the next virtual deadline.
//! * **Proportional share by weight.** The three [`Priority`] bands map
//!   to a 4:2:1 weight ratio; a task accrues virtual time inversely to
//!   its weight, so a `High` task is dispatched roughly four times as
//!   often as a `Low` one while neither is ever starved (every eligible
//!   task has a finite deadline).
//! * **Per-CPU run queues, work-stealing across cores.** Each CPU owns
//!   one virtual-time `RunQueue` with its own clock; idle
//!   CPUs steal the earliest-deadline task from a pseudo-random victim
//!   and rebase its virtual times onto the stealing CPU's clock (the
//!   EEVDF migration rule — a task carries no lag across CPUs).
//! * **IPI-based preemption.** [`Scheduler::spawn`] and
//!   [`Scheduler::unpark`] notify the home CPU via
//!   [`SchedulerArch::send_ipi`]; the arch port decides whether that is
//!   an immediate context switch or a deferred reschedule.
//! * **No global mutable state outside per-CPU areas.** The only shared
//!   structures are an `Arc<A: SchedulerArch>`, an `RwLock`-protected
//!   task registry, and a `SpinLock`-protected overflow list for when a
//!   home CPU's queue is full.
//!
//! Full algorithmic detail, invariants, and the EEVDF-vs-MLFQ comparison
//! live in `docs/src/architecture/scheduler.md`.

#![no_std]

extern crate alloc;

// Host tests need `std` for `Arc` and `AtomicU*` ergonomics; the crate
// itself remains `no_std` for production builds (`AGENTS.md` §1).
#[cfg(test)]
extern crate std;

mod runqueue;
mod scheduler;
mod task;

// Re-export the contract vocabulary so call sites
// (`rustos_kernel_sched_eevdf::{Scheduler, Priority, …}`) resolve to the
// single canonical definitions in `kernel/sched/api` (`AGENTS.md` §2.2 —
// no duplication).
pub use rustos_kernel_sched_api::{
    CpuId, Priority, SchedError, SchedResult, SchedulerArch, SchedulerConfig, SchedulerPolicy,
    StepOutcome, TaskAction, TaskContext, TaskId, TaskState,
};

#[cfg(any(test, feature = "test-arch"))]
pub use rustos_kernel_sched_api::TestArch;

pub use scheduler::Scheduler;
