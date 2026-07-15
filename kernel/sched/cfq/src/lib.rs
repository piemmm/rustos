//! RustOS CFQ scheduler policy.
//!
//! This `kernel/sched/cfq` crate is one concrete implementation of the
//! [`SchedulerPolicy`] contract defined in `kernel/sched/api`. It is a
//! sibling of the `kernel/sched/eevdf` and `kernel/sched/mlfq` policy
//! crates; adding or replacing a policy means adding a sibling crate,
//! never editing another one (carve-out — parallel policy
//! implementations are deliberate, not duplication).
//!
//! # Design summary
//!
//! * **Completely Fair Queuing (Linux-CFS-like).** Each task carries a
//!   virtual runtime `vruntime`; the ready task with the *smallest*
//!   `vruntime` is dispatched next (the leftmost node of Linux CFS's
//!   red-black tree, here an ordered [`alloc::collections::BTreeSet`] keyed
//!   by `(vruntime, id)` — `O(log n)` pick/insert/remove). A dispatch
//!   charges the running task `SCALE / weight` of virtual runtime, so a
//!   heavier task's `vruntime` rises more slowly and it is dispatched more
//!   often — proportional share, with no band ever starved (every
//!   `vruntime` advances monotonically).
//! * **Non-tickless — the tickless carve-out.** RustOS is otherwise a
//!   tickless (`NO_HZ`) kernel: a policy arms its preemption one-shot only
//!   when a CPU is *contended* and disarms for a sole runnable task, so a
//!   quiet core takes no timer interrupts. **CFQ is the one scheduler
//!   permitted to violate that.** It keeps a fixed-frequency periodic
//!   quantum tick armed for *any* running task — including a lone
//!   CPU-bound one — so the timer interrupt fires at a steady `HZ` cadence
//!   like Linux's scheduler tick. Concretely the dispatch path calls
//!   [`SchedulerArch::set_preemption`]`(true)` unconditionally while a task
//!   runs; only a genuinely idle CPU (nothing runnable at all) disarms in
//!   [`Scheduler::step`]. A fired tick forces a context switch only when
//!   there is another runnable task (the kernel's `preempt_current` gate,
//!   mirroring Linux `check_preempt_tick`), so a lone task's tick does its
//!   accounting without a needless switch-to-self. This is the
//!   charter-blessed non-tickless exception and no other policy may take it.
//! * **Weight by priority.** The three [`Priority`] bands map to a 4:2:1
//!   weight ratio (the CFS "nice level" analog): a `High` task is
//!   dispatched roughly four times as often as a `Low` one.
//! * **Per-CPU run queues, work-stealing across cores.** Each CPU owns one
//!   `vruntime`-ordered [`Scheduler`]-internal run queue with its own
//!   monotonic `min_vruntime` floor; idle CPUs steal the smallest-vruntime
//!   task from a victim chosen by the project's shared non-cryptographic
//!   `FastRng` (`lib/rng`; no second PRNG) and rebase its `vruntime` onto
//!   the stealing CPU's front (a task carries no lag across CPUs).
//! * **IPI-based preemption.** [`Scheduler::spawn`] and
//!   [`Scheduler::unpark`] notify the home CPU via
//!   [`SchedulerArch::send_ipi`]; the arch port decides whether that is an
//!   immediate context switch or a deferred reschedule.
//! * **No global mutable state outside per-CPU areas.** The only shared
//!   structures are an `Arc<A: SchedulerArch>`, an `RwLock`-protected task
//!   registry, and a `SpinLock`-protected overflow list for when a home
//!   CPU's queue is full.
//!
//! Full algorithmic detail, invariants, and the policy comparison live in
//! `docs/src/architecture/scheduler.md`.

#![no_std]

extern crate alloc;

// Host tests need `std` for `Arc` and `AtomicU*` ergonomics; the crate
// itself remains `no_std` for production builds.
#[cfg(test)]
extern crate std;

mod runqueue;
mod scheduler;
mod task;

// Re-export the contract vocabulary so call sites
// (`rustos_kernel_sched_cfq::{Scheduler, Priority, …}`) resolve to the
// single canonical definitions in `kernel/sched/api` (no duplication).
pub use rustos_kernel_sched_api::{
    CoreClass, CpuId, Priority, SchedError, SchedResult, SchedulerArch, SchedulerConfig,
    SchedulerPolicy, StepOutcome, TaskAction, TaskContext, TaskId, TaskState,
};

#[cfg(any(test, feature = "test-arch"))]
pub use rustos_kernel_sched_api::TestArch;

pub use scheduler::Scheduler;
