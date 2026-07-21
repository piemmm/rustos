//! The kernel-side process-signal seam the `signal` (`abi-v1` number 64)
//! syscall uses (`plans/SPAWN.md` SP7).
//!
//! [`ProcessSignal`] is the one object-safe boundary between the
//! arch-neutral syscall handler in `kernel/core` and the scheduler-side
//! producer that delivers a control signal to one of the sender's children.
//! Like the [`ProcessWait`],
//! [`ArchImageBuilder`](crate::spawn::ArchImageBuilder), and
//! [`MemMap`](crate::memmap::MemMap) seams, the concrete producer is
//! installed at boot through the `with_process_signal` builder and the
//! handler reaches it through this trait.
//!
//! Until a producer is installed the handler holds [`NULL_PROCESS_SIGNAL`],
//! which fails closed: every `signal` returns [`Errno::NotImplemented`],
//! never pretending a signal was delivered — exactly as
//! [`NULL_PROCESS_WAIT`](crate::procwait::NULL_PROCESS_WAIT) does for
//! `wait`. The scheduler-side producer that actually delivers the signal is
//! [`KernelProcessSignal`] (`plans/SPAWN.md` `SP7b`), installed at boot in
//! place of the fail-closed floor.

use alloc::collections::{BTreeMap, BTreeSet};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use tairix_abi::{Errno, Signal};
use tairix_kernel_sched_api::{ExitDisposition, SchedError, SchedulerArch, SchedulerPolicy};
use tairix_kernel_sec::TaskId;
use tairix_sync::once::OnceCell;
use tairix_sync::SpinLock;

use crate::procwait::{KernelProcessWait, ProcessWait};

/// Scheduler task ids currently stopped by [`Signal::Stop`].
///
/// The scheduler's park/unpark state is shared with every blocking wait, so
/// a stopped task could otherwise be resumed by any broadcast wake (a
/// console byte waking all parked readers). This set is the stop overlay:
/// the kthread dispatch shim re-parks a task found here instead of running
/// it, so only an explicit [`Signal::Continue`] (which clears the entry)
/// genuinely resumes it. Grows with the number of concurrently stopped
/// jobs, never a fixed ceiling.
static STOPPED_TASKS: SpinLock<BTreeSet<u64>> = SpinLock::new(BTreeSet::new());

/// Whether `task` is currently stopped by [`Signal::Stop`].
///
/// Consulted by the kthread dispatch shim on every dispatch of the task, so
/// a spurious wake (a broadcast waitq drain) re-parks a stopped task rather
/// than running it.
#[must_use]
pub fn task_is_stopped(task: u64) -> bool {
    STOPPED_TASKS.lock().contains(&task)
}

/// Per-task signal-intake state (`plans/STRESSTEST.md` ST3): key present
/// means the task opted in through `signal_intake` (`SignalIntakeOp::Enable`),
/// and the value is its **one** pending observed termination-request signal
/// (`Interrupt`/`Terminate`), `None` while nothing is pending.
///
/// A single slot, not a queue, by design: while one observed signal is
/// pending undrained, a second termination-request signal **escalates to
/// the default terminate path** ([`try_intake`] declines it), so an
/// opted-in process that stops draining stays killable with a plain
/// `^C ^C`. Entries are cleared on task teardown ([`clear_intake`], driven
/// by the shared reclaim) and never inherited: a fresh task id is never in
/// the map. Grows with the number of concurrently opted-in tasks, never a
/// fixed ceiling.
static SIGNAL_INTAKE: SpinLock<BTreeMap<u64, Option<Signal>>> = SpinLock::new(BTreeMap::new());

/// Opt `task` into observable delivery of its own `Interrupt`/`Terminate`.
/// Idempotent: enabling an already-enabled intake keeps its pending slot
/// (a recorded signal is never discarded by a re-enable).
pub fn intake_enable(task: u64) {
    SIGNAL_INTAKE.lock().entry(task).or_insert(None);
}

/// Restore `task`'s default terminate disposition.
///
/// Idempotent: already disabled is success. Refused with
/// [`Errno::WouldBlock`] while an observed signal is pending undrained — a
/// recorded termination request is never silently discarded; the caller
/// drains it ([`intake_take`]) and acts on it first.
///
/// # Errors
///
/// [`Errno::WouldBlock`] when a pending observed signal is undrained.
pub fn intake_disable(task: u64) -> Result<(), Errno> {
    let mut intake = SIGNAL_INTAKE.lock();
    match intake.get(&task) {
        Some(Some(_)) => Err(Errno::WouldBlock),
        Some(None) => {
            intake.remove(&task);
            Ok(())
        }
        None => Ok(()),
    }
}

/// Drain `task`'s one pending observed signal.
///
/// # Errors
///
/// [`Errno::NotFound`] when the intake was never enabled;
/// [`Errno::WouldBlock`] when nothing is pending (the intake stays
/// enabled — the caller parks on its wait-set member, never a poll loop).
pub fn intake_take(task: u64) -> Result<Signal, Errno> {
    match SIGNAL_INTAKE.lock().get_mut(&task) {
        Some(pending) => pending.take().ok_or(Errno::WouldBlock),
        None => Err(Errno::NotFound),
    }
}

/// Whether `task` has opted into signal observation — the `waitset_ctl`
/// add-time check for a `WaitSourceKind::Signal` member (without the
/// opt-in there is no intake to observe).
#[must_use]
pub fn intake_enabled(task: u64) -> bool {
    SIGNAL_INTAKE.lock().contains_key(&task)
}

/// Whether `task` has an observed signal pending undrained — the
/// non-consuming `waitset_wait` readiness peek for a
/// `WaitSourceKind::Signal` member (the woken owner drains through
/// [`intake_take`], never the wait).
#[must_use]
pub fn intake_ready(task: u64) -> bool {
    matches!(SIGNAL_INTAKE.lock().get(&task), Some(Some(_)))
}

/// Drop `task`'s intake state on teardown. Idempotent; driven by the one
/// shared task-reclaim path so an exited or killed task leaves no stale
/// opt-in or pending slot behind (task ids are never reused, so a leaked
/// entry could never be reclaimed later).
pub fn clear_intake(task: u64) {
    SIGNAL_INTAKE.lock().remove(&task);
}

/// The kill gate: which tasks are currently inside a syscall dispatch,
/// and the termination signal deferred against each of them.
///
/// A task inside a syscall may hold kernel state only its own unwind can
/// release — a mount's `SleepLock`, an in-flight block-I/O descriptor the
/// device is still writing, heap allocations owned by handler stack
/// frames. Destroying it mid-flight leaks that state: the observed shape
/// is a killed writer leaving its volume's lock held forever, deadlocking
/// every later filesystem call on that mount. The terminate path
/// therefore never ends an in-syscall task directly; it records the
/// signal here and wakes the task, and the syscall dispatch boundary
/// lands the kill once the handler has unwound and released everything
/// it held. Both maps grow with live tasks, never a fixed ceiling; the
/// shared task reclaim clears a dead task's entries.
struct KillGate {
    /// Tasks currently between [`syscall_enter`] and
    /// [`syscall_exit_take_kill`] — i.e. inside a syscall dispatch,
    /// parked or running.
    in_syscall: BTreeSet<u64>,
    /// The first termination signal deferred against each in-syscall
    /// task. First request wins: a later `Kill` against an
    /// already-doomed task changes nothing (it is already dying at the
    /// same boundary), matching the immediate path where a second
    /// signal finds the child already gone.
    pending: BTreeMap<u64, Signal>,
}

/// The one kill-gate instance shared by the signal producer and the
/// syscall dispatch boundary.
static KILL_GATE: SpinLock<KillGate> = SpinLock::new(KillGate {
    in_syscall: BTreeSet::new(),
    pending: BTreeMap::new(),
});

/// Mark `task` as inside a syscall dispatch. Paired with
/// [`syscall_exit_take_kill`] by the dispatch hook around every syscall.
///
/// If a termination was deferred against this task while it was running in
/// user mode ([`defer_running_kill`]) and it has now entered a syscall, the
/// kill is migrated into the in-syscall gate here so the syscall boundary
/// lands it after the handler unwinds — the handler may take locks the
/// running-kill (dispatch-loop) path must never reclaim under. Taken before
/// the gate lock so the two maps are never locked nested.
pub fn syscall_enter(task: u64) {
    let migrated = take_running_kill(task);
    match migrated {
        Some(DeferredTeardown::Signalled(signal)) => {
            let mut gate = KILL_GATE.lock();
            gate.in_syscall.insert(task);
            gate.pending.entry(task).or_insert(signal);
        }
        // A plain (driver-unload) teardown carries no signal for the kill
        // gate; leave it deferred for the dispatch loop, which reclaims once
        // the driver's dispatch retires it (honouring any `Park` in between
        // so handler state is never reclaimed under).
        Some(DeferredTeardown::Plain) => {
            insert_deferred(task, DeferredTeardown::Plain);
            KILL_GATE.lock().in_syscall.insert(task);
        }
        None => {
            KILL_GATE.lock().in_syscall.insert(task);
        }
    }
}

/// Mark `task` as leaving its syscall dispatch and take any termination
/// deferred against it while it was inside.
///
/// `Some(signal)` obliges the caller to land the kill now: the handler
/// has unwound (every lock and buffer it held is released), so this is
/// the first safe point the task can die at.
#[must_use]
pub fn syscall_exit_take_kill(task: u64) -> Option<Signal> {
    let mut gate = KILL_GATE.lock();
    gate.in_syscall.remove(&task);
    gate.pending.remove(&task)
}

