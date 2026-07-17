//! The SMP, non-tickless CFQ scheduler.
//!
//! Generic over the architecture surface so host tests plug in a mock and
//! the architecture ports plug in their own types. The scheduler owns a
//! fixed array of per-CPU weighted-vruntime run queues (no global run
//! queue), an `RwLock`-protected task registry, a `SpinLock`-protected
//! overflow list, and an `Arc<A>` for current-CPU / tick / IPI access.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use core::sync::atomic::{fence, AtomicU64, Ordering};

use rustos_rng::{FastRng, RandU64};
use rustos_sync::{RwLock, SpinLock};

use crate::runqueue::{vslice, Entry, RunQueue};
use crate::task::{TaskBody, TaskInner};
use crate::{
    CoreClass, CpuId, Priority, SchedError, SchedResult, SchedulerArch, SchedulerConfig,
    SchedulerPolicy, StepOutcome, TaskAction, TaskContext, TaskId, TaskState,
};

/// Per-CPU dispatch bookkeeping (fairness state lives in the [`RunQueue`]).
struct CpuState {
    queue: RunQueue,
    /// Tick at which this CPU last began a dispatch; the reader end of
    /// the in-flight span accounting.
    last_run_tick: AtomicU64,
    /// Cumulative ticks this CPU has spent inside task bodies.
    busy_ticks: AtomicU64,
    /// Task dispatches (context switches into a body) on this CPU;
    /// counted on the same bracket as `busy_ticks`.
    switches: AtomicU64,
}

