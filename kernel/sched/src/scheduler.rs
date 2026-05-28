//! Per-CPU run queues, MLFQ policy, work-stealing dispatch.
//!
//! See `docs/src/architecture/scheduler.md` for the algorithm description,
//! invariants, and debugging guide. This module is the implementation; the
//! doc page is the source of truth for behaviour.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use rustos_kernel_sync::{RwLock, SpinLock};

use crate::arch::{CpuId, SchedulerArch};
use crate::error::{SchedError, SchedResult};
use crate::loom_compat::{AtomicU64, Ordering};
use crate::runqueue::{RunDeque, Steal};
use crate::task::{Priority, TaskAction, TaskBody, TaskContext, TaskId, TaskInner, TaskState};

/// Static configuration for a [`Scheduler`].
///
/// The configuration is consumed at construction and is never mutated; all
/// limits are therefore enforceable without locks. Defaults are tuned for
/// kernel use (`AGENTS.md` §5 — security defaults): bounded queues, a
/// short quantum, and a frequent priority boost.
#[derive(Copy, Clone, Debug)]
pub struct SchedulerConfig {
    /// Number of CPUs the scheduler will manage. Must equal the count
    /// reported by the underlying [`SchedulerArch`].
    pub cpus: u32,
    /// Per-band queue capacity. Must be a power of two ≥ 2 (see
    /// [`RunDeque::try_new`]).
    pub queue_capacity_per_band: usize,
    /// Number of `Yield`s permitted at a single priority before MLFQ
    /// demotion kicks in. A value of `1` matches the classical MLFQ
    /// description; larger values make demotion gentler.
    pub yields_before_demotion: u64,
    /// Tick interval at which every non-exited task is promoted back to
    /// [`Priority::High`]. Bounds the worst-case starvation latency to
    /// this many ticks (`docs/src/architecture/scheduler.md` §"Starvation
    /// freedom").
    pub boost_interval_ticks: u64,
}

impl SchedulerConfig {
    /// Reasonable defaults for host tests and small embedded ports.
    ///
    /// Production ports are expected to override these after measuring
    /// their timer resolution and workload mix.
    #[must_use]
    pub const fn defaults_for(cpus: u32) -> Self {
        Self {
            cpus,
            queue_capacity_per_band: 16_384,
            yields_before_demotion: 1,
            boost_interval_ticks: 256,
        }
    }
}

/// Outcome of one [`Scheduler::step`] call.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum StepOutcome {
    /// A task was dispatched. The contained `TaskId` ran exactly once.
    Ran(TaskId),
    /// No runnable work for this CPU after both the local queues and
    /// stealing were exhausted. The arch port should idle (HLT / WFI /
    /// `yield_now`).
    Idle,
}

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
        Ok(Self {
            arch,
            cpus: cpus.into_boxed_slice(),
            tasks: RwLock::new(BTreeMap::new()),
            next_id: AtomicU64::new(1),
            config,
            last_boost_tick: AtomicU64::new(0),
            victim_rng: AtomicU64::new(0x9E37_79B9_7F4A_7C15),
            overflow: SpinLock::new(Vec::new()),
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
                TaskState::Parked => return Ok(()),
                cur @ (TaskState::Ready | TaskState::Running) => {
                    if task.cas_state(cur, TaskState::Parked).is_ok() {
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
        Ok(())
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arch::TestArch;

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
}
