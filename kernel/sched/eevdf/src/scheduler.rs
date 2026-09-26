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

use tairix_kernel_sched_api::park::{self, Settled};
use tairix_kernel_sched_api::share::{vslice, Competition, Counted};
use tairix_kernel_sched_api::{ParkableTask, StealScan};
use tairix_sync::{RwLock, SpinLock};

use crate::runqueue::{Entry, RunQueue};
use crate::task::{TaskBody, TaskInner};
use crate::{
    choose_task_id, CoreClass, CpuId, ExitDisposition, Priority, SchedClass, SchedError,
    SchedResult, SchedulerArch, SchedulerConfig, SchedulerPolicy, StepOutcome, TaskAction,
    TaskContext, TaskId, TaskState,
};

/// Per-CPU scheduler state: the virtual-time run queue plus the timer
/// observation counter and last-run bookkeeping.
struct CpuState {
    queue: RunQueue,
    last_run_tick: AtomicU64,
    /// Cumulative ticks this CPU has spent inside task bodies, accumulated
    /// on the same dispatch bracket that credits the task; the busy half of
    /// the System Information busy/idle utilisation split.
    busy_ticks: AtomicU64,
    /// Task dispatches (context switches into a task body) on this CPU,
    /// counted on the same bracket as `busy_ticks`; the System Information
    /// per-CPU switch counter.
    switches: AtomicU64,
}

/// The per-CPU competing weights as a task's ledger changes them: the total
/// placement balances, and the time-shared share that paces each CPU's `V`.
struct Cpus<'a>(&'a [CpuState]);

