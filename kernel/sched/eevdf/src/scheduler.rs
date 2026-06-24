//! Fully tickless EEVDF dispatch over per-CPU virtual-time run queues.
//!
//! See `docs/src/architecture/scheduler.md` for the algorithm
//! description, invariants, and the EEVDF vs MLFQ comparison. This module
//! is the implementation; the doc page is the source of truth.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use core::sync::atomic::{AtomicU64, Ordering};

use rustos_rng::{FastRng, RandU64};
use rustos_sync::{RwLock, SpinLock};

use crate::runqueue::{Entry, RunQueue, SCALE, SERVICE_PER_DISPATCH};
use crate::task::{TaskBody, TaskInner};
use crate::{
    CoreClass, CpuId, Priority, SchedError, SchedResult, SchedulerArch, SchedulerConfig,
    SchedulerPolicy, StepOutcome, TaskAction, TaskContext, TaskId, TaskState,
};

/// Per-CPU scheduler state: the virtual-time run queue plus the timer
/// observation counter and last-run bookkeeping.
struct CpuState {
    queue: RunQueue,
    last_run_tick: AtomicU64,
}

/// Virtual-time request size of a single dispatch for `weight`.
fn request(weight: u64) -> u64 {
    SERVICE_PER_DISPATCH.saturating_mul(SCALE) / weight.max(1)
}

/// The SMP, fully tickless EEVDF scheduler.
///
/// Generic over the architecture surface so host tests plug in a mock and
/// the architecture ports plug in their own types. The scheduler owns a
/// fixed array of per-CPU virtual-time run queues (no global run queue), an
/// `RwLock`-protected task registry, a `SpinLock`-protected overflow
/// list, and an `Arc<A>` for current-CPU / tick / IPI access.
pub struct Scheduler<A: SchedulerArch> {
    arch: Arc<A>,
    cpus: Box<[CpuState]>,
    tasks: RwLock<BTreeMap<TaskId, Arc<TaskInner>>>,
    next_id: AtomicU64,
    config: SchedulerConfig,
    /// Per-CPU fast, non-cryptographic generators for work-stealing
    /// victim selection. This is a "scheduler decision" in the sense of
    /// `lib/rng` (there is no second PRNG): the
    /// project's shared [`FastRng`] (xoshiro256++), not a hand-rolled
    /// one. `next_victim` takes `&self`, so each generator's `&mut self`
    /// stepping lives behind a [`SpinLock`] — but the slot is indexed by
    /// the *calling* CPU, so the lock is always uncontended on the
    /// work-stealing path (every idle CPU draws from its own stream
    /// rather than serialising on one global generator).
    victim_rng: Box<[SpinLock<FastRng>]>,
    overflow: SpinLock<Vec<TaskId>>,
    preemptions: Box<[AtomicU64]>,
    current: Box<[AtomicU64]>,
    /// Static [`CoreClass`] of each CPU, snapshotted once at construction
    /// from the arch HAL ([`SchedulerArch::core_class`]).
    core_classes: Box<[CoreClass]>,
    /// Dense list of the performance-class CPUs.
    perf_cpus: Box<[CpuId]>,
    /// Dense list of the efficiency-class CPUs. Empty on a homogeneous
    /// machine, where placement then falls back to the task's home CPU.
    eff_cpus: Box<[CpuId]>,
    /// Round-robin cursor used to spread same-class placements.
    class_rr: AtomicU64,
}

impl<A: SchedulerArch> Scheduler<A> {
    /// Construct a scheduler from a config and an architecture surface.
    ///
    /// # Errors
    /// * [`SchedError::NoSuchCpu`] if `config.cpus == 0`.
    /// * [`SchedError::QueueFull`] if the queue capacity is invalid (not
    ///   a power of two, or below 2).
    pub fn new(config: SchedulerConfig, arch: Arc<A>) -> SchedResult<Self> {
        if config.cpus == 0 {
            return Err(SchedError::NoSuchCpu);
        }
        let mut cpus = Vec::with_capacity(config.cpus as usize);
        for _ in 0..config.cpus {
            let queue =
                RunQueue::try_new(config.queue_capacity_per_band).ok_or(SchedError::QueueFull)?;
            cpus.push(CpuState {
                queue,
                last_run_tick: AtomicU64::new(0),
            });
        }
        let mut preemptions = Vec::with_capacity(config.cpus as usize);
        let mut current = Vec::with_capacity(config.cpus as usize);
        let mut victim_rng = Vec::with_capacity(config.cpus as usize);
        for cpu in 0..config.cpus {
            preemptions.push(AtomicU64::new(0));
            current.push(AtomicU64::new(0));
            // One generator per CPU so the work-stealing draw never
            // contends; the base constant is perturbed by the CPU id so
            // the streams start independent.
            victim_rng.push(SpinLock::new(FastRng::seed_from_u64(
                0x9E37_79B9_7F4A_7C15 ^ u64::from(cpu),
            )));
        }
        let mut core_classes = Vec::with_capacity(config.cpus as usize);
        let mut perf_cpus = Vec::new();
        let mut eff_cpus = Vec::new();
        for cpu in 0..config.cpus {
            let class = arch.core_class(cpu);
            core_classes.push(class);
            match class {
                CoreClass::Performance => perf_cpus.push(cpu),
                CoreClass::Efficiency => eff_cpus.push(cpu),
            }
        }
        Ok(Self {
            arch,
            cpus: cpus.into_boxed_slice(),
            tasks: RwLock::new(BTreeMap::new()),
            next_id: AtomicU64::new(1),
            config,
            victim_rng: victim_rng.into_boxed_slice(),
            overflow: SpinLock::new(Vec::new()),
            preemptions: preemptions.into_boxed_slice(),
            current: current.into_boxed_slice(),
            core_classes: core_classes.into_boxed_slice(),
            perf_cpus: perf_cpus.into_boxed_slice(),
            eff_cpus: eff_cpus.into_boxed_slice(),
            class_rr: AtomicU64::new(0),
        })
    }