/// Whether a termination is deferred against `task`.
///
/// The in-kernel park loops consult this after every wake and unwind
/// with `Errno::Interrupted` instead of re-parking, so a doomed task
/// reaches its syscall boundary promptly rather than sleeping on as an
/// unkillable waiter. The errno never reaches user space — the boundary
/// lands the kill first.
#[must_use]
pub fn kill_pending(task: u64) -> bool {
    KILL_GATE.lock().pending.contains_key(&task)
}

/// Drop `task`'s kill-gate state on teardown. Idempotent; driven by the
/// one shared task-reclaim path exactly like [`clear_intake`], so a task
/// that exits on its own while a kill is deferred against it leaves no
/// stale entry behind (task ids are never reused).
pub fn clear_kill_gate(task: u64) {
    let mut gate = KILL_GATE.lock();
    gate.in_syscall.remove(&task);
    gate.pending.remove(&task);
}

/// Terminations deferred against tasks that were **executing in user mode**
/// (not inside a syscall) when the kill was requested.
///
/// The kill gate above covers a task inside a syscall handler. A task
/// running in EL0/user mode is a *different* deferral case: it holds no
/// handler state, but it is still physically executing on another CPU, so
/// reclaiming its address space now would turn its own legitimate accesses
/// into wild faults. The scheduler reports such a victim
/// [`ExitDisposition::Deferred`]; the signal producer records the pending
/// termination here and the dispatch loop lands it
/// ([`land_running_kill`]) once the owning dispatch has retired the task
/// (the scheduler already IPI'd that CPU). Grows with live tasks, never a
/// fixed ceiling; the shared task reclaim clears a dead task's entry.
static RUNNING_KILLS: SpinLock<BTreeMap<u64, DeferredTeardown>> = SpinLock::new(BTreeMap::new());

/// Count of entries in [`RUNNING_KILLS`], so the dispatch loop's
/// per-dispatch [`land_running_kill`] check is a single relaxed atomic read
/// on the hot path and only takes the lock when a teardown is actually
/// pending (deferrals are rare — only during teardown).
static PENDING_RUNNING_KILLS: AtomicUsize = AtomicUsize::new(0);

/// What teardown a still-executing task owes once its owning dispatch
/// retires it. Both kinds reclaim the task's kernel resources; they differ
/// only in whether a parent's `wait` is also given a signalled-exit status.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DeferredTeardown {
    /// A terminating signal was delivered: record `128 + n` for the
    /// parent's `wait` **and** reclaim (the signal-terminate path).
    Signalled(Signal),
    /// A non-signal teardown (an unloaded driver whose process was still
    /// executing): reclaim its kernel resources only, no `wait` reap.
    Plain,
}

/// Record a termination deferred against `task` while it was executing in
/// user mode. First request wins: a later signal against an already-doomed
/// task changes nothing (it is already dying at the same rendezvous),
/// matching the immediate and in-syscall paths.
pub fn defer_running_kill(task: u64, signal: Signal) {
    insert_deferred(task, DeferredTeardown::Signalled(signal));
}

/// Record a non-signal teardown deferred against `task` (an unloaded driver
/// still executing on another CPU): the dispatch loop reclaims its kernel
/// resources once the owning dispatch retires it, without a `wait` reap.
pub fn defer_plain_reclaim(task: u64) {
    insert_deferred(task, DeferredTeardown::Plain);
}

/// Insert a deferred teardown, first-request-wins, keeping the pending
/// counter in step.
fn insert_deferred(task: u64, teardown: DeferredTeardown) {
    // First request wins: an existing deferral is never overwritten, so a
    // later kill against an already-doomed task changes neither the recorded
    // teardown nor the count.
    if let alloc::collections::btree_map::Entry::Vacant(slot) = RUNNING_KILLS.lock().entry(task) {
        slot.insert(teardown);
        PENDING_RUNNING_KILLS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Take (remove) any teardown deferred against `task`, returning it if
/// present.
fn take_running_kill(task: u64) -> Option<DeferredTeardown> {
    let mut map = RUNNING_KILLS.lock();
    let taken = map.remove(&task);
    if taken.is_some() {
        PENDING_RUNNING_KILLS.fetch_sub(1, Ordering::Relaxed);
    }
    taken
}

/// Drop `task`'s deferred running-kill on teardown. Idempotent; driven by
/// the one shared task-reclaim path exactly like [`clear_kill_gate`], so a
/// task that exits on its own (or faults) while a user-mode kill was
/// deferred against it leaves no stale entry the dispatch loop could act on
/// twice.
pub fn clear_running_kill(task: u64) {
    let _ = take_running_kill(task);
}

/// The seam through which the dispatch loop lands a deferred running-kill:
/// records the signalled-exit status so the parent's `wait` reaps it and
/// reclaims the task's kernel resources — the same reap+reclaim the
/// immediate terminate path performs, but only *after* the task has
/// stopped executing. Installed per boot by [`install_deferred_kill_lander`]
/// (the leaked [`KernelProcessSignal`] holds both the wait producer and the
/// reclaim seam), so the free-function dispatch loop can drive it without
/// borrowing the producer directly.
pub trait DeferredKillLander: Sync {
    /// Land the deferred `teardown` against the now-quiescent `task`.
    /// Idempotent from the caller's side (the deferral is taken from the
    /// running-kill set exactly once before this is called).
    fn land_deferred_teardown(&self, task: TaskId, teardown: DeferredTeardown);
}

/// The one deferred-kill lander shared by the dispatch loop.
static DEFERRED_KILL_LANDER: OnceCell<&'static (dyn DeferredKillLander + 'static)> =
    OnceCell::new();

/// Error returned when [`install_deferred_kill_lander`] is called twice.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct DeferredKillLanderAlreadyInstalled;

/// Publish the [`DeferredKillLander`] the dispatch loop drives (the boot
/// path's leaked signal producer). Set-once: a second call fails closed
/// rather than re-pointing the live seam.
///
/// # Errors
///
/// [`DeferredKillLanderAlreadyInstalled`] if a lander was already installed.
pub fn install_deferred_kill_lander(
    lander: &'static (dyn DeferredKillLander + 'static),
) -> Result<(), DeferredKillLanderAlreadyInstalled> {
    DEFERRED_KILL_LANDER
        .set(lander)
        .map_err(|_| DeferredKillLanderAlreadyInstalled)
}

/// Land any termination deferred against `task` now that its owning
/// dispatch has retired it (the task ran once and returned to the dispatch
/// loop, so it is executing nowhere and holds no handler state). Called
/// from the dispatch loop after every dispatched task.
///
/// The common case — a task that was not killed while running — is a single
/// relaxed atomic read of the pending-teardown counter and no lock, so the
/// hot path pays almost nothing. Idempotent: the pending signal is taken
/// exactly once, and a self-exit/fault teardown already cleared the entry
/// ([`clear_running_kill`]), so a normal exit lands nothing.
pub fn land_running_kill(task: u64) {
    if PENDING_RUNNING_KILLS.load(Ordering::Relaxed) == 0 {
        return;
    }
    // A lander must be installed before any deferred kill can be recorded;
    // fail closed (leave the entry) rather than dropping the kill if not.
    let Ok(Some(lander)) = DEFERRED_KILL_LANDER.get() else {
        return;
    };
    if let Some(teardown) = take_running_kill(task) {
        lander.land_deferred_teardown(TaskId(task), teardown);
    }
}

/// Try to record a termination-request `signal` as `target`'s observable
/// pending event instead of terminating it.
///
/// Returns `true` when the signal was recorded (the delivery is complete:
/// the target's wait-set waiter is woken and will drain it). Returns
/// `false` when the default terminate path must run instead — the target
/// never opted in, or its pending slot is already occupied (the
/// escalation rule: a second undrained termination request kills, so an
/// unresponsive opted-in program stays terminable with plain `^C ^C`).
///
/// Only `Interrupt`/`Terminate` are ever offered here; `Kill` is
/// unconditionally fatal and unmaskable, so no caller routes it through
/// the intake. `pub(crate)` solely so the `waitset_wait` host tests can
/// stage a pending observation; production deliveries flow only through
/// [`KernelProcessSignal`].
pub(crate) fn try_intake(target: u64, signal: Signal) -> bool {
    let recorded = match SIGNAL_INTAKE.lock().get_mut(&target) {
        Some(pending @ None) => {
            *pending = Some(signal);
            true
        }
        Some(Some(_)) | None => false,
    };
    if recorded {
        // Wake the target's parked wait-set waiter (if any) outside the
        // intake lock; the wake is targeted, so unrelated waiters sleep on.
        crate::waitq::signal_intake_wake(target);
    }
    recorded
}

/// The one place a foreground `^C`/`^Z` signal is actually delivered.
///
/// Implemented by the scheduler-side signal producer and installed at boot
/// beside `with_process_signal`; the console line discipline reaches it only
/// through [`queue_foreground_signal`] / [`drain_pending_foreground`], never
/// directly, because the queueing side may run in interrupt context where
/// scheduler locks must not be taken.
pub trait ForegroundSignal: Sync {
    /// Deliver `signal` to the console's recorded foreground task.
    ///
    /// The authority was established when the parent marked the task
    /// foreground through `console_foreground` (a live child of the caller
    /// on that console); by delivery time the task may already have exited,
    /// in which case the delivery fails closed with [`Errno::NotFound`] and
    /// signals no one — task ids are never reused, so a stale target can
    /// never resolve to a different task.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] when the target no longer exists;
    /// [`Errno::OutOfRange`] for a signal the line discipline never maps.
    fn deliver(&self, target: TaskId, signal: Signal) -> Result<(), Errno>;
}

/// The boot-installed [`ForegroundSignal`] hook (set-once per boot).
static FOREGROUND_SIGNAL: OnceCell<&'static (dyn ForegroundSignal + 'static)> = OnceCell::new();

/// Error returned when [`install_foreground_signal`] is called twice.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct ForegroundSignalAlreadyInstalled;

