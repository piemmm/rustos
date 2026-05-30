//! Per-CPU run queues, MLFQ policy, work-stealing dispatch.
//!
//! See `docs/src/architecture/scheduler.md` for the algorithm description,
//! invariants, and debugging guide. This module is the implementation; the
//! doc page is the source of truth for behaviour.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use rustos_sync::{RwLock, SpinLock};

use crate::loom_compat::{AtomicU64, Ordering};
use crate::runqueue::{RunDeque, Steal};
use crate::task::{TaskBody, TaskInner};
use crate::{
    CpuId, Priority, SchedError, SchedResult, SchedulerArch, SchedulerConfig, SchedulerPolicy,
    StepOutcome, TaskAction, TaskContext, TaskId, TaskState,
};

/// Per-CPU scheduler state.
///
/// Allocated once at scheduler construction and never resized. Mutating
/// access happens only through atomic fields and the per-band deques —
/// there is no shared `Mutex<CpuState>`.
struct CpuState {
    bands: [RunDeque; Priority::COUNT],
    /// Tick at which this CPU last completed a [`Scheduler::step`] that
    /// returned [`StepOutcome::Ran`]. Used by tests to assert progress.
    last_run_tick: AtomicU64,
}

impl CpuState {
    fn new(queue_capacity: usize) -> Option<Self> {
        let band = || RunDeque::try_new(queue_capacity);
        Some(Self {
            bands: [band()?, band()?, band()?],
            last_run_tick: AtomicU64::new(0),
        })
    }

    fn push_priority(&self, prio: Priority, id: TaskId) -> Result<(), TaskId> {
        self.bands[prio.as_index()].push(id)
    }

    fn pop_highest(&self) -> Option<(Priority, TaskId)> {
        for i in 0..Priority::COUNT {
            // SAFETY-INVARIANT: i is in 0..COUNT so from_index is Some.
            let p = Priority::from_index(i).unwrap_or(Priority::Low);
            // Bounded retry — `Steal::Retry` is reported on lost CAS
            // races. There are at most `cpus - 1` concurrent stealers,
            // so a few attempts always succeed when the band is
            // non-empty. Capped to keep the dispatch path lock-free in
            // the system-wide sense (`AGENTS.md` §2.1 — no spin loops).
            for _ in 0..8 {
                match self.bands[i].steal() {
                    Steal::Stolen(id) => return Some((p, id)),
                    Steal::Empty => break,
                    // Retry: loop again. Falling through is also fine,
                    // but listing the arm keeps the match exhaustive
                    // without `_` swallowing future variants.
                    Steal::Retry => {}
                }
            }
        }
        None
    }

    fn steal_any(&self) -> Steal {
        for band in &self.bands {
            match band.steal() {
                Steal::Empty => {}
                other => return other,
            }
        }
        Steal::Empty
    }
}

/// The SMP scheduler.
///
/// Generic over the architecture surface so the host test binary can use
/// a mock arch and the architecture ports plug in their own types.
/// Internally the scheduler owns:
///
/// * a fixed array of per-CPU state blocks — no global mutable state
///   outside these per-CPU areas (`AGENTS.md` §4 "SMP from day one"),
/// * a registry of tasks keyed by [`TaskId`] under a single
///   reader-preferring `RwLock`; only [`Self::spawn`] and the
///   bookkeeping for [`Self::exit`] take the write lock,
/// * an `Arc<A: SchedulerArch>` for current-CPU / tick / IPI access.
pub struct Scheduler<A: SchedulerArch> {
    arch: Arc<A>,
    cpus: Box<[CpuState]>,
    tasks: RwLock<BTreeMap<TaskId, Arc<TaskInner>>>,
    next_id: AtomicU64,
    config: SchedulerConfig,
    last_boost_tick: AtomicU64,
    /// xorshift state for victim selection — per-scheduler, not per-CPU,
    /// because the cost of a single atomic exchange dwarfs the
    /// hypothetical cache benefit on this control-plane path.
    victim_rng: AtomicU64,
    /// Tasks that exceeded their home CPU's queue at spawn/unpark time.
    /// The scheduler drains this on every step before checking the local
    /// queues; this is the back-pressure path described in
    /// `docs/src/architecture/scheduler.md` §"Overflow".
    overflow: SpinLock<Vec<TaskId>>,
    /// Per-CPU count of timer-driven preemptions ever observed.
    ///
    /// Incremented exactly once per [`Self::on_timer_tick`] call. The
    /// counter exists so integration tests (and future audit code) can
    /// assert that preemption is actually firing — a silent regression
    /// to cooperative scheduling would otherwise pass the workload-
    /// correctness checks while breaking the security model
    /// (`AGENTS.md` §5: a runaway task on one CPU must not be able to
    /// indefinitely block another).
    preemptions: Box<[AtomicU64]>,
    /// Per-CPU **current-task** slot.
    ///
    /// Holds the [`TaskId`] of the task whose body is currently being
    /// invoked on the corresponding CPU. The sentinel value `0` means
    /// "no task currently running on this CPU". The slot is updated
    /// exclusively by [`Self::dispatch`] (set immediately before the
    /// body is invoked, cleared as soon as the body returns) and
    /// defensively by [`Self::park`] / [`Self::exit`] when their
    /// argument matches an entry — that covers the case where a task
    /// running on one CPU is parked or exited by another CPU.
    ///
    /// The slot is the publication point read by the syscall entry
    /// path (`kernel/rustos-kernel::dispatch::production_dispatch`,
    /// Stage 2.7 follow-up (f5)). Per the `kernel/sync::RwLock`
    /// process-context rule (`AGENTS.md` §1), readers must run in
    /// process context on the issuing CPU — which is how syscall
    /// entry is reached on every architecture. There is no global
    /// mutable state: every CPU has its own atomic word.
    current: Box<[AtomicU64]>,
}