    /// The [`CoreClass`] a task at `prio` should run on: interactive /
    /// throughput work (`High`, `Normal`) on a performance core,
    /// background work (`Low`) on an efficiency core.
    const fn preferred_class(prio: Priority) -> CoreClass {
        match prio {
            Priority::High | Priority::Normal => CoreClass::Performance,
            Priority::Low => CoreClass::Efficiency,
        }
    }

    /// Choose the CPU a task at `prio` should be enqueued on, given its
    /// current `home`. Keeps `home` when it already matches the preferred
    /// class (so the path is a strict no-op on a homogeneous machine);
    /// otherwise round-robins across the CPUs of the preferred class,
    /// falling back to `home` when that pool is empty.
    fn preferred_home(&self, prio: Priority, home: CpuId) -> CpuId {
        let want = Self::preferred_class(prio);
        if self.class_of(home) == want {
            return home;
        }
        let pool = match want {
            CoreClass::Performance => &self.perf_cpus,
            CoreClass::Efficiency => &self.eff_cpus,
        };
        if pool.is_empty() {
            return home;
        }
        // `pool.len()` is at most the CPU count (`u32`), so the modulus
        // result fits a `usize` without truncation.
        #[allow(clippy::cast_possible_truncation)]
        let idx = {
            let len = pool.len() as u64;
            (self.class_rr.fetch_add(1, Ordering::Relaxed) % len) as usize
        };
        pool[idx]
    }