impl Competition for Cpus<'_> {
    fn add(&self, counted: Counted) {
        if let Some(cpu) = self.0.get(counted.cpu as usize) {
            cpu.queue.add_weight(counted.weight, counted.class);
        }
    }

    fn remove(&self, counted: Counted) {
        if let Some(cpu) = self.0.get(counted.cpu as usize) {
            cpu.queue.remove_weight(counted.weight, counted.class);
        }
    }
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
    config: SchedulerConfig,
    /// Where each CPU begins its work-stealing scan. The shared per-CPU
    /// generator, not a private one: a scan rotation is not policy.
    victim_scan: StealScan,
    overflow: SpinLock<Vec<TaskId>>,
    preemptions: Box<[AtomicU64]>,
    current: Box<[AtomicU64]>,
    /// Static [`CoreClass`] of each CPU, snapshotted once at construction
    /// from the arch HAL ([`SchedulerArch::core_class`]).
    core_classes: Box<[CoreClass]>,
    /// Dense list of the performance-class CPUs.
    perf_cpus: Box<[CpuId]>,
    /// Dense list of the efficiency-class CPUs. Empty on a homogeneous
    /// machine, where placement then draws from the performance pool
    /// (every CPU).
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
        for _ in 0..config.cpus {
            preemptions.push(AtomicU64::new(0));
            current.push(AtomicU64::new(0));
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
            config,
            victim_scan: StealScan::new(config.cpus),
            overflow: SpinLock::new(Vec::new()),
            preemptions: preemptions.into_boxed_slice(),
            current: current.into_boxed_slice(),
            core_classes: core_classes.into_boxed_slice(),
            perf_cpus: perf_cpus.into_boxed_slice(),
            eff_cpus: eff_cpus.into_boxed_slice(),
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

    /// The CPU in `pool` carrying the least competing weight, preferring
    /// `prefer` on a tie so an equally-loaded hint keeps its locality.
    /// `None` only for an empty pool.
    ///
    /// Each admission adds the placed task's weight to its queue, so a
    /// burst of placements spreads across equally-idle CPUs instead of
    /// herding onto one. The scan is O(pool) uncontended lock reads —
    /// placement runs per spawn/wake, never per dispatch.
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
    /// pool, or — when the machine has no CPU of that class — the other
    /// class's pool, so placement always has a real candidate set.
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
    /// least-loaded CPU of its preferred class (`hint`-preferring on a
    /// tie). An idle CPU competes with weight `0`, so it is chosen ahead
    /// of a busy hint and the follow-up IPI wakes it from its idle park
    /// — work spreads to sleeping cores instead of piling onto the
    /// spawning CPU while they sleep.
    fn placement_for(&self, prio: Priority, hint: CpuId) -> CpuId {
        let want = Self::preferred_class(prio);
        self.least_loaded(self.placement_pool(want), hint)
            .unwrap_or(hint)
    }

    /// The CPU a task **already running** on `home` should stay or be
    /// re-homed on after a yield: `home` itself when its class matches
    /// the task's priority, else the least-loaded CPU of the preferred
    /// class (falling back to `home` on a machine without one).
    ///
    /// Deliberately *not* [`Self::placement_for`]: a same-class yield
    /// must stay put — re-placing on every yield would migrate a task
    /// each time another CPU dipped below its home's load, thrashing
    /// caches for no fairness gain. Only a class mismatch (e.g. a `Low`
    /// task work-stealing parked on a performance core) migrates.
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

    fn competition(&self) -> Cpus<'_> {
        Cpus(&self.cpus)
    }

    /// Count `task` on `cpu` at its present priority and class, unless it has
    /// left the competition since the caller chose to; reports which.
    fn count_on(&self, task: &TaskInner, cpu: CpuId) -> bool {
        task.ledger.count(
            Counted::of(cpu, task.load_priority(), task.load_sched_class()),
            || task.is_runnable(),
            &self.competition(),
        )
    }

    /// The virtual length of one request for a task of `weight`: one quantum
    /// of the calling CPU's service, which is what preempts a task that runs
    /// on. An uncalibrated quantum is the smallest request, a single tick.
    fn request(&self, weight: u64) -> u64 {
        vslice(self.arch.quantum_ticks(), weight)
    }

    /// Place `task` on `rq`'s clock with zero lag: eligible at its virtual
    /// time `V`, deadline one request later — the EEVDF admission rule for a
    /// task joining a CPU's competition, whether new, woken or migrated.
    fn admit(&self, task: &TaskInner, rq: &RunQueue) -> Entry {
        let eligible = rq.virtual_time();
        let deadline = eligible.saturating_add(self.request(task.weight()));
        task.set_virtual(eligible, deadline);
        Entry {
            id: task.id,
            eligible,
            deadline,
        }
    }

    /// Charge `task` the `ticks` it just ran on its own clock: its eligible
    /// time advances by the weighted service, and once that fulfils its
    /// request the deadline moves a whole request on. A task that ran short
    /// keeps its deadline, since the rest of its request is still owed.
    fn charge(&self, task: &TaskInner, ticks: u64) {
        let weight = task.weight();
        let eligible = task.eligible().saturating_add(vslice(ticks, weight));
        let deadline = if eligible >= task.deadline() {
            eligible.saturating_add(self.request(weight))
        } else {
            task.deadline()
        };
        task.set_virtual(eligible, deadline);
    }

    /// Enqueue `task` onto its home CPU, falling back to the global
    /// overflow list when that queue is at its compile-time bound. The task
    /// stays in this CPU's competition, so its count is only re-taken, to
    /// pick up a priority or class it adopted while it ran.
    fn enqueue_home(&self, task: &TaskInner) {
        let home = task.home_cpu.load(Ordering::Acquire);
        if !self.count_on(task, home) {
            return;
        }
        let full = match self.cpus.get(home as usize) {
            Some(cpu) => {
                if task.load_sched_class().is_realtime() {
                    cpu.queue.push_rt(task.id).is_err()
                } else {
                    let entry = Entry {
                        id: task.id,
                        eligible: task.eligible(),
                        deadline: task.deadline(),
                    };
                    cpu.queue.push(entry).is_err()
                }
            }
            None => true,
        };
        if full {
            self.overflow.lock().push(task.id);
        }
    }

    /// Admit `task` onto `cpu`'s queue in its scheduling class, counting its
    /// weight there and routing to the overflow list if the band is full. For
    /// a task newly joining this CPU — a spawn, a wake from parked or a
    /// cross-CPU yield migration, whose ledger moves its weight off the CPU it
    /// left. A real-time task joins the strict-priority band (weight counted,
    /// no virtual-time placement); a time-shared task is EEVDF-admitted to the
    /// fair set.
    fn admit_fresh_on(&self, task: &TaskInner, cpu: CpuId) {
        // A task parked or killed again since it was made runnable is left to
        // whoever makes it runnable next.
        if !self.count_on(task, cpu) {
            return;
        }
        let full = match self.cpus.get(cpu as usize) {
            Some(state) => {
                if task.load_sched_class().is_realtime() {
                    state.queue.push_rt(task.id).is_err()
                } else {
                    let entry = self.admit(task, &state.queue);
                    state.queue.push(entry).is_err()
                }
            }
            None => true,
        };
        if full {
            self.overflow.lock().push(task.id);
        }
    }

    /// Spawn a new task at `priority` with a body closure.
    ///
    /// `home_cpu` is the caller's hint. The task lands on the
    /// least-loaded CPU of the [`CoreClass`] its priority calls for —
    /// a `Low` background task on an efficiency core, interactive work
    /// on a performance core — with the hint preferred on an equal-load
    /// tie, so an idle core is put to work ahead of an already-busy
    /// hint.
    ///
    /// # Errors
    /// * [`SchedError::NoSuchCpu`] if `home_cpu` is out of range.
    pub fn spawn<F>(&self, home_cpu: CpuId, priority: Priority, body: F) -> SchedResult<TaskId>
    where
        F: FnMut(&mut TaskContext) -> TaskAction + Send + 'static,
    {
        self.cpu_state(home_cpu)?;
        let placed = self.placement_for(priority, home_cpu);
        self.cpu_state(placed)?;
        let boxed: Box<TaskBody> = Box::new(body);
        // The id is chosen and registered under one write lock, so a
        // concurrent admission cannot take it in between; the queue work
        // below then runs with the lock released, preserving the
        // tasks-before-queue order every other path takes.
        let inner = {
            let mut tasks = self.tasks.write();
            let id = choose_task_id(None, |c| tasks.contains_key(&c))?;
            let inner = Arc::new(TaskInner::new(id, placed, priority, boxed));
            tasks.insert(id, Arc::clone(&inner));
            inner
        };
        let id = inner.id;
        self.admit_fresh_on(&inner, placed);
        self.arch.send_ipi(placed);
        Ok(id)
    }

    /// Admit a new task at `priority` but leave it [`TaskState::Parked`]
    /// and unqueued — see [`SchedulerPolicy::spawn_parked`].
    ///
    /// The task is registered with a minted id, homed exactly where
    /// [`spawn`](Self::spawn) would place it, and left parked with **no**
    /// run-queue entry and **no** wake IPI. It becomes runnable only
    /// through a later [`unpark`](Self::unpark), which computes its
    /// placement, admits its weight, and sends the IPI — so no CPU can
    /// dispatch the task before the caller finishes installing its
    /// per-task state under the returned id.
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
        self.register_parked(None, home_cpu, priority, body)
    }

    /// Admit a parked task at the reserved id `id` — see
    /// [`SchedulerPolicy::spawn_parked_as`].
    ///
    /// # Errors
    /// * [`SchedError::NoSuchCpu`] if `home_cpu` is out of range.
    /// * [`SchedError::TaskIdInUse`] if a live task already holds `id`.
    /// * [`SchedError::NoTaskIdAvailable`] if `id` names no task.
    pub fn spawn_parked_as<F>(
        &self,
        id: TaskId,
        home_cpu: CpuId,
        priority: Priority,
        body: F,
    ) -> SchedResult<TaskId>
    where
        F: FnMut(&mut TaskContext) -> TaskAction + Send + 'static,
    {
        self.register_parked(Some(id), home_cpu, priority, body)
    }

    /// Register a parked task under a drawn id (`requested` is `None`) or the
    /// caller's reserved one, so both birth forms share one registration.
    fn register_parked<F>(
        &self,
        requested: Option<TaskId>,
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
        let boxed: Box<TaskBody> = Box::new(body);
        let mut tasks = self.tasks.write();
        let id = choose_task_id(requested, |c| tasks.contains_key(&c))?;
        let inner = Arc::new(TaskInner::new(id, placed, priority, boxed));
        // Born parked: no queue entry and no competing weight are admitted
        // now, so no CPU can pick the task up. The later `unpark` performs
        // the placement, weight admission, enqueue, and IPI.
        inner.store_state(TaskState::Parked);
        tasks.insert(id, inner);
        Ok(id)
    }

    /// Block a task. Cancellation-safe.
    ///
    /// The transition out of the ready set and the removal of the task's
    /// weight happen together under its ledger, so a stale run-queue entry
    /// discovered later by [`Self::step`] is simply discarded, never counted
    /// twice.
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
                    self.release_current_slot(&task, id);
                    return Ok(());
                }
                cur @ (TaskState::Ready | TaskState::Running) => {
                    let parked = task.ledger.depart(
                        || task.cas_state(cur, TaskState::Parked).is_ok(),
                        &self.competition(),
                    );
                    if parked {
                        self.release_current_slot(&task, id);
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Release `id`'s per-CPU current-task slot, but only once no CPU is
    /// executing its body.
    ///
    /// That slot is the identity every syscall from the task is attributed
    /// through, so clearing it while the task still runs in user mode would
    /// leave its next trap unattributable. Holding the body lock across the
    /// clear proves no dispatch owns the task; failing to take it means one
    /// does, and that CPU clears its own slot when the body returns — the
    /// IPI only makes it prompt, so a victim alone on a quiet core is not
    /// left running until its next tick.
    fn release_current_slot(&self, task: &Arc<TaskInner>, id: TaskId) {
        if let Some(_dispatch_owns_nothing) = task.body.try_lock() {
            self.clear_current_matching(id);
        } else if let Some(cpu) = self.running_cpu_of(id) {
            self.arch.send_ipi(cpu);
        }
    }

    /// Wake a parked task, re-admitting it to its home CPU. Cancellation-safe.
    ///
    /// # Errors
    /// * [`SchedError::NoSuchTask`] if no task ever held that id.
    /// * [`SchedError::InvalidState`] if the task is terminal. A wake another
    ///   waker already satisfied is `Ok`, not an error: the task is runnable,
    ///   which is what the wake asked for.
    pub fn unpark(&self, id: TaskId) -> SchedResult<()> {
        let task = self.lookup(id)?;
        park::unpark_task(&*task, |woken| self.admit_woken(woken))
    }

    /// Place a woken task and send its placement IPI.
    ///
    /// A wake is the moment a previously-idle task needs a CPU, so it is
    /// re-placed on the least-loaded CPU of its priority's class rather than
    /// queued behind its old home's backlog. An out-of-range placement falls
    /// to the overflow list, so a woken task is never left
    /// runnable-but-unqueued.
    fn admit_woken(&self, task: &TaskInner) {
        let home = task.home_cpu.load(Ordering::Acquire);
        let target = self.placement_for(task.load_priority(), home);
        task.home_cpu.store(target, Ordering::Release);
        self.admit_fresh_on(task, target);
        self.arch.send_ipi(target);
    }

    /// Terminate a task. Cancellation-safe and idempotent.
    ///
    /// Returns an [`ExitDisposition`] describing the SMP ownership handoff:
    /// a task that is still executing on another CPU is **never** reported
    /// as reclaimable, because reclaiming its resources while its own code
    /// still runs turns a legitimate access into a wild fault. See
    /// [`SchedulerPolicy::exit`].
    ///
    /// # Errors
    /// * [`SchedError::NoSuchTask`] if the id was never spawned.
    pub fn exit(&self, id: TaskId) -> SchedResult<ExitDisposition> {
        let task = self.lookup(id)?;
        // First termination request wins and owns the teardown; any repeat
        // owes nothing, so reclaim runs exactly once even under a burst of
        // kills against the same task.
        if !park::doom(&task.doomed) {
            // The repeat owns no teardown, but an escalation must still be
            // able to escalate: re-nudge a victim that is *still executing*
            // so it reaches its stopping point now rather than running on
            // until its next quantum. Silently issuing nothing is what makes
            // the `Kill` a grace window escalates to a no-op.
            self.nudge_if_executing(id);
            return Ok(ExitDisposition::AlreadyExited);
        }
        // Already terminal (a self-exit, or a prior dispatch retired it):
        // no teardown is owed to this caller.
        if task.load_state() == TaskState::Exited {
            return Ok(ExitDisposition::AlreadyExited);
        }
        // The body lock is held for the entire time a dispatch executes
        // this task — its user-mode run and any syscall handler nested
        // inside that run. Acquiring it here therefore *proves* no CPU is
        // currently executing the task, so we may retire it now and let the
        // caller reclaim: the task can take no further fault. Drop the guard
        // (end of this block) before touching the registry, so no lock-guard
        // temporary outlives the `task` handle.
        {
            let Some(mut body) = task.body.try_lock() else {
                // A dispatch owns the body and, by `doom`'s pairing, reads the
                // mark when it returns and retires the task itself.
                park::nudge_doomed(&*self.arch, self.running_cpu_of(id), self.cpu_count());
                return Ok(ExitDisposition::Deferred);
            };
            *body = None;
            task.ledger.depart(
                || {
                    task.store_state(TaskState::Exited);
                    true
                },
                &self.competition(),
            );
        }
        // Leave the now-`Exited` registry entry in place (a repeat `exit`
        // finds it `AlreadyExited` via the `doomed` mark, so teardown is
        // idempotent). Any stale run-queue entry is dropped when a later
        // `step` picks it and `dispatch` observes `Exited`.
        self.clear_current_matching(id);
        Ok(ExitDisposition::Quiesced)
    }

    /// Preempt the CPU running `id` so a victim that was told to die reaches
    /// its stopping point now instead of at its next quantum. To the calling
    /// CPU the IPI is a documented no-op.
    fn nudge_running_cpu(&self, id: TaskId) {
        if let Some(cpu) = self.running_cpu_of(id) {
            self.arch.send_ipi(cpu);
        }
    }

    /// Nudge `id` only if a dispatch still owns its body. Used by a repeat
    /// termination request, which owes no teardown but must not leave a
    /// still-running victim un-nudged.
    fn nudge_if_executing(&self, id: TaskId) {
        if let Ok(task) = self.lookup(id) {
            if task.body.try_lock().is_none() {
                self.nudge_running_cpu(id);
            }
        }
    }

    /// The CPU whose current-task slot equals `id`, or `None` when the task
    /// is not the current task on any CPU. A non-clearing scan (unlike
    /// [`Self::clear_current_matching`]): the exit path uses it only to
    /// direct a preemption IPI at a still-running victim.
    fn running_cpu_of(&self, id: TaskId) -> Option<CpuId> {
        self.current.iter().enumerate().find_map(|(cpu, slot)| {
            if slot.load(Ordering::Acquire) == id {
                #[allow(clippy::cast_possible_truncation)]
                // The slot index is bounded by the configured CPU count.
                Some(cpu as CpuId)
            } else {
                None
            }
        })
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

    /// Keep-alive hook the kernel preempt path calls after a fired tick
    /// that did not owe a context switch. EEVDF is **fully tickless**: a
    /// lone runnable task has no quantum armed and its core takes no
    /// timer interrupts, so there is no periodic tick to re-arm — this is
    /// a deliberate no-op. Only the non-tickless CFQ sibling re-arms here.
    #[allow(clippy::unused_self)]
    pub fn rearm_periodic_tick(&self) {}

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
        self.drain_overflow(cpu);

        if let Some((id, band)) = me.queue.pick() {
            return Ok(self.dispatch(cpu, id, band));
        }
        if let Some((id, band)) = self.try_steal(cpu) {
            return Ok(self.dispatch(cpu, id, band));
        }
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
            // Re-home in the task's scheduling class. Its weight is still
            // counted on `home`, so only the band placement is restored.
            let placed = match self.cpus.get(home as usize) {
                Some(cpu) => {
                    if task.load_sched_class().is_realtime() {
                        cpu.queue.push_rt(id).is_ok()
                    } else {
                        let entry = Entry {
                            id,
                            eligible: task.eligible(),
                            deadline: task.deadline(),
                        };
                        cpu.queue.push(entry).is_ok()
                    }
                }
                None => false,
            };
            if placed {
                // `home` may be idle, and a queue entry alone cannot wake it.
                if home != current_cpu {
                    self.arch.send_ipi(home);
                }
            } else {
                self.overflow.lock().push(id);
            }
        }
    }

    fn try_steal(&self, cpu: CpuId) -> Option<(TaskId, SchedClass)> {
        let n = self.cpus.len();
        if n <= 1 {
            return None;
        }
        let start = self.victim_scan.start(cpu, n);
        for offset in 0..n {
            let v = (start + offset) % n;
            if v == cpu as usize {
                continue;
            }
            if let Some((id, band)) = self.cpus[v].queue.steal() {
                let Ok(task) = self.lookup(id) else {
                    continue;
                };
                // Migrate: the ledger moves the weight off the victim. An entry
                // whose task is no longer ready is stale, so keep looking.
                let moved = task.ledger.count(
                    Counted::of(cpu, task.load_priority(), task.load_sched_class()),
                    || task.load_state() == TaskState::Ready,
                    &self.competition(),
                );
                if !moved {
                    continue;
                }
                task.home_cpu.store(cpu, Ordering::Release);
                // The caller dispatches it at once, so it is not pushed: a fair
                // task only rebases onto this CPU's clock, since a task carries
                // no lag across CPUs.
                if !band.is_realtime() {
                    let _ = self.admit(&task, &self.cpus[cpu as usize].queue);
                }
                return Some((id, band));
            }
        }
        None
    }

    /// Book one finished body run: bump the task's run count, accumulate
    /// the elapsed ticks (the span between the two tick reads is exactly
    /// the time the body held this CPU — raw ticks, so the hot path pays a
    /// subtraction, never a unit conversion; the reader converts), and
    /// stamp the CPU's last-run tick. Returns the span, the service the run
    /// is charged.
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

    /// Run `id`, picked from `band` on this CPU or stolen from another's, for
    /// one body invocation, and settle what its return owes.
    fn dispatch(&self, cpu: CpuId, id: TaskId, band: SchedClass) -> StepOutcome {
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

        // The body has returned: clear the current slot *before* settling
        // the run into `busy_ticks`/`run_ticks`. `cpu_busy_ticks` /
        // `cpu_ticks_of` add the in-flight span of whatever the current
        // slot points at, so clearing first means the span this dispatch
        // just completed is counted by `settle_run_accounting` and never
        // also as in-flight — one span, one place.
        self.clear_current(cpu);
        let ran = self.settle_run_accounting(cpu, &task, tick);

        // Time-shared service paces this CPU's virtual clock, advanced
        // before the task can leave the competition so the share reflects the
        // run that just happened.
        if !band.is_realtime() {
            self.cpus[cpu as usize].queue.advance(ran);
        }

        let doomed = park::observe_doom(&task.doomed);
        let settled = park::settle(&*task, action, doomed, |transition: &dyn Fn() -> bool| {
            task.ledger.depart(transition, &self.competition())
        });
        match settled {
            Settled::Retire => {
                if let Some(mut guard) = task.body.try_lock() {
                    *guard = None;
                }
                self.tasks.write().remove(&id);
            }
            // A woken task rejoins with zero lag, so the run is not charged.
            Settled::Park => park::commit_park(&*task, |woken| self.admit_woken(woken)),
            Settled::Requeue => {
                let dest = self.class_home(task.load_priority(), cpu);
                if dest == cpu && task.load_sched_class() == band {
                    if !band.is_realtime() {
                        self.charge(&task, ran);
                    }
                    self.enqueue_home(&task);
                } else {
                    // A task on the wrong class of core for its priority, or
                    // one that changed band while it ran, rejoins afresh: on
                    // the preferred CPU's clock with zero lag, the same rule a
                    // steal follows.
                    task.home_cpu.store(dest, Ordering::Release);
                    self.admit_fresh_on(&task, dest);
                    // `dest` may be idle, and a queue entry alone cannot
                    // wake it; signalling after publishing orders the entry
                    // before the target looks.
                    if dest != cpu {
                        self.arch.send_ipi(dest);
                    }
                }
            }
            Settled::Readmitted => {}
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

    /// Cumulative ticks `id` has spent running, in
    /// [`SchedulerArch::ticks_now`] units.
    ///
    /// Includes the in-flight span of a run that has started but not yet
    /// returned to the dispatch loop (the task is still the current task
    /// on its home CPU): a CPU-bound task that never yields is correctly
    /// left unpreempted with its one-shot disarmed, so without this its
    /// reported time would freeze between dispatch returns and appear to
    /// jump only when it finally yielded.
    ///
    /// # Errors
    /// * [`SchedError::NoSuchTask`] if the id is unknown (including an
    ///   exited task whose record has been drained).
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

    /// Ticks the task currently dispatching on `cpu` has been running
    /// since its last dispatch but has not yet settled into `busy_ticks`,
    /// or `0` when the CPU is idle.
    ///
    /// A tickless, sole CPU-bound task runs without returning to the
    /// dispatch loop (its one-shot is disarmed — nothing preempts it), so
    /// it would otherwise contribute nothing to its CPU's busy total
    /// until it finally yielded, making a fully-busy core read as idle
    /// between dispatch returns. The current slot is cleared before
    /// `settle_run_accounting` credits the completed span, so a span is
    /// never counted both here and there.
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

    /// Cumulative ticks `cpu` has spent inside task bodies, in
    /// [`SchedulerArch::ticks_now`] units.
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

    /// Task dispatches (context switches into a task body) on `cpu`,
    /// counted on the same bracket as [`Self::cpu_busy_ticks`].
    ///
    /// # Errors
    /// * [`SchedError::NoSuchCpu`] if `cpu` is out of range.
    pub fn cpu_switches(&self, cpu: CpuId) -> SchedResult<u64> {
        self.cpus
            .get(cpu as usize)
            .map(|state| state.switches.load(Ordering::Acquire))
            .ok_or(SchedError::NoSuchCpu)
    }

    /// Instantaneous count of ready tasks queued on `cpu`'s virtual-time
    /// run queue (the running task sits in the current slot, not the
    /// queue).
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

    /// Move `id` into scheduling class `class`, governing its next enqueue
    /// onward (see [`SchedulerPolicy::set_sched_class`]).
    ///
    /// # Errors
    /// * [`SchedError::NoSuchTask`] if no task ever held that id.
    /// * [`SchedError::InvalidState`] if the task is terminal.
    pub fn set_sched_class(&self, id: TaskId, class: SchedClass) -> SchedResult<()> {
        let task = self.lookup(id)?;
        if task.load_state() == TaskState::Exited {
            return Err(SchedError::InvalidState);
        }
        // Record the class; every enqueue point (`admit_woken`,
        // `enqueue_home`, the yield-migrate path, overflow drain, steal)
        // reads it and routes the task to the matching band, so a task
        // adopts the class the next time it is placed on a run queue — its
        // next wake or yield. The usual caller elevates itself while
        // Running, so it is strict-priority from its very next wake.
        task.store_sched_class(class);
        Ok(())
    }

    /// The current [`SchedClass`] of `id`.
    ///
    /// # Errors
    /// * [`SchedError::NoSuchTask`] if no task ever held that id.
    pub fn sched_class(&self, id: TaskId) -> SchedResult<SchedClass> {
        self.tasks
            .read()
            .get(&id)
            .map(|t| t.load_sched_class())
            .ok_or(SchedError::NoSuchTask)
    }

    /// Move `id` to `priority`, governing its next enqueue onward (see
    /// [`SchedulerPolicy::set_priority`]).
    ///
    /// # Errors
    /// * [`SchedError::NoSuchTask`] if no task ever held that id.
    /// * [`SchedError::InvalidState`] if the task is terminal.
    pub fn set_priority(&self, id: TaskId, priority: Priority) -> SchedResult<()> {
        let task = self.lookup(id)?;
        if task.load_state() == TaskState::Exited {
            return Err(SchedError::InvalidState);
        }
        // Record the priority; the next enqueue derives its virtual
        // eligible/deadline pair from the new weight, so the task's fair
        // share changes without any queued entry needing surgery.
        task.store_priority(priority);
        Ok(())
    }

    /// The current [`Priority`] of `id`.
    ///
    /// # Errors
    /// * [`SchedError::NoSuchTask`] if no task ever held that id.
    pub fn priority(&self, id: TaskId) -> SchedResult<Priority> {
        self.tasks
            .read()
            .get(&id)
            .map(|t| t.load_priority())
            .ok_or(SchedError::NoSuchTask)
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

    /// Clear every per-CPU current-task slot equal to `id`, returning the
    /// CPU it was the current task on (there is at most one). The CAS is
    /// per-slot, so a concurrent `dispatch` of a *different* task on a
    /// sibling CPU is untouched.
    fn clear_current_matching(&self, id: TaskId) -> Option<CpuId> {
        let mut ran_on = None;
        for (cpu, slot) in self.current.iter().enumerate() {
            if slot
                .compare_exchange(id, 0, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                #[allow(clippy::cast_possible_truncation)]
                // The slot index is bounded by the configured CPU count.
                let cpu = cpu as CpuId;
                ran_on = Some(cpu);
            }
        }
        ran_on
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

    fn spawn_parked<F>(&self, home_cpu: CpuId, priority: Priority, body: F) -> SchedResult<TaskId>
    where
        F: FnMut(&mut TaskContext) -> TaskAction + Send + 'static,
    {
        Scheduler::spawn_parked(self, home_cpu, priority, body)
    }

    fn spawn_parked_as<F>(
        &self,
        id: TaskId,
        home_cpu: CpuId,
        priority: Priority,
        body: F,
    ) -> SchedResult<TaskId>
    where
        F: FnMut(&mut TaskContext) -> TaskAction + Send + 'static,
    {
        Scheduler::spawn_parked_as(self, id, home_cpu, priority, body)
    }

    fn park(&self, id: TaskId) -> SchedResult<()> {
        Scheduler::park(self, id)
    }

    fn unpark(&self, id: TaskId) -> SchedResult<()> {
        Scheduler::unpark(self, id)
    }

    fn exit(&self, id: TaskId) -> SchedResult<ExitDisposition> {
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

    fn set_sched_class(&self, id: TaskId, class: SchedClass) -> SchedResult<()> {
        Scheduler::set_sched_class(self, id, class)
    }

    fn set_priority(&self, id: TaskId, priority: Priority) -> SchedResult<()> {
        Scheduler::set_priority(self, id, priority)
    }

    fn priority(&self, id: TaskId) -> SchedResult<Priority> {
        Scheduler::priority(self, id)
    }

    fn sched_class(&self, id: TaskId) -> SchedResult<SchedClass> {
        Scheduler::sched_class(self, id)
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
            boost_interval_quanta: 1024,
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

    /// Placement prefers an idle CPU over a busy hint: with weight
    /// already competing on the hinted CPU, a new spawn lands on the
    /// idle sibling and IPIs it — the wake that pulls a `wfi`-parked
    /// secondary core into service.
    #[test]
    fn spawn_prefers_an_idle_cpu_over_a_busy_hint() {
        let (arch, sched) = mk(2);
        let _busy = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Yield)
            .expect("spawn busy");
        let ipis_before = arch.ipi_count(1);
        let placed = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Exit)
            .expect("spawn placed");
        assert!(
            arch.ipi_count(1) > ipis_before,
            "the idle CPU must receive the placement IPI"
        );
        arch.set_current_cpu(1);
        assert_eq!(sched.step(1), Ok(StepOutcome::Ran(placed)));
    }

    /// A burst of spawns from one CPU spreads over every idle CPU:
    /// each admission adds the placed task's weight, so the next
    /// placement sees that CPU as loaded and moves on.
    #[test]
    fn spawn_burst_from_one_cpu_spreads_over_idle_cpus() {
        let (arch, sched) = mk(4);
        for _ in 0..4 {
            sched
                .spawn(0, Priority::Normal, |_| TaskAction::Yield)
                .expect("spawn");
        }
        for cpu in 0..4 {
            assert!(
                arch.ipi_count(cpu) >= 1,
                "cpu {cpu} must have been placed work (got no IPI)"
            );
        }
    }

    /// A woken task is re-placed onto an idle CPU (and that CPU is
    /// IPI'd) rather than queueing behind its old home's backlog.
    #[test]
    fn unpark_moves_a_woken_task_to_an_idle_cpu() {
        let (arch, sched) = mk(2);
        let parker = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Park)
            .expect("spawn parker");
        arch.set_current_cpu(0);
        assert_eq!(sched.step(0), Ok(StepOutcome::Ran(parker)));
        assert_eq!(sched.state_of(parker), TaskState::Parked);
        let _busy = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Yield)
            .expect("spawn busy");
        let ipis_before = arch.ipi_count(1);
        sched.unpark(parker).expect("unpark");
        assert!(
            arch.ipi_count(1) > ipis_before,
            "the idle CPU must be IPI'd for the woken task"
        );
        arch.set_current_cpu(1);
        assert_eq!(sched.step(1), Ok(StepOutcome::Ran(parker)));
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
        let runs = Arc::new(AtomicU32::new(0));
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

    /// A task that runs `ticks` of simulated work per dispatch, then yields.
    fn runner(sched: &Scheduler<TestArch>, arch: &Arc<TestArch>, ticks: u64) -> TaskId {
        let clock = Arc::clone(arch);
        sched
            .spawn(0, Priority::Normal, move |_| {
                clock.advance_ticks(ticks);
                TaskAction::Yield
            })
            .expect("spawn")
    }

    /// Two equal-weight tasks split the CPU's *time* evenly however they spend
    /// it: one running a whole quantum per dispatch and one running a tick
    /// before it yields each get half. Charging every dispatch one quantum
    /// handed the long runner eight ticks for each of the short runner's one.
    #[test]
    fn equal_weights_share_time_not_dispatches() {
        let (arch, sched) = mk(1);
        arch.set_current_cpu(0);
        arch.set_quantum_ticks(8);
        let long = runner(&sched, &arch, 8);
        let short = runner(&sched, &arch, 1);
        for _ in 0..400 {
            let _ = sched.step(0).expect("step");
        }
        let long_ticks = sched.cpu_ticks_of(long).expect("live");
        let short_ticks = sched.cpu_ticks_of(short).expect("live");
        assert!(
            long_ticks.abs_diff(short_ticks) <= 8,
            "at most one request apart ({long_ticks} vs {short_ticks})"
        );
        assert!(
            sched.run_count(short).expect("live") > 4 * sched.run_count(long).expect("live"),
            "the short runner makes up its share in many short dispatches"
        );
    }

    /// A run shorter than the request leaves the rest of it owed: the eligible
    /// time advances by what was used and the deadline stays put.
    #[test]
    fn a_short_run_keeps_its_deadline_until_the_request_is_served() {
        let (arch, sched) = mk(1);
        arch.set_current_cpu(0);
        arch.set_quantum_ticks(8);
        let id = runner(&sched, &arch, 1);
        let task = sched.lookup(id).expect("live");
        let request = vslice(8, task.weight());
        assert_eq!(task.deadline(), request, "admitted one request out");
        let _ = sched.step(0).expect("step");
        assert_eq!(task.eligible(), vslice(1, task.weight()));
        assert_eq!(task.deadline(), request, "still owed the rest of it");
    }

    /// Real-time service is not delivered to the fair competition, so it does
    /// not move the fair clock.
    #[test]
    fn a_realtime_run_leaves_the_fair_clock_alone() {
        let (arch, sched) = mk(1);
        arch.set_current_cpu(0);
        let clock = Arc::clone(&arch);
        let rt = sched
            .spawn_parked(0, Priority::Normal, move |_| {
                clock.advance_ticks(5);
                TaskAction::Yield
            })
            .expect("spawn");
        sched
            .set_sched_class(rt, SchedClass::Realtime)
            .expect("realtime");
        sched.unpark(rt).expect("wake");
        let _fair = runner(&sched, &arch, 3);
        assert_eq!(sched.step(0), Ok(StepOutcome::Ran(rt)));
        assert_eq!(sched.cpus[0].queue.virtual_time(), 0);
    }

    /// A priority change while a task is counted must not move what comes off
    /// when it leaves: removing the *new* weight let any parent that lowered
    /// its own child leak weight onto a CPU for the rest of the boot, slowing
    /// its clock and skewing placement away from it.
    #[test]
    fn a_reprioritised_task_takes_off_the_weight_it_was_counted_at() {
        let (_arch, sched) = mk(1);
        let id = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Park)
            .expect("spawn");
        assert_eq!(sched.cpus[0].queue.competing_weight(), 2);
        sched.set_priority(id, Priority::Low).expect("lower it");
        sched.park(id).expect("park");
        assert_eq!(sched.cpus[0].queue.competing_weight(), 0);
    }

    /// A steal that finds the entry of a task parked since it was queued must
    /// neither take the weight off the victim again nor count it on the
    /// stealer, where nothing would ever take it off.
    #[test]
    fn a_stale_entry_a_steal_finds_moves_no_weight() {
        let (arch, sched) = mk(2);
        let id = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Exit)
            .expect("spawn");
        let home = home_of(&sched, id);
        let other = 1 - home;
        sched.park(id).expect("park while queued");
        arch.set_current_cpu(other);
        assert_eq!(sched.step(other), Ok(StepOutcome::Idle));
        assert_eq!(sched.cpus[0].queue.competing_weight(), 0);
        assert_eq!(sched.cpus[1].queue.competing_weight(), 0);
        assert_eq!(sched.state_of(id), TaskState::Parked);
    }

    /// A task drained from overflow onto another CPU's queue is announced to
    /// that CPU, which may be idle.
    #[test]
    fn an_overflow_drain_announces_the_home_it_requeues_onto() {
        let (arch, sched) = mk(2);
        let id = sched
            .spawn_parked(0, Priority::Normal, |_| TaskAction::Exit)
            .expect("spawn");
        let task = sched.lookup(id).expect("live");
        task.home_cpu.store(0, Ordering::Release);
        task.store_state(TaskState::Ready);
        assert!(sched.count_on(&task, 0));
        sched.overflow.lock().push(id);
        let before = arch.ipi_count(0);
        sched.drain_overflow(1);
        assert_eq!(arch.ipi_count(0), before + 1);
        assert_eq!(sched.queue_depth(0), Ok(1));
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

    /// A repeat termination request against a victim that is **still
    /// executing** owes no teardown, but must still nudge it. Short-circuiting
    /// on the `doomed` claim before looking at whether the victim is on-CPU
    /// makes the `Kill` a grace window escalates to issue nothing at all, so
    /// an unresponsive task runs on until its next quantum.
    #[test]
    fn a_repeat_exit_still_nudges_a_still_executing_victim() {
        let (arch, sched) = mk(4);
        let id = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Exit)
            .expect("spawn");
        let task = sched.lookup(id).expect("task present");
        // Simulate a live dispatch of this task on CPU 3.
        let body_guard = task.body.lock();
        sched.set_current(3, id);
        assert_eq!(sched.exit(id).expect("exit"), ExitDisposition::Deferred);
        let after_first = arch.ipi_count(3);

        assert_eq!(
            sched.exit(id).expect("repeat exit"),
            ExitDisposition::AlreadyExited,
            "the repeat owes no teardown"
        );
        assert_eq!(
            arch.ipi_count(3),
            after_first + 1,
            "but it still nudges the CPU running the victim"
        );
        drop(body_guard);

        // Once no dispatch owns the body, a repeat nudges nobody.
        let quiet = arch.ipi_count(3);
        assert_eq!(
            sched.exit(id).expect("repeat exit"),
            ExitDisposition::AlreadyExited
        );
        assert_eq!(arch.ipi_count(3), quiet, "a quiescent victim needs no IPI");
    }

    /// Killing a task that is **executing its body** on a remote CPU must
    /// defer teardown to that dispatch and nudge the CPU with a reschedule
    /// IPI, so a CPU-bound victim running alone on a tickless core is
    /// knocked off its context promptly instead of pegging the core — and
    /// is *not* reclaimed by the killer while it is still on-CPU (the
    /// wild-fault fix). A held body lock is what a live dispatch holds
    /// while a task runs, so it models "still executing" faithfully.
    #[test]
    fn exit_of_an_executing_victim_defers_and_ipis_without_reclaiming() {
        let (arch, sched) = mk(4);
        let id = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Exit)
            .expect("spawn");
        let task = sched.lookup(id).expect("task present");
        let body_guard = task.body.lock();
        sched.set_current(2, id);
        let before = arch.ipi_count(2);
        assert_eq!(
            sched.exit(id).expect("exit"),
            ExitDisposition::Deferred,
            "a still-executing victim is deferred, never reclaimed by the killer"
        );
        assert_eq!(
            arch.ipi_count(2),
            before + 1,
            "exit must nudge the CPU running the victim"
        );
        assert!(sched.tasks.read().contains_key(&id), "not yet reclaimed");
        assert_eq!(sched.current_task(2), Some(id));
        assert_ne!(sched.state_of(id), TaskState::Exited, "not yet exited");
        drop(body_guard);
    }

    /// Parking a task that is still executing must leave its per-CPU
    /// current-task slot alone.
    ///
    /// That slot is how the syscall dispatcher attributes the task's next
    /// trap. Clearing it from a remote CPU while the victim still runs in
    /// user mode left the following trap unattributable — which used to
    /// halt the CPU outright. The running CPU clears its own slot when the
    /// body returns; the killer only nudges it.
    #[test]
    fn park_of_an_executing_task_leaves_its_current_slot_intact() {
        let (arch, sched) = mk(4);
        let id = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Exit)
            .expect("spawn");
        let task = sched.lookup(id).expect("task present");
        let body_guard = task.body.lock();
        sched.set_current(2, id);
        let before = arch.ipi_count(2);

        sched.park(id).expect("park");

        assert_eq!(
            sched.current_task(2),
            Some(id),
            "the caller-identity slot must outlive a remote park of a running task"
        );
        assert_eq!(
            arch.ipi_count(2),
            before + 1,
            "park must nudge the CPU running the victim"
        );
        drop(body_guard);
    }

    /// The same park on a task no dispatch owns clears the slot at once:
    /// the body lock is free, which proves no CPU can trap as this task.
    #[test]
    fn park_of_a_quiescent_task_clears_its_current_slot() {
        let (_arch, sched) = mk(4);
        let id = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Exit)
            .expect("spawn");
        sched.set_current(2, id);

        sched.park(id).expect("park");

        assert_eq!(sched.current_task(2), None);
    }

    /// Killing a task that is **not executing** quiesces it immediately:
    /// the killer owns teardown, no IPI is owed, and the task is retired.
    #[test]
    fn exit_of_a_quiescent_task_reclaims_immediately() {
        let (arch, sched) = mk(4);
        let id = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Exit)
            .expect("spawn");
        let before: u64 = (0..4).map(|c| arch.ipi_count(c)).sum();
        assert_eq!(
            sched.exit(id).expect("exit"),
            ExitDisposition::Quiesced,
            "a non-executing task is quiesced and owned by the killer"
        );
        let after: u64 = (0..4).map(|c| arch.ipi_count(c)).sum();
        assert_eq!(before, after, "no running context, so no reschedule IPI");
        assert_eq!(sched.state_of(id), TaskState::Exited);
    }

    /// A second termination request against an already-killed task owes no
    /// teardown, so a burst of kills reclaims exactly once.
    #[test]
    fn a_repeat_exit_reports_already_exited() {
        let (_arch, sched) = mk(4);
        let id = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Exit)
            .expect("spawn");
        assert_eq!(sched.exit(id), Ok(ExitDisposition::Quiesced));
        assert_eq!(sched.exit(id), Ok(ExitDisposition::AlreadyExited));
    }

    /// The dispatch loop accumulates the ticks that elapse while a body
    /// runs, and an unknown id is refused rather than reported as zero.
    #[test]
    fn cpu_ticks_accumulate_across_dispatches() {
        let arch = Arc::new(TestArch::new(1).expect("arch"));
        let sched =
            Arc::new(Scheduler::new(SchedulerConfig::defaults_for(1), arch.clone()).unwrap());
        let a2 = arch.clone();
        let id = sched
            .spawn(0, Priority::Normal, move |_| {
                // Simulate 7 ticks of work inside the body.
                a2.advance_ticks(7);
                TaskAction::Yield
            })
            .expect("spawn");
        arch.set_current_cpu(0);
        assert_eq!(sched.cpu_ticks_of(id), Ok(0), "never ran yet");
        assert_eq!(sched.step(0), Ok(StepOutcome::Ran(id)));
        assert_eq!(sched.cpu_ticks_of(id), Ok(7));
        assert_eq!(sched.step(0), Ok(StepOutcome::Ran(id)));
        assert_eq!(sched.cpu_ticks_of(id), Ok(14));
        assert_eq!(sched.cpu_ticks_of(9999), Err(SchedError::NoSuchTask));
    }

    /// A task that is still running — its body has not yet returned to
    /// the dispatch loop — has its elapsed run counted live, both in its
    /// CPU's busy total and in its own CPU-time. This is what keeps a
    /// tickless, never-yielding CPU-bound task from reading as 0% between
    /// dispatch returns (and then spiking) instead of a steady 100%. The
    /// span is settled exactly once, so the mid-run and post-run figures
    /// agree rather than doubling.
    #[test]
    fn a_running_task_reports_in_flight_time_before_it_yields() {
        let arch = Arc::new(TestArch::new(1).expect("arch"));
        let sched =
            Arc::new(Scheduler::new(SchedulerConfig::defaults_for(1), arch.clone()).unwrap());
        let a2 = arch.clone();
        let s2 = sched.clone();
        let seen_cpu = Arc::new(AtomicU64::new(0));
        let seen_task = Arc::new(AtomicU64::new(0));
        let seen_cpu2 = seen_cpu.clone();
        let seen_task2 = seen_task.clone();
        let id = sched
            .spawn(0, Priority::Normal, move |ctx| {
                // Simulate 50 ticks of work, then observe *before* the
                // body returns (while the task is still current).
                a2.advance_ticks(50);
                seen_cpu2.store(s2.cpu_busy_ticks(ctx.cpu).unwrap(), Ordering::Release);
                seen_task2.store(s2.cpu_ticks_of(ctx.task_id).unwrap(), Ordering::Release);
                TaskAction::Yield
            })
            .expect("spawn");
        arch.set_current_cpu(0);
        assert_eq!(sched.step(0), Ok(StepOutcome::Ran(id)));
        assert_eq!(
            seen_cpu.load(Ordering::Acquire),
            50,
            "per-CPU busy must count the in-flight run"
        );
        assert_eq!(
            seen_task.load(Ordering::Acquire),
            50,
            "per-task CPU time must count the in-flight run"
        );
        // Settled exactly once after the body returned: no double count,
        // and the current slot no longer contributes in-flight.
        assert_eq!(sched.cpu_busy_ticks(0), Ok(50));
        assert_eq!(sched.cpu_ticks_of(id), Ok(50));
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
        // performance core and count its weight there, mimicking a steal.
        let task = sched.tasks.read().get(&id).cloned().expect("task");
        assert!(sched.count_on(&task, 0));
        task.home_cpu.store(0, Ordering::Release);
        let entry = sched.admit(&task, &sched.cpus[0].queue);
        sched.cpus[0].queue.push(entry).expect("seed perf queue");
        assert_eq!(sched.class_of(home_of(&sched, id)), CoreClass::Performance);
        assert_eq!(sched.cpus[0].queue.competing_weight(), 1);

        // Dispatch on the performance core: the yield migrates it back.
        arch.set_current_cpu(0);
        assert_eq!(sched.step(0), Ok(StepOutcome::Ran(id)));
        assert_eq!(
            sched.cpus[0].queue.competing_weight(),
            0,
            "its weight left with it"
        );
        let dest = home_of(&sched, id);
        assert_eq!(sched.cpus[dest as usize].queue.competing_weight(), 1);
        assert_eq!(
            sched.class_of(dest),
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
