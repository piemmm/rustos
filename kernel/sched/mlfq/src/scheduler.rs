//! Per-CPU run queues, MLFQ policy, work-stealing dispatch.
//!
//! See `docs/src/architecture/scheduler.md` for the algorithm description,
//! invariants, and debugging guide. This module is the implementation; the
//! doc page is the source of truth for behaviour.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use tairix_rng::{FastRng, RandU64};
use tairix_sync::{RwLock, SpinLock};

use crate::loom_compat::{fence, AtomicU64, Ordering};
use crate::runqueue::{RunDeque, Steal};
use crate::task::{TaskBody, TaskInner};
use crate::{
    choose_task_id, CoreClass, CpuId, ExitDisposition, Priority, SchedClass, SchedError,
    SchedResult, SchedulerArch, SchedulerConfig, SchedulerPolicy, StepOutcome, TaskAction,
    TaskContext, TaskId, TaskState,
};

/// Per-CPU scheduler state.
///
/// Allocated once at scheduler construction and never resized. Mutating
/// access happens only through atomic fields and the per-band deques —
/// there is no shared `Mutex<CpuState>`.
struct CpuState {
    /// Strict-priority real-time band. A task here is dispatched (and
    /// stolen) before *any* time-shared band, and re-enqueued to the back
    /// on yield (FIFO / round-robin among real-time peers) — the
    /// [`SchedClass::Realtime`] guarantee. A real-time task never enters
    /// `bands`; the MLFQ demotion/boost machinery does not apply to it.
    rt: RunDeque,
    bands: [RunDeque; Priority::COUNT],
    /// Tick at which this CPU last completed a [`Scheduler::step`] that
    /// returned [`StepOutcome::Ran`]. Used by tests to assert progress.
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

impl CpuState {
    fn new(queue_capacity: usize) -> Option<Self> {
        let band = || RunDeque::try_new(queue_capacity);
        Some(Self {
            rt: band()?,
            bands: [band()?, band()?, band()?],
            last_run_tick: AtomicU64::new(0),
            busy_ticks: AtomicU64::new(0),
            switches: AtomicU64::new(0),
        })
    }

    fn push_priority(&self, prio: Priority, id: TaskId) -> Result<(), TaskId> {
        self.bands[prio.as_index()].push(id)
    }

    /// Push a real-time task onto the back of the strict-priority band
    /// (FIFO / round-robin). Returns `Err(id)` when the band is full, like
    /// [`Self::push_priority`].
    fn push_realtime(&self, id: TaskId) -> Result<(), TaskId> {
        self.rt.push(id)
    }

    /// Enqueue `task` in its scheduling class: a real-time task onto the
    /// strict-priority band, a time-shared task onto its `prio` band.
    fn push_class(&self, class: SchedClass, prio: Priority, id: TaskId) -> Result<(), TaskId> {
        if class.is_realtime() {
            self.push_realtime(id)
        } else {
            self.push_priority(prio, id)
        }
    }

    /// Consume the front real-time task, if any, ahead of every
    /// time-shared band. Bounded lost-CAS retry, exactly as
    /// [`Self::pop_highest`].
    fn pop_realtime(&self) -> Option<TaskId> {
        for _ in 0..8 {
            match self.rt.steal() {
                Steal::Stolen(id) => return Some(id),
                Steal::Empty => return None,
                Steal::Retry => {}
            }
        }
        None
    }

