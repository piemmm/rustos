//! The [`SchedulerPolicy`] contract.
//!
//! Every concrete scheduler lives in its own `kernel/sched/<impl>` crate
//! and implements this trait; no crate outside `kernel/sched/*` and
//! `kernel/core` may name a concrete scheduler type. `kernel/core`
//! selects exactly one implementation at build time and the rest of the
//! kernel depends on this trait (or a generic `Scheduler<P: SchedulerPolicy>`),
//! never on a concrete policy.
//!
//! The trait deliberately mirrors the operational surface enumerates:
//! task admission ([`spawn`](SchedulerPolicy::spawn)), picking the next
//! runnable task on a CPU ([`step`](SchedulerPolicy::step)), cooperative
//! yield ([`yield_current`](SchedulerPolicy::yield_current)), block/wake
//! ([`park`](SchedulerPolicy::park) / [`unpark`](SchedulerPolicy::unpark) /
//! [`exit`](SchedulerPolicy::exit)), priority/quantum accounting (driven by
//! [`on_timer_tick`](SchedulerPolicy::on_timer_tick) and observable through
//! [`preemption_count`](SchedulerPolicy::preemption_count)), and the SMP
//! hooks (per-CPU run queues, work stealing, IPI-driven preemption) which
//! the implementation routes through the [`SchedulerArch`] surface.

use alloc::sync::Arc;

use crate::arch::{CpuId, SchedulerArch};
use crate::config::SchedulerConfig;
use crate::error::SchedResult;
use crate::outcome::StepOutcome;
use crate::task::{Priority, TaskAction, TaskContext, TaskId, TaskState};

/// The architecture-neutral contract every RustOS scheduler implements.
///
/// `A` is the [`SchedulerArch`] surface the policy drives for current-CPU,
/// tick, and IPI access; it is a type parameter so a host test can plug in
/// [`crate::TestArch`] while a production image plugs in its arch port.
///
/// Implementations are required to honour the lifecycle state machine in
/// [`crate::task`] and the cancellation-safety guarantees documented on
/// [`park`](Self::park) / [`unpark`](Self::unpark) / [`exit`](Self::exit).
/// The shared `conformance` suite asserts the behaviour every
/// policy must exhibit (fairness, no starvation, correct yield/wake
/// semantics, SMP stress on ≥ 4 cores).
pub trait SchedulerPolicy<A: SchedulerArch>: Sized {
    /// Construct a scheduler from a config and an architecture surface.
    ///
    /// # Errors
    /// * [`crate::SchedError::NoSuchCpu`] if `config.cpus == 0`.
    /// * [`crate::SchedError::QueueFull`] if the queue capacity is invalid
    ///   (not a power of two, or below 2).
    fn new(config: SchedulerConfig, arch: Arc<A>) -> SchedResult<Self>;

    /// Returns the configured CPU count.
    fn cpu_count(&self) -> u32;

    /// Returns the configuration snapshot.
    fn config(&self) -> SchedulerConfig;

    /// Admit a new task at `priority` with a body closure, enqueued on
    /// `home_cpu`.
    ///
    /// Implementations must never panic on a full home queue; they
    /// back-pressure or relocate the task instead.
    ///
    /// # Errors
    /// * [`crate::SchedError::NoSuchCpu`] if `home_cpu` is out of range.
    fn spawn<F>(&self, home_cpu: CpuId, priority: Priority, body: F) -> SchedResult<TaskId>
    where
        F: FnMut(&mut TaskContext) -> TaskAction + Send + 'static;

    /// Block a task. Cancellation-safe.
    ///
    /// # Errors
    /// * [`crate::SchedError::NoSuchTask`] if no task ever held that id.
    /// * [`crate::SchedError::InvalidState`] if the task is terminal.
    fn park(&self, id: TaskId) -> SchedResult<()>;

    /// Wake a parked task. Cancellation-safe.
    ///
    /// # Errors
    /// * [`crate::SchedError::NoSuchTask`] if no task ever held that id.
    /// * [`crate::SchedError::InvalidState`] if the task is terminal.
    fn unpark(&self, id: TaskId) -> SchedResult<()>;

    /// Terminate a task. Cancellation-safe and idempotent.
    ///
    /// # Errors
    /// * [`crate::SchedError::NoSuchTask`] if no task ever held that id.
    fn exit(&self, id: TaskId) -> SchedResult<()>;

    /// Observation point the arch port's timer ISR calls after
    /// acknowledging the device-level interrupt source. Drives quantum
    /// accounting / preemption.
    ///
    /// # Errors
    /// * [`crate::SchedError::NoSuchCpu`] if `cpu` is out of range.
    fn on_timer_tick(&self, cpu: CpuId) -> SchedResult<()>;

    /// Number of timer-driven preemptions observed on `cpu`.
    ///
    /// # Errors
    /// * [`crate::SchedError::NoSuchCpu`] if `cpu` is out of range.
    fn preemption_count(&self, cpu: CpuId) -> SchedResult<u64>;

    /// Sum of [`preemption_count`](Self::preemption_count) across all CPUs.
    fn total_preemption_count(&self) -> u64;

    /// Pick and run the next runnable task on `cpu` exactly once, falling
    /// back to work-stealing before reporting [`StepOutcome::Idle`].
    ///
    /// # Errors
    /// * [`crate::SchedError::NoSuchCpu`] if `cpu` is out of range.
    fn step(&self, cpu: CpuId) -> SchedResult<StepOutcome>;

    /// Total number of times the given task's body has been invoked.
    ///
    /// # Errors
    /// * [`crate::SchedError::NoSuchTask`] if the id is unknown.
    fn run_count(&self, id: TaskId) -> SchedResult<u64>;

    /// Most recent state of `id` ([`TaskState::Exited`] once drained).
    fn state_of(&self, id: TaskId) -> TaskState;

    /// Number of live (non-[`TaskState::Exited`]) tasks.
    fn live_task_count(&self) -> usize;

    /// [`TaskId`] of the task currently dispatching on `cpu`, or `None`.
    ///
    /// This is the publication point the syscall entry path reads to
    /// recover the caller's identity (step 1).
    fn current_task(&self, cpu: CpuId) -> Option<TaskId>;

    /// The [`CpuId`] on which `id` is currently executing, or `None` when it
    /// is not presently scheduled on any CPU (runnable-but-waiting, blocked,
    /// or exited).
    ///
    /// A read-only observation for the System Information introspection feed.
    /// It never tracks a task's *last* CPU — only its current one — so the
    /// answer is always truthful: a task that is not running reports `None`,
    /// never a stale CPU. The default scans the per-CPU current-task slots via
    /// [`current_task`](Self::current_task), so every policy shares this one
    /// definition and none carries its own copy; a policy only overrides it
    /// if it can answer more cheaply. The `conformance` suite pins the
    /// contract (`Some(cpu)` iff the task is the current task on `cpu`).
    fn running_cpu(&self, id: TaskId) -> Option<CpuId> {
        (0..self.cpu_count()).find(|&cpu| self.current_task(cpu) == Some(id))
    }

    /// Cooperatively yield the currently dispatching task on its CPU.
    ///
    /// Models a voluntary syscall yield (no MLFQ demotion); the task is
    /// re-enqueued at its current priority and the current-task slot is
    /// cleared.
    ///
    /// # Errors
    /// * [`crate::SchedError::NoSuchTask`] if the id is unknown.
    /// * [`crate::SchedError::InvalidState`] if the task is not running.
    fn yield_current(&self, id: TaskId) -> SchedResult<()>;
}