/// Publish the foreground-signal producer the console line discipline
/// delivers through. Called once by the boot path, beside
/// `with_process_signal`.
///
/// # Errors
///
/// [`ForegroundSignalAlreadyInstalled`] if a producer was already published.
pub fn install_foreground_signal(
    hook: &'static (dyn ForegroundSignal + 'static),
) -> Result<(), ForegroundSignalAlreadyInstalled> {
    FOREGROUND_SIGNAL
        .set(hook)
        .map_err(|_| ForegroundSignalAlreadyInstalled)
}

/// Whether a foreground-signal producer has been installed.
///
/// The console line discipline consults this before consuming a `^C`/`^Z`
/// byte: with no producer the byte flows to the reader unchanged (the inert
/// pre-install behaviour) rather than being swallowed with no one to act.
#[must_use]
pub fn foreground_signal_installed() -> bool {
    matches!(FOREGROUND_SIGNAL.get(), Ok(Some(_)))
}

/// Reclaims every kernel-held resource of a task that terminated without
/// running its own `exit` syscall — the seam through which the
/// signal-terminate path drives the same
/// `KernelSyscallHandlers::reclaim_task_resources` the `exit` handler
/// runs (IRQ bindings, served call endpoints, shared memory, wait-sets,
/// console foreground ownership, the capability record, and the
/// address-space registry entry whose open pipe ends wake their parked
/// peers). One definition of task teardown, two death paths. Installed
/// per producer instance ([`KernelProcessSignal::install_task_reclaim`])
/// rather than through a process-global slot, so each host-test fixture
/// observes only its own terminations.
pub trait TaskReclaim: Sync {
    /// Release every kernel-held resource of the dead task `task` (its
    /// scheduler/security id). Idempotent: reclaiming an already-reclaimed
    /// or never-registered task is a no-op.
    fn reclaim(&self, task: u64);
}

/// Error returned when [`KernelProcessSignal::install_task_reclaim`] is
/// called more than once on the same producer.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct TaskReclaimAlreadyInstalled;

/// The one pending foreground signal, packed `(task id << 32) | signal`.
///
/// `0` means empty (a defined signal discriminant is never `0`). A single
/// slot, not a queue: a later `^C`/`^Z` typed before the previous one was
/// delivered simply replaces it, which matches what the keystrokes mean —
/// the newest request wins, and a terminated target makes the older one
/// moot. Written from interrupt context with one atomic store (no lock),
/// drained in dispatcher context where scheduler locks are safe.
static PENDING_FOREGROUND: AtomicU64 = AtomicU64::new(0);

/// Record a foreground signal for delivery at the next dispatcher-context
/// drain ([`drain_pending_foreground`]).
///
/// Interrupt-safe: a single atomic store, no locks — the same discipline as
/// `waitq`'s deferred wakes, because the UART RX handler that maps `^C`
/// runs in interrupt context where taking scheduler locks could deadlock
/// against the interrupted task. A target id that does not fit the packed
/// slot is refused (fail closed) — scheduler ids stay well below that.
pub fn queue_foreground_signal(target: TaskId, signal: Signal) {
    let Ok(narrow) = u32::try_from(target.0) else {
        return;
    };
    let packed = (u64::from(narrow) << 32) | u64::from(signal.as_u32());
    PENDING_FOREGROUND.store(packed, Ordering::Release);
}

/// Non-consuming peek: whether a foreground signal (`^C`/`^Z`) is queued
/// awaiting its dispatcher-context [`drain_pending_foreground`].
///
/// The preemption gate consults this so a timer tick on a lone-task CPU
/// still reschedules when a queued signal needs delivering — the delivery
/// only runs once the dispatch loop regains control.
#[must_use]
pub fn has_pending_foreground() -> bool {
    PENDING_FOREGROUND.load(Ordering::Acquire) != 0
}

/// Deliver the pending foreground signal, if any, through the installed
/// [`ForegroundSignal`] producer.
///
/// Called from the dispatch loop between task dispatches (the same slot
/// `drain_pending_wakes` runs in), where taking scheduler locks is safe.
/// Returns `true` when a delivery was attempted, so the idle path knows
/// work happened. A failed delivery (the target already exited) is dropped:
/// the signal has no one left to go to, and ids are never reused.
pub fn drain_pending_foreground() -> bool {
    let packed = PENDING_FOREGROUND.swap(0, Ordering::Acquire);
    if packed == 0 {
        return false;
    }
    let Ok(Some(hook)) = FOREGROUND_SIGNAL.get() else {
        // No producer installed (or the cell poisoned): nothing can be
        // delivered — fail closed. The slot is already cleared, never
        // retried into a later boot phase.
        return false;
    };
    let target = TaskId(packed >> 32);
    // The packed low word was a defined discriminant when stored; decode
    // fail-closed anyway rather than trusting the round-trip.
    #[allow(clippy::cast_possible_truncation)] // Low 32 bits by construction.
    let Ok(signal) = Signal::from_u32(packed as u32) else {
        return false;
    };
    let _ = hook.deliver(target, signal);
    true
}

/// The kernel-side producer of the `signal` syscall.
///
/// Implemented by the scheduler-side producer that authorises the target
/// against the sender's own children (a process may signal only children it
/// spawned) and delivers the control signal. Implementations must be [`Sync`]:
/// the single installed producer is shared by the per-CPU syscall handlers,
/// exactly like the process-wait producer, the spawn producer, and the
/// console device.
pub trait ProcessSignal: Sync {
    /// Deliver `signal` to the child selected by `pid` on behalf of `sender`.
    ///
    /// `sender` is the kernel-attested identity of the calling task (supplied
    /// by the dispatcher, never by the caller), and `pid` names a child the
    /// sender spawned. The implementation validates the parent/child
    /// relationship — a process may signal only its **own** children — and
    /// fails closed.
    ///
    /// # Errors
    ///
    /// Returns [`Errno::NotFound`] when `pid` does not name a child of
    /// `sender`. The default producer ([`NullProcessSignal`]) returns
    /// [`Errno::NotImplemented`] to mark an inert interface.
    fn signal(&self, sender: TaskId, pid: i32, signal: Signal) -> Result<(), Errno>;
}

/// The process-signal producer installed before any real one exists.
///
/// Every `signal` fails closed with [`Errno::NotImplemented`] — the
/// fail-closed default, so a `signal` issued before the boot path installs
/// the scheduler-side producer announces an inert interface rather than
/// pretending a signal was delivered.
#[derive(Debug, Default, Copy, Clone)]
pub struct NullProcessSignal;

impl ProcessSignal for NullProcessSignal {
    fn signal(&self, _sender: TaskId, _pid: i32, _signal: Signal) -> Result<(), Errno> {
        Err(Errno::NotImplemented)
    }
}

/// The shared [`NullProcessSignal`] instance the syscall handler defaults to.
///
/// `KernelSyscallHandlers::new` points its `process_signal` borrow here so
/// the field is always valid without an `Option` branch on the hot path; the
/// boot path replaces it with the concrete producer through
/// `KernelSyscallHandlers::with_process_signal`.
pub static NULL_PROCESS_SIGNAL: NullProcessSignal = NullProcessSignal;

/// The scheduler-side `signal` producer the boot path installs
/// (`plans/SPAWN.md` `SP7b`).
///
/// It carries **no bookkeeping of its own**: it authorises the target and
/// records a signalled termination through the one
/// [`KernelProcessWait`] that already
/// owns the parent/child + exit-status table (the `wait` producer), so the
/// two syscalls share a single source of truth for who parents whom and how
/// a child's terminal status is reported. Delivery itself drives the
/// scheduler directly — the only new capability signalling needs over
/// `wait` — through the [`SchedulerPolicy`] contract:
///
/// * [`Signal::Continue`] resumes a stopped child ([`SchedulerPolicy::unpark`],
///   clearing its stop overlay and any unreported stop);
/// * [`Signal::Terminate`] / [`Signal::Kill`] / [`Signal::Interrupt`]
///   terminate the child ([`SchedulerPolicy::exit`]) and record the
///   signal's POSIX-familiar termination status so the parent's `wait`
///   reaps it;
/// * [`Signal::Stop`] parks the child ([`SchedulerPolicy::park`]), marks it
///   in the stop overlay so no broadcast wake resumes it, and records the
///   stop so a `WaitFlags::STOPPED` wait observes it.
///
/// `P` is the concrete scheduler policy (the `SchedulerPolicy` methods take
/// generic bodies, so the contract is not object-safe and cannot be held as
/// `&dyn`); only `kernel/core` names it, keeping the rest of the kernel
/// policy-agnostic. The producer holds `'static` borrows of both the wait
/// producer and the scheduler, exactly like the other boot-installed seams.
pub struct KernelProcessSignal<A, P>
where
    A: SchedulerArch + Send + Sync + 'static,
    P: SchedulerPolicy<A> + Send + Sync + 'static,
{
    /// The `wait` producer that owns the parent/child bookkeeping this
    /// producer authorises and records against — never a second copy.
    wait: &'static KernelProcessWait<A>,
    /// The live scheduler this producer drives to deliver a signal
    /// (unpark / exit the target task).
    scheduler: &'static P,
    /// The boot-installed [`TaskReclaim`] seam a terminating signal drives
    /// (set-once; the dispatch hook is leaked *after* this producer is
    /// built, so the reference arrives through
    /// [`Self::install_task_reclaim`] rather than the constructor). Unset
    /// — a host fixture of the signal bookkeeping alone — reclaims
    /// nothing: such a build registered no kernel resources either.
    reclaim: OnceCell<&'static (dyn TaskReclaim + 'static)>,
}