    fn pop_highest(&self) -> Option<(Priority, TaskId)> {
        for i in 0..Priority::COUNT {
            // SAFETY-INVARIANT: i is in 0..COUNT so from_index is Some.
            let p = Priority::from_index(i).unwrap_or(Priority::Low);
            // Bounded retry — `Steal::Retry` is reported on lost CAS
            // races. There are at most `cpus - 1` concurrent stealers,
            // so a few attempts always succeed when the band is
            // non-empty. Capped to keep the dispatch path lock-free in
            // the system-wide sense (no spin loops).
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
        // The real-time band is stolen first: moving a waiting real-time
        // task to an idle CPU shortens its dispatch latency, and it stays
        // strict-priority there.
        match self.rt.steal() {
            Steal::Empty => {}
            other => return other,
        }
        for band in &self.bands {
            match band.steal() {
                Steal::Empty => {}
                other => return other,
            }
        }
        Steal::Empty
    }

    /// Whether any band holds a ready task. The running task sits in the
    /// current-task slot, not in a band, so a `true` here means there is a
    /// **competitor** the running task must be preempted for — the
    /// tickless one-shot arming decision. Includes the real-time band.
    fn has_ready_competitor(&self) -> bool {
        self.rt.has_ready() || self.bands.iter().any(RunDeque::has_ready)
    }
}

/// The SMP scheduler.
///
/// Generic over the architecture surface so the host test binary can use
/// a mock arch and the architecture ports plug in their own types.
/// Internally the scheduler owns:
///
/// * a fixed array of per-CPU state blocks — no global mutable state
///   outside these per-CPU areas ("SMP from day one"),
/// * a registry of tasks keyed by [`TaskId`] under a single
///   reader-preferring `RwLock`; only [`Self::spawn`] and the
///   bookkeeping for [`Self::exit`] take the write lock,
/// * an `Arc<A: SchedulerArch>` for current-CPU / tick / IPI access.
pub struct Scheduler<A: SchedulerArch> {
    arch: Arc<A>,
    cpus: Box<[CpuState]>,
    tasks: RwLock<BTreeMap<TaskId, Arc<TaskInner>>>,
    config: SchedulerConfig,
    last_boost_tick: AtomicU64,
    /// Per-CPU fast, non-cryptographic generators for work-stealing
    /// victim selection. This is a "scheduler decision" in the sense of
    /// `lib/rng` (there is no second PRNG): the
    /// project's shared [`FastRng`] (xoshiro256++), not a hand-rolled
    /// one. `next_victim` takes `&self`, so each generator's `&mut self`
    /// stepping lives behind a [`SpinLock`] — but the slot is indexed by
    /// the *calling* CPU, so the lock is always uncontended on the
    /// work-stealing path (every idle CPU draws from its own stream
    /// instead of serialising on one global generator).
    victim_rng: Box<[SpinLock<FastRng>]>,
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
    /// (: a runaway task on one CPU must not be able to
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
    /// path (`kernel/tairix-kernel::dispatch::production_dispatch`,
    /// Stage 2.7 follow-up (f5)). Per the `kernel/sync::RwLock`
    /// process-context rule, readers must run in
    /// process context on the issuing CPU — which is how syscall
    /// entry is reached on every architecture. There is no global
    /// mutable state: every CPU has its own atomic word.
    current: Box<[AtomicU64]>,
    /// Static [`CoreClass`] of each CPU, indexed by [`CpuId`].
    ///
    /// Snapshotted once at construction from the arch HAL
    /// ([`SchedulerArch::core_class`]); the class is a static identity
    /// property so a snapshot is authoritative for
    /// the scheduler's lifetime.
    core_classes: Box<[CoreClass]>,
    /// Dense list of the performance-class CPUs.
    perf_cpus: Box<[CpuId]>,
    /// Dense list of the efficiency-class CPUs. Empty on a homogeneous
    /// machine — placement then falls back to the task's home CPU, so
    /// the heterogeneous path is a strict no-op there.
    eff_cpus: Box<[CpuId]>,
    /// Round-robin cursor used to spread same-class placements across the
    /// CPUs of a class. A single relaxed counter is ample for this
    /// control-plane decision.
    class_rr: AtomicU64,
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
        let mut victim_rng = Vec::with_capacity(config.cpus as usize);
        for cpu in 0..config.cpus {
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
            config,
            last_boost_tick: AtomicU64::new(0),
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

    /// The [`CoreClass`] a task at `prio` should run on.
    ///
    /// Interactive / throughput-sensitive work (`High`, `Normal`) prefers
    /// a performance core; background work (`Low`) prefers an efficiency
    /// core. Because the periodic priority boost promotes a starved `Low`
    /// task to `High` and demotion returns it to `Low`, this single rule
    /// is what makes a background task migrate *up* to a performance core
    /// when it is given a turn and back *down* to an efficiency core once
    /// it settles — the promote/demote behaviour heterogeneous CPUs need.
    const fn preferred_class(prio: Priority) -> CoreClass {
        match prio {
            Priority::High | Priority::Normal => CoreClass::Performance,
            Priority::Low => CoreClass::Efficiency,
        }
    }

    /// Choose the CPU a task at `prio` should be enqueued on, given its
    /// current `home`.
    ///
    /// Keeps `home` when it already matches the preferred class — that
    /// avoids needless migration and makes the whole heterogeneous path
    /// a strict no-op on a homogeneous machine (every CPU is a
    /// performance core, so `Performance`-class work never moves and the
    /// efficiency pool is empty). Otherwise it round-robins across the
    /// CPUs of the preferred class, falling back to `home` when that pool
    /// is empty (a headless / single-class configuration).
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

    /// Spawn a new task at `priority` with a body closure.
    ///
    /// `home_cpu` is the caller's hint. On a heterogeneous machine the
    /// task is placed on a CPU of the [`CoreClass`] that matches its
    /// priority (`preferred_home`) — a `Low` background task
    /// lands on an efficiency core, interactive work on a performance
    /// core — so the spawn already honours the asymmetric topology. On a
    /// homogeneous machine `preferred_home` returns `home_cpu` unchanged.
    /// If the chosen CPU's queue is full the task is parked in the global
    /// overflow list and any CPU that next runs [`Self::step`] will pick
    /// it up; this preserves the "spawn never panics" invariant.
    ///
    /// # Errors
    /// * [`SchedError::NoSuchCpu`] if `home_cpu` is out of range.
    pub fn spawn<F>(&self, home_cpu: CpuId, priority: Priority, body: F) -> SchedResult<TaskId>
    where
        F: FnMut(&mut TaskContext) -> TaskAction + Send + 'static,
    {
        self.cpu_state(home_cpu)?;
        let placed = self.preferred_home(priority, home_cpu);
        let boxed: Box<TaskBody> = Box::new(body);
        // The id is chosen and registered under one write lock, so a
        // concurrent admission cannot take it in between; the enqueue below
        // then runs with the lock released, preserving the
        // tasks-before-queue order every other path takes.
        let id = {
            let mut tasks = self.tasks.write();
            let id = choose_task_id(None, |c| tasks.contains_key(&c))?;
            let inner = Arc::new(TaskInner::new(id, placed, priority, boxed));
            tasks.insert(id, inner);
            id
        };
        // Try to enqueue on the placed CPU; on overflow, push to the
        // global overflow list. Both paths preserve cancellation safety
        // because state is updated only after enqueue confirms.
        match self.cpu_state(placed)?.push_priority(priority, id) {
            Ok(()) => {}
            Err(_) => self.overflow.lock().push(id),
        }
        // Wake the placed CPU. Spawning at higher priority than what is
        // running there should preempt — but at this layer we have no
        // visibility into "what is running there", so we always notify
        // and let the arch port decide. (no creep.)
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
    /// placement, enqueues it, and sends the IPI — so no CPU can dispatch
    /// the task before the caller finishes installing its per-task state
    /// under the returned id.
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
        let placed = self.preferred_home(priority, home_cpu);
        self.cpu_state(placed)?;
        let boxed: Box<TaskBody> = Box::new(body);
        let mut tasks = self.tasks.write();
        let id = choose_task_id(requested, |c| tasks.contains_key(&c))?;
        let inner = Arc::new(TaskInner::new(id, placed, priority, boxed));
        // Born parked: no queue entry is pushed and no IPI is raised, so
        // no CPU can pick the task up. The later `unpark` performs the
        // placement, enqueue, and IPI.
        inner.store_state(TaskState::Parked);
        tasks.insert(id, inner);
        Ok(id)
    }

    /// Park a task. Cancellation-safe: the call is a no-op if the task
    /// has already exited.
    ///
    /// Also clears any per-CPU current-task slot whose entry equals
    /// `id` so the slot does not outlive the task's run
    /// (fail-closed: no syscall must reach a parked
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
                    self.release_current_slot(&task, id);
                    return Ok(());
                }
                cur @ (TaskState::Ready | TaskState::Running) => {
                    if task.cas_state(cur, TaskState::Parked).is_ok() {
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

    /// Re-admit a task that is currently [`TaskState::Parked`], steering it
    /// onto a CPU of the class its priority calls for and re-enqueuing it
    /// at its current band (a wake is not a voluntary yield, so no
    /// demotion). The one definition of the wake-from-parked transition,
    /// shared by [`Self::unpark`] and the `step` Park-commit token
    /// re-check. Fails closed with [`SchedError::InvalidState`] when the
    /// task is no longer `Parked` (a concurrent waker already re-readied
    /// it), so a racing second caller never enqueues the same task twice.
    fn wake_from_parked(&self, task: &Arc<TaskInner>) -> SchedResult<()> {
        task.cas_state(TaskState::Parked, TaskState::Ready)
            .map_err(|_| SchedError::InvalidState)?;
        let prio = task.load_priority();
        let class = task.load_sched_class();
        let home = task.home_cpu.load(Ordering::Acquire);
        let target = self.preferred_home(prio, home);
        task.home_cpu.store(target, Ordering::Release);
        let cpu = self.cpu_state(target)?;
        if cpu.push_class(class, prio, task.id).is_err() {
            self.overflow.lock().push(task.id);
        }
        self.arch.send_ipi(target);
        Ok(())
    }

    /// Terminate a task. Cancellation-safe; idempotent.
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
        let task = self
            .tasks
            .read()
            .get(&id)
            .cloned()
            .ok_or(SchedError::NoSuchTask)?;
        // First termination request wins and owns the teardown; any repeat
        // owes nothing, so reclaim runs exactly once even under a burst of
        // kills against the same task.
        if task.doomed.swap(true, Ordering::AcqRel) {
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
                // A dispatch owns the task right now (it is running its body
                // on some CPU). It will observe the `doomed` mark when its
                // body returns and perform the final `Exited` transition
                // itself, so the killer must not reclaim yet. Nudge the
                // running CPU into the scheduler so a CPU-bound victim alone
                // on a tickless core (no quantum armed) is preempted
                // promptly rather than running on after it was told to die;
                // to the calling CPU the IPI is a documented no-op.
                if let Some(cpu) = self.running_cpu_of(id) {
                    self.arch.send_ipi(cpu);
                }
                return Ok(ExitDisposition::Deferred);
            };
            *body = None;
            task.store_state(TaskState::Exited);
        }
        // Leave the now-`Exited` registry entry in place (a repeat `exit`
        // finds it `AlreadyExited` via the `doomed` mark, so teardown is
        // idempotent). Any stale run-queue entry is dropped when a later
        // `step` picks it and `dispatch` observes `Exited`.
        self.clear_current_matching(id);
        Ok(ExitDisposition::Quiesced)
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
    /// assert that preemption fires (: a silent
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
    /// would be interface creep because the two
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

    /// Keep-alive hook the kernel preempt path calls after a fired tick
    /// that did not owe a context switch. MLFQ is **tickless** for
    /// preemption — a lone runnable task has no quantum armed and its core
    /// takes no timer interrupts (its anti-starvation boost cadence rides
    /// its own on-demand one-shots, never a global periodic tick) — so
    /// there is no periodic tick to re-arm here: a deliberate no-op. Only
    /// the non-tickless CFQ sibling re-arms.
    #[allow(clippy::unused_self)]
    pub fn rearm_periodic_tick(&self) {}

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

        // 2. Strict priority: any local real-time task runs before every
        //    time-shared band, regardless of MLFQ level.
        if let Some(id) = me.pop_realtime() {
            return Ok(self.dispatch(cpu, id));
        }

        // 3. Local time-shared bands: high → normal → low.
        if let Some((_band, id)) = me.pop_highest() {
            return Ok(self.dispatch(cpu, id));
        }

        // 4. Work-stealing pass over the other CPUs.
        if let Some(id) = self.try_steal(cpu) {
            return Ok(self.dispatch(cpu, id));
        }

        Ok(StepOutcome::Idle)
    }