    /// The static [`CoreClass`] of `cpu`, or [`CoreClass::Performance`]
    /// (the safe default) for an out-of-range id.
    fn class_of(&self, cpu: CpuId) -> CoreClass {
        self.core_classes
            .get(cpu as usize)
            .copied()
            .unwrap_or(CoreClass::Performance)
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

    fn lookup(&self, id: TaskId) -> SchedResult<Arc<TaskInner>> {
        self.tasks
            .read()
            .get(&id)
            .cloned()
            .ok_or(SchedError::NoSuchTask)
    }

    /// Admit `task` to `rq`'s competition and compute its EEVDF virtual
    /// times: eligible at the queue's current virtual time `V` (zero
    /// initial lag), deadline one request later. The weight is added to
    /// the queue's competing total so `V` advances proportionally.
    fn admit(task: &TaskInner, rq: &RunQueue) -> Entry {
        let weight = task.weight();
        let eligible = rq.admit_weight(weight);
        let deadline = eligible.saturating_add(request(weight));
        task.set_virtual(eligible, deadline);
        Entry {
            id: task.id,
            eligible,
            deadline,
        }
    }

    /// Enqueue `task` onto its home CPU, falling back to the global
    /// overflow list when that queue is at its compile-time bound. The
    /// task's weight must already be accounted on its home CPU (via
    /// [`Self::admit`] or a prior dispatch).
    fn enqueue_home(&self, task: &TaskInner) {
        let home = task.home_cpu.load(Ordering::Acquire);
        let entry = Entry {
            id: task.id,
            eligible: task.eligible(),
            deadline: task.deadline(),
        };
        match self.cpus.get(home as usize) {
            Some(cpu) if cpu.queue.push(entry).is_ok() => {}
            _ => self.overflow.lock().push(task.id),
        }
    }

    /// Spawn a new task at `priority` with a body closure.
    ///
    /// `home_cpu` is the caller's hint. On a heterogeneous machine the
    /// task is placed on a CPU of the [`CoreClass`] its priority calls
    /// for (`preferred_home`) — a `Low` background task on an
    /// efficiency core, interactive work on a performance core; on a
    /// homogeneous machine the hint is honoured unchanged.
    ///
    /// # Errors
    /// * [`SchedError::NoSuchCpu`] if `home_cpu` is out of range.
    pub fn spawn<F>(&self, home_cpu: CpuId, priority: Priority, body: F) -> SchedResult<TaskId>
    where
        F: FnMut(&mut TaskContext) -> TaskAction + Send + 'static,
    {
        self.cpu_state(home_cpu)?;
        let placed = self.preferred_home(priority, home_cpu);
        let cpu = self.cpu_state(placed)?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let id = if id == 0 {
            self.next_id.fetch_add(1, Ordering::Relaxed)
        } else {
            id
        };
        let boxed: Box<TaskBody> = Box::new(body);
        let inner = Arc::new(TaskInner::new(id, placed, priority, boxed));
        let entry = Self::admit(&inner, &cpu.queue);
        self.tasks.write().insert(id, inner);
        if cpu.queue.push(entry).is_err() {
            self.overflow.lock().push(id);
        }
        self.arch.send_ipi(placed);
        Ok(id)
    }

    /// Block a task. Cancellation-safe.
    ///
    /// Only the transition that actually moves the task out of
    /// [`TaskState::Ready`] / [`TaskState::Running`] drops its competing
    /// weight, so a stale run-queue entry discovered later by
    /// [`Self::step`] is simply discarded without double-counting.
    ///
    /// # Errors
    /// * [`SchedError::NoSuchTask`] if no task ever held that id.
    /// * [`SchedError::InvalidState`] if the task is terminal.
    pub fn park(&self, id: TaskId) -> SchedResult<()> {
        let task = self.lookup(id)?;
        loop {
            match task.load_state() {
                TaskState::Exited => return Err(SchedError::InvalidState),
                TaskState::Parked => {
                    self.clear_current_matching(id);
                    return Ok(());
                }
                cur @ (TaskState::Ready | TaskState::Running) => {
                    if task.cas_state(cur, TaskState::Parked).is_ok() {
                        self.remove_weight_on_home(&task);
                        self.clear_current_matching(id);
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Wake a parked task, re-admitting it to its home CPU. Cancellation-safe.
    ///
    /// # Errors
    /// * [`SchedError::NoSuchTask`] if no task ever held that id.
    /// * [`SchedError::InvalidState`] if the task is terminal.
    pub fn unpark(&self, id: TaskId) -> SchedResult<()> {
        let task = self.lookup(id)?;
        match task.load_state() {
            TaskState::Exited => return Err(SchedError::InvalidState),
            // The task has not committed to park yet (it is running its
            // body, or already queued). A `cas` to Ready would be a no-op
            // here, so the wake would be *lost* if the task then parked.
            // Record the wake as a pending token instead; the dispatch
            // loop's `Park` commit consumes it and re-readies the task
            // rather than sleeping it (no lost wake-ups).
            TaskState::Ready | TaskState::Running => {
                task.set_wake_pending();
                return Ok(());
            }
            TaskState::Parked => {}
        }
        task.cas_state(TaskState::Parked, TaskState::Ready)
            .map_err(|_| SchedError::InvalidState)?;
        // A wake is the moment a previously-idle task "needs power":
        // re-place it onto a CPU of the class its priority calls for
        // before re-admitting its weight there. On a homogeneous machine
        // this is its old home, so admission is unchanged.
        let home = task.home_cpu.load(Ordering::Acquire);
        let target = self.preferred_home(task.load_priority(), home);
        task.home_cpu.store(target, Ordering::Release);
        let cpu = self.cpu_state(target)?;
        let entry = Self::admit(&task, &cpu.queue);
        if cpu.queue.push(entry).is_err() {
            self.overflow.lock().push(id);
        }
        self.arch.send_ipi(target);
        Ok(())
    }

    /// Terminate a task. Cancellation-safe and idempotent.
    ///
    /// # Errors
    /// * [`SchedError::NoSuchTask`] if the id was never spawned.
    pub fn exit(&self, id: TaskId) -> SchedResult<()> {
        let task = self.lookup(id)?;
        let prev = task.swap_state(TaskState::Exited);
        if matches!(prev, TaskState::Ready | TaskState::Running) {
            self.remove_weight_on_home(&task);
        }
        if let Some(mut guard) = task.body.try_lock() {
            *guard = None;
        }
        self.clear_current_matching(id);
        Ok(())
    }

    /// Drop `task`'s weight from its current home CPU's competition.
    fn remove_weight_on_home(&self, task: &TaskInner) {
        let home = task.home_cpu.load(Ordering::Acquire);
        if let Some(cpu) = self.cpus.get(home as usize) {
            cpu.queue.remove_weight(task.weight());
        }
    }

    /// Timer observation point. EEVDF is **fully tickless** — fairness,
    /// eligibility, and preemption are driven entirely by virtual time
    /// advanced inside [`Self::step`], never by this counter. The hook
    /// exists only so the arch port's timer ISR (where one is wired) can
    /// record that it fired; the count is observable through
    /// [`Self::preemption_count`] for audit and integration tests. No
    /// scheduling decision ever reads it.
    ///
    /// # Errors
    /// * [`SchedError::NoSuchCpu`] if `cpu` is out of range. The counter
    ///   is not incremented in that case.
    pub fn on_timer_tick(&self, cpu: CpuId) -> SchedResult<()> {
        let _ = self.cpu_state(cpu)?;
        self.preemptions[cpu as usize].fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Returns the per-CPU timer-observation count.
    ///
    /// # Errors
    /// * [`SchedError::NoSuchCpu`] if `cpu` is out of range.
    pub fn preemption_count(&self, cpu: CpuId) -> SchedResult<u64> {
        let _ = self.cpu_state(cpu)?;
        Ok(self.preemptions[cpu as usize].load(Ordering::Relaxed))
    }

    /// Returns the sum of [`Self::preemption_count`] across every CPU.
    #[must_use]
    pub fn total_preemption_count(&self) -> u64 {
        self.preemptions
            .iter()
            .map(|p| p.load(Ordering::Relaxed))
            .sum()
    }

    /// One scheduler step on `cpu`: pick the earliest-eligible-virtual-
    /// deadline task, run it once, then re-enqueue or retire it. Falls
    /// back to work-stealing before reporting [`StepOutcome::Idle`].
    ///
    /// # Errors
    /// * [`SchedError::NoSuchCpu`] if `cpu` is out of range.
    pub fn step(&self, cpu: CpuId) -> SchedResult<StepOutcome> {
        let me = self.cpu_state(cpu)?;
        self.drain_overflow();

        if let Some(entry) = me.queue.pick() {
            return Ok(self.dispatch(cpu, entry.id));
        }
        if let Some(id) = self.try_steal(cpu) {
            return Ok(self.dispatch(cpu, id));
        }
        Ok(StepOutcome::Idle)
    }

    /// Best-effort drain of the overflow list back onto each task's home
    /// CPU. A task that still does not fit is left for a later step.
    fn drain_overflow(&self) {
        let mut g = self.overflow.lock();
        let pending: Vec<TaskId> = g.drain(..).collect();
        drop(g);
        for id in pending {
            let Ok(task) = self.lookup(id) else { continue };
            if task.load_state() != TaskState::Ready {
                continue;
            }
            let home = task.home_cpu.load(Ordering::Acquire);
            let entry = Entry {
                id,
                eligible: task.eligible(),
                deadline: task.deadline(),
            };
            match self.cpus.get(home as usize) {
                Some(cpu) if cpu.queue.push(entry).is_ok() => {}
                _ => self.overflow.lock().push(id),
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
            #[allow(clippy::cast_possible_truncation)]
            if v as u32 == cpu {
                continue;
            }
            if let Some(entry) = self.cpus[v].queue.steal() {
                // Migrate the task: drop its weight from the victim and
                // re-admit it on the stealing CPU, rebasing its virtual
                // times to the new queue's clock (the EEVDF migration
                // rule — a task carries no lag across CPUs).
                let Ok(task) = self.lookup(entry.id) else {
                    continue;
                };
                #[allow(clippy::cast_possible_truncation)]
                {
                    self.cpus[v].queue.release_weight(task.weight());
                    task.home_cpu.store(cpu, Ordering::Release);
                    let _ = Self::admit(&task, &self.cpus[cpu as usize].queue);
                }
                return Some(entry.id);
            }
        }
        None
    }

    fn next_victim(&self, cpu: CpuId) -> u32 {
        let s = self.victim_rng[cpu as usize].lock().next_u64();
        #[allow(clippy::cast_possible_truncation)]
        {
            let n = self.cpus.len() as u64;
            (s % n) as u32
        }
    }

    fn dispatch(&self, cpu: CpuId, id: TaskId) -> StepOutcome {
        let Some(task) = self.tasks.read().get(&id).cloned() else {
            return StepOutcome::Idle;
        };
        match task.load_state() {
            TaskState::Exited => {
                self.tasks.write().remove(&id);
                return StepOutcome::Idle;
            }
            // A stale entry for a task parked or already running
            // elsewhere: discard it, the owner re-enqueues it.
            TaskState::Parked | TaskState::Running => return StepOutcome::Idle,
            TaskState::Ready => {}
        }
        if task
            .cas_state(TaskState::Ready, TaskState::Running)
            .is_err()
        {
            return StepOutcome::Idle;
        }

        self.set_current(cpu, id);
        let tick = self.arch.ticks_now();
        task.last_started.store(tick, Ordering::Release);
        task.home_cpu.store(cpu, Ordering::Release);

        // Tickless preemption: arm this CPU's one-shot
        // timer for a single quantum iff at least one *other* ready task
        // is waiting on it (a competitor the task we are about to run must
        // be preempted for), and disarm otherwise so a CPU running a sole
        // runnable task takes no timer interrupts at all. The running task
        // sits in the current slot, not the queue, so a non-empty ready
        // queue is exactly "there is a competitor". The port owns the
        // quantum length (`set_preemption` is a pure boolean); the
        // host `TestArch` inherits the no-op default. Armed *before* the
        // body switches into the task so the deadline is live for the run.
        self.arch
            .set_preemption(self.cpus[cpu as usize].queue.ready_len() > 0);

        let mut ctx = TaskContext {
            cpu,
            tick,
            task_id: id,
        };
        let action = {
            let mut body_guard = task.body.lock();
            match body_guard.as_mut() {
                Some(b) => b(&mut ctx),
                None => TaskAction::Exit,
            }
        };
        task.total_runs.fetch_add(1, Ordering::Relaxed);
        self.cpus[cpu as usize]
            .last_run_tick
            .store(tick, Ordering::Release);

        // The task received one unit of service while it was active;
        // advance this CPU's virtual time before settling weight so the
        // share reflects the run that just happened.
        self.cpus[cpu as usize].queue.advance(SERVICE_PER_DISPATCH);

        let observed = task.load_state();
        let mut effective = match (observed, action) {
            (TaskState::Exited, _) | (_, TaskAction::Exit) => TaskAction::Exit,
            (TaskState::Parked, _) | (_, TaskAction::Park) => TaskAction::Park,
            _ => TaskAction::Yield,
        };
        // Close the park/unpark lost-wakeup race: if an `unpark` arrived
        // while this task was running its body (it could not move a
        // non-parked task, so it left a token), cancel the park and
        // re-ready the task so the wake is honoured rather than dropped. Consume the token exactly when we would have
        // parked, so a token left by an earlier already-honoured wake never
        // suppresses a genuine later park.
        if effective == TaskAction::Park && task.take_wake_pending() {
            effective = TaskAction::Yield;
        }

        self.clear_current(cpu);
        match effective {
            TaskAction::Exit => {
                let prev = task.swap_state(TaskState::Exited);
                if matches!(prev, TaskState::Ready | TaskState::Running) {
                    self.cpus[cpu as usize].queue.remove_weight(task.weight());
                }
                if let Some(mut guard) = task.body.try_lock() {
                    *guard = None;
                }
                self.tasks.write().remove(&id);
            }
            TaskAction::Park => {
                let prev = task.swap_state(TaskState::Parked);
                if matches!(prev, TaskState::Ready | TaskState::Running) {
                    self.cpus[cpu as usize].queue.remove_weight(task.weight());
                }
            }
            TaskAction::Yield => {
                task.store_state(TaskState::Ready);
                let dest = self.preferred_home(task.load_priority(), cpu);
                if dest == cpu {
                    // EEVDF: the fulfilled request rolls the eligible time
                    // forward to the old deadline and sets the next
                    // deadline a request later. The weight stays on this
                    // CPU. This is the homogeneous / already-correct-class
                    // path.
                    let weight = task.weight();
                    let new_eligible = task.deadline();
                    let new_deadline = new_eligible.saturating_add(request(weight));
                    task.set_virtual(new_eligible, new_deadline);
                    self.enqueue_home(&task);
                } else {
                    // The task is on the wrong class for its priority
                    // (e.g. a Low task that work-stealing parked on a
                    // performance core). Migrate it: drop its weight here
                    // and re-admit it on the preferred-class CPU, rebasing
                    // its virtual times onto that queue's clock — the same
                    // no-lag-across-CPUs rule `try_steal` uses.
                    self.cpus[cpu as usize].queue.remove_weight(task.weight());
                    task.home_cpu.store(dest, Ordering::Release);
                    let entry = Self::admit(&task, &self.cpus[dest as usize].queue);
                    if self.cpus[dest as usize].queue.push(entry).is_err() {
                        self.overflow.lock().push(id);
                    }
                }
            }
        }
        StepOutcome::Ran(id)
    }

    /// Total number of times the given task's body has been invoked.
    ///
    /// # Errors
    /// * [`SchedError::NoSuchTask`] if the id is unknown.
    pub fn run_count(&self, id: TaskId) -> SchedResult<u64> {
        self.tasks
            .read()
            .get(&id)
            .map(|t| t.total_runs.load(Ordering::Acquire))
            .ok_or(SchedError::NoSuchTask)
    }

    /// Most recent state of `id` ([`TaskState::Exited`] once drained).
    #[must_use]
    pub fn state_of(&self, id: TaskId) -> TaskState {
        self.tasks
            .read()
            .get(&id)
            .map_or(TaskState::Exited, |t| t.load_state())
    }

    /// Number of live (non-[`TaskState::Exited`]) tasks.
    #[must_use]
    pub fn live_task_count(&self) -> usize {
        self.tasks
            .read()
            .values()
            .filter(|t| t.load_state() != TaskState::Exited)
            .count()
    }

    /// [`TaskId`] of the task currently dispatching on `cpu`, or `None`.
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
    /// Models a voluntary syscall yield: the task's virtual times roll
    /// forward exactly as a dispatch-loop `Yield` would, it is
    /// re-enqueued on its home CPU at its current priority, and the
    /// per-CPU current-task slot is cleared.
    ///
    /// # Errors
    /// * [`SchedError::NoSuchTask`] if the id is unknown.
    /// * [`SchedError::InvalidState`] if the task is not running.
    pub fn yield_current(&self, id: TaskId) -> SchedResult<()> {
        let task = self.lookup(id)?;
        task.cas_state(TaskState::Running, TaskState::Ready)
            .map_err(|_| SchedError::InvalidState)?;
        let weight = task.weight();
        let new_eligible = task.deadline();
        let new_deadline = new_eligible.saturating_add(request(weight));
        task.set_virtual(new_eligible, new_deadline);
        self.enqueue_home(&task);
        self.clear_current_matching(id);
        Ok(())
    }

    fn set_current(&self, cpu: CpuId, id: TaskId) {
        if let Some(slot) = self.current.get(cpu as usize) {
            slot.store(id, Ordering::Release);
        }
    }

    fn clear_current(&self, cpu: CpuId) {
        if let Some(slot) = self.current.get(cpu as usize) {
            slot.store(0, Ordering::Release);
        }
    }

    fn clear_current_matching(&self, id: TaskId) {
        for slot in self.current.iter() {
            let _ = slot.compare_exchange(id, 0, Ordering::AcqRel, Ordering::Relaxed);
        }
    }
}

/// EEVDF implements the architecture-neutral [`SchedulerPolicy`] contract.
///
/// Each method forwards to the inherent implementation above; the
/// forwarding adapter is what lets `kernel/core` select this policy by
/// the trait while the inherent surface stays available to this crate's
/// own dispatch internals and tests.
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
    use core::sync::atomic::AtomicU32;

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
    fn rejects_zero_cpu_and_bad_capacity() {
        let arch = Arc::new(TestArch::new(1).expect("arch"));
        let bad_cpus = SchedulerConfig {
            cpus: 0,
            ..SchedulerConfig::defaults_for(1)
        };
        assert_eq!(
            Scheduler::new(bad_cpus, arch.clone()).err(),
            Some(SchedError::NoSuchCpu)
        );
        let bad_cap = SchedulerConfig {
            cpus: 1,
            queue_capacity_per_band: 3,
            ..SchedulerConfig::defaults_for(1)
        };
        assert_eq!(
            Scheduler::new(bad_cap, arch).err(),
            Some(SchedError::QueueFull)
        );
    }

    #[test]
    fn spawn_runs_once_and_ipis_home() {
        let (arch, sched) = mk(2);
        let counter = Arc::new(AtomicU32::new(0));
        let c2 = counter.clone();
        let id = sched
            .spawn(0, Priority::Normal, move |_| {
                c2.fetch_add(1, Ordering::Relaxed);
                TaskAction::Exit
            })
            .expect("spawn");
        arch.set_current_cpu(0);
        assert_eq!(sched.step(0), Ok(StepOutcome::Ran(id)));
        assert_eq!(counter.load(Ordering::Relaxed), 1);
        assert_eq!(sched.state_of(id), TaskState::Exited);
        assert!(arch.ipi_count(0) >= 1, "spawn must IPI the home CPU");
        assert_eq!(sched.step(0), Ok(StepOutcome::Idle));
    }

    #[test]
    fn idle_when_no_work() {
        let (_arch, sched) = mk(2);
        assert_eq!(sched.step(0), Ok(StepOutcome::Idle));
    }

    #[test]
    fn park_unpark_roundtrip() {
        let (_arch, sched) = mk(1);
        let id = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Park)
            .expect("spawn");
        assert_eq!(sched.step(0), Ok(StepOutcome::Ran(id)));
        assert_eq!(sched.state_of(id), TaskState::Parked);
        sched.unpark(id).expect("unpark");
        assert_eq!(sched.step(0), Ok(StepOutcome::Ran(id)));
    }

    /// Tickless preemption arming: a CPU running a
    /// sole runnable task disarms its one-shot timer (no competitor to
    /// preempt for), and a CPU with a second ready task arms it. Proven
    /// through the `TestArch` `set_preemption` ledger without a real
    /// timer.
    #[test]
    fn sole_task_disarms_and_a_competitor_arms_preemption() {
        let (arch, sched) = mk(1);
        arch.set_current_cpu(0);

        // One perpetual yielder: every dispatch leaves the ready queue
        // empty (the running task sits in the current slot), so the CPU
        // disarms.
        let solo = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Yield)
            .expect("solo");
        let _ = sched.step(0).expect("step");
        assert_eq!(
            arch.last_preemption(),
            Some(false),
            "a sole runnable task must disarm preemption (tickless idle)"
        );
        assert_eq!(arch.arm_count(), 0, "no competitor — never armed");
        assert!(arch.disarm_count() >= 1);

        // Add a second runnable task: now each dispatch leaves the other
        // queued, so the CPU arms its one-shot quantum.
        sched
            .spawn(0, Priority::Normal, |_| TaskAction::Yield)
            .expect("rival");
        let arms_before = arch.arm_count();
        for _ in 0..4 {
            let _ = sched.step(0).expect("step");
        }
        assert!(
            arch.arm_count() > arms_before,
            "a contended CPU must arm the one-shot preemption timer"
        );
        assert_eq!(
            arch.last_preemption(),
            Some(true),
            "the last dispatch had a queued competitor"
        );
        let _ = solo;
    }

    /// EEVDF is tickless: fairness must hold without ever advancing the
    /// arch tick counter. A High (weight 4) and a Low (weight 1) task,
    /// both perpetual yielders on one CPU, must both run — and the High
    /// task must run strictly more often, in proportion to its weight —
    /// even though `ticks_now()` never moves and `on_timer_tick` is never
    /// called.
    #[test]
    fn tickless_weight_proportional_fairness() {
        let (arch, sched) = mk(1);
        arch.set_current_cpu(0);
        let high = Arc::new(AtomicU32::new(0));
        let low = Arc::new(AtomicU32::new(0));
        let h = high.clone();
        let l = low.clone();
        sched
            .spawn(0, Priority::High, move |_| {
                h.fetch_add(1, Ordering::Relaxed);
                TaskAction::Yield
            })
            .expect("hi");
        sched
            .spawn(0, Priority::Low, move |_| {
                l.fetch_add(1, Ordering::Relaxed);
                TaskAction::Yield
            })
            .expect("lo");
        for _ in 0..600 {
            let _ = sched.step(0).expect("step");
            // Deliberately do NOT advance ticks or call on_timer_tick.
        }
        assert_eq!(sched.total_preemption_count(), 0, "no ticks were used");
        let hv = high.load(Ordering::Relaxed);
        let lv = low.load(Ordering::Relaxed);
        assert!(lv > 0, "low-band task is never starved (ran {lv})");
        assert!(hv > lv, "high-band task runs more often ({hv} vs {lv})");
        // 4:1 weight ratio — allow a generous band for integer rounding.
        assert!(
            hv >= lv * 3,
            "share should track the 4:1 weight ({hv}:{lv})"
        );
    }

    #[test]
    fn equal_weight_tasks_share_evenly() {
        let (arch, sched) = mk(1);
        arch.set_current_cpu(0);
        let a = Arc::new(AtomicU32::new(0));
        let b = Arc::new(AtomicU32::new(0));
        let a2 = a.clone();
        let b2 = b.clone();
        sched
            .spawn(0, Priority::Normal, move |_| {
                a2.fetch_add(1, Ordering::Relaxed);
                TaskAction::Yield
            })
            .expect("a");
        sched
            .spawn(0, Priority::Normal, move |_| {
                b2.fetch_add(1, Ordering::Relaxed);
                TaskAction::Yield
            })
            .expect("b");
        for _ in 0..200 {
            let _ = sched.step(0).expect("step");
        }
        let av = a.load(Ordering::Relaxed);
        let bv = b.load(Ordering::Relaxed);
        let diff = av.abs_diff(bv);
        assert!(diff <= 1, "equal-weight tasks share evenly ({av} vs {bv})");
    }

    #[test]
    fn work_stealing_finds_remote_task() {
        let (arch, sched) = mk(4);
        let id = sched
            .spawn(3, Priority::Normal, |_| TaskAction::Exit)
            .expect("spawn");
        arch.set_current_cpu(0);
        assert_eq!(sched.step(0), Ok(StepOutcome::Ran(id)));
        assert_eq!(sched.state_of(id), TaskState::Exited);
    }

    #[test]
    fn current_task_published_during_body() {
        let arch = Arc::new(TestArch::new(1).expect("arch"));
        let sched =
            Arc::new(Scheduler::new(SchedulerConfig::defaults_for(1), arch.clone()).unwrap());
        let observed = Arc::new(AtomicU64::new(0));
        let o2 = observed.clone();
        let s2 = sched.clone();
        let id = sched
            .spawn(0, Priority::Normal, move |ctx| {
                o2.store(s2.current_task(ctx.cpu).unwrap_or(0), Ordering::Release);
                TaskAction::Exit
            })
            .expect("spawn");
        arch.set_current_cpu(0);
        assert_eq!(sched.step(0), Ok(StepOutcome::Ran(id)));
        assert_eq!(observed.load(Ordering::Acquire), id);
        assert_eq!(sched.current_task(0), None);
    }

    #[test]
    fn exit_clears_current_slot() {
        let (_arch, sched) = mk(2);
        let id = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Exit)
            .expect("spawn");
        sched.set_current(0, id);
        sched.set_current(1, id);
        sched.exit(id).expect("exit");
        assert_eq!(sched.current_task(0), None);
        assert_eq!(sched.current_task(1), None);
    }

    #[test]
    fn yield_current_requeues_running_task() {
        let (arch, sched) = mk(1);
        let id = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Yield)
            .expect("spawn");
        arch.set_current_cpu(0);
        sched
            .tasks
            .read()
            .get(&id)
            .cloned()
            .expect("task")
            .store_state(TaskState::Running);
        sched.set_current(0, id);
        sched.yield_current(id).expect("yield");
        assert_eq!(sched.current_task(0), None);
        assert_eq!(sched.state_of(id), TaskState::Ready);
        assert_eq!(sched.step(0), Ok(StepOutcome::Ran(id)));
    }

    #[test]
    fn yield_current_rejects_non_running_and_unknown() {
        let (_arch, sched) = mk(1);
        assert_eq!(sched.yield_current(999), Err(SchedError::NoSuchTask));
        let id = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Exit)
            .expect("spawn");
        assert_eq!(sched.yield_current(id), Err(SchedError::InvalidState));
    }

    #[test]
    fn on_timer_tick_is_observation_only() {
        let (_arch, sched) = mk(2);
        let id = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Exit)
            .expect("spawn");
        sched.on_timer_tick(0).expect("tick");
        assert_eq!(sched.preemption_count(0), Ok(1));
        assert_eq!(sched.preemption_count(1), Ok(0));
        // The tick must not have dispatched the task — only `step` runs work.
        assert_eq!(sched.state_of(id), TaskState::Ready);
        assert_eq!(sched.on_timer_tick(7), Err(SchedError::NoSuchCpu));
        assert_eq!(sched.total_preemption_count(), 1);
    }

    // ---------------------------------------------------------------
    // Heterogeneous CPUs (performance + efficiency cores).
    // ---------------------------------------------------------------

    /// 4-CPU machine: CPUs 0/1 performance, CPUs 2/3 efficiency.
    fn mk_hetero() -> (Arc<TestArch>, Scheduler<TestArch>) {
        let arch = Arc::new(TestArch::new(4).expect("arch"));
        arch.set_core_class(2, CoreClass::Efficiency);
        arch.set_core_class(3, CoreClass::Efficiency);
        let sched = Scheduler::new(SchedulerConfig::defaults_for(4), arch.clone()).expect("sched");
        (arch, sched)
    }

    fn home_of(sched: &Scheduler<TestArch>, id: TaskId) -> CpuId {
        sched
            .tasks
            .read()
            .get(&id)
            .expect("task")
            .home_cpu
            .load(Ordering::Acquire)
    }

    #[test]
    fn background_task_is_placed_on_an_efficiency_core() {
        let (_arch, sched) = mk_hetero();
        let id = sched
            .spawn(0, Priority::Low, |_| TaskAction::Park)
            .expect("spawn");
        assert_eq!(sched.class_of(home_of(&sched, id)), CoreClass::Efficiency);
    }

    #[test]
    fn interactive_task_is_placed_on_a_performance_core() {
        let (_arch, sched) = mk_hetero();
        let id = sched
            .spawn(3, Priority::High, |_| TaskAction::Park)
            .expect("spawn");
        assert_eq!(sched.class_of(home_of(&sched, id)), CoreClass::Performance);
    }

    #[test]
    fn wrong_class_task_migrates_back_to_its_class_on_yield() {
        // Model what work-stealing can do: a Low task ends up homed on a
        // performance core. On its next yield it must migrate back down
        // to an efficiency core, carrying its weight with it (the
        // performance core's competing weight returns to zero).
        let (arch, sched) = mk_hetero();
        let id = sched
            .spawn(0, Priority::Low, |_| TaskAction::Yield)
            .expect("spawn");
        // It started on an efficiency core; forcibly re-home it onto a
        // performance core and admit its weight there, mimicking a steal.
        let task = sched.tasks.read().get(&id).cloned().expect("task");
        sched.remove_weight_on_home(&task);
        task.home_cpu.store(0, Ordering::Release);
        let _ = Scheduler::<TestArch>::admit(&task, &sched.cpus[0].queue);
        sched.cpus[0]
            .queue
            .push(Entry {
                id,
                eligible: task.eligible(),
                deadline: task.deadline(),
            })
            .expect("seed perf queue");
        assert_eq!(sched.class_of(home_of(&sched, id)), CoreClass::Performance);

        // Dispatch on the performance core: the yield migrates it back.
        arch.set_current_cpu(0);
        assert_eq!(sched.step(0), Ok(StepOutcome::Ran(id)));
        assert_eq!(
            sched.class_of(home_of(&sched, id)),
            CoreClass::Efficiency,
            "a Low task migrates back down to an efficiency core on yield"
        );
    }

    #[test]
    fn homogeneous_machine_keeps_the_caller_home() {
        // Default (all-performance) topology: a Low task stays on the
        // CPU the caller named — the heterogeneous path is a strict
        // no-op when there are no efficiency cores.
        let (_arch, sched) = mk(4);
        let id = sched
            .spawn(2, Priority::Low, |_| TaskAction::Park)
            .expect("spawn");
        assert_eq!(home_of(&sched, id), 2);
    }
}