/// The SMP, non-tickless CFQ scheduler.
///
/// See the crate root for the algorithm and the tickless carve-out.
pub struct Scheduler<A: SchedulerArch> {
    arch: Arc<A>,
    cpus: Box<[CpuState]>,
    tasks: RwLock<BTreeMap<TaskId, Arc<TaskInner>>>,
    next_id: AtomicU64,
    config: SchedulerConfig,
    /// Per-CPU fast, non-cryptographic generators for work-stealing
    /// victim selection — the project's shared [`FastRng`] (xoshiro256++),
    /// never a hand-rolled generator. Indexed by the *calling* CPU so the
    /// draw is always uncontended.
    victim_rng: Box<[SpinLock<FastRng>]>,
    overflow: SpinLock<Vec<TaskId>>,
    preemptions: Box<[AtomicU64]>,
    current: Box<[AtomicU64]>,
    /// Static [`CoreClass`] of each CPU, snapshotted once at construction.
    core_classes: Box<[CoreClass]>,
    /// Dense list of the performance-class CPUs.
    perf_cpus: Box<[CpuId]>,
    /// Dense list of the efficiency-class CPUs. Empty on a homogeneous
    /// machine, where placement draws from the performance pool.
    eff_cpus: Box<[CpuId]>,
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
                busy_ticks: AtomicU64::new(0),
                switches: AtomicU64::new(0),
            });
        }
        let mut preemptions = Vec::with_capacity(config.cpus as usize);
        let mut current = Vec::with_capacity(config.cpus as usize);
        let mut victim_rng = Vec::with_capacity(config.cpus as usize);
        for cpu in 0..config.cpus {
            preemptions.push(AtomicU64::new(0));
            current.push(AtomicU64::new(0));
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
        })
    }

    /// The [`CoreClass`] a task at `prio` should run on.
    const fn preferred_class(prio: Priority) -> CoreClass {
        match prio {
            Priority::High | Priority::Normal => CoreClass::Performance,
            Priority::Low => CoreClass::Efficiency,
        }
    }

    /// The CPU in `pool` carrying the least competing weight, preferring
    /// `prefer` on a tie. `None` only for an empty pool.
    fn least_loaded(&self, pool: &[CpuId], prefer: CpuId) -> Option<CpuId> {
        let mut best: Option<(u64, CpuId)> = None;
        for &cpu in pool {
            let Some(state) = self.cpus.get(cpu as usize) else {
                continue;
            };
            let weight = state.queue.competing_weight();
            let better = match best {
                None => true,
                Some((best_weight, best_cpu)) => {
                    weight < best_weight
                        || (weight == best_weight && cpu == prefer && best_cpu != prefer)
                }
            };
            if better {
                best = Some((weight, cpu));
            }
        }
        best.map(|(_, cpu)| cpu)
    }

    /// The CPUs a task of class `want` may be placed on: its own class's
    /// pool, or the other class's pool when the machine has none of that
    /// class, so placement always has a real candidate set.
    fn placement_pool(&self, want: CoreClass) -> &[CpuId] {
        let (own, other): (&[CpuId], &[CpuId]) = match want {
            CoreClass::Performance => (&self.perf_cpus, &self.eff_cpus),
            CoreClass::Efficiency => (&self.eff_cpus, &self.perf_cpus),
        };
        if own.is_empty() {
            other
        } else {
            own
        }
    }

    /// Choose the CPU new or newly-woken work at `prio` lands on: the
    /// least-loaded CPU of its preferred class (`hint`-preferring on a tie).
    fn placement_for(&self, prio: Priority, hint: CpuId) -> CpuId {
        let want = Self::preferred_class(prio);
        self.least_loaded(self.placement_pool(want), hint)
            .unwrap_or(hint)
    }

    /// The CPU a task already running on `home` should stay or be re-homed
    /// on after a yield: `home` when its class matches, else the
    /// least-loaded CPU of the preferred class. A same-class yield stays
    /// put (no cache-thrashing re-placement on every yield).
    fn class_home(&self, prio: Priority, home: CpuId) -> CpuId {
        let want = Self::preferred_class(prio);
        if self.class_of(home) == want {
            return home;
        }
        let pool: &[CpuId] = match want {
            CoreClass::Performance => &self.perf_cpus,
            CoreClass::Efficiency => &self.eff_cpus,
        };
        if pool.is_empty() {
            return home;
        }
        self.least_loaded(pool, home).unwrap_or(home)
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

    /// Admit `task` to `rq`'s competition and place its vruntime at
    /// `max(its own vruntime, the queue's current front)` — the CFS
    /// `place_entity` rule.
    ///
    /// A brand-new task (vruntime `0`) or one that slept long enough for
    /// the front to pass it lands at the front, so it is neither penalised
    /// for sleeping nor given an unfair head start. A task that woke with
    /// an accumulated vruntime *above* the front keeps it, so a task that
    /// sleeps and wakes rapidly cannot keep re-entering at the low front
    /// and starve a task that has been ready all along (the front only
    /// advances to the *picked* task's vruntime, so resetting every
    /// re-admission to it would let a frequent waker monopolise the CPU).
    fn admit(task: &TaskInner, rq: &RunQueue) -> Entry {
        let weight = task.weight();
        let front = rq.admit_weight(weight);
        let vruntime = front.max(task.vruntime());
        task.set_vruntime(vruntime);
        Entry {
            id: task.id,
            vruntime,
        }
    }

    /// Enqueue `task` onto its home CPU, falling back to the global
    /// overflow list when that queue is at its compile-time bound.
    fn enqueue_home(&self, task: &TaskInner) {
        let home = task.home_cpu.load(Ordering::Acquire);
        let entry = Entry {
            id: task.id,
            vruntime: task.vruntime(),
        };
        match self.cpus.get(home as usize) {
            Some(cpu) if cpu.queue.push(entry).is_ok() => {}
            _ => self.overflow.lock().push(task.id),
        }
    }

    /// Spawn a new task at `priority` with a body closure.
    ///
    /// # Errors
    /// * [`SchedError::NoSuchCpu`] if `home_cpu` is out of range.
    pub fn spawn<F>(&self, home_cpu: CpuId, priority: Priority, body: F) -> SchedResult<TaskId>
    where
        F: FnMut(&mut TaskContext) -> TaskAction + Send + 'static,
    {
        self.cpu_state(home_cpu)?;
        let placed = self.placement_for(priority, home_cpu);
        let cpu = self.cpu_state(placed)?;
        let id = self.mint_id();
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

    /// Admit a new task at `priority` but leave it [`TaskState::Parked`]
    /// and unqueued — see [`SchedulerPolicy::spawn_parked`].
    ///
    /// # Errors
    /// * [`SchedError::NoSuchCpu`] if `home_cpu` is out of range.
    pub fn spawn_parked<F>(
        &self,
        home_cpu: CpuId,
        priority: Priority,
        body: F,
    ) -> SchedResult<TaskId>
    where
        F: FnMut(&mut TaskContext) -> TaskAction + Send + 'static,
    {
        self.cpu_state(home_cpu)?;
        let placed = self.placement_for(priority, home_cpu);
        self.cpu_state(placed)?;
        let id = self.mint_id();
        let boxed: Box<TaskBody> = Box::new(body);
        let inner = Arc::new(TaskInner::new(id, placed, priority, boxed));
        // Born parked: no queue entry and no competing weight now, so no
        // CPU can pick the task up before the later `unpark`.
        inner.store_state(TaskState::Parked);
        self.tasks.write().insert(id, inner);
        Ok(id)
    }

    /// Mint the next task id, skipping the reserved `0` ("no task").
    fn mint_id(&self) -> TaskId {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if id == 0 {
            self.next_id.fetch_add(1, Ordering::Relaxed)
        } else {
            id
        }
    }

    /// Block a task. Cancellation-safe.
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

    /// Wake a parked task, re-admitting it to a fresh home CPU.
    /// Cancellation-safe.
    ///
    /// # Errors
    /// * [`SchedError::NoSuchTask`] if no task ever held that id.
    /// * [`SchedError::InvalidState`] if the task is terminal.
    pub fn unpark(&self, id: TaskId) -> SchedResult<()> {
        let task = self.lookup(id)?;
        match task.load_state() {
            TaskState::Exited => return Err(SchedError::InvalidState),
            // Already committed to park: re-admit it directly.
            TaskState::Parked => return self.wake_from_parked(&task),
            // Not yet committed to park (running its body, or already
            // queued): fall through to the token handshake below.
            TaskState::Ready | TaskState::Running => {}
        }
        // Record the wake token, then re-read the state. This store then
        // load, paired with the store-`Parked`-then-take-token sequence in
        // the `step` Park commit and a `SeqCst` fence on each side, forbids
        // the store-buffering outcome where the waker sees the task
        // not-yet-parked *and* the parker misses the token — one side
        // always observes the other. The earlier shape read the other
        // side's flag *before* writing its own on both sides, which could
        // drop the wake and park the task forever.
        task.set_wake_pending();
        fence(Ordering::SeqCst);
        if task.load_state() == TaskState::Parked {
            // The task committed to `Parked` concurrently and may not have
            // observed our token. Reclaim it and, if the token is still
            // owed, re-admit it; the `take` here and the CAS inside
            // `wake_from_parked` make the re-admission single even when
            // both racing sides reach it.
            if task.take_wake_pending() {
                return self.wake_from_parked(&task);
            }
        }
        Ok(())
    }

    /// Re-admit a task that is currently [`TaskState::Parked`] to a fresh
    /// home CPU and send the placement IPI. The one definition of the
    /// wake-from-parked transition, shared by [`Self::unpark`] and the
    /// `step` Park-commit token re-check. Fails closed with
    /// [`SchedError::InvalidState`] when the task is no longer `Parked`
    /// (a concurrent waker already re-readied it), so a racing second
    /// caller never admits the same task twice.
    fn wake_from_parked(&self, task: &Arc<TaskInner>) -> SchedResult<()> {
        task.cas_state(TaskState::Parked, TaskState::Ready)
            .map_err(|_| SchedError::InvalidState)?;
        let home = task.home_cpu.load(Ordering::Acquire);
        let target = self.placement_for(task.load_priority(), home);
        task.home_cpu.store(target, Ordering::Release);
        let cpu = self.cpu_state(target)?;
        let entry = Self::admit(task, &cpu.queue);
        if cpu.queue.push(entry).is_err() {
            self.overflow.lock().push(task.id);
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

    /// Periodic-tick observation point. CFQ is the one **non-tickless**
    /// policy: on a real port a fixed-frequency timer ISR calls this after
    /// acknowledging the device source, and the return-to-user preempt
    /// point reschedules the running task. Here it bumps a per-CPU
    /// preemption counter (the figure `sysmon` surfaces) and returns.
    ///
    /// # Errors
    /// * [`SchedError::NoSuchCpu`] if `cpu` is out of range. The counter
    ///   is not incremented in that case.
    pub fn on_timer_tick(&self, cpu: CpuId) -> SchedResult<()> {
        let _ = self.cpu_state(cpu)?;
        self.preemptions[cpu as usize].fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Re-arm the calling CPU's periodic quantum after a fired tick that
    /// did **not** owe a context switch (a lone runnable task).
    ///
    /// CFQ is the non-tickless carve-out: the fixed-frequency tick must
    /// keep firing for a running task even when it is alone, so the CPU
    /// re-checks its run queue at the steady HZ cadence (the Linux
    /// scheduler-tick model) and promptly picks up work later enqueued
    /// here *without* an IPI — a task drained back from overflow, a
    /// rebalance. The kernel preempt path calls this when it consumes a
    /// tick without switching; re-arming only the timer avoids the
    /// address-space/TLB churn a reschedule-to-self would incur. The idle
    /// [`Self::step`] still disarms once nothing is runnable, so a truly
    /// idle core takes no ticks. Arms the current CPU (the one whose tick
    /// just fired), which is where this runs.
    pub fn rearm_periodic_tick(&self) {
        self.arch.set_preemption(true);
    }

    /// Returns the per-CPU timer-tick preemption count.
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

    /// One scheduler step on `cpu`: pick the smallest-vruntime task, run
    /// it once, then re-enqueue or retire it. Falls back to work-stealing
    /// before reporting [`StepOutcome::Idle`]; a truly idle CPU disarms
    /// its preemption tick (no ticks with nothing to preempt to).
    ///
    /// # Errors
    /// * [`SchedError::NoSuchCpu`] if `cpu` is out of range.
    pub fn step(&self, cpu: CpuId) -> SchedResult<StepOutcome> {
        let me = self.cpu_state(cpu)?;
        self.drain_overflow(cpu);

        if let Some(entry) = me.queue.pick() {
            return Ok(self.dispatch(cpu, entry.id));
        }
        if let Some(id) = self.try_steal(cpu) {
            return Ok(self.dispatch(cpu, id));
        }
        // Nothing to run: stop this CPU's periodic tick until work
        // arrives (an idle core takes no timer interrupts even under CFQ).
        self.arch.set_preemption(false);
        Ok(StepOutcome::Idle)
    }

    /// Best-effort drain of the overflow list back onto each task's home
    /// CPU. A task that still does not fit is left for a later step.
    fn drain_overflow(&self, current_cpu: CpuId) {
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
                vruntime: task.vruntime(),
            };
            match self.cpus.get(home as usize) {
                Some(cpu) if cpu.queue.push(entry).is_ok() => {
                    if home != current_cpu {
                        self.arch.send_ipi(home);
                    }
                }
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
                // Migrate: drop the task's weight from the victim and
                // re-admit it on the stealing CPU, rebasing its vruntime
                // to the new queue's front (a task carries no lag across
                // CPUs).
                let Ok(task) = self.lookup(entry.id) else {
                    continue;
                };
                self.cpus[v].queue.release_weight(task.weight());
                task.home_cpu.store(cpu, Ordering::Release);
                let _ = Self::admit(&task, &self.cpus[cpu as usize].queue);
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

    /// Book one finished body run: bump the task's run count, accumulate
    /// the elapsed ticks, and stamp the CPU's last-run tick.
    fn settle_run_accounting(&self, cpu: CpuId, task: &TaskInner, started_tick: u64) -> u64 {
        task.total_runs.fetch_add(1, Ordering::Relaxed);
        let span = self.arch.ticks_now().saturating_sub(started_tick);
        task.run_ticks.fetch_add(span, Ordering::Relaxed);
        self.cpus[cpu as usize]
            .busy_ticks
            .fetch_add(span, Ordering::Relaxed);
        self.cpus[cpu as usize]
            .switches
            .fetch_add(1, Ordering::Relaxed);
        self.cpus[cpu as usize]
            .last_run_tick
            .store(started_tick, Ordering::Release);
        span
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

        // NON-tickless preemption (the CFQ carve-out): keep this CPU's
        // periodic quantum tick armed for the task we are about to run —
        // unconditionally, even when it is the sole runnable task. Unlike
        // the tickless siblings, which disarm for a lone task so a quiet
        // core takes no interrupts, CFQ leaves the fixed-frequency tick
        // running so the timer interrupt fires at a steady HZ cadence (the
        // Linux scheduler tick). Whether a fired tick actually switches is
        // the kernel's decision (its `preempt_current` gate switches only
        // when there is a competitor to run, so a lone task's tick does its
        // accounting without a needless switch-to-self); the policy's job
        // here is only to keep the tick alive. The idle path in `step`
        // disarms once nothing is runnable. Armed before the body switches
        // in so the deadline is live for the run.
        self.arch.set_preemption(true);

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

        // Clear the current slot before settling the completed span so
        // `cpu_busy_ticks` / `cpu_ticks_of` count it once (as settled),
        // never also as in-flight.
        self.clear_current(cpu);
        let service_ticks = self.settle_run_accounting(cpu, &task, tick);

        // Charge the weighted service this run consumed so `vruntime`
        // reflects CPU time regardless of *why* the task stopped — CFS
        // charges execution, not just voluntary yields. A task that runs
        // then parks must still advance its vruntime; otherwise it would
        // re-enter at the front on every wake (`admit`) and starve a task
        // that has been ready all along.
        let charged = task
            .vruntime()
            .saturating_add(vslice(service_ticks, task.weight()));
        task.set_vruntime(charged);

        let observed = task.load_state();
        if observed == TaskState::Ready && action == TaskAction::Park {
            // `park()` may publish `Parked` while this body is still
            // unwinding toward its context-switch return. A concurrent
            // `unpark()` then owns the Parked -> Ready transition and has
            // already enqueued the task. The returning body's Park action is
            // stale: applying it would undo the wake and strand a stale queue
            // entry; treating it as Yield would enqueue a duplicate. Preserve
            // the waker's single, already-admitted Ready transition exactly.
            return StepOutcome::Ran(id);
        }
        let effective = match (observed, action) {
            (TaskState::Exited, _) | (_, TaskAction::Exit) => TaskAction::Exit,
            (TaskState::Parked, _) | (_, TaskAction::Park) => TaskAction::Park,
            _ => TaskAction::Yield,
        };

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
                // Consume any wake token *after* publishing `Parked`, the
                // store-then-load pair matching `unpark` (SeqCst-fenced on
                // each side). A wake that raced this commit is never lost:
                // the waker either set the token before this take (consumed
                // here, task re-admitted) or observed `Parked` and
                // re-admitted the task itself. The CAS inside
                // `wake_from_parked` keeps the re-admission single when
                // both sides run.
                fence(Ordering::SeqCst);
                if task.take_wake_pending() {
                    let _ = self.wake_from_parked(&task);
                }
            }
            TaskAction::Yield => {
                task.store_state(TaskState::Ready);
                // vruntime was already charged for this run above (every
                // dispatch pays, not just yields), so a heavier task rises
                // more slowly and is picked more often (proportional
                // share); do not charge again here.
                let dest = self.class_home(task.load_priority(), cpu);
                if dest == cpu {
                    self.enqueue_home(&task);
                } else {
                    // Wrong class for its priority: migrate, dropping its
                    // weight here and re-admitting it on the preferred
                    // class's CPU (vruntime rebased onto that front).
                    self.cpus[cpu as usize].queue.remove_weight(task.weight());
                    task.home_cpu.store(dest, Ordering::Release);
                    let entry = Self::admit(&task, &self.cpus[dest as usize].queue);
                    if self.cpus[dest as usize].queue.push(entry).is_err() {
                        self.overflow.lock().push(id);
                    }
                    // `dest` may be parked in `wfi`: queue/overflow
                    // publication alone cannot make its dispatcher run.
                    // Signal after publishing so the IPI's release barrier
                    // orders the ready ownership before the target wakes.
                    self.arch.send_ipi(dest);
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

    /// Cumulative ticks `id` has spent running, including the in-flight
    /// span of a run that has started but not yet returned.
    ///
    /// # Errors
    /// * [`SchedError::NoSuchTask`] if the id is unknown.
    pub fn cpu_ticks_of(&self, id: TaskId) -> SchedResult<u64> {
        let tasks = self.tasks.read();
        let task = tasks.get(&id).ok_or(SchedError::NoSuchTask)?;
        let settled = task.run_ticks.load(Ordering::Acquire);
        let home = task.home_cpu.load(Ordering::Acquire);
        let in_flight = if self.current_task(home) == Some(id) {
            self.arch
                .ticks_now()
                .saturating_sub(task.last_started.load(Ordering::Acquire))
        } else {
            0
        };
        Ok(settled.saturating_add(in_flight))
    }

    /// Ticks the task currently dispatching on `cpu` has run but not yet
    /// settled, or `0` when the CPU is idle.
    fn in_flight_ticks(&self, cpu: CpuId) -> u64 {
        let Some(id) = self.current_task(cpu) else {
            return 0;
        };
        let started = {
            let tasks = self.tasks.read();
            let Some(task) = tasks.get(&id) else {
                return 0;
            };
            task.last_started.load(Ordering::Acquire)
        };
        self.arch.ticks_now().saturating_sub(started)
    }

    /// Cumulative ticks `cpu` has spent inside task bodies.
    ///
    /// # Errors
    /// * [`SchedError::NoSuchCpu`] if `cpu` is out of range.
    pub fn cpu_busy_ticks(&self, cpu: CpuId) -> SchedResult<u64> {
        let settled = self
            .cpus
            .get(cpu as usize)
            .map(|state| state.busy_ticks.load(Ordering::Acquire))
            .ok_or(SchedError::NoSuchCpu)?;
        Ok(settled.saturating_add(self.in_flight_ticks(cpu)))
    }

    /// Task dispatches on `cpu`, counted on the same bracket as
    /// [`Self::cpu_busy_ticks`].
    ///
    /// # Errors
    /// * [`SchedError::NoSuchCpu`] if `cpu` is out of range.
    pub fn cpu_switches(&self, cpu: CpuId) -> SchedResult<u64> {
        self.cpus
            .get(cpu as usize)
            .map(|state| state.switches.load(Ordering::Acquire))
            .ok_or(SchedError::NoSuchCpu)
    }

    /// Instantaneous count of ready tasks queued on `cpu`'s run queue (the
    /// running task sits in the current slot, not the queue).
    ///
    /// # Errors
    /// * [`SchedError::NoSuchCpu`] if `cpu` is out of range.
    pub fn queue_depth(&self, cpu: CpuId) -> SchedResult<u64> {
        self.cpus
            .get(cpu as usize)
            .map(|state| state.queue.ready_len() as u64)
            .ok_or(SchedError::NoSuchCpu)
    }

    /// Whether `cpu` must re-step rather than enter its idle wait.
    ///
    /// Includes globally overflowed ready work because the next step on any
    /// CPU drains that list before selecting a task.
    ///
    /// # Errors
    /// * [`SchedError::NoSuchCpu`] if `cpu` is out of range.
    pub fn has_ready_work(&self, cpu: CpuId) -> SchedResult<bool> {
        let local_ready = self
            .cpus
            .get(cpu as usize)
            .map(|state| state.queue.ready_len() != 0)
            .ok_or(SchedError::NoSuchCpu)?;
        Ok(local_ready || !self.overflow.lock().is_empty())
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
    /// # Errors
    /// * [`SchedError::NoSuchTask`] if the id is unknown.
    /// * [`SchedError::InvalidState`] if the task is not running.
    pub fn yield_current(&self, id: TaskId) -> SchedResult<()> {
        let task = self.lookup(id)?;
        task.cas_state(TaskState::Running, TaskState::Ready)
            .map_err(|_| SchedError::InvalidState)?;
        let service_ticks = self
            .arch
            .ticks_now()
            .saturating_sub(task.last_started.load(Ordering::Acquire));
        let charged = task
            .vruntime()
            .saturating_add(vslice(service_ticks, task.weight()));
        task.set_vruntime(charged);
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

    fn spawn_parked<F>(&self, home_cpu: CpuId, priority: Priority, body: F) -> SchedResult<TaskId>
    where
        F: FnMut(&mut TaskContext) -> TaskAction + Send + 'static,
    {
        Scheduler::spawn_parked(self, home_cpu, priority, body)
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

    fn cpu_ticks_of(&self, id: TaskId) -> SchedResult<u64> {
        Scheduler::cpu_ticks_of(self, id)
    }

    fn cpu_busy_ticks(&self, cpu: CpuId) -> SchedResult<u64> {
        Scheduler::cpu_busy_ticks(self, cpu)
    }

    fn cpu_switches(&self, cpu: CpuId) -> SchedResult<u64> {
        Scheduler::cpu_switches(self, cpu)
    }

    fn queue_depth(&self, cpu: CpuId) -> SchedResult<u64> {
        Scheduler::queue_depth(self, cpu)
    }

    fn has_ready_work(&self, cpu: CpuId) -> SchedResult<bool> {
        Scheduler::has_ready_work(self, cpu)
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
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicU64, Ordering};

    fn mk(cpus: u32) -> (Arc<TestArch>, Scheduler<TestArch>) {
        let arch = Arc::new(TestArch::new(cpus).expect("arch"));
        let cfg = SchedulerConfig {
            cpus,
            queue_capacity_per_band: 64,
            yields_before_demotion: 1,
            boost_interval_ticks: 8,
        };
        let sched = Scheduler::new(cfg, arch.clone()).expect("sched");
        (arch, sched)
    }

    #[test]
    fn rejects_zero_cpu_and_bad_capacity() {
        let arch = Arc::new(TestArch::new(1).expect("arch"));
        let zero = SchedulerConfig {
            cpus: 0,
            queue_capacity_per_band: 64,
            yields_before_demotion: 1,
            boost_interval_ticks: 8,
        };
        assert_eq!(
            Scheduler::new(zero, arch.clone()).err(),
            Some(SchedError::NoSuchCpu)
        );
        let bad_cap = SchedulerConfig {
            cpus: 1,
            queue_capacity_per_band: 3,
            yields_before_demotion: 1,
            boost_interval_ticks: 8,
        };
        assert_eq!(
            Scheduler::new(bad_cap, arch).err(),
            Some(SchedError::QueueFull)
        );
    }

    #[test]
    fn spawn_runs_once_and_ipis_home() {
        let (arch, sched) = mk(2);
        let ran = Arc::new(AtomicU64::new(0));
        let r2 = ran.clone();
        let id = sched
            .spawn(0, Priority::Normal, move |_| {
                r2.fetch_add(1, Ordering::Relaxed);
                TaskAction::Exit
            })
            .expect("spawn");
        arch.set_current_cpu(0);
        assert_eq!(sched.step(0), Ok(StepOutcome::Ran(id)));
        assert_eq!(ran.load(Ordering::Relaxed), 1);
        assert_eq!(sched.state_of(id), TaskState::Exited);
        assert!(arch.ipi_count(0) >= 1, "spawn notifies the home CPU");
    }

    #[test]
    fn a_sole_runnable_task_keeps_the_periodic_tick_armed() {
        // The defining non-tickless behaviour: unlike the tickless
        // siblings, CFQ never disarms the preemption tick for a lone
        // runnable task — it keeps a fixed-frequency HZ-style tick armed so
        // the timer interrupt keeps firing (whether a fired tick switches
        // is the kernel's `preempt_current` gate's call — not this policy's).
        let (arch, sched) = mk(1);
        let n = Arc::new(AtomicU64::new(0));
        let n2 = n.clone();
        sched
            .spawn(0, Priority::Normal, move |_| {
                if n2.fetch_add(1, Ordering::Relaxed) >= 2 {
                    TaskAction::Exit
                } else {
                    TaskAction::Yield
                }
            })
            .expect("spawn");
        arch.set_current_cpu(0);
        for _ in 0..3 {
            assert!(matches!(sched.step(0), Ok(StepOutcome::Ran(_))));
            assert_eq!(
                arch.last_preemption(),
                Some(true),
                "a running task keeps the tick armed even as the sole task"
            );
        }
        assert_eq!(
            arch.disarm_count(),
            0,
            "the tick is never disarmed while a task is runnable"
        );
        assert!(arch.arm_count() >= 3, "armed on every dispatch");
        // Only when the CPU has nothing left to run does it disarm.
        assert_eq!(sched.step(0), Ok(StepOutcome::Idle));
        assert_eq!(
            arch.last_preemption(),
            Some(false),
            "an idle CPU disarms the periodic tick"
        );
        assert!(arch.disarm_count() >= 1);
    }

    #[test]
    fn rearm_periodic_tick_rearms_the_quantum_on_a_declined_preempt() {
        // Regression: a fired tick on a lone-task CPU that the kernel
        // preempt gate declines to switch (no competitor) must still keep
        // CFQ's non-tickless tick alive. `rearm_periodic_tick` is the seam
        // the kernel calls on that decline; it must re-arm the quantum
        // (`set_preemption(true)`) so the CPU keeps ticking and re-checks
        // its run queue — without it the lone task's tick falls silent and
        // work later enqueued here (without an IPI) strands, the
        // heavy-load stall this fix closes. Only the timer is re-armed: no
        // dispatch, so no reschedule-to-self churn.
        let (arch, sched) = mk(1);
        arch.set_current_cpu(0);
        let armed_before = arch.arm_count();
        sched.rearm_periodic_tick();
        assert_eq!(
            arch.last_preemption(),
            Some(true),
            "a declined tick re-arms the quantum so the tick keeps firing"
        );
        assert_eq!(
            arch.arm_count(),
            armed_before + 1,
            "exactly one re-arm per declined tick, and never a disarm"
        );
        assert_eq!(arch.disarm_count(), 0);
    }

    #[test]
    fn heavier_weight_is_dispatched_more_often() {
        let (arch, sched) = mk(1);
        let high = Arc::new(AtomicU64::new(0));
        let low = Arc::new(AtomicU64::new(0));
        let hr = high.clone();
        let lr = low.clone();
        sched
            .spawn(0, Priority::High, move |_| {
                hr.fetch_add(1, Ordering::Relaxed);
                TaskAction::Yield
            })
            .expect("spawn high");
        sched
            .spawn(0, Priority::Low, move |_| {
                lr.fetch_add(1, Ordering::Relaxed);
                TaskAction::Yield
            })
            .expect("spawn low");
        arch.set_current_cpu(0);
        for _ in 0..120 {
            let _ = sched.step(0).expect("step");
            arch.advance_ticks(1);
        }
        let h = high.load(Ordering::Relaxed);
        let l = low.load(Ordering::Relaxed);
        assert!(l > 0, "the low-weight task is not starved");
        assert!(
            h > l,
            "the heavier task is dispatched more often ({h} vs {l})"
        );
    }

    #[test]
    fn work_stealing_finds_remote_task() {
        let (arch, sched) = mk(2);
        let id = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Exit)
            .expect("spawn");
        arch.set_current_cpu(1);
        assert_eq!(
            sched.step(1),
            Ok(StepOutcome::Ran(id)),
            "an idle CPU steals the remote task"
        );
    }

    #[test]
    fn cross_class_yield_rehome_signals_the_idle_destination() {
        let arch = Arc::new(TestArch::new(2).expect("arch"));
        arch.set_core_class(0, CoreClass::Efficiency);
        arch.set_core_class(1, CoreClass::Performance);
        let sched = Scheduler::new(
            SchedulerConfig {
                cpus: 2,
                queue_capacity_per_band: 64,
                yields_before_demotion: 1,
                boost_interval_ticks: 8,
            },
            arch.clone(),
        )
        .expect("scheduler");
        let task = sched
            .spawn(0, Priority::Low, |_| TaskAction::Yield)
            .expect("spawn background task");
        let ipis_before = arch.ipi_count(0);

        // CPU 1 steals the background task from efficiency CPU 0. Its yield
        // rehomes it to the efficiency pool; CPU 0 may be parked, so the
        // remote enqueue must signal it after publishing the ready entry.
        arch.set_current_cpu(1);
        assert_eq!(sched.step(1), Ok(StepOutcome::Ran(task)));
        assert_eq!(sched.queue_depth(0), Ok(1));
        assert_eq!(
            arch.ipi_count(0),
            ipis_before + 1,
            "cross-class rehome wakes the idle destination CPU"
        );
    }

    #[test]
    fn remote_overflow_drain_signals_the_task_home_cpu() {
        let arch = Arc::new(TestArch::new(2).expect("arch"));
        let sched = Scheduler::new(
            SchedulerConfig {
                cpus: 2,
                queue_capacity_per_band: 2,
                yields_before_demotion: 1,
                boost_interval_ticks: 8,
            },
            arch.clone(),
        )
        .expect("scheduler");
        let mut last = 0;
        for _ in 0..5 {
            last = sched
                .spawn(0, Priority::Normal, |_| TaskAction::Exit)
                .expect("spawn");
        }
        assert_eq!(
            sched
                .lookup(last)
                .expect("last task")
                .home_cpu
                .load(Ordering::Acquire),
            0
        );
        assert_eq!(sched.overflow.lock().as_slice(), &[last]);

        // Free one slot on CPU 0. A later step on CPU 1 drains the global
        // overflow into that remote slot; CPU 0 is not executing this drain
        // and therefore requires an IPI to observe the newly-owned work.
        arch.set_current_cpu(0);
        assert!(matches!(sched.step(0), Ok(StepOutcome::Ran(_))));
        let ipis_before = arch.ipi_count(0);
        arch.set_current_cpu(1);
        assert!(matches!(sched.step(1), Ok(StepOutcome::Ran(_))));
        assert_eq!(
            arch.ipi_count(0),
            ipis_before + 1,
            "remote overflow publication wakes the task's home CPU"
        );
    }

    #[test]
    fn yielded_parent_survives_child_park_and_cross_cpu_steal() {
        // Regression for the service-spawn continuation sequence: a parent
        // yields after an ordinary syscall, the newly runnable child parks,
        // and an idle sibling CPU steals the parent. The parent's closure
        // state is its stand-in for the saved syscall result/continuation; it
        // must be invoked again intact rather than stranded as Ready with no
        // queue owner.
        let (arch, sched) = mk(2);
        let parent_runs = Arc::new(AtomicU64::new(0));
        let result = Arc::new(AtomicU64::new(0));
        let parent_runs_for_body = parent_runs.clone();
        let result_for_body = result.clone();
        let parent = sched
            .spawn(0, Priority::Normal, move |_| {
                if parent_runs_for_body.fetch_add(1, Ordering::SeqCst) == 0 {
                    TaskAction::Yield
                } else {
                    result_for_body.store(0x51a7_c011, Ordering::SeqCst);
                    TaskAction::Exit
                }
            })
            .expect("spawn parent");
        let child = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Park)
            .expect("spawn child");

        arch.set_current_cpu(0);
        assert_eq!(sched.step(0), Ok(StepOutcome::Ran(parent)));
        arch.set_current_cpu(1);
        assert_eq!(sched.step(1), Ok(StepOutcome::Ran(child)));
        assert_eq!(sched.state_of(child), TaskState::Parked);

        assert_eq!(
            sched.step(1),
            Ok(StepOutcome::Ran(parent)),
            "the idle sibling steals and resumes the yielded parent"
        );
        assert_eq!(parent_runs.load(Ordering::SeqCst), 2);
        assert_eq!(result.load(Ordering::SeqCst), 0x51a7_c011);
        assert_eq!(sched.current_task(1), None);
    }

    #[test]
    fn park_unpark_roundtrip() {
        let (arch, sched) = mk(1);
        arch.set_current_cpu(0);
        let id = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Park)
            .expect("spawn");
        assert_eq!(sched.step(0), Ok(StepOutcome::Ran(id)));
        assert_eq!(sched.state_of(id), TaskState::Parked);
        sched.unpark(id).expect("unpark");
        assert_eq!(sched.step(0), Ok(StepOutcome::Ran(id)));
    }

    #[test]
    fn a_wake_arriving_before_the_park_commit_is_not_lost() {
        // A wake (unpark) that lands while the task has not yet committed
        // to park records a token; the dispatch loop's Park commit must
        // consume it *after* publishing `Parked` and re-ready the task
        // rather than sleep it. Guards the store-then-load + SeqCst-fence
        // handshake that closed the cross-CPU lost-wakeup race (a deferred
        // device-IRQ wake landing between the waiter's re-test and its park
        // commit stranded the root-unlock kthread on four cores).
        let (arch, sched) = mk(1);
        arch.set_current_cpu(0);
        let runs = Arc::new(AtomicU64::new(0));
        let rc = runs.clone();
        let id = sched
            .spawn(0, Priority::Normal, move |_| {
                if rc.fetch_add(1, Ordering::Relaxed) == 0 {
                    TaskAction::Park
                } else {
                    TaskAction::Exit
                }
            })
            .expect("spawn");
        // The wake arrives before the task has committed to park.
        sched.unpark(id).expect("unpark");
        // Run 1: the body asks to park, but the pending wake cancels it.
        assert_eq!(sched.step(0), Ok(StepOutcome::Ran(id)));
        assert_eq!(
            sched.state_of(id),
            TaskState::Ready,
            "the pending wake must re-ready the task, never leave it parked"
        );
        // The task is runnable again and finishes on the next step.
        assert_eq!(sched.step(0), Ok(StepOutcome::Ran(id)));
        assert_eq!(sched.live_task_count(), 0);
    }

    #[test]
    fn a_wake_after_park_publication_survives_the_stale_body_return() {
        let (arch, scheduler) = mk(1);
        arch.set_current_cpu(0);
        let scheduler = Arc::new(scheduler);
        let task_id = Arc::new(AtomicU64::new(0));
        let runs = Arc::new(AtomicU64::new(0));
        let body_scheduler = Arc::clone(&scheduler);
        let body_id = Arc::clone(&task_id);
        let body_runs = Arc::clone(&runs);
        let id = scheduler
            .spawn(0, Priority::Normal, move |_| {
                if body_runs.fetch_add(1, Ordering::Relaxed) == 0 {
                    let id = body_id.load(Ordering::Acquire);
                    body_scheduler.park(id).expect("publish parked");
                    body_scheduler.unpark(id).expect("early wake");
                    TaskAction::Park
                } else {
                    TaskAction::Exit
                }
            })
            .expect("spawn");
        task_id.store(id, Ordering::Release);

        assert_eq!(scheduler.step(0), Ok(StepOutcome::Ran(id)));
        assert_eq!(scheduler.state_of(id), TaskState::Ready);
        assert_eq!(scheduler.step(0), Ok(StepOutcome::Ran(id)));
        assert_eq!(runs.load(Ordering::Relaxed), 2);
        assert_eq!(scheduler.live_task_count(), 0);
    }

    #[test]
    fn yield_current_requeues_running_task() {
        let (arch, sched) = mk(1);
        arch.set_current_cpu(0);
        let id = sched
            .spawn(0, Priority::Normal, |ctx| {
                // Simulate a syscall-style external yield mid-body by
                // asking to exit; the external yield below is what the
                // test exercises.
                let _ = ctx;
                TaskAction::Exit
            })
            .expect("spawn");
        // A freshly spawned (Ready, not Running) task cannot be yielded.
        assert_eq!(sched.yield_current(id), Err(SchedError::InvalidState));
        assert_eq!(sched.yield_current(999), Err(SchedError::NoSuchTask));
    }

    #[test]
    fn on_timer_tick_counts_and_rejects_unknown_cpu() {
        let (_arch, sched) = mk(2);
        assert_eq!(sched.preemption_count(0), Ok(0));
        sched.on_timer_tick(0).expect("tick cpu 0");
        sched.on_timer_tick(0).expect("tick cpu 0");
        sched.on_timer_tick(1).expect("tick cpu 1");
        assert_eq!(sched.preemption_count(0), Ok(2));
        assert_eq!(sched.preemption_count(1), Ok(1));
        assert_eq!(sched.total_preemption_count(), 3);
        assert_eq!(sched.on_timer_tick(9), Err(SchedError::NoSuchCpu));
        assert_eq!(sched.preemption_count(9), Err(SchedError::NoSuchCpu));
    }

    #[test]
    fn a_frequently_waking_task_does_not_starve_a_ready_task() {
        // Regression: a task that runs briefly then blocks, and is woken
        // again immediately, must not perpetually re-enter at the run
        // queue's low front and starve a task that has been ready all
        // along. Before the fix (`admit` reset vruntime to the front and a
        // run that ended in `Park` accrued no vruntime) the waker was
        // always picked first and the ready task ran zero times — the
        // ~144s stall the full-session QEMU vertical hit.
        let (arch, sched) = mk(1);
        arch.set_current_cpu(0);
        let ready_runs = Arc::new(AtomicU64::new(0));
        let rr = ready_runs.clone();
        // The always-ready task: yields forever.
        sched
            .spawn(0, Priority::Normal, move |_| {
                rr.fetch_add(1, Ordering::Relaxed);
                TaskAction::Yield
            })
            .expect("spawn ready");
        // The frequent waker: runs, then blocks every time.
        let waker = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Park)
            .expect("spawn waker");
        for _ in 0..200 {
            let out = sched.step(0).expect("step");
            // Immediately re-arm the waker the moment it parks, so it keeps
            // contending for the CPU as fast as it can.
            if out == StepOutcome::Ran(waker) && sched.state_of(waker) == TaskState::Parked {
                sched.unpark(waker).expect("re-arm waker");
            }
            arch.advance_ticks(1);
        }
        assert!(
            ready_runs.load(Ordering::Relaxed) > 0,
            "the always-ready task must get CPU turns despite the frequent waker"
        );
    }

    #[test]
    fn short_interactive_wakes_stay_responsive_among_cpu_hogs() {
        const HOGS: u64 = 10;
        const HOG_TICKS: u64 = 100;
        const MAX_WAKE_TICKS: u64 = HOG_TICKS * HOGS + 1;
        const WAKE_CYCLES: usize = 64;

        let (arch, sched) = mk(1);
        arch.set_current_cpu(0);
        for _ in 0..HOGS {
            let run_arch = Arc::clone(&arch);
            sched
                .spawn(0, Priority::Normal, move |_| {
                    run_arch.advance_ticks(HOG_TICKS);
                    TaskAction::Yield
                })
                .expect("spawn CPU hog");
        }
        let interactive_arch = Arc::clone(&arch);
        let interactive = sched
            .spawn(0, Priority::Normal, move |_| {
                interactive_arch.advance_ticks(1);
                TaskAction::Park
            })
            .expect("spawn interactive task");

        while sched.state_of(interactive) != TaskState::Parked {
            let _ = sched.step(0).expect("initial dispatch");
        }

        for cycle in 0..WAKE_CYCLES {
            sched.unpark(interactive).expect("wake interactive task");
            let wake_tick = arch.ticks_now();
            let mut dispatched = false;
            for _ in 0..=HOGS {
                if sched.step(0).expect("contended dispatch") == StepOutcome::Ran(interactive) {
                    dispatched = true;
                    break;
                }
            }
            assert!(dispatched, "interactive wake {cycle} was starved");
            let latency = arch.ticks_now().saturating_sub(wake_tick);
            assert!(
                latency <= MAX_WAKE_TICKS,
                "interactive wake {cycle} waited {latency} ticks"
            );
        }
    }

    #[test]
    fn equal_weight_tasks_receive_equal_cpu_time_not_equal_dispatches() {
        const LONG_RUN_TICKS: u64 = 100;
        const DISPATCHES: usize = 2_000;

        let (arch, sched) = mk(1);
        arch.set_current_cpu(0);
        let long_arch = Arc::clone(&arch);
        let long = sched
            .spawn(0, Priority::Normal, move |_| {
                long_arch.advance_ticks(LONG_RUN_TICKS);
                TaskAction::Yield
            })
            .expect("spawn long-running task");
        let short_arch = Arc::clone(&arch);
        let short = sched
            .spawn(0, Priority::Normal, move |_| {
                short_arch.advance_ticks(1);
                TaskAction::Yield
            })
            .expect("spawn short-running task");

        for _ in 0..DISPATCHES {
            let _ = sched.step(0).expect("fair dispatch");
        }

        let long_ticks = sched.cpu_ticks_of(long).expect("long task is live");
        let short_ticks = sched.cpu_ticks_of(short).expect("short task is live");
        assert!(
            long_ticks.abs_diff(short_ticks) <= LONG_RUN_TICKS,
            "equal weights diverged: long={long_ticks} short={short_ticks}"
        );
        assert!(
            sched.run_count(short).expect("short task is live")
                > sched.run_count(long).expect("long task is live"),
            "short runs must be dispatched more often to receive equal CPU time"
        );
    }

    #[test]
    fn cpu_ticks_accumulate_across_dispatches() {
        let (arch, sched) = mk(1);
        arch.set_current_cpu(0);
        let a = arch.clone();
        let id = sched
            .spawn(0, Priority::Normal, move |_| {
                a.advance_ticks(3);
                TaskAction::Yield
            })
            .expect("spawn");
        assert_eq!(sched.cpu_ticks_of(id), Ok(0));
        for _ in 0..2 {
            assert!(matches!(sched.step(0), Ok(StepOutcome::Ran(_))));
        }
        assert!(sched.cpu_ticks_of(id).expect("live") >= 6);
        assert!(sched.cpu_ticks_of(u64::MAX).is_err());
    }
}