impl<A, P> KernelProcessSignal<A, P>
where
    A: SchedulerArch + Send + Sync + 'static,
    P: SchedulerPolicy<A> + Send + Sync + 'static,
{
    /// Build a producer that authorises and records against `wait` and
    /// delivers through `scheduler`.
    #[must_use]
    pub const fn new(wait: &'static KernelProcessWait<A>, scheduler: &'static P) -> Self {
        Self {
            wait,
            scheduler,
            reclaim: OnceCell::new(),
        }
    }

    /// Publish the [`TaskReclaim`] seam a terminating signal drives (the
    /// boot path's leaked dispatch hook). Set-once per producer: a second
    /// call fails closed rather than re-pointing the live seam.
    ///
    /// # Errors
    ///
    /// [`TaskReclaimAlreadyInstalled`] if a seam was already installed.
    pub fn install_task_reclaim(
        &self,
        hook: &'static (dyn TaskReclaim + 'static),
    ) -> Result<(), TaskReclaimAlreadyInstalled> {
        self.reclaim
            .set(hook)
            .map_err(|_| TaskReclaimAlreadyInstalled)
    }

    /// Resume a stopped child ([`Signal::Continue`]).
    ///
    /// A continue delivered to a child that is not actually stopped is a
    /// harmless no-op — matching the long-standing Unix behaviour where
    /// continuing a running process succeeds without effect — so an
    /// [`SchedError::InvalidState`] from `unpark` is folded to `Ok`. A child
    /// the scheduler no longer knows (it exited between authorisation and
    /// delivery) fails closed with [`Errno::NotFound`].
    fn resume(&self, child: TaskId) -> Result<(), Errno> {
        // Lift the stop overlay *before* the unpark, so the dispatch that
        // the unpark makes possible finds the task runnable rather than
        // re-parking it.
        STOPPED_TASKS.lock().remove(&child.0);
        // The resume also clears any stop the parent never observed: a
        // stale "stopped" report after the child is running again would
        // mislead the job table.
        self.wait.record_continue(child);
        match self.scheduler.unpark(child.0) {
            Ok(()) | Err(SchedError::InvalidState) => Ok(()),
            Err(_) => Err(Errno::NotFound),
        }
    }

    /// Stop a child without terminating it ([`Signal::Stop`]).
    ///
    /// Marks the child in the stop overlay *first* — so a wake racing the
    /// park cannot slip it back onto a CPU — then parks it and records the
    /// stop for a `WaitFlags::STOPPED` wait. A child the scheduler no
    /// longer knows fails closed with [`Errno::NotFound`] and leaves no
    /// overlay entry behind.
    fn stop(&self, child: TaskId) -> Result<(), Errno> {
        STOPPED_TASKS.lock().insert(child.0);
        if self.scheduler.park(child.0).is_ok() {
            self.wait.record_stop(child, Signal::Stop);
            Ok(())
        } else {
            STOPPED_TASKS.lock().remove(&child.0);
            Err(Errno::NotFound)
        }
    }

    /// Terminate a child ([`Signal::Terminate`] / [`Signal::Kill`]).
    ///
    /// Drives the scheduler to end the task, then records the signal's
    /// termination status so the parent's `wait` reaps the child and reports
    /// `128 + n`. A child the scheduler no longer knows fails closed with
    /// [`Errno::NotFound`] and records nothing (never a fabricated zombie).
    ///
    /// A child that is not safe to reclaim *here* is never destroyed
    /// mid-flight; the reap+reclaim is deferred to the point the child
    /// reaches safely:
    ///
    /// * A child currently **inside a syscall** may hold kernel state only
    ///   its own unwind can release (a mount's `SleepLock`, an in-flight
    ///   block-I/O descriptor), so the kill is deferred through the kill
    ///   gate — recorded pending, the child woken out of any park so an
    ///   indefinite wait unwinds (`Errno::Interrupted`) — and the syscall
    ///   dispatch boundary lands it once the handler has unwound.
    /// * A child currently **executing in user mode on another CPU** holds
    ///   no handler state, but reclaiming its address space while its own
    ///   code still runs would turn a legitimate access into a wild fault.
    ///   The scheduler reports it [`ExitDisposition::Deferred`]; the kill is
    ///   recorded against the running-kill set and the dispatch loop lands
    ///   it once the owning dispatch has retired the task.
    ///
    /// Either way the delivery is complete from the caller's perspective:
    /// the child is doomed and the parent's `wait` reaps it when the exit is
    /// recorded. Only a child that is neither in a syscall nor executing
    /// ([`ExitDisposition::Quiesced`]) is reaped and reclaimed inline here.
    fn terminate(&self, child: TaskId, signal: Signal) -> Result<(), Errno> {
        {
            let mut gate = KILL_GATE.lock();
            if gate.in_syscall.contains(&child.0) {
                gate.pending.entry(child.0).or_insert(signal);
                drop(gate);
                // A stopped child must still die: lift its overlay entry so
                // the wake below runs it to its boundary instead of the
                // dispatch shim re-parking it forever.
                STOPPED_TASKS.lock().remove(&child.0);
                // Wake the child out of any in-kernel park. Every park loop
                // re-tests its condition after a wake and consults the kill
                // gate, so a spurious wake is harmless and a doomed waiter
                // unwinds promptly. `InvalidState` means the child is
                // runnable or running — it reaches its boundary by itself.
                match self.scheduler.unpark(child.0) {
                    Ok(()) | Err(SchedError::InvalidState) => return Ok(()),
                    Err(_) => return Err(Errno::NotFound),
                }
            }
        }
        // A stopped child can be killed: lift its overlay entry so the set
        // never accumulates entries for dead tasks, whichever disposition
        // the scheduler reports.
        match self.scheduler.exit(child.0) {
            Ok(ExitDisposition::Quiesced) => {
                STOPPED_TASKS.lock().remove(&child.0);
                self.land_termination(child, signal);
                Ok(())
            }
            Ok(ExitDisposition::Deferred) => {
                // The victim is still executing its body on another CPU.
                // Reclaiming its resources now would race its own
                // legitimate accesses into a wild fault, so defer the
                // reap+reclaim: record the pending termination and let the
                // dispatch loop land it once the owning dispatch has retired
                // the task (the scheduler already IPI'd that CPU). The
                // delivery is still complete from the caller's view — the
                // child is doomed and its `wait` reap follows.
                STOPPED_TASKS.lock().remove(&child.0);
                defer_running_kill(child.0, signal);
                Ok(())
            }
            Ok(ExitDisposition::AlreadyExited) => {
                // A prior termination already owns this task's teardown, or
                // it exited on its own between authorisation and here.
                // Reclaim runs exactly once, so there is nothing to do.
                STOPPED_TASKS.lock().remove(&child.0);
                Ok(())
            }
            Err(_) => Err(Errno::NotFound),
        }
    }

    /// Record the signalled-exit status and reclaim `child`'s kernel
    /// resources — the teardown a killed task never runs itself. The one
    /// definition shared by the immediate ([`ExitDisposition::Quiesced`])
    /// terminate arm and the deferred dispatch-loop landing
    /// ([`DeferredKillLander`]), so both death timings tear down identically.
    fn land_termination(&self, child: TaskId, signal: Signal) {
        // `termination_status` is `Some` for every terminating signal
        // (Terminate/Kill/Interrupt); this is never reached for
        // Continue or Stop.
        if let Some(status) = signal.termination_status() {
            self.wait.record_signalled_exit(child, status);
        }
        // The victim never runs its own `exit` handler, so drive the shared
        // teardown here: without it a killed task's capability record, IRQ
        // bindings, endpoints, and open files (pipe ends whose peers park
        // forever) would leak.
        if let Ok(Some(hook)) = self.reclaim.get() {
            hook.reclaim(child.0);
        }
    }
}

impl<A, P> DeferredKillLander for KernelProcessSignal<A, P>
where
    A: SchedulerArch + Send + Sync + 'static,
    P: SchedulerPolicy<A> + Send + Sync + 'static,
{
    fn land_deferred_teardown(&self, task: TaskId, teardown: DeferredTeardown) {
        // The task has returned to the dispatch loop and executes nowhere,
        // so the teardown deferred at request time is now safe.
        match teardown {
            // A signalled kill: the very reap+reclaim the immediate
            // terminate path runs, one definition.
            DeferredTeardown::Signalled(signal) => self.land_termination(task, signal),
            // A driver unload whose process was still executing: reclaim its
            // kernel resources only, with no `wait` reap (a driver is not a
            // waited-for child).
            DeferredTeardown::Plain => {
                if let Ok(Some(hook)) = self.reclaim.get() {
                    hook.reclaim(task.0);
                }
            }
        }
    }
}

impl<A, P> ProcessSignal for KernelProcessSignal<A, P>
where
    A: SchedulerArch + Send + Sync + 'static,
    P: SchedulerPolicy<A> + Send + Sync + 'static,
{
    fn signal(&self, sender: TaskId, pid: i32, signal: Signal) -> Result<(), Errno> {
        // Authorise before touching any scheduler state: a process may signal
        // only a live child it spawned. This shares the `wait`
        // producer's bookkeeping, so authority is decided in one place.
        let child = self.wait.authorise_child(sender, pid)?;
        match signal {
            Signal::Continue => self.resume(child),
            // A termination *request* is observable: an opted-in child with
            // a free pending slot records it instead of dying (the recorded
            // delivery is complete — the child's waiter is woken to drain
            // it). A child that never opted in, or whose slot is already
            // occupied (the escalation rule), terminates by default.
            Signal::Terminate | Signal::Interrupt => {
                if try_intake(child.0, signal) {
                    Ok(())
                } else {
                    self.terminate(child, signal)
                }
            }
            // `Kill` is unconditionally fatal and unmaskable: it is never
            // offered to the intake.
            Signal::Kill => self.terminate(child, signal),
            Signal::Stop => self.stop(child),
        }
    }
}