    fn drain_overflow_to(&self, me: &CpuState) {
        let mut g = self.overflow.lock();
        while let Some(id) = g.pop() {
            let (class, prio) = self
                .tasks
                .read()
                .get(&id)
                .map_or((SchedClass::TimeShared, Priority::Normal), |t| {
                    (t.load_sched_class(), t.load_priority())
                });
            if me.push_class(class, prio, id).is_err() {
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
                task.store_priority(Priority::High);
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
        // Draw from this CPU's own `FastRng` (uncontended; the per-CPU
        // seeds already keep the streams independent).
        let s = self.victim_rng[cpu as usize].lock().next_u64();
        // n > 0 by construction (config.cpus != 0); both casts are
        // narrow because cpus is `u32` and the modulus is < n <= u32::MAX.
        #[allow(clippy::cast_possible_truncation)]
        {
            let n = self.cpus.len() as u64;
            (s % n) as u32
        }
    }

    /// Book one finished body run: bump the task's run count, accumulate
    /// the elapsed ticks (the span between the two tick reads is exactly
    /// the time the body held this CPU — raw ticks, so the hot path pays a
    /// subtraction, never a unit conversion; the reader converts), and
    /// stamp the CPU's last-run tick.
    fn settle_run_accounting(&self, cpu: CpuId, task: &TaskInner, started_tick: u64) {
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

        // Tickless preemption: arm this CPU's one-shot
        // timer for a single quantum iff another ready task is queued (a
        // competitor), and disarm otherwise so a CPU running a sole
        // runnable task takes no timer interrupts. The port owns the
        // quantum length; `set_preemption` is a pure boolean. The
        // contended-CPU one-shots that result are also what service MLFQ's
        // anti-starvation boost cadence (`maybe_priority_boost`) without
        // any global fixed-frequency tick — see the crate `README.md`
        // (carve-out). Armed *before* the body switches into the
        // task so the deadline is live for the run.
        self.arch
            .set_preemption(self.cpus[cpu as usize].has_ready_competitor());

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
        // The body has returned: clear the current slot *before* settling
        // the run into `busy_ticks`/`run_ticks`. `cpu_busy_ticks` /
        // `cpu_ticks_of` add the in-flight span of whatever the current
        // slot points at, so clearing first means the span this dispatch
        // just completed is counted by `settle_run_accounting` and never
        // also as in-flight — one span, one place.
        self.clear_current(cpu);
        self.settle_run_accounting(cpu, &task, tick);

        self.retire_park_or_requeue(&task, id, prio, cpu, action);
        StepOutcome::Ran(id)
    }

    /// Resolve the effective disposition of a task whose body has just
    /// returned `action` and apply it: retire it, park it, or re-enqueue it.
    ///
    /// The body's `action` is reconciled against external races. A
    /// termination requested while the task executed
    /// ([`ExitDisposition::Deferred`]) makes this dispatch own the final
    /// transition to quiescence — the task is retired rather than
    /// re-enqueued — *unless* it returned `Park`, which means it blocked
    /// inside a syscall handler and still holds kernel state only its own
    /// unwind can release; that kill is landed at the syscall boundary once
    /// the handler unwinds, so the park is honoured and the task is resumed
    /// to reach it. A concurrent external `park`/`exit` observed on the task
    /// likewise wins over a stale `Yield`.
    fn retire_park_or_requeue(
        &self,
        task: &Arc<TaskInner>,
        id: TaskId,
        prio: Priority,
        cpu: CpuId,
        action: TaskAction,
    ) {
        let observed_state = task.load_state();
        let doomed = task.doomed.load(Ordering::Acquire);
        let effective = if doomed && action != TaskAction::Park {
            TaskAction::Exit
        } else {
            match (observed_state, action) {
                (TaskState::Exited, _) | (_, TaskAction::Exit) => TaskAction::Exit,
                (TaskState::Parked, _) | (_, TaskAction::Park) => TaskAction::Park,
                _ => TaskAction::Yield,
            }
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
                // Publish `Parked`, then consume any wake token *after* the
                // store (the store-then-load pair matching `unpark`,
                // SeqCst-fenced on each side). A wake that raced this commit
                // is never lost: the waker either set the token before this
                // take (consumed here, task re-admitted at its current band
                // — no demotion, this is not a voluntary yield) or observed
                // `Parked` and re-admitted the task itself. The CAS inside
                // `wake_from_parked` keeps the re-admission single when both
                // sides run.
                task.store_state(TaskState::Parked);
                fence(Ordering::SeqCst);
                if task.take_wake_pending() {
                    let _ = self.wake_from_parked(task);
                }
            }
            TaskAction::Yield => self.reenqueue_after_yield(task, id, prio, cpu),
        }
    }

    /// Re-enqueue `task` after a voluntary `Yield`, in its scheduling class.
    ///
    /// A **real-time** task keeps its priority (which selects its core class)
    /// and re-enters the strict-priority band FIFO — round-robin among
    /// real-time peers; the MLFQ band/demotion machinery is time-shared only,
    /// so no demotion applies. A **time-shared** task runs the MLFQ demotion
    /// rule (a full quantum of consecutive yields demotes it one band) and is
    /// re-placed on a CPU of the class its (possibly demoted or boosted)
    /// priority now calls for — a task boosted to `High` migrates *up* to a
    /// performance core, one demoted to `Low` migrates back *down*; on a
    /// homogeneous machine this resolves to the CPU it just ran on.
    fn reenqueue_after_yield(&self, task: &TaskInner, id: TaskId, prio: Priority, cpu: CpuId) {
        task.store_state(TaskState::Ready);
        let (class, dest_prio) = if task.load_sched_class().is_realtime() {
            (SchedClass::Realtime, prio)
        } else {
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
            (SchedClass::TimeShared, new_prio)
        };
        let dest = self.preferred_home(dest_prio, cpu);
        task.home_cpu.store(dest, Ordering::Release);
        let target = self
            .cpus
            .get(dest as usize)
            .unwrap_or(&self.cpus[cpu as usize]);
        if target.push_class(class, dest_prio, id).is_err() {
            self.overflow.lock().push(id);
        }
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

    /// Instantaneous count of ready tasks queued across `cpu`'s priority
    /// bands (approximate under concurrent stealers; the running task
    /// sits in the current slot, not a band).
    ///
    /// # Errors
    /// * [`SchedError::NoSuchCpu`] if `cpu` is out of range.
    pub fn queue_depth(&self, cpu: CpuId) -> SchedResult<u64> {
        self.cpus
            .get(cpu as usize)
            .map(|state| {
                state
                    .bands
                    .iter()
                    .map(|band| band.len_approx() as u64)
                    .sum()
            })
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
            .map(|state| state.bands.iter().any(|band| band.len_approx() != 0))
            .ok_or(SchedError::NoSuchCpu)?;
        Ok(local_ready || !self.overflow.lock().is_empty())
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
    /// (step 1: identify the caller — kernel-provided,
    /// not caller-supplied). Per the `kernel/sync::RwLock`
    /// process-context rule, this method must be
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
    /// notions distinct, avoiding the interface creep
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
        let class = task.load_sched_class();
        let home = task.home_cpu.load(Ordering::Acquire);
        let cpu = self.cpu_state(home)?;
        if cpu.push_class(class, prio, id).is_err() {
            self.overflow.lock().push(id);
        }
        self.clear_current_matching(id);
        Ok(())
    }

    /// Move `id` into scheduling class `class`, governing its next enqueue
    /// onward (see [`SchedulerPolicy::set_sched_class`]).
    ///
    /// # Errors
    /// * [`SchedError::NoSuchTask`] if no task ever held that id.
    /// * [`SchedError::InvalidState`] if the task is terminal.
    pub fn set_sched_class(&self, id: TaskId, class: SchedClass) -> SchedResult<()> {
        let task = self
            .tasks
            .read()
            .get(&id)
            .cloned()
            .ok_or(SchedError::NoSuchTask)?;
        if task.load_state() == TaskState::Exited {
            return Err(SchedError::InvalidState);
        }
        // Record the class; every enqueue point (`wake_from_parked`, the
        // dispatch yield path, overflow drain, `yield_current`) reads it and
        // routes the task to the matching band, so a task adopts the class
        // the next time it is placed on a run queue — its next wake or
        // yield. The usual caller elevates itself while Running, so it is
        // strict-priority from its very next wake. A task already sitting in
        // a time-shared band keeps that slot until its next enqueue (the
        // Chase–Lev deque supports no arbitrary removal); this matches the
        // policy-neutral contract.
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

    /// Move `id` to the `priority` band, governing its next enqueue onward
    /// (see [`SchedulerPolicy::set_priority`]).
    ///
    /// MLFQ priorities are dynamic by definition: the demotion rule and
    /// the anti-starvation boost keep adjusting the band afterwards, so an
    /// external placement is the task's band *now*, not a pinned level —
    /// the starvation guarantee is never suspended to hold a task low.
    ///
    /// # Errors
    /// * [`SchedError::NoSuchTask`] if no task ever held that id.
    /// * [`SchedError::InvalidState`] if the task is terminal.
    pub fn set_priority(&self, id: TaskId, priority: Priority) -> SchedResult<()> {
        let task = self
            .tasks
            .read()
            .get(&id)
            .cloned()
            .ok_or(SchedError::NoSuchTask)?;
        if task.load_state() == TaskState::Exited {
            return Err(SchedError::InvalidState);
        }
        // Record the band with fresh yield residency; every enqueue point
        // reads it, and a task already sitting in a band keeps that slot
        // until its next enqueue (the Chase–Lev deque supports no
        // arbitrary removal), matching the policy-neutral contract.
        task.store_priority(priority);
        Ok(())
    }

    /// The current [`Priority`] band of `id`.
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

/// MLFQ implements the architecture-neutral [`SchedulerPolicy`] contract.
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

    fn yield_current(&self, id: TaskId) -> SchedResult<()> {
        Scheduler::yield_current(self, id)
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

    /// Killing a task that is **executing its body** on a remote CPU must
    /// defer teardown to that dispatch and nudge the CPU with a reschedule
    /// IPI, so a CPU-bound victim is knocked off its context promptly
    /// instead of running past its death — and is *not* reclaimed by the
    /// killer while it is still on-CPU (the wild-fault fix). A held body
    /// lock is what a live dispatch holds while a task runs, so it models
    /// "still executing" faithfully.
    #[test]
    fn exit_of_an_executing_victim_defers_and_ipis_without_reclaiming() {
        let (arch, sched) = mk(4);
        let id = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Exit)
            .expect("spawn");
        let task = sched.tasks.read().get(&id).cloned().expect("task present");
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
        let task = sched.tasks.read().get(&id).cloned().expect("task present");
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

    /// Exiting a task that is not the current task on any CPU sends no IPI:
    /// there is no running context to knock off.
    #[test]
    fn exit_of_non_running_task_sends_no_ipi() {
        let (arch, sched) = mk(4);
        let id = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Exit)
            .expect("spawn");
        let before: u64 = (0..4).map(|c| arch.ipi_count(c)).sum();
        sched.exit(id).expect("exit");
        let after: u64 = (0..4).map(|c| arch.ipi_count(c)).sum();
        assert_eq!(before, after, "no running context, so no reschedule IPI");
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

    /// The dispatch loop accumulates the ticks that elapse while a body
    /// runs, and an unknown id is refused rather than reported as zero.
    #[test]
    fn cpu_ticks_accumulate_across_dispatches() {
        let (arch, sched) = mk(1);
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
        let cfg = SchedulerConfig {
            cpus: 1,
            queue_capacity_per_band: 64,
            yields_before_demotion: 1,
            boost_interval_ticks: 1024,
        };
        let sched = Arc::new(Scheduler::new(cfg, arch.clone()).expect("sched"));
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

    /// Tickless preemption arming: a CPU running a
    /// sole runnable task disarms its one-shot timer, and a CPU with a
    /// second ready task arms it. Asserted through the `TestArch`
    /// `set_preemption` ledger.
    #[test]
    fn sole_task_disarms_and_a_competitor_arms_preemption() {
        let (arch, sched) = mk(1);
        arch.set_current_cpu(0);

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
        let _ = solo;
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
        // else (fail-closed).
        assert_eq!(sched.yield_current(id), Err(SchedError::InvalidState));
    }

    #[test]
    fn yield_current_rejects_unknown_task() {
        let (_arch, sched) = mk(1);
        assert_eq!(sched.yield_current(424_242), Err(SchedError::NoSuchTask));
    }

    // ---------------------------------------------------------------
    // Heterogeneous CPUs (performance + efficiency cores).
    // ---------------------------------------------------------------

    /// 4-CPU machine, CPUs 0/1 performance and 2/3 efficiency, with a
    /// short boost interval and eager demotion for deterministic tests.
    fn mk_hetero() -> (Arc<TestArch>, Scheduler<TestArch>) {
        let arch = Arc::new(TestArch::new(4).expect("arch"));
        arch.set_core_class(2, CoreClass::Efficiency);
        arch.set_core_class(3, CoreClass::Efficiency);
        let cfg = SchedulerConfig {
            cpus: 4,
            queue_capacity_per_band: 64,
            yields_before_demotion: 1,
            boost_interval_ticks: 2,
        };
        let sched = Scheduler::new(cfg, arch.clone()).expect("sched");
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
        // Spawn a Low (background) task with a performance CPU as the
        // caller hint; it must be steered onto an efficiency core.
        let id = sched
            .spawn(0, Priority::Low, |_| TaskAction::Park)
            .expect("spawn");
        assert_eq!(sched.class_of(home_of(&sched, id)), CoreClass::Efficiency);
    }

    #[test]
    fn interactive_task_is_placed_on_a_performance_core() {
        let (_arch, sched) = mk_hetero();
        // Spawn an interactive (High) task with an efficiency CPU as the
        // caller hint; it must be steered onto a performance core.
        let id = sched
            .spawn(3, Priority::High, |_| TaskAction::Park)
            .expect("spawn");
        assert_eq!(sched.class_of(home_of(&sched, id)), CoreClass::Performance);
    }

    #[test]
    fn unpark_resteers_a_background_task_by_priority() {
        let (_arch, sched) = mk_hetero();
        let id = sched
            .spawn(0, Priority::High, |_| TaskAction::Park)
            .expect("spawn");
        // Runs once: body parks it. It started on a performance core.
        assert_eq!(sched.class_of(home_of(&sched, id)), CoreClass::Performance);
        sched.step(home_of(&sched, id)).expect("step");
        assert_eq!(sched.state_of(id), TaskState::Parked);
        // Demote it to Low while parked, then wake it: the wake must
        // re-place it onto an efficiency core.
        sched
            .tasks
            .read()
            .get(&id)
            .expect("task")
            .priority
            .store(Priority::Low as u8, Ordering::Release);
        sched.unpark(id).expect("unpark");
        assert_eq!(sched.class_of(home_of(&sched, id)), CoreClass::Efficiency);
    }

    #[test]
    fn boost_promotes_idle_task_to_performance_then_demotes_back() {
        let (arch, sched) = mk_hetero();
        // A background task that keeps yielding (it "wants" turns).
        let runs = Arc::new(core::sync::atomic::AtomicU32::new(0));
        let r2 = runs.clone();
        let id = sched
            .spawn(0, Priority::Low, move |_| {
                if r2.fetch_add(1, Ordering::Relaxed) >= 8 {
                    TaskAction::Exit
                } else {
                    TaskAction::Yield
                }
            })
            .expect("spawn");
        // Starts demoted on an efficiency core.
        let start_home = home_of(&sched, id);
        assert_eq!(sched.class_of(start_home), CoreClass::Efficiency);

        // Fire the periodic boost: it promotes the task to High. Running
        // it once on its efficiency core then yields → demote High→Normal
        // → migrate *up* to a performance core (it needed power).
        arch.advance_ticks(10);
        arch.set_current_cpu(start_home);
        sched.step(start_home).expect("boosted step");
        let promoted_home = home_of(&sched, id);
        assert_eq!(
            sched.class_of(promoted_home),
            CoreClass::Performance,
            "a boosted background task migrates up to a performance core"
        );

        // Run it again on the performance core: Normal→Low demotion
        // settles it back *down* onto an efficiency core once idle again.
        arch.set_current_cpu(promoted_home);
        sched.step(promoted_home).expect("demote step");
        let settled_home = home_of(&sched, id);
        assert_eq!(
            sched.class_of(settled_home),
            CoreClass::Efficiency,
            "once demoted to Low the task returns to an efficiency core"
        );
    }

    #[test]
    fn homogeneous_machine_keeps_the_caller_home() {
        // With the default (all-performance) topology, a Low task must
        // stay on the CPU the caller named — the heterogeneous path is a
        // strict no-op when there are no efficiency cores.
        let (_arch, sched) = mk(4);
        let id = sched
            .spawn(2, Priority::Low, |_| TaskAction::Park)
            .expect("spawn");
        assert_eq!(home_of(&sched, id), 2);
    }
}