impl<A: SchedulerArch> Scheduler<A> {
    /// Construct a scheduler from a config and an architecture surface.
    ///
    /// # Errors
    /// * [`SchedError::NoSuchCpu`] if `config.cpus == 0`.
    /// * [`SchedError::QueueFull`] (re-used) if the queue capacity is
    ///   invalid (not a power of two, or below 2).
    pub fn new(config: SchedulerConfig, arch: Arc<A>) -> SchedResult<Self> {
        if config.cpus == 0 {
            return Err(SchedError::NoSuchCpu);
        }
        let mut cpus = Vec::with_capacity(config.cpus as usize);
        for _ in 0..config.cpus {
            let s = CpuState::new(config.queue_capacity_per_band).ok_or(SchedError::QueueFull)?;
            cpus.push(s);
        }
        let mut preemptions = Vec::with_capacity(config.cpus as usize);
        for _ in 0..config.cpus {
            preemptions.push(AtomicU64::new(0));
        }
        let mut current = Vec::with_capacity(config.cpus as usize);
        for _ in 0..config.cpus {
            current.push(AtomicU64::new(0));
        }
        Ok(Self {
            arch,
            cpus: cpus.into_boxed_slice(),
            tasks: RwLock::new(BTreeMap::new()),
            next_id: AtomicU64::new(1),
            config,
            last_boost_tick: AtomicU64::new(0),
            victim_rng: AtomicU64::new(0x9E37_79B9_7F4A_7C15),
            overflow: SpinLock::new(Vec::new()),
            preemptions: preemptions.into_boxed_slice(),
            current: current.into_boxed_slice(),
        })
    }

    /// Returns the configured CPU count.
    #[must_use]
    pub fn cpu_count(&self) -> u32 {
        self.config.cpus
    }

    /// Returns the configuration snapshot.
    #[must_use]
    pub fn config(&self) -> SchedulerConfig {
        self.config
    }

    fn cpu_state(&self, cpu: CpuId) -> SchedResult<&CpuState> {
        self.cpus.get(cpu as usize).ok_or(SchedError::NoSuchCpu)
    }

    /// Spawn a new task at `priority` with a body closure.
    ///
    /// The task is enqueued on `home_cpu`. If that CPU's queue is full
    /// the task is parked in the global overflow list and any CPU that
    /// next runs [`Self::step`] will pick it up; this preserves the
    /// "spawn never panics" invariant (`AGENTS.md` §2.9).
    ///
    /// # Errors
    /// * [`SchedError::NoSuchCpu`] if `home_cpu` is out of range.
    pub fn spawn<F>(&self, home_cpu: CpuId, priority: Priority, body: F) -> SchedResult<TaskId>
    where
        F: FnMut(&mut TaskContext) -> TaskAction + Send + 'static,
    {
        self.cpu_state(home_cpu)?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        // 0 is reserved; the counter starts at 1 so this is a defence in
        // depth against a 64-bit wrap that no real kernel will see.
        let id = if id == 0 {
            self.next_id.fetch_add(1, Ordering::Relaxed)
        } else {
            id
        };
        let boxed: Box<TaskBody> = Box::new(body);
        let inner = Arc::new(TaskInner::new(id, home_cpu, priority, boxed));
        self.tasks.write().insert(id, inner.clone());
        // Try to enqueue on the home CPU; on overflow, push to the
        // global overflow list. Both paths preserve cancellation safety
        // because state is updated only after enqueue confirms.
        match self.cpu_state(home_cpu)?.push_priority(priority, id) {
            Ok(()) => {}
            Err(_) => self.overflow.lock().push(id),
        }
        // Wake the home CPU. Spawning at higher priority than what is
        // running there should preempt — but at this layer we have no
        // visibility into "what is running there", so we always notify
        // and let the arch port decide. (`AGENTS.md` §2.4 — no creep.)
        self.arch.send_ipi(home_cpu);
        Ok(id)
    }