impl<A, P> ForegroundSignal for KernelProcessSignal<A, P>
where
    A: SchedulerArch + Send + Sync + 'static,
    P: SchedulerPolicy<A> + Send + Sync + 'static,
{
    fn deliver(&self, target: TaskId, signal: Signal) -> Result<(), Errno> {
        // No parent/child authorisation here: the authority was checked when
        // the parent marked the target foreground on its own console, and
        // the console line discipline is the kernel acting on the terminal
        // owner's standing instruction. The delivery itself still fails
        // closed on a target the scheduler no longer knows.
        match signal {
            // The foreground `^C` is a termination *request* like a
            // parent's `Terminate`: an opted-in target with a free pending
            // slot observes it; otherwise (never opted in, or a second `^C`
            // while one is pending undrained — the escalation rule) the
            // default terminate runs.
            Signal::Interrupt => {
                if try_intake(target.0, Signal::Interrupt) {
                    Ok(())
                } else {
                    self.terminate(target, Signal::Interrupt)
                }
            }
            Signal::Stop => self.stop(target),
            // The line discipline maps only `^C`/`^Z`; any other signal on
            // this path is a programming error refused outright.
            Signal::Continue | Signal::Terminate | Signal::Kill => Err(Errno::OutOfRange),
        }
    }
}

/// Serialises host tests that touch the process-global foreground state
/// ([`PENDING_FOREGROUND`], [`FOREGROUND_SIGNAL`]): the tests in this module
/// and the console line-discipline tests share one pending slot, so they
/// take this lock to keep their queue/drain sequences from interleaving.
#[cfg(test)]
pub(crate) fn foreground_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // A panicking holder does not corrupt the `()` state; continue.
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Serialises host tests that touch the process-global stopped-task overlay
/// ([`STOPPED_TASKS`]): it is keyed by numeric task id, and each test's own
/// leaked scheduler hands out the same small ids, so two tests signalling
/// "their" child in parallel would insert and remove each other's entries.
/// Every test that stops, continues, or terminates a child takes this lock.
#[cfg(test)]
pub(crate) fn stopped_overlay_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // A panicking holder does not corrupt the `()` state; continue.
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Serialises host tests that touch the process-global deferred-teardown
/// state ([`RUNNING_KILLS`], [`PENDING_RUNNING_KILLS`], and the once-set
/// [`DEFERRED_KILL_LANDER`]): they are keyed by numeric task id and share
/// one lander, so two tests deferring/landing "their" task in parallel
/// would race each other's entries.
#[cfg(test)]
pub(crate) fn running_kill_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // A panicking holder does not corrupt the `()` state; continue.
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Test-only: consume and decode the pending foreground slot, so the console
/// line-discipline tests can assert what the filter queued without invoking
/// whichever process-global hook another test may have installed.
#[cfg(test)]
pub(crate) fn take_pending_foreground_for_test() -> Option<(TaskId, Signal)> {
    let packed = PENDING_FOREGROUND.swap(0, Ordering::Acquire);
    if packed == 0 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)] // Low 32 bits by construction.
    Signal::from_u32(packed as u32)
        .ok()
        .map(|signal| (TaskId(packed >> 32), signal))
}