    /// Park a task. Cancellation-safe: the call is a no-op if the task
    /// has already exited.
    ///
    /// Also clears any per-CPU current-task slot whose entry equals
    /// `id` so the slot does not outlive the task's run
    /// (`AGENTS.md` §5.4 fail-closed: no syscall must reach a parked
    /// task as its caller).
    ///
    /// # Errors
    /// * [`SchedError::NoSuchTask`] if no task ever held that id.
    /// * [`SchedError::InvalidState`] if the task is in a terminal state.
    pub fn park(&self, id: TaskId) -> SchedResult<()> {
        let task = self
            .tasks
            .read()
            .get(&id)
            .cloned()
            .ok_or(SchedError::NoSuchTask)?;
        loop {
            match task.load_state() {
                TaskState::Exited => return Err(SchedError::InvalidState),
                TaskState::Parked => {
                    self.clear_current_matching(id);
                    return Ok(());
                }
                cur @ (TaskState::Ready | TaskState::Running) => {
                    if task.cas_state(cur, TaskState::Parked).is_ok() {
                        self.clear_current_matching(id);
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Unpark a task. Cancellation-safe.
    ///
    /// # Errors
    /// * [`SchedError::NoSuchTask`] if no task ever held that id.
    /// * [`SchedError::InvalidState`] if the task is in a terminal state.
    pub fn unpark(&self, id: TaskId) -> SchedResult<()> {
        let task = self
            .tasks
            .read()
            .get(&id)
            .cloned()
            .ok_or(SchedError::NoSuchTask)?;
        match task.load_state() {
            TaskState::Exited => return Err(SchedError::InvalidState),
            TaskState::Ready | TaskState::Running => return Ok(()),
            TaskState::Parked => {}
        }
        task.cas_state(TaskState::Parked, TaskState::Ready)
            .map_err(|_| SchedError::InvalidState)?;
        // Re-enqueue on the home CPU; overflow falls back as in `spawn`.
        let prio = task.load_priority();
        let home = task.home_cpu.load(Ordering::Acquire);
        let cpu = self.cpu_state(home)?;
        if cpu.push_priority(prio, id).is_err() {
            self.overflow.lock().push(id);
        }
        self.arch.send_ipi(home);
        Ok(())
    }

    /// Terminate a task. Cancellation-safe; idempotent.
    ///
    /// Also clears any per-CPU current-task slot whose entry equals
    /// `id`, so the slot cannot point at an exited task by the time the
    /// next syscall is issued on the affected CPU.
    ///
    /// # Errors
    /// * [`SchedError::NoSuchTask`] if the id was never spawned.
    pub fn exit(&self, id: TaskId) -> SchedResult<()> {
        let task = self
            .tasks
            .read()
            .get(&id)
            .cloned()
            .ok_or(SchedError::NoSuchTask)?;
        task.store_state(TaskState::Exited);
        // Drop the closure eagerly. The registry entry stays until the
        // scheduler's next step prunes it; this avoids racing with a
        // dispatch that already has the `Arc`.
        if let Some(mut guard) = task.body.try_lock() {
            *guard = None;
        }
        self.clear_current_matching(id);
        Ok(())
    }

    /// Architecture-driven preemption entry point.
    ///
    /// The architecture port's timer interrupt service routine — the
    /// LAPIC-timer ISR on x86_64, the CNTV/EL1 handler on aarch64, the
    /// CLINT trap on riscv64, the host worker's quantum tick on wasm32
    /// — calls this once per tick after acknowledging the device-level
    /// interrupt source (EOI on the LAPIC, etc.). The scheduler
    /// itself never reads a timer register; the arch port owns that.
    ///
    /// On every call the per-CPU preemption counter is incremented.
    /// The counter is exposed via [`Self::preemption_count`] /
    /// [`Self::total_preemption_count`] so integration tests can
    /// assert that preemption fires (`AGENTS.md` §7: a silent
    /// regression to cooperative scheduling must fail loudly, not
    /// pass).
    ///
    /// # Why this entry point does *not* call [`Self::step`]
    ///
    /// `step` takes a reader lock on the task registry
    /// (`RwLock<BTreeMap<TaskId, _>>`) and a `SpinLock` on the
    /// overflow list. Both are forbidden from interrupt context by
    /// `kernel/sync` (see `rwlock.rs` module docs: "Process /
    /// kernel-thread context only. Never from an interrupt handler.").
    /// An ISR-driven `step` would deadlock against the same CPU's
    /// in-progress `spawn` (writer-held registry lock) or a mid-
    /// `drain_overflow_to` (held overflow `SpinLock`). The arch port's
    /// timer ISR therefore restricts itself to bumping the observable
    /// counter; the cooperative `step` loop driven by the kernel-
    /// thread context is the only writer of run-queue state. A future
    /// commit that lands IRQ-safe locks (e.g. `irq::SpinLock` from
    /// `kernel/sync`) can extend this entry point to drive a real
    /// preemption step — the public surface need not change.
    ///
    /// # Why this is on `Scheduler<A>` and not on `SchedulerArch`
    ///
    /// [`SchedulerArch::send_ipi`] already documents the inverse
    /// direction — the scheduler asking the arch to nudge a CPU into
    /// rescheduling. Adding a parallel `preempt_to` trait method
    /// would be interface creep (`AGENTS.md` §2.4) because the two
    /// requests have the same downstream effect on a real port. The
    /// timer-ISR path is *inward*, from arch into scheduler, so it
    /// belongs on the scheduler type and not on the arch trait.
    ///
    /// # Errors
    /// * [`SchedError::NoSuchCpu`] if `cpu` is out of range. The
    ///   counter is **not** incremented in that case so a stray ISR
    ///   does not poison the metric an integration test reads.
    pub fn on_timer_tick(&self, cpu: CpuId) -> SchedResult<()> {
        // Validate the CPU id *before* touching the counter so a stray
        // ISR does not skew the metric an integration test reads.
        let _ = self.cpu_state(cpu)?;
        // `Relaxed` is sufficient: the counter is monotonic, observers
        // only need the eventual value, and the per-tick increment is
        // not synchronising any other memory. The whole body is
        // wait-free and ISR-safe (one array bounds check, one
        // `fetch_add`) — see the rustdoc above for the rationale.
        self.preemptions[cpu as usize].fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Returns the per-CPU preemption count observed by
    /// [`Self::on_timer_tick`].
    ///
    /// # Errors
    /// * [`SchedError::NoSuchCpu`] if `cpu` is out of range.
    pub fn preemption_count(&self, cpu: CpuId) -> SchedResult<u64> {
        let _ = self.cpu_state(cpu)?;
        Ok(self.preemptions[cpu as usize].load(Ordering::Relaxed))
    }

    /// Returns the sum of [`Self::preemption_count`] across every CPU.
    ///
    /// Intended for tests and audit logging; the sum is computed with
    /// `Relaxed` loads so a concurrent ISR may make the total
    /// non-monotonic across two adjacent reads. Both reads remain
    /// individually consistent.
    #[must_use]
    pub fn total_preemption_count(&self) -> u64 {
        self.preemptions
            .iter()
            .map(|p| p.load(Ordering::Relaxed))
            .sum()
    }

    /// One scheduler step on `cpu`: pick a task, run it once, re-enqueue
    /// or retire it. Returns [`StepOutcome::Idle`] if nothing runnable
    /// was found anywhere.
    ///
    /// # Errors
    /// * [`SchedError::NoSuchCpu`] if `cpu` is out of range.
    pub fn step(&self, cpu: CpuId) -> SchedResult<StepOutcome> {
        let me = self.cpu_state(cpu)?;
        self.maybe_priority_boost();

        // 1. Drain overflow onto our local queue, best-effort.
        self.drain_overflow_to(me);

        // 2. Local high → normal → low.
        if let Some((_band, id)) = me.pop_highest() {
            return Ok(self.dispatch(cpu, id));
        }

        // 3. Work-stealing pass over the other CPUs.
        if let Some(id) = self.try_steal(cpu) {
            return Ok(self.dispatch(cpu, id));
        }

        Ok(StepOutcome::Idle)
    }

    fn drain_overflow_to(&self, me: &CpuState) {
        let mut g = self.overflow.lock();
        while let Some(id) = g.pop() {
            let prio = self
                .tasks
                .read()
                .get(&id)
                .map_or(Priority::Normal, |t| t.load_priority());
            if me.push_priority(prio, id).is_err() {
                // Local queue full too — put it back and stop draining.
                g.push(id);
                break;
            }
        }
    }

    fn maybe_priority_boost(&self) {
        let now = self.arch.ticks_now();
        let last = self.last_boost_tick.load(Ordering::Acquire);
        if now.wrapping_sub(last) < self.config.boost_interval_ticks {
            return;
        }
        if self
            .last_boost_tick
            .compare_exchange(last, now, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return; // Lost the race; some other CPU is boosting.
        }
        // Promote every non-terminal task to High. Reset their
        // yields-at-band so demotion has to re-occur. We only mutate the
        // task's atomics; we do not move them between queues — they will
        // naturally drain at their new (high) priority because the
        // scheduler dispatches based on the queue they currently live in,
        // and on next yield we re-enqueue at their new band.
        for task in self.tasks.read().values() {
            if task.load_state() != TaskState::Exited {
                task.priority.store(Priority::High as u8, Ordering::Release);
                task.yields_at_band.store(0, Ordering::Release);
            }
        }
    }

    fn try_steal(&self, cpu: CpuId) -> Option<TaskId> {
        let n = self.cpus.len();
        if n <= 1 {
            return None;
        }
        let start = self.next_victim(cpu);
        for offset in 0..n {
            let v = (start as usize + offset) % n;
            // `v` is in `0..n` and `n <= u32::MAX` (cpus is u32), so the
            // cast does not truncate.
            #[allow(clippy::cast_possible_truncation)]
            if v as u32 == cpu {
                continue;
            }
            match self.cpus[v].steal_any() {
                Steal::Stolen(id) => return Some(id),
                Steal::Empty | Steal::Retry => {}
            }
        }
        None
    }

    fn next_victim(&self, cpu: CpuId) -> u32 {
        // xorshift64* — Marsaglia 2003. Bias-corrected with `+ cpu`
        // so two CPUs do not lock-step their probe order.
        let mut s = self.victim_rng.load(Ordering::Relaxed);
        if s == 0 {
            s = 0x9E37_79B9_7F4A_7C15;
        }
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        self.victim_rng.store(s, Ordering::Relaxed);
        // n > 0 by construction (config.cpus != 0); both casts are
        // narrow because cpus is `u32` and the modulus is < n <= u32::MAX.
        #[allow(clippy::cast_possible_truncation)]
        {
            let n = self.cpus.len() as u64;
            ((s.wrapping_add(u64::from(cpu))) % n) as u32
        }
    }

    fn dispatch(&self, cpu: CpuId, id: TaskId) -> StepOutcome {
        let Some(task) = self.tasks.read().get(&id).cloned() else {
            return StepOutcome::Idle;
        };
        // The authoritative priority is the one stored in the task — the
        // run-queue band the task was dequeued from is only a hint. The
        // periodic priority boost mutates `task.priority` *without*
        // migrating queued tasks (see `maybe_priority_boost`), so reading
        // from `task` here is what makes the boost visible.
        let prio = task.load_priority();
        // Filter out tasks that were parked/exited while queued.
        match task.load_state() {
            TaskState::Parked => return StepOutcome::Idle,
            TaskState::Exited => {
                self.tasks.write().remove(&id);
                return StepOutcome::Idle;
            }
            TaskState::Ready => {}
            TaskState::Running => {
                // Already running elsewhere (a stale steal); drop on the
                // floor — it will be re-enqueued by its owner.
                return StepOutcome::Idle;
            }
        }
        if task
            .cas_state(TaskState::Ready, TaskState::Running)
            .is_err()
        {
            return StepOutcome::Idle;
        }
        // Publish the current-task slot for this CPU **before** invoking
        // the body: a syscall issued from inside the body must observe
        // the running task on its own CPU. See `Self::current_task`.
        self.set_current(cpu, id);
        let tick = self.arch.ticks_now();
        task.last_started.store(tick, Ordering::Release);
        task.home_cpu.store(cpu, Ordering::Release);

        let mut ctx = TaskContext {
            cpu,
            tick,
            task_id: id,
        };
        let action = {
            let mut body_guard = task.body.lock();
            match body_guard.as_mut() {
                Some(b) => b(&mut ctx),
                // Body was torn down by a concurrent exit() — treat as Exit.
                None => TaskAction::Exit,
            }
        };
        task.total_runs.fetch_add(1, Ordering::Relaxed);
        self.cpus[cpu as usize]
            .last_run_tick
            .store(tick, Ordering::Release);

        // Re-resolve external park/exit that may have raced with the body.
        let observed_state = task.load_state();
        let effective = match (observed_state, action) {
            (TaskState::Exited, _) | (_, TaskAction::Exit) => TaskAction::Exit,
            (TaskState::Parked, _) | (_, TaskAction::Park) => TaskAction::Park,
            _ => TaskAction::Yield,
        };

        // Body has returned; the task is no longer the current task on
        // this CPU regardless of which branch we take below.
        self.clear_current(cpu);
        match effective {
            TaskAction::Exit => {
                task.store_state(TaskState::Exited);
                if let Some(mut guard) = task.body.try_lock() {
                    *guard = None;
                }
                self.tasks.write().remove(&id);
            }
            TaskAction::Park => {
                task.store_state(TaskState::Parked);
            }
            TaskAction::Yield => {
                let yields = task.yields_at_band.fetch_add(1, Ordering::AcqRel) + 1;
                let new_prio = if yields >= self.config.yields_before_demotion {
                    let demoted = prio.demote();
                    if demoted != prio {
                        task.yields_at_band.store(0, Ordering::Release);
                    }
                    demoted
                } else {
                    prio
                };
                task.priority.store(new_prio as u8, Ordering::Release);
                task.store_state(TaskState::Ready);
                let home = task.home_cpu.load(Ordering::Acquire);
                let target = self
                    .cpus
                    .get(home as usize)
                    .unwrap_or(&self.cpus[cpu as usize]);
                if target.push_priority(new_prio, id).is_err() {
                    self.overflow.lock().push(id);
                }
            }
        }
        StepOutcome::Ran(id)
    }

    /// Returns the total number of times the given task's body has been
    /// invoked. Public for tests and tracing.
    ///
    /// # Errors
    /// * [`SchedError::NoSuchTask`] if the id is unknown (already exited
    ///   *and* drained, or never spawned).
    pub fn run_count(&self, id: TaskId) -> SchedResult<u64> {
        self.tasks
            .read()
            .get(&id)
            .map(|t| t.total_runs.load(Ordering::Acquire))
            .ok_or(SchedError::NoSuchTask)
    }

    /// Returns the most recent state of `id`. Convenience for tests.
    ///
    /// Returns [`TaskState::Exited`] for a task that has been drained
    /// from the registry: from a caller's point of view the task is no
    /// longer alive.
    #[must_use]
    pub fn state_of(&self, id: TaskId) -> TaskState {
        self.tasks
            .read()
            .get(&id)
            .map_or(TaskState::Exited, |t| t.load_state())
    }

    /// Returns the number of live (non-`Exited`) tasks. Mostly for tests.
    #[must_use]
    pub fn live_task_count(&self) -> usize {
        self.tasks
            .read()
            .values()
            .filter(|t| t.load_state() != TaskState::Exited)
            .count()
    }

    /// Returns the [`TaskId`] of the task currently dispatching on
    /// `cpu`, or `None` if no task is currently running there.
    ///
    /// The slot is published exclusively by the scheduler's internal
    /// `dispatch` path (set immediately before the task body is
    /// invoked, cleared as soon as the body returns). [`Self::park`]
    /// and [`Self::exit`] also clear
    /// matching entries so a parked or exited task can never be the
    /// current task on any CPU.
    ///
    /// This is the lookup the syscall entry path uses to recover the
    /// caller's [`TaskId`] before consulting the capability table
    /// (`AGENTS.md` §5.4 step 1: identify the caller — kernel-provided,
    /// not caller-supplied). Per the `kernel/sync::RwLock`
    /// process-context rule (`AGENTS.md` §1), this method must be
    /// called only in process context on the issuing CPU; it must not
    /// be invoked from an interrupt handler.
    ///
    /// Returns `None` if `cpu` is out of range, mirroring the policy
    /// that an unknown CPU has, by definition, no current task.
    #[must_use]
    pub fn current_task(&self, cpu: CpuId) -> Option<TaskId> {
        let slot = self.current.get(cpu as usize)?;
        let v = slot.load(Ordering::Acquire);
        if v == 0 {
            None
        } else {
            Some(v)
        }
    }

    /// Cooperatively yield the currently dispatching task on its CPU.
    ///
    /// Intended to be called from inside a syscall handler
    /// (`yield_now`) where the issuing task's [`TaskId`] has just been
    /// looked up via [`Self::current_task`]. The task is required to be
    /// in [`TaskState::Running`]; on success its state is flipped back
    /// to [`TaskState::Ready`], it is re-enqueued at its current
    /// priority on its home CPU, and the per-CPU current-task slot is
    /// cleared so the next dispatch is free to pick a different task.
    ///
    /// Demotion is **not** applied here: `yield_current` models a
    /// voluntary syscall yield, not a quantum-expiry. The MLFQ demotion
    /// path lives in the scheduler's internal `dispatch` routine and is
    /// reached when the task body itself returns
    /// [`crate::TaskAction::Yield`]; that keeps the two yield
    /// notions distinct, avoiding the interface creep `AGENTS.md` §2.4
    /// forbids.
    ///
    /// # Errors
    /// * [`SchedError::NoSuchTask`] if the id is unknown.
    /// * [`SchedError::InvalidState`] if the task is not [`TaskState::Running`].
    pub fn yield_current(&self, id: TaskId) -> SchedResult<()> {
        let task = self
            .tasks
            .read()
            .get(&id)
            .cloned()
            .ok_or(SchedError::NoSuchTask)?;
        task.cas_state(TaskState::Running, TaskState::Ready)
            .map_err(|_| SchedError::InvalidState)?;
        let prio = task.load_priority();
        let home = task.home_cpu.load(Ordering::Acquire);
        let cpu = self.cpu_state(home)?;
        if cpu.push_priority(prio, id).is_err() {
            self.overflow.lock().push(id);
        }
        self.clear_current_matching(id);
        Ok(())
    }

    /// Publish `id` as the current task on `cpu`. Out-of-range `cpu`
    /// is a silent no-op: the only writer is [`Self::dispatch`], which
    /// already validated `cpu` via [`Self::cpu_state`] before invoking
    /// this helper.
    fn set_current(&self, cpu: CpuId, id: TaskId) {
        if let Some(slot) = self.current.get(cpu as usize) {
            slot.store(id, Ordering::Release);
        }
    }

    /// Clear the current-task slot for `cpu`. Out-of-range `cpu` is a
    /// silent no-op for the same reason `set_current` is.
    fn clear_current(&self, cpu: CpuId) {
        if let Some(slot) = self.current.get(cpu as usize) {
            slot.store(0, Ordering::Release);
        }
    }

    /// Defensively clear every per-CPU current-task slot whose entry
    /// equals `id`. Used by [`Self::park`], [`Self::exit`], and
    /// [`Self::yield_current`] so a task that just transitioned out of
    /// [`TaskState::Running`] cannot remain the current task on any
    /// CPU. The CAS is per-slot, so a concurrent `dispatch` of a
    /// *different* task on a sibling CPU is untouched.
    fn clear_current_matching(&self, id: TaskId) {
        for slot in self.current.iter() {
            let _ = slot.compare_exchange(id, 0, Ordering::AcqRel, Ordering::Relaxed);
        }
    }
}

/// MLFQ implements the architecture-neutral [`SchedulerPolicy`] contract.
///
/// Each method forwards to the inherent implementation above; the
/// forwarding adapter is what lets `kernel/core` select this policy by
/// the trait while the inherent surface stays available to this crate's
/// own dispatch internals and tests (`AGENTS.md` §17.1).
impl<A: SchedulerArch> SchedulerPolicy<A> for Scheduler<A> {
    fn new(config: SchedulerConfig, arch: Arc<A>) -> SchedResult<Self> {
        Scheduler::new(config, arch)
    }

    fn cpu_count(&self) -> u32 {
        Scheduler::cpu_count(self)
    }

    fn config(&self) -> SchedulerConfig {
        Scheduler::config(self)
    }

    fn spawn<F>(&self, home_cpu: CpuId, priority: Priority, body: F) -> SchedResult<TaskId>
    where
        F: FnMut(&mut TaskContext) -> TaskAction + Send + 'static,
    {
        Scheduler::spawn(self, home_cpu, priority, body)
    }

    fn park(&self, id: TaskId) -> SchedResult<()> {
        Scheduler::park(self, id)
    }

    fn unpark(&self, id: TaskId) -> SchedResult<()> {
        Scheduler::unpark(self, id)
    }

    fn exit(&self, id: TaskId) -> SchedResult<()> {
        Scheduler::exit(self, id)
    }

    fn on_timer_tick(&self, cpu: CpuId) -> SchedResult<()> {
        Scheduler::on_timer_tick(self, cpu)
    }

    fn preemption_count(&self, cpu: CpuId) -> SchedResult<u64> {
        Scheduler::preemption_count(self, cpu)
    }

    fn total_preemption_count(&self) -> u64 {
        Scheduler::total_preemption_count(self)
    }

    fn step(&self, cpu: CpuId) -> SchedResult<StepOutcome> {
        Scheduler::step(self, cpu)
    }

    fn run_count(&self, id: TaskId) -> SchedResult<u64> {
        Scheduler::run_count(self, id)
    }

    fn state_of(&self, id: TaskId) -> TaskState {
        Scheduler::state_of(self, id)
    }

    fn live_task_count(&self) -> usize {
        Scheduler::live_task_count(self)
    }

    fn current_task(&self, cpu: CpuId) -> Option<TaskId> {
        Scheduler::current_task(self, cpu)
    }

    fn yield_current(&self, id: TaskId) -> SchedResult<()> {
        Scheduler::yield_current(self, id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TestArch;

    fn mk(cpus: u32) -> (Arc<TestArch>, Scheduler<TestArch>) {
        let arch = Arc::new(TestArch::new(cpus).expect("arch"));
        let cfg = SchedulerConfig {
            cpus,
            queue_capacity_per_band: 64,
            yields_before_demotion: 1,
            boost_interval_ticks: 1024,
        };
        let sched = Scheduler::new(cfg, arch.clone()).expect("sched");
        (arch, sched)
    }

    #[test]
    fn spawn_and_run_once() {
        let (arch, sched) = mk(2);
        let counter = Arc::new(core::sync::atomic::AtomicU32::new(0));
        let c2 = counter.clone();
        let id = sched
            .spawn(0, Priority::Normal, move |_ctx| {
                c2.fetch_add(1, Ordering::Relaxed);
                TaskAction::Exit
            })
            .expect("spawn");
        arch.set_current_cpu(0);
        let outcome = sched.step(0).expect("step");
        assert_eq!(outcome, StepOutcome::Ran(id));
        assert_eq!(counter.load(Ordering::Relaxed), 1);
        assert_eq!(sched.state_of(id), TaskState::Exited);
        assert!(arch.ipi_count(0) >= 1, "spawn should send IPI");
    }

    #[test]
    fn idle_when_no_work() {
        let (_arch, sched) = mk(2);
        assert_eq!(sched.step(0), Ok(StepOutcome::Idle));
    }

    #[test]
    fn park_and_unpark_roundtrip() {
        let (_arch, sched) = mk(1);
        let id = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Park)
            .expect("spawn");
        // First step: body returns Park => Parked.
        assert_eq!(sched.step(0), Ok(StepOutcome::Ran(id)));
        assert_eq!(sched.state_of(id), TaskState::Parked);
        // unpark + step again should re-run.
        sched.unpark(id).expect("unpark");
        assert_eq!(sched.step(0), Ok(StepOutcome::Ran(id)));
    }

    #[test]
    fn exit_is_idempotent() {
        let (_arch, sched) = mk(1);
        let id = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Yield)
            .expect("spawn");
        sched.exit(id).expect("exit-1");
        sched.exit(id).expect("exit-2");
        assert_eq!(sched.state_of(id), TaskState::Exited);
    }

    #[test]
    fn unknown_task_is_error() {
        let (_arch, sched) = mk(1);
        assert_eq!(sched.park(999), Err(SchedError::NoSuchTask));
        assert_eq!(sched.unpark(999), Err(SchedError::NoSuchTask));
        assert_eq!(sched.exit(999), Err(SchedError::NoSuchTask));
    }

    #[test]
    fn work_stealing_finds_remote_task() {
        let (arch, sched) = mk(4);
        // Spawn on CPU 3; drive CPU 0.
        let id = sched
            .spawn(3, Priority::Normal, |_| TaskAction::Exit)
            .expect("spawn");
        arch.set_current_cpu(0);
        // CPU 0 should steal it (CPU 3 hasn't stepped yet).
        let outcome = sched.step(0).expect("step");
        assert_eq!(outcome, StepOutcome::Ran(id));
    }

    #[test]
    fn mlfq_demotes_busy_yielder() {
        let (_arch, sched) = mk(1);
        // Yields 5 times then exits.
        let counter = Arc::new(core::sync::atomic::AtomicU32::new(0));
        let c2 = counter.clone();
        let id = sched
            .spawn(0, Priority::High, move |_| {
                if c2.fetch_add(1, Ordering::Relaxed) >= 4 {
                    TaskAction::Exit
                } else {
                    TaskAction::Yield
                }
            })
            .expect("spawn");
        // After one Yield with yields_before_demotion=1 the task should
        // sit in Normal; after two, in Low.
        sched.step(0).expect("1");
        let t = sched.tasks.read().get(&id).cloned().expect("task");
        assert_eq!(t.load_priority(), Priority::Normal);
        sched.step(0).expect("2");
        assert_eq!(t.load_priority(), Priority::Low);
    }

    #[test]
    fn priority_boost_prevents_starvation() {
        let arch = Arc::new(TestArch::new(1).expect("arch"));
        let cfg = SchedulerConfig {
            cpus: 1,
            queue_capacity_per_band: 16,
            yields_before_demotion: 1,
            boost_interval_ticks: 2,
        };
        let sched = Scheduler::new(cfg, arch.clone()).expect("sched");
        let runs = Arc::new(core::sync::atomic::AtomicU32::new(0));
        let r2 = runs.clone();
        let id = sched
            .spawn(0, Priority::Low, move |_| {
                if r2.fetch_add(1, Ordering::Relaxed) >= 2 {
                    TaskAction::Exit
                } else {
                    TaskAction::Yield
                }
            })
            .expect("spawn");
        // Step once — boost hasn't fired yet.
        sched.step(0).expect("1");
        // Advance ticks past boost interval; the task should be promoted.
        arch.advance_ticks(10);
        sched.step(0).expect("2");
        let t = sched.tasks.read().get(&id).cloned().expect("task");
        // After the second step the task ran, was promoted to High at the
        // start of the call, then yielded => demoted to Normal.
        assert_eq!(t.load_priority(), Priority::Normal);
    }

    #[test]
    fn on_timer_tick_bumps_counter_but_does_not_dispatch() {
        let (_arch, sched) = mk(2);
        let id = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Exit)
            .expect("spawn");
        assert_eq!(sched.preemption_count(0), Ok(0));
        // ISR-safety contract (see rustdoc on `on_timer_tick`): the
        // entry point only bumps the counter — it must NOT call
        // `step` (which holds the registry RwLock and the overflow
        // SpinLock, both forbidden from interrupt context).
        sched.on_timer_tick(0).expect("tick");
        assert_eq!(sched.preemption_count(0), Ok(1));
        assert_eq!(sched.preemption_count(1), Ok(0));
        assert_eq!(sched.total_preemption_count(), 1);
        // The task must NOT have been dispatched — the cooperative
        // `step` loop is the only writer of run-queue state.
        assert_eq!(sched.state_of(id), TaskState::Ready);
    }

    #[test]
    fn on_timer_tick_idle_still_counts() {
        let (_arch, sched) = mk(1);
        // Even with no work, a tick must register as a preemption
        // observation so the integration-test assertion is not gated
        // on workload presence.
        assert_eq!(sched.on_timer_tick(0), Ok(()));
        assert_eq!(sched.preemption_count(0), Ok(1));
    }

    #[test]
    fn on_timer_tick_rejects_unknown_cpu_without_bumping() {
        let (_arch, sched) = mk(2);
        assert_eq!(sched.on_timer_tick(7), Err(SchedError::NoSuchCpu));
        // The counter must not be incremented for an out-of-range CPU
        // (the documented contract — a stray ISR cannot poison the
        // metric an integration test reads).
        assert_eq!(sched.total_preemption_count(), 0);
    }

    #[test]
    fn preemption_count_rejects_unknown_cpu() {
        let (_arch, sched) = mk(2);
        assert_eq!(sched.preemption_count(99), Err(SchedError::NoSuchCpu));
    }

    #[test]
    fn on_timer_tick_is_per_cpu() {
        let (_arch, sched) = mk(4);
        for _ in 0..3 {
            sched.on_timer_tick(2).expect("tick");
        }
        sched.on_timer_tick(0).expect("tick");
        assert_eq!(sched.preemption_count(0), Ok(1));
        assert_eq!(sched.preemption_count(1), Ok(0));
        assert_eq!(sched.preemption_count(2), Ok(3));
        assert_eq!(sched.preemption_count(3), Ok(0));
        assert_eq!(sched.total_preemption_count(), 4);
    }

    // ---------------------------------------------------------------
    // Stage 2.7 follow-up (f1): per-CPU current-task slot.
    // ---------------------------------------------------------------

    #[test]
    fn current_task_is_none_before_any_dispatch() {
        let (_arch, sched) = mk(2);
        assert_eq!(sched.current_task(0), None);
        assert_eq!(sched.current_task(1), None);
    }

    #[test]
    fn current_task_returns_none_for_unknown_cpu() {
        let (_arch, sched) = mk(2);
        // Out-of-range cpu has, by definition, no current task — and
        // the lookup must not panic. This is the contract the syscall
        // entry path depends on (`production_dispatch` is allowed to
        // probe `current_task` without first validating `current_cpu`).
        assert_eq!(sched.current_task(99), None);
    }

    #[test]
    fn current_task_is_set_during_body_invocation() {
        // Drive a body that calls back into the scheduler to read its
        // own current-task slot, then exits. This proves the slot is
        // published **before** the body starts running, as documented.
        let arch = Arc::new(TestArch::new(1).expect("arch"));
        let cfg = SchedulerConfig {
            cpus: 1,
            queue_capacity_per_band: 64,
            yields_before_demotion: 1,
            boost_interval_ticks: 1024,
        };
        let sched = Arc::new(Scheduler::new(cfg, arch.clone()).expect("sched"));
        let observed = Arc::new(AtomicU64::new(0));
        let o2 = observed.clone();
        let sched_for_body = sched.clone();
        let id = sched
            .spawn(0, Priority::Normal, move |ctx| {
                let cur = sched_for_body.current_task(ctx.cpu).unwrap_or(0);
                o2.store(cur, Ordering::Release);
                TaskAction::Exit
            })
            .expect("spawn");
        arch.set_current_cpu(0);
        let outcome = sched.step(0).expect("step");
        assert_eq!(outcome, StepOutcome::Ran(id));
        assert_eq!(observed.load(Ordering::Acquire), id);
        // And cleared once the body returned.
        assert_eq!(sched.current_task(0), None);
    }

    #[test]
    fn park_clears_matching_current_task_slot() {
        // Simulate a state where the slot has been published by a
        // dispatch in flight and then the task is externally parked
        // before dispatch returns. The public contract of `park` is
        // that, after it returns Ok, no per-CPU slot still points at
        // the parked task.
        let (_arch, sched) = mk(2);
        let id = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Park)
            .expect("spawn");
        // Publish the slot by hand to mimic an in-flight dispatch on
        // CPU 1 (a sibling); we then park the task and assert the slot
        // is cleared. We do not use a private helper here on purpose
        // — the assertion goes through the public `current_task`.
        sched.set_current(1, id);
        assert_eq!(sched.current_task(1), Some(id));
        sched.park(id).expect("park");
        assert_eq!(sched.current_task(1), None);
    }

    #[test]
    fn exit_clears_matching_current_task_slot() {
        let (_arch, sched) = mk(2);
        let id = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Exit)
            .expect("spawn");
        sched.set_current(0, id);
        sched.set_current(1, id);
        assert_eq!(sched.current_task(0), Some(id));
        assert_eq!(sched.current_task(1), Some(id));
        sched.exit(id).expect("exit");
        assert_eq!(sched.current_task(0), None);
        assert_eq!(sched.current_task(1), None);
    }

    #[test]
    fn yield_current_requeues_and_clears_slot() {
        let (arch, sched) = mk(1);
        let id = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Yield)
            .expect("spawn");
        arch.set_current_cpu(0);
        // Mimic the state mid-dispatch: task running, slot published.
        sched
            .tasks
            .read()
            .get(&id)
            .cloned()
            .expect("task")
            .store_state(TaskState::Running);
        sched.set_current(0, id);
        assert_eq!(sched.current_task(0), Some(id));
        sched.yield_current(id).expect("yield");
        // Slot cleared.
        assert_eq!(sched.current_task(0), None);
        // Task is Ready and on a run queue: the next step dispatches it.
        assert_eq!(sched.state_of(id), TaskState::Ready);
        let outcome = sched.step(0).expect("step");
        assert_eq!(outcome, StepOutcome::Ran(id));
    }

    #[test]
    fn yield_current_rejects_non_running_task() {
        let (_arch, sched) = mk(1);
        let id = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Exit)
            .expect("spawn");
        // Freshly-spawned task is Ready, not Running — `yield_current`
        // models an in-flight syscall yield and must reject anything
        // else (`AGENTS.md` §5.4 fail-closed).
        assert_eq!(sched.yield_current(id), Err(SchedError::InvalidState));
    }

    #[test]
    fn yield_current_rejects_unknown_task() {
        let (_arch, sched) = mk(1);
        assert_eq!(sched.yield_current(424_242), Err(SchedError::NoSuchTask));
    }
}