/// Test-only: whether some [`ForegroundSignal`] hook is installed, and if
/// not, install an inert one — so the console line-discipline tests always
/// run with the interception gate open, regardless of test ordering.
#[cfg(test)]
pub(crate) fn ensure_foreground_hook_for_test() {
    struct InertHook;
    impl ForegroundSignal for InertHook {
        fn deliver(&self, _target: TaskId, _signal: Signal) -> Result<(), Errno> {
            Ok(())
        }
    }
    static HOOK: InertHook = InertHook;
    let _ = install_foreground_signal(&HOOK);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::boxed::Box;

    use tairix_abi::{WaitFlags, WaitStatus};
    use tairix_kernel_sched_api::{Priority, SchedulerConfig, TaskAction};

    use crate::procwait::{ProcessWait, WaitedChild};
    use crate::sched::Scheduler;
    use crate::test_arch::TestArch;

    /// Build a leaked `'static` wait producer + live single-CPU scheduler for
    /// a producer-level test. The wait producer gets its own leaked
    /// [`TestArch`] (it only reads the current CPU), and the scheduler is a
    /// real [`Scheduler`] so `exit`/`unpark` exercise a genuine
    /// [`SchedulerPolicy`], not a fake double.
    fn scaffold() -> (
        &'static KernelProcessWait<TestArch>,
        &'static Scheduler<TestArch>,
    ) {
        let sched_arch = std::sync::Arc::new(TestArch::with_cpus(1));
        let scheduler =
            Scheduler::new(SchedulerConfig::defaults_for(1), sched_arch).expect("scheduler builds");
        let scheduler: &'static Scheduler<TestArch> = Box::leak(Box::new(scheduler));
        let wait_arch: &'static TestArch = Box::leak(Box::new(TestArch::with_cpus(1)));
        let wait: &'static KernelProcessWait<TestArch> =
            Box::leak(Box::new(KernelProcessWait::new(wait_arch)));
        (wait, scheduler)
    }

    /// Admit a task on `scheduler` and return its id as an `i32` pid, failing
    /// the test if the id does not fit (host ids always do).
    fn spawn_child(scheduler: &Scheduler<TestArch>) -> (u64, i32) {
        let id = scheduler
            .spawn(0, Priority::Normal, |_ctx| TaskAction::Exit)
            .expect("task admitted");
        (id, i32::try_from(id).expect("host task id fits i32"))
    }

    #[test]
    fn null_process_signal_fails_closed() {
        // Every variant of the closed signal set fails closed on the inert
        // default rather than pretending it was delivered.
        for signal in [
            Signal::Continue,
            Signal::Terminate,
            Signal::Kill,
            Signal::Interrupt,
            Signal::Stop,
        ] {
            assert_eq!(
                NULL_PROCESS_SIGNAL.signal(TaskId(1), 2, signal),
                Err(Errno::NotImplemented)
            );
        }
    }

    #[test]
    fn signalling_a_non_child_fails_closed() {
        let (wait, scheduler) = scaffold();
        let signaller = KernelProcessSignal::new(wait, scheduler);
        // A caller with no children signals nothing.
        assert_eq!(
            signaller.signal(TaskId(1), 2, Signal::Terminate),
            Err(Errno::NotFound)
        );
        // A live task that is not *this* caller's child is off-limits: task 9
        // may not signal task 7's child.
        let (child, child_pid) = spawn_child(scheduler);
        wait.register_child(TaskId(7), TaskId(child));
        assert_eq!(
            signaller.signal(TaskId(9), child_pid, Signal::Kill),
            Err(Errno::NotFound)
        );
        // The child was untouched by the denied signal.
        assert_eq!(scheduler.live_task_count(), 1);
    }

    #[test]
    fn terminate_ends_the_child_and_records_its_signalled_status() {
        let _overlay = stopped_overlay_test_lock();
        let (wait, scheduler) = scaffold();
        let (child, child_pid) = spawn_child(scheduler);
        wait.register_child(TaskId(7), TaskId(child));
        let signaller = KernelProcessSignal::new(wait, scheduler);

        assert_eq!(
            signaller.signal(TaskId(7), child_pid, Signal::Terminate),
            Ok(())
        );
        // The child was terminated on the scheduler.
        assert_eq!(scheduler.live_task_count(), 0);
        // ... and reaps with Terminate's POSIX-familiar 143 status, exactly
        // as if it had exited with that code itself.
        let pid = u32::try_from(child).expect("host task id fits u32");
        assert_eq!(
            wait.wait(TaskId(7), tairix_abi::WAIT_PID_ANY, WaitFlags::empty()),
            Ok(WaitedChild {
                pid,
                status: WaitStatus::Exited(143)
            })
        );
    }

    #[test]
    fn the_kill_gate_round_trips_enter_take_and_clear() {
        // Pure gate bookkeeping, on raw ids no scheduler-backed test uses.
        syscall_enter(0x00de_ad01);
        assert!(!kill_pending(0x00de_ad01));
        assert_eq!(syscall_exit_take_kill(0x00de_ad01), None);
        // Clearing an open window leaves nothing behind.
        syscall_enter(0x00de_ad02);
        clear_kill_gate(0x00de_ad02);
        assert_eq!(syscall_exit_take_kill(0x00de_ad02), None);
    }

    #[test]
    fn terminating_a_task_inside_a_syscall_defers_the_kill_to_its_boundary() {
        let _overlay = stopped_overlay_test_lock();
        let (wait, scheduler) = scaffold();
        let (child, child_pid) = spawn_child(scheduler);
        wait.register_child(TaskId(7), TaskId(child));
        let signaller = KernelProcessSignal::new(wait, scheduler);

        // The child is mid-syscall: its handler may hold kernel state only
        // its own unwind can release (a mount's `SleepLock`, an in-flight
        // block-I/O descriptor), so the kill must not land here — the
        // regression this pins down is a killed writer leaving its volume's
        // lock held forever, deadlocking every later filesystem call.
        syscall_enter(child);
        assert_eq!(signaller.signal(TaskId(7), child_pid, Signal::Kill), Ok(()));
        // The child was not destroyed mid-handler: it is still live on the
        // scheduler, the kill is pending against it, and the parent cannot
        // reap it yet.
        assert_eq!(scheduler.live_task_count(), 1);
        assert!(kill_pending(child));
        assert_eq!(
            wait.poll(TaskId(7), tairix_abi::WAIT_PID_ANY, WaitFlags::empty()),
            Err(Errno::WouldBlock)
        );
        // The syscall boundary takes the deferred kill exactly once.
        assert_eq!(syscall_exit_take_kill(child), Some(Signal::Kill));
        assert_eq!(syscall_exit_take_kill(child), None);
    }

    #[test]
    fn a_deferred_kill_keeps_the_first_termination_request() {
        let _overlay = stopped_overlay_test_lock();
        let (wait, scheduler) = scaffold();
        let (child, child_pid) = spawn_child(scheduler);
        wait.register_child(TaskId(7), TaskId(child));
        let signaller = KernelProcessSignal::new(wait, scheduler);

        syscall_enter(child);
        assert_eq!(
            signaller.signal(TaskId(7), child_pid, Signal::Terminate),
            Ok(())
        );
        // A follow-up `Kill` against the already-doomed child changes
        // nothing: it dies at the same boundary, with the first request's
        // status — matching the immediate path, where a second signal finds
        // the child already gone.
        assert_eq!(signaller.signal(TaskId(7), child_pid, Signal::Kill), Ok(()));
        assert_eq!(syscall_exit_take_kill(child), Some(Signal::Terminate));
        assert_eq!(scheduler.live_task_count(), 1);
    }

    /// Every task id the test [`TaskReclaim`] hook was handed — the
    /// witness that a terminate drives the shared task teardown
    /// (`plans/SPAWN.md` SP10: a killed pipeline member must release its
    /// open pipe ends, or its peer parks forever).
    static RECLAIMED: SpinLock<BTreeSet<u64>> = SpinLock::new(BTreeSet::new());

    struct RecordingReclaim;
    impl TaskReclaim for RecordingReclaim {
        fn reclaim(&self, task: u64) {
            RECLAIMED.lock().insert(task);
        }
    }

    /// A terminating signal drives the installed [`TaskReclaim`] seam
    /// with the victim's id — the kill-path half of the one task
    /// teardown the `exit` handler runs directly (regression: before the
    /// seam existed a killed task leaked its capability record, IRQ
    /// bindings, endpoints, and open files, and a pipe peer parked
    /// forever).
    #[test]
    fn terminate_drives_the_installed_task_reclaim() {
        let _overlay = stopped_overlay_test_lock();
        let (wait, scheduler) = scaffold();
        let (child, child_pid) = spawn_child(scheduler);
        wait.register_child(TaskId(7), TaskId(child));
        let signaller = KernelProcessSignal::new(wait, scheduler);
        signaller
            .install_task_reclaim(&RecordingReclaim)
            .expect("first install on this producer");

        assert_eq!(signaller.signal(TaskId(7), child_pid, Signal::Kill), Ok(()));
        assert!(RECLAIMED.lock().contains(&child));
    }

    /// Deferring a user-mode kill records it (first-request-wins) and the
    /// pending counter tracks it; `take` removes it and `clear` drops it
    /// without landing. Pure facility bookkeeping on raw ids.
    #[test]
    fn defer_take_and_clear_a_running_kill_round_trip() {
        let _g = running_kill_test_lock();
        // Distinct ids no scheduler-backed test hands out.
        let (a, b) = (0x00c0_ffe1, 0x00c0_ffe2);
        let _ = take_running_kill(a);
        let _ = take_running_kill(b);
        let before = PENDING_RUNNING_KILLS.load(Ordering::Relaxed);

        defer_running_kill(a, Signal::Kill);
        // First request wins: a later signal against the same task is ignored.
        defer_running_kill(a, Signal::Terminate);
        assert_eq!(
            PENDING_RUNNING_KILLS.load(Ordering::Relaxed),
            before + 1,
            "one entry, counted once despite the repeat"
        );
        assert_eq!(
            take_running_kill(a),
            Some(DeferredTeardown::Signalled(Signal::Kill)),
            "the first-recorded signal is taken"
        );
        assert_eq!(take_running_kill(a), None, "taken exactly once");
        assert_eq!(PENDING_RUNNING_KILLS.load(Ordering::Relaxed), before);

        // `clear_running_kill` drops an entry without landing it (the
        // self-exit/fault teardown path, so the dispatch loop never lands a
        // kill for an already-reclaimed task).
        defer_running_kill(b, Signal::Kill);
        clear_running_kill(b);
        assert_eq!(take_running_kill(b), None);
        assert_eq!(PENDING_RUNNING_KILLS.load(Ordering::Relaxed), before);
    }

    /// A user-mode kill that later enters a syscall is migrated into the
    /// in-syscall kill gate at `syscall_enter`, so the syscall boundary
    /// lands it after the handler unwinds — a signalled kill goes to the
    /// gate, a plain (driver-unload) teardown stays for the dispatch loop.
    #[test]
    fn syscall_enter_migrates_a_signalled_kill_but_keeps_a_plain_reclaim() {
        let _g = running_kill_test_lock();
        let (sig_id, plain_id) = (0x00c0_ffe3, 0x00c0_ffe4);
        let _ = take_running_kill(sig_id);
        let _ = take_running_kill(plain_id);

        // A signalled kill migrates into the gate and out of RUNNING_KILLS.
        defer_running_kill(sig_id, Signal::Terminate);
        syscall_enter(sig_id);
        assert_eq!(
            take_running_kill(sig_id),
            None,
            "migrated out of RUNNING_KILLS"
        );
        assert!(kill_pending(sig_id), "now owed at the syscall boundary");
        assert_eq!(syscall_exit_take_kill(sig_id), Some(Signal::Terminate));

        // A plain reclaim carries no signal for the gate; it stays deferred
        // for the dispatch loop.
        defer_plain_reclaim(plain_id);
        syscall_enter(plain_id);
        assert!(!kill_pending(plain_id), "no signal handed to the gate");
        assert_eq!(
            take_running_kill(plain_id),
            Some(DeferredTeardown::Plain),
            "still deferred for the dispatch loop"
        );
        clear_kill_gate(plain_id);
    }

    /// Tasks the deterministic test lander reclaimed, and the signalled
    /// statuses it reaped — kept as process-global sets so the `land`
    /// end-to-end tests share one installed lander regardless of test order
    /// (the global lander is set-once). Dedicated to these tests so they
    /// never collide with the immediate-terminate test's [`RECLAIMED`].
    static LAND_RECLAIMED: SpinLock<BTreeSet<u64>> = SpinLock::new(BTreeSet::new());
    static LAND_REAPED: SpinLock<BTreeMap<u64, i32>> = SpinLock::new(BTreeMap::new());

    /// A deterministic, self-contained [`DeferredKillLander`] for the
    /// dispatch-loop end-to-end tests: it records what it reclaimed and any
    /// signalled status it reaped, mirroring the real
    /// [`KernelProcessSignal`] lander's Signalled-vs-Plain split without
    /// depending on any one test's wait producer.
    struct TestLander;
    impl DeferredKillLander for TestLander {
        fn land_deferred_teardown(&self, task: TaskId, teardown: DeferredTeardown) {
            match teardown {
                DeferredTeardown::Signalled(signal) => {
                    if let Some(status) = signal.termination_status() {
                        LAND_REAPED.lock().insert(task.0, status);
                    }
                    LAND_RECLAIMED.lock().insert(task.0);
                }
                DeferredTeardown::Plain => {
                    LAND_RECLAIMED.lock().insert(task.0);
                }
            }
        }
    }
    static TEST_LANDER: TestLander = TestLander;

    /// Ensure some deferred-kill lander is installed and report whether it
    /// is our deterministic [`TestLander`]. The lander is a process-global
    /// set-once cell: another suite (a full-boot init test) may have
    /// installed the real producer first, in which case `land_running_kill`
    /// still *consumes* the deferral (the assertion every land test makes)
    /// but records its side effects elsewhere — so side-effect assertions
    /// are gated on this returning `true`.
    fn ensure_test_lander() -> bool {
        static TEST_LANDER_INSTALLED: AtomicUsize = AtomicUsize::new(0);
        if install_deferred_kill_lander(&TEST_LANDER).is_ok() {
            TEST_LANDER_INSTALLED.store(1, Ordering::Relaxed);
        }
        TEST_LANDER_INSTALLED.load(Ordering::Relaxed) == 1
    }

    /// The headline wild-fault regression, kernel/core half: a termination
    /// deferred while its target was executing is **not** landed at
    /// terminate time, and is landed only when the dispatch loop calls
    /// [`land_running_kill`] after the owning dispatch has retired the task
    /// — which *consumes* the deferral exactly once (idempotent thereafter).
    /// When our deterministic lander is the installed one, the reap+reclaim
    /// side effects are asserted too.
    #[test]
    fn a_deferred_running_kill_lands_only_after_the_task_retires() {
        let _g = running_kill_test_lock();
        let ours = ensure_test_lander();
        let child = 0x00c0_ffe5;
        let _ = take_running_kill(child);
        LAND_RECLAIMED.lock().remove(&child);
        LAND_REAPED.lock().remove(&child);

        // The scheduler reported the victim still-executing, so terminate
        // deferred rather than landing: the entry is pending and untouched.
        defer_running_kill(child, Signal::Kill);
        assert!(!LAND_RECLAIMED.lock().contains(&child));

        // The dispatch loop lands it once the task has retired (executes
        // nowhere), consuming the deferral exactly once.
        land_running_kill(child);
        assert_eq!(take_running_kill(child), None, "the deferral was consumed");
        if ours {
            assert!(
                LAND_RECLAIMED.lock().contains(&child),
                "the retired victim is reclaimed by the dispatch loop"
            );
            assert_eq!(LAND_REAPED.lock().get(&child), Some(&137));
        }

        // Idempotent: nothing remains, so a second land is a no-op.
        LAND_RECLAIMED.lock().remove(&child);
        land_running_kill(child);
        assert!(!LAND_RECLAIMED.lock().contains(&child));
    }

    /// A self-exit/fault teardown that ran [`clear_running_kill`] leaves the
    /// dispatch loop nothing to land, so a deferred kill is never applied
    /// twice against an already-reclaimed task.
    #[test]
    fn clearing_a_running_kill_stops_the_dispatch_loop_landing_it() {
        let _g = running_kill_test_lock();
        let ours = ensure_test_lander();
        let child = 0x00c0_ffe6;
        let _ = take_running_kill(child);
        LAND_RECLAIMED.lock().remove(&child);

        defer_running_kill(child, Signal::Kill);
        clear_running_kill(child);
        land_running_kill(child);
        assert_eq!(take_running_kill(child), None, "nothing left to land");
        if ours {
            assert!(
                !LAND_RECLAIMED.lock().contains(&child),
                "a cleared deferral is never landed"
            );
        }
    }

    /// A plain (driver-unload) deferral lands as a reclaim only — no reap,
    /// since a driver is not a waited-for child.
    #[test]
    fn a_deferred_plain_reclaim_lands_without_a_reap() {
        let _g = running_kill_test_lock();
        let ours = ensure_test_lander();
        let driver = 0x00c0_ffe7;
        let _ = take_running_kill(driver);
        LAND_RECLAIMED.lock().remove(&driver);
        LAND_REAPED.lock().remove(&driver);

        defer_plain_reclaim(driver);
        land_running_kill(driver);
        assert_eq!(take_running_kill(driver), None, "the deferral was consumed");
        if ours {
            assert!(
                LAND_RECLAIMED.lock().contains(&driver),
                "a plain deferral reclaims the driver's resources"
            );
            assert!(
                !LAND_REAPED.lock().contains_key(&driver),
                "a driver is not a waited-for child, so nothing is reaped"
            );
        }
    }

    /// The real [`KernelProcessSignal`] lander maps a deferred `Signalled`
    /// teardown to the same reap + reclaim the immediate terminate path runs
    /// (one definition), and a `Plain` teardown to a reclaim only.
    #[test]
    fn the_signal_producer_lander_reaps_and_reclaims() {
        let _overlay = stopped_overlay_test_lock();
        let (wait, scheduler) = scaffold();
        let (child, _pid) = spawn_child(scheduler);
        wait.register_child(TaskId(7), TaskId(child));
        let signaller = KernelProcessSignal::new(wait, scheduler);
        signaller
            .install_task_reclaim(&RecordingReclaim)
            .expect("first install on this producer");

        // Signalled: reclaim the resources and record the kill's 137 status.
        signaller.land_deferred_teardown(TaskId(child), DeferredTeardown::Signalled(Signal::Kill));
        assert!(RECLAIMED.lock().contains(&child));
        let pid = u32::try_from(child).expect("host task id fits u32");
        assert_eq!(
            wait.wait(TaskId(7), tairix_abi::WAIT_PID_ANY, WaitFlags::empty()),
            Ok(WaitedChild {
                pid,
                status: WaitStatus::Exited(137)
            })
        );

        // Plain: reclaim only, no reap (a second, unregistered driver id).
        let driver = scheduler
            .spawn(0, Priority::Normal, |_| TaskAction::Exit)
            .expect("driver task");
        signaller.land_deferred_teardown(TaskId(driver), DeferredTeardown::Plain);
        assert!(RECLAIMED.lock().contains(&driver));
        assert_eq!(
            wait.poll(TaskId(7), tairix_abi::WAIT_PID_ANY, WaitFlags::empty()),
            Err(Errno::NotFound),
            "a plain reclaim records no reap"
        );
    }

    #[test]
    fn kill_records_its_own_status() {
        let _overlay = stopped_overlay_test_lock();
        let (wait, scheduler) = scaffold();
        let (child, child_pid) = spawn_child(scheduler);
        wait.register_child(TaskId(7), TaskId(child));
        let signaller = KernelProcessSignal::new(wait, scheduler);

        assert_eq!(signaller.signal(TaskId(7), child_pid, Signal::Kill), Ok(()));
        let pid = u32::try_from(child).expect("host task id fits u32");
        // Kill surfaces as SIGKILL's familiar 137, distinct from Terminate.
        assert_eq!(
            wait.wait(TaskId(7), tairix_abi::WAIT_PID_ANY, WaitFlags::empty()),
            Ok(WaitedChild {
                pid,
                status: WaitStatus::Exited(137)
            })
        );
    }

    #[test]
    fn interrupt_terminates_with_the_ctrl_c_status() {
        let _overlay = stopped_overlay_test_lock();
        let (wait, scheduler) = scaffold();
        let (child, child_pid) = spawn_child(scheduler);
        wait.register_child(TaskId(7), TaskId(child));
        let signaller = KernelProcessSignal::new(wait, scheduler);

        assert_eq!(
            signaller.signal(TaskId(7), child_pid, Signal::Interrupt),
            Ok(())
        );
        assert_eq!(scheduler.live_task_count(), 0);
        let pid = u32::try_from(child).expect("host task id fits u32");
        // Interrupt surfaces as the `^C` 130 every POSIX shell reports.
        assert_eq!(
            wait.wait(TaskId(7), tairix_abi::WAIT_PID_ANY, WaitFlags::empty()),
            Ok(WaitedChild {
                pid,
                status: WaitStatus::Exited(130)
            })
        );
    }

    #[test]
    fn stop_parks_marks_and_reports_and_continue_lifts_it() {
        let _overlay = stopped_overlay_test_lock();
        let (wait, scheduler) = scaffold();
        let (child, child_pid) = spawn_child(scheduler);
        wait.register_child(TaskId(7), TaskId(child));
        let signaller = KernelProcessSignal::new(wait, scheduler);

        assert_eq!(signaller.signal(TaskId(7), child_pid, Signal::Stop), Ok(()));
        // The stop overlay holds the child, so no broadcast wake can run it.
        assert!(task_is_stopped(child));
        // The child is still live (stopped, not terminated) …
        assert_eq!(scheduler.live_task_count(), 1);
        // … and a STOPPED wait observes the stop without reaping.
        assert_eq!(
            wait.poll(
                TaskId(7),
                tairix_abi::WAIT_PID_ANY,
                WaitFlags::from_bits(WaitFlags::NONBLOCK.bits() | WaitFlags::STOPPED.bits())
                    .expect("defined bits")
            ),
            Ok(WaitedChild {
                pid: u32::try_from(child).expect("host task id fits u32"),
                status: WaitStatus::Stopped(Signal::Stop)
            })
        );
        // Continue lifts the overlay and resumes the child.
        assert_eq!(
            signaller.signal(TaskId(7), child_pid, Signal::Continue),
            Ok(())
        );
        assert!(!task_is_stopped(child));
        assert_eq!(scheduler.live_task_count(), 1);
    }

    #[test]
    fn continue_clears_a_stop_the_parent_never_observed() {
        let _overlay = stopped_overlay_test_lock();
        let (wait, scheduler) = scaffold();
        let (child, child_pid) = spawn_child(scheduler);
        wait.register_child(TaskId(7), TaskId(child));
        let signaller = KernelProcessSignal::new(wait, scheduler);

        assert_eq!(signaller.signal(TaskId(7), child_pid, Signal::Stop), Ok(()));
        assert_eq!(
            signaller.signal(TaskId(7), child_pid, Signal::Continue),
            Ok(())
        );
        // The unobserved stop was cleared by the resume: nothing to report.
        assert_eq!(
            wait.poll(
                TaskId(7),
                tairix_abi::WAIT_PID_ANY,
                WaitFlags::from_bits(WaitFlags::NONBLOCK.bits() | WaitFlags::STOPPED.bits())
                    .expect("defined bits")
            ),
            Err(Errno::WouldBlock)
        );
    }

    #[test]
    fn killing_a_stopped_child_lifts_its_overlay_entry() {
        let _overlay = stopped_overlay_test_lock();
        let (wait, scheduler) = scaffold();
        let (child, child_pid) = spawn_child(scheduler);
        wait.register_child(TaskId(7), TaskId(child));
        let signaller = KernelProcessSignal::new(wait, scheduler);

        assert_eq!(signaller.signal(TaskId(7), child_pid, Signal::Stop), Ok(()));
        assert!(task_is_stopped(child));
        assert_eq!(signaller.signal(TaskId(7), child_pid, Signal::Kill), Ok(()));
        // The dead child leaves no stale overlay entry behind.
        assert!(!task_is_stopped(child));
        // The terminal exit superseded the unobserved stop.
        assert_eq!(
            wait.wait(TaskId(7), tairix_abi::WAIT_PID_ANY, WaitFlags::STOPPED),
            Ok(WaitedChild {
                pid: u32::try_from(child).expect("host task id fits u32"),
                status: WaitStatus::Exited(137)
            })
        );
    }

    #[test]
    fn foreground_deliver_maps_only_the_line_discipline_signals() {
        let _overlay = stopped_overlay_test_lock();
        let (wait, scheduler) = scaffold();
        let (child, _child_pid) = spawn_child(scheduler);
        wait.register_child(TaskId(7), TaskId(child));
        let signaller = KernelProcessSignal::new(wait, scheduler);

        // The console path never delivers Continue/Terminate/Kill.
        for signal in [Signal::Continue, Signal::Terminate, Signal::Kill] {
            assert_eq!(
                signaller.deliver(TaskId(child), signal),
                Err(Errno::OutOfRange)
            );
        }
        // `^Z` stops the foreground task …
        assert_eq!(signaller.deliver(TaskId(child), Signal::Stop), Ok(()));
        assert!(task_is_stopped(child));
        assert_eq!(scheduler.live_task_count(), 1);
        // … and `^C` terminates it with the 130 status.
        assert_eq!(signaller.deliver(TaskId(child), Signal::Interrupt), Ok(()));
        assert_eq!(scheduler.live_task_count(), 0);
        assert_eq!(
            wait.wait(TaskId(7), tairix_abi::WAIT_PID_ANY, WaitFlags::empty()),
            Ok(WaitedChild {
                pid: u32::try_from(child).expect("host task id fits u32"),
                status: WaitStatus::Exited(130)
            })
        );
    }

    #[test]
    fn foreground_deliver_to_a_dead_target_fails_closed() {
        let (wait, scheduler) = scaffold();
        let signaller = KernelProcessSignal::new(wait, scheduler);
        // Task 9999 was never admitted: the delivery reaches no one.
        assert_eq!(
            signaller.deliver(TaskId(9999), Signal::Interrupt),
            Err(Errno::NotFound)
        );
        assert_eq!(
            signaller.deliver(TaskId(9999), Signal::Stop),
            Err(Errno::NotFound)
        );
        // A refused stop leaves no overlay entry behind.
        assert!(!task_is_stopped(9999));
    }

    #[test]
    fn queued_foreground_signal_round_trips_through_the_pending_slot() {
        // The pending slot is process-global, so serialise against the
        // console line-discipline tests that share it.
        let _guard = foreground_test_lock();
        queue_foreground_signal(TaskId(77), Signal::Interrupt);
        // The drain consumes the slot (delivering only if a boot-style hook
        // was installed by another test; either way the slot empties).
        drain_pending_foreground();
        // A second drain finds the slot empty.
        assert!(!drain_pending_foreground());
    }

    #[test]
    fn continue_of_a_running_child_is_a_harmless_success() {
        let _overlay = stopped_overlay_test_lock();
        let (wait, scheduler) = scaffold();
        let (child, child_pid) = spawn_child(scheduler);
        wait.register_child(TaskId(7), TaskId(child));
        let signaller = KernelProcessSignal::new(wait, scheduler);

        // The child is runnable, not stopped, so Continue succeeds as a no-op
        // (it neither terminates the child nor records an exit).
        assert_eq!(
            signaller.signal(TaskId(7), child_pid, Signal::Continue),
            Ok(())
        );
        assert_eq!(scheduler.live_task_count(), 1);
        // The child is still a live, signallable child (no status recorded).
        assert_eq!(
            wait.authorise_child(TaskId(7), child_pid),
            Ok(TaskId(child))
        );
    }

    /// Serialises host tests that touch the process-global signal-intake
    /// map ([`SIGNAL_INTAKE`]): it is keyed by numeric task id, and each
    /// test's own leaked scheduler hands out the same small ids, so two
    /// tests observing "their" intake in parallel would read and clear
    /// each other's entries. Every test that enables, drains, or delivers
    /// through an intake takes this lock and clears its ids on entry.
    fn intake_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        // A panicking holder does not corrupt the `()` state; continue.
        LOCK.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn intake_lifecycle_is_idempotent_and_fail_closed() {
        let _intake = intake_test_lock();
        clear_intake(4001);
        // Take and disable before any opt-in: no intake exists.
        assert_eq!(intake_take(4001), Err(Errno::NotFound));
        assert!(!intake_enabled(4001));
        assert!(!intake_ready(4001));
        assert_eq!(intake_disable(4001), Ok(()));
        // Enable is idempotent; nothing is pending yet.
        intake_enable(4001);
        intake_enable(4001);
        assert!(intake_enabled(4001));
        assert!(!intake_ready(4001));
        assert_eq!(intake_take(4001), Err(Errno::WouldBlock));
        // A recorded signal is observable, drains exactly once, and a
        // re-enable never discards it.
        assert!(try_intake(4001, Signal::Interrupt));
        intake_enable(4001);
        assert!(intake_ready(4001));
        assert_eq!(intake_take(4001), Ok(Signal::Interrupt));
        assert_eq!(intake_take(4001), Err(Errno::WouldBlock));
        // Disable removes the opt-in; a later delivery goes default again.
        assert_eq!(intake_disable(4001), Ok(()));
        assert!(!try_intake(4001, Signal::Terminate));
        clear_intake(4001);
    }

    #[test]
    fn disable_with_a_pending_signal_is_refused_until_drained() {
        let _intake = intake_test_lock();
        clear_intake(4002);
        intake_enable(4002);
        assert!(try_intake(4002, Signal::Terminate));
        // A recorded termination request is never silently discarded.
        assert_eq!(intake_disable(4002), Err(Errno::WouldBlock));
        assert!(intake_enabled(4002));
        assert_eq!(intake_take(4002), Ok(Signal::Terminate));
        assert_eq!(intake_disable(4002), Ok(()));
        clear_intake(4002);
    }

    #[test]
    fn a_second_pending_termination_request_is_declined_for_escalation() {
        let _intake = intake_test_lock();
        clear_intake(4003);
        intake_enable(4003);
        assert!(try_intake(4003, Signal::Interrupt));
        // The slot is occupied: the second request is declined so the
        // caller escalates to the default terminate path (`^C ^C` kills).
        assert!(!try_intake(4003, Signal::Interrupt));
        assert!(!try_intake(4003, Signal::Terminate));
        // The first observation is still intact for the drain.
        assert_eq!(intake_take(4003), Ok(Signal::Interrupt));
        clear_intake(4003);
    }

    #[test]
    fn opted_in_interrupt_is_observed_not_fatal_and_kill_still_kills() {
        let _overlay = stopped_overlay_test_lock();
        let _intake = intake_test_lock();
        let (wait, scheduler) = scaffold();
        let (child, child_pid) = spawn_child(scheduler);
        clear_intake(child);
        wait.register_child(TaskId(7), TaskId(child));
        let signaller = KernelProcessSignal::new(wait, scheduler);

        intake_enable(child);
        // The interrupt is recorded, not delivered as a termination: the
        // child stays live and nothing becomes reapable.
        assert_eq!(
            signaller.signal(TaskId(7), child_pid, Signal::Interrupt),
            Ok(())
        );
        assert_eq!(scheduler.live_task_count(), 1);
        assert_eq!(
            wait.poll(TaskId(7), tairix_abi::WAIT_PID_ANY, WaitFlags::NONBLOCK),
            Err(Errno::WouldBlock)
        );
        assert_eq!(intake_take(child), Ok(Signal::Interrupt));
        // `Kill` is unconditionally fatal regardless of the opt-in and
        // reaps with SIGKILL's familiar 137.
        assert_eq!(signaller.signal(TaskId(7), child_pid, Signal::Kill), Ok(()));
        assert_eq!(scheduler.live_task_count(), 0);
        assert_eq!(
            wait.wait(TaskId(7), tairix_abi::WAIT_PID_ANY, WaitFlags::empty()),
            Ok(WaitedChild {
                pid: u32::try_from(child).expect("host task id fits u32"),
                status: WaitStatus::Exited(137)
            })
        );
        clear_intake(child);
    }

    #[test]
    fn a_second_interrupt_escalates_to_the_default_terminate() {
        let _overlay = stopped_overlay_test_lock();
        let _intake = intake_test_lock();
        let (wait, scheduler) = scaffold();
        let (child, child_pid) = spawn_child(scheduler);
        clear_intake(child);
        wait.register_child(TaskId(7), TaskId(child));
        let signaller = KernelProcessSignal::new(wait, scheduler);

        intake_enable(child);
        assert_eq!(
            signaller.signal(TaskId(7), child_pid, Signal::Interrupt),
            Ok(())
        );
        assert_eq!(scheduler.live_task_count(), 1);
        // The second interrupt finds the slot occupied and terminates the
        // child with the `^C` 130 — an unresponsive opted-in program stays
        // killable with plain `^C ^C`.
        assert_eq!(
            signaller.signal(TaskId(7), child_pid, Signal::Interrupt),
            Ok(())
        );
        assert_eq!(scheduler.live_task_count(), 0);
        assert_eq!(
            wait.wait(TaskId(7), tairix_abi::WAIT_PID_ANY, WaitFlags::empty()),
            Ok(WaitedChild {
                pid: u32::try_from(child).expect("host task id fits u32"),
                status: WaitStatus::Exited(130)
            })
        );
        clear_intake(child);
    }

    #[test]
    fn foreground_interrupt_reaches_an_opted_in_target_without_killing_it() {
        let _overlay = stopped_overlay_test_lock();
        let _intake = intake_test_lock();
        let (wait, scheduler) = scaffold();
        let (child, _child_pid) = spawn_child(scheduler);
        clear_intake(child);
        wait.register_child(TaskId(7), TaskId(child));
        let signaller = KernelProcessSignal::new(wait, scheduler);

        intake_enable(child);
        // The console `^C` is observed, not fatal …
        assert_eq!(signaller.deliver(TaskId(child), Signal::Interrupt), Ok(()));
        assert_eq!(scheduler.live_task_count(), 1);
        assert_eq!(intake_take(child), Ok(Signal::Interrupt));
        // … and `^Z` still stops the opted-in target (only termination
        // requests are observable; `Stop` stays scheduler-side).
        assert_eq!(signaller.deliver(TaskId(child), Signal::Stop), Ok(()));
        assert!(task_is_stopped(child));
        // Lift the overlay so the shared set holds no stale entry.
        assert_eq!(signaller.deliver(TaskId(child), Signal::Interrupt), Ok(()));
        assert_eq!(intake_take(child), Ok(Signal::Interrupt));
        STOPPED_TASKS.lock().remove(&child);
        clear_intake(child);
    }

    #[test]
    fn a_signalled_child_cannot_be_signalled_twice() {
        let _overlay = stopped_overlay_test_lock();
        let (wait, scheduler) = scaffold();
        let (child, child_pid) = spawn_child(scheduler);
        wait.register_child(TaskId(7), TaskId(child));
        let signaller = KernelProcessSignal::new(wait, scheduler);

        assert_eq!(
            signaller.signal(TaskId(7), child_pid, Signal::Terminate),
            Ok(())
        );
        // Once terminated the child is a zombie awaiting reap, not a live
        // process: a second signal fails closed rather than re-terminating it.
        assert_eq!(
            signaller.signal(TaskId(7), child_pid, Signal::Kill),
            Err(Errno::NotFound)
        );
    }
}
