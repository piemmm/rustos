//! The kernel-side process-signal seam the `signal` (`abi-v1` number 64)
//! syscall uses (`plans/SPAWN.md` SP7).
//!
//! [`ProcessSignal`] is the one object-safe boundary between the
//! arch-neutral syscall handler in `kernel/core` and the scheduler-side
//! producer that delivers a control signal. It carries the two halves
//! separately — resolving a pid against the sender's own children, and
//! delivering to an already-authorised target — because who may signal whom
//! is decided by the syscall handler alone (own child, else the target's own
//! principal, else `CAP_PROC_CONTROL`, `plans/NEW-TASKBAR.md` T11). Like the
//! [`ProcessWait`], [`ArchImageBuilder`](crate::spawn::ArchImageBuilder), and
//! [`MemMap`](crate::memmap::MemMap) seams, the concrete producer is
//! installed at boot through the `with_process_signal` builder and the
//! handler reaches it through this trait.
//!
//! Until a producer is installed the handler holds [`NULL_PROCESS_SIGNAL`],
//! which fails closed: both halves return [`Errno::NotImplemented`], never
//! pretending a signal was delivered — exactly as
//! [`NULL_PROCESS_WAIT`](crate::procwait::NULL_PROCESS_WAIT) does for
//! `wait`. The scheduler-side producer that actually delivers the signal is
//! [`KernelProcessSignal`] (`plans/SPAWN.md` `SP7b`), installed at boot in
//! place of the fail-closed floor.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tairix_abi::{Errno, ProcId, Signal};
use tairix_kernel_sched_api::{ExitDisposition, SchedError, SchedulerArch, SchedulerPolicy};
use tairix_kernel_sec::{CapTable, ProcessId, TaskId};
use tairix_sync::once::OnceCell;
use tairix_sync::{IrqSafeSpinLock, RwLock, SpinLock};

use crate::foreground::ForegroundOwner;
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
/// opt-in or pending slot behind — the entry goes before the id can be drawn
/// again, so a later task never inherits one.
pub fn clear_intake(task: u64) {
    SIGNAL_INTAKE.lock().remove(&task);
}

/// The kill gate: which threads are executing **inside the kernel on their
/// own stack**, and the death each thread owes.
///
/// A thread inside a kernel body — a syscall handler, the deferred-load body,
/// the user-fault resolver — holds state only its own unwind can release, so
/// its death is owed at the body's boundary (`plans/OPEN-DEFECTS.md` D112). A
/// thread outside the kernel holds none but may still be executing, so its
/// death is owed where the scheduler retires it.
///
/// A death is recorded before anything acts on it — recorded later, it could
/// arrive after the only point that looks for it — and exactly one party takes
/// it: the boundary, the dispatch loop once the scheduler has retired the
/// thread, or a killer that retired it itself. A thread entering a kernel body
/// owing one never runs the body. Both registers share one lock because where a
/// death is owed is a single decision on [`in_kernel`](Self::in_kernel), taken
/// concurrently with the thread's own entry into the kernel. Each grows with
/// live threads, never a fixed ceiling, and a thread's teardown clears its
/// entries.
struct KillGate {
    /// Threads between [`kernel_enter`] and [`kernel_exit_take_kill`], parked
    /// or running.
    in_kernel: BTreeSet<u64>,
    /// The death each thread owes, first claim wins.
    ///
    /// A [`DeferredTeardown`], not a bare status, because the deaths claimed
    /// here carry different ones and none may overwrite another: a signalled
    /// kill's `128 + n`, a group `exit(code)`'s own code, a fault kill's crash
    /// status, and a driver unload's *no* status at all.
    owed: BTreeMap<u64, DeferredTeardown>,
}

/// The one kill-gate instance shared by the killers, the kernel bodies'
/// boundaries, and the dispatch loop.
static KILL_GATE: SpinLock<KillGate> = SpinLock::new(KillGate {
    in_kernel: BTreeSet::new(),
    owed: BTreeMap::new(),
});

/// How many deaths [`KillGate::owed`] holds, so the dispatch loop's
/// per-dispatch [`land_retired_kill`] is one relaxed load and no lock while
/// nothing is owed anywhere.
static OWED_KILLS: AtomicUsize = AtomicUsize::new(0);

/// What teardown a thread's death owes. Both kinds reclaim the dying
/// **process's** kernel resources once its last thread is down; they differ
/// only in whether a parent's `wait` is also given a status.
///
/// The death is keyed by the *thread* that owes it — that is what a boundary
/// and the dispatch loop can recognise — but the teardown names the
/// **process**, because reclaiming an address space, capability record,
/// endpoints, and open files is a process operation.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DeferredTeardown {
    /// The process dies carrying this terminal status: record it for the
    /// parent's `wait` **and** reclaim, once the group's last thread is down.
    Exit {
        /// The process to reap and reclaim.
        process: ProcessId,
        /// The status the parent's `wait` reports — a signal's `128 + n`, a
        /// group `exit`'s own code, or a fault kill's crash status.
        status: i32,
    },
    /// A teardown nobody reaps (an unloaded driver): reclaim the process's
    /// kernel resources only.
    Plain {
        /// The process to reclaim.
        process: ProcessId,
    },
}

impl DeferredTeardown {
    /// The process this death tears down.
    #[must_use]
    pub const fn process(self) -> ProcessId {
        match self {
            Self::Exit { process, .. } | Self::Plain { process } => process,
        }
    }

    /// The status a parent's `wait` reports, or [`None`] when nobody reaps
    /// this death (an unloaded driver is not a waited-for child).
    #[must_use]
    pub const fn reaped_status(self) -> Option<i32> {
        match self {
            Self::Exit { status, .. } => Some(status),
            Self::Plain { .. } => None,
        }
    }
}

/// Where a claimed death is owed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum KillSite {
    /// Inside a kernel body: its boundary lands the death, and the killer
    /// wakes the thread so a body parked in the kernel unwinds to it.
    Boundary,
    /// Outside the kernel: the killer asks the scheduler to retire the thread,
    /// and the death lands wherever that retire happens.
    Retire,
}

/// One thread's share of a group death recorded by [`claim_group_kill`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ClaimedKill {
    /// The thread that owes the death.
    pub thread: u64,
    /// Where it is owed.
    pub site: KillSite,
    /// Whether this claim recorded the death, rather than finding one another
    /// death already owed, which this claim must not undo.
    pub recorded: bool,
}

/// Record `teardown` as the death every live thread of its process owes, but
/// `spare`, and report where each is owed.
///
/// The thread set is read, and every death recorded, under the thread-group
/// table's read lock, which makes the claim exact against both ends of a
/// thread's life: a thread registered after it sees its creator's death owed
/// and is refused ([`kill_pending`]), and a thread's teardown withdraws its
/// membership before it clears the gate, so no death is recorded that the
/// teardown would not clear. A build with no table treats the process as its
/// single leader thread.
pub fn claim_group_kill(
    caps: Option<&RwLock<CapTable>>,
    teardown: DeferredTeardown,
    spare: Option<u64>,
) -> Vec<ClaimedKill> {
    let process = teardown.process();
    let members = caps.map(RwLock::read);
    let threads: Vec<u64> = match &members {
        Some(table) => table.threads_of(process).map(|thread| thread.0).collect(),
        None => alloc::vec![process.leader_task().0],
    };
    let mut claims = Vec::with_capacity(threads.len());
    let mut gate = KILL_GATE.lock();
    for thread in threads.into_iter().filter(|thread| Some(*thread) != spare) {
        let recorded = match gate.owed.entry(thread) {
            alloc::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(teardown);
                OWED_KILLS.fetch_add(1, Ordering::Relaxed);
                true
            }
            alloc::collections::btree_map::Entry::Occupied(_) => false,
        };
        let site = if gate.in_kernel.contains(&thread) {
            KillSite::Boundary
        } else {
            KillSite::Retire
        };
        claims.push(ClaimedKill {
            thread,
            site,
            recorded,
        });
    }
    claims
}

/// Take (remove) the death `task` owes, keeping the owed count in step.
fn take_owed_locked(gate: &mut KillGate, task: u64) -> Option<DeferredTeardown> {
    let taken = gate.owed.remove(&task);
    if taken.is_some() {
        OWED_KILLS.fetch_sub(1, Ordering::Relaxed);
    }
    taken
}

/// Take the death `task` owes, for a killer whose scheduler call retired the
/// thread itself (or found no thread to retire) and so owns the landing.
pub fn take_owed_kill(task: u64) -> Option<DeferredTeardown> {
    take_owed_locked(&mut KILL_GATE.lock(), task)
}

/// Mark `task` as executing inside the kernel on its own stack, reporting
/// whether it already owes a death. Paired with [`kernel_exit_take_kill`] by
/// every kernel body a thread runs on its own stack.
///
/// `true` obliges the caller to skip its body and go straight to its boundary,
/// which lands the death: a thread owing one may already have been told to
/// die, and the scheduler retires such a thread at its next stopping point —
/// which, inside a body, would free a stack whose frames still own kernel
/// state.
#[must_use]
pub fn kernel_enter(task: u64) -> bool {
    let mut gate = KILL_GATE.lock();
    gate.in_kernel.insert(task);
    gate.owed.contains_key(&task)
}

/// Mark `task` as leaving the kernel and take any death it owes.
///
/// `Some(teardown)` obliges the caller to land the death now: the body has
/// unwound (every lock and buffer it held is released), so this is the first
/// safe point the thread can die at. The thread never returns to user mode.
#[must_use]
pub fn kernel_exit_take_kill(task: u64) -> Option<DeferredTeardown> {
    let mut gate = KILL_GATE.lock();
    gate.in_kernel.remove(&task);
    take_owed_locked(&mut gate, task)
}

/// Whether a death is owed by `task`.
///
/// The in-kernel park loops consult this after every wake and unwind with
/// `Errno::Interrupted` instead of re-parking, so a doomed thread reaches its
/// boundary promptly rather than sleeping on as an unkillable waiter. The
/// errno never reaches user space — the boundary lands the death first.
#[must_use]
pub fn kill_pending(task: u64) -> bool {
    KILL_GATE.lock().owed.contains_key(&task)
}

/// Drop every trace of `task` from the gate on teardown — its in-kernel window
/// and any death it owes. Idempotent; driven by the one shared thread teardown
/// once the thread has left its group, so a death claimed while it was still a
/// member is cleared here and none can be claimed after.
pub fn clear_kill_gate(task: u64) {
    let mut gate = KILL_GATE.lock();
    gate.in_kernel.remove(&task);
    take_owed_locked(&mut gate, task);
}

/// The seam through which the dispatch loop lands a retired thread's death:
/// records the status so the parent's `wait` reaps it and reclaims the
/// process's kernel resources — the same reap and reclaim a boundary
/// performs. Installed per boot by [`install_deferred_kill_lander`] (the
/// leaked [`KernelProcessSignal`] holds both the wait producer and the reclaim
/// seam), so the free-function dispatch loop can drive it without borrowing
/// the producer directly.
pub trait DeferredKillLander: Sync {
    /// Land `teardown`, the death the retired `task` owed. The death has
    /// already been taken from the gate, so this is called exactly once.
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

/// Land the death `task` owes once its dispatch has returned, if the
/// scheduler has retired it. Called from the dispatch loop after every
/// dispatched task, with `retired` answering whether the scheduler has.
///
/// A dispatch returns on a park and a yield as well as on a retire, and a
/// thread owing a death may be queued again or parked in a kernel body when it
/// does; only a retired thread executes nowhere and never will again, so only
/// its death is landed here. `retired` is consulted only while some death is
/// owed, so the common dispatch pays one relaxed load. Idempotent: a death is
/// taken exactly once, and a thread whose own teardown already ran left
/// nothing to take.
pub fn land_retired_kill(task: u64, retired: impl FnOnce() -> bool) {
    if OWED_KILLS.load(Ordering::Relaxed) == 0 {
        return;
    }
    // A lander must be installed before any death can be claimed; fail closed
    // (leave the death owed) rather than dropping it if not.
    let Ok(Some(lander)) = DEFERRED_KILL_LANDER.get() else {
        return;
    };
    if !retired() {
        return;
    }
    if let Some(teardown) = take_owed_kill(task) {
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
    /// Deliver `signal` to the terminal's recorded foreground owner.
    ///
    /// The authority was established when the parent marked the task
    /// foreground through `console_foreground` (a live child of the caller
    /// on that console); by delivery time the task may already have exited,
    /// in which case the delivery fails closed with [`Errno::NotFound`] and
    /// signals no one. `owner` carries the process *instance* the ownership
    /// was granted at, not merely its pid, because an id whose task is gone
    /// may be drawn again: an implementation must refuse rather than signal
    /// whichever process holds the number at delivery time.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] when the target no longer exists, or the pid now
    /// names a different process instance; [`Errno::OutOfRange`] for a signal
    /// the line discipline never maps.
    fn deliver(&self, owner: ForegroundOwner, signal: Signal) -> Result<(), Errno>;
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

/// Retires one thread of a dying process and, once the group's **last** thread
/// is down, tears the process itself down — the seam through which the
/// signal-terminate path drives the one
/// `KernelSyscallHandlers::land_thread_down` every death path shares (the
/// `exit` and `thread_exit` syscalls, the fault kill, the deferred kill, and a
/// driver unload).
///
/// Installed per producer instance
/// ([`KernelProcessSignal::install_task_reclaim`]) rather than through a
/// process-global slot, so each host-test fixture observes only its own
/// terminations.
pub trait TaskReclaim: Sync {
    /// Retire `thread` from `process`'s thread group and, when it was the last
    /// thread of the group still executing, record `status` (when a `wait` reap
    /// is owed) and release every kernel-held resource of the process.
    ///
    /// Called once per thread death, which the kill gate guarantees by handing
    /// each death it records to exactly one landing. Not idempotent: a thread
    /// the group table no longer holds reads as the last one down, so a second
    /// call would tear the process down again.
    fn land_thread_down(&self, process: ProcessId, thread: TaskId, status: Option<i32>);
}

/// Error returned when [`KernelProcessSignal::install_task_reclaim`] is
/// called more than once on the same producer.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct TaskReclaimAlreadyInstalled;

/// The one pending foreground signal and the owner it is aimed at.
///
/// A single slot, not a queue: a later `^C`/`^Z` typed before the previous
/// one was delivered simply replaces it, which matches what the keystrokes
/// mean — the newest request wins, and a terminated target makes the older
/// one moot.
///
/// Written from the UART receive interrupt and drained in dispatcher
/// context, so the lock masks the writing CPU for the store rather than
/// risking a self-deadlock against the task it interrupted.
static PENDING_FOREGROUND: IrqSafeSpinLock<Option<(ForegroundOwner, Signal)>> =
    IrqSafeSpinLock::new(None);

/// Whether [`PENDING_FOREGROUND`] holds a signal, for the preemption gate's
/// per-tick peek.
///
/// Advisory and lock-free on purpose: the gate consults it on every timer
/// tick, where an uncontended lock acquire would still cost a mask/unmask
/// pair. A spurious `true` costs one extra reschedule that finds the slot
/// empty; the delivery itself always reads the slot under the lock.
static FOREGROUND_QUEUED: AtomicBool = AtomicBool::new(false);

/// Record a foreground signal for delivery at the next dispatcher-context
/// drain ([`drain_pending_foreground`]).
///
/// Interrupt-safe: the slot's lock masks the writing CPU for one store, so
/// the UART RX handler that maps `^C` cannot deadlock against the task it
/// interrupted, and it takes no scheduler lock at all — the delivery that
/// does runs later, in dispatcher context.
pub fn queue_foreground_signal(owner: ForegroundOwner, signal: Signal) {
    *PENDING_FOREGROUND.lock() = Some((owner, signal));
    FOREGROUND_QUEUED.store(true, Ordering::Release);
}

/// Non-consuming peek: whether a foreground signal (`^C`/`^Z`) is queued
/// awaiting its dispatcher-context [`drain_pending_foreground`].
///
/// The preemption gate consults this so a timer tick on a lone-task CPU
/// still reschedules when a queued signal needs delivering — the delivery
/// only runs once the dispatch loop regains control.
#[must_use]
pub fn has_pending_foreground() -> bool {
    FOREGROUND_QUEUED.load(Ordering::Acquire)
}

/// Deliver the pending foreground signal, if any, through the installed
/// [`ForegroundSignal`] producer.
///
/// Called from the dispatch loop between task dispatches (the same slot
/// `drain_pending_wakes` runs in), where taking scheduler locks is safe.
/// Returns `true` when a delivery was attempted, so the idle path knows
/// work happened. A failed delivery is dropped: the aimed-at process
/// instance is gone, so the signal has no one left to go to and is never
/// re-aimed at whoever holds its pid now.
pub fn drain_pending_foreground() -> bool {
    FOREGROUND_QUEUED.store(false, Ordering::Release);
    let Some((owner, signal)) = PENDING_FOREGROUND.lock().take() else {
        return false;
    };
    let Ok(Some(hook)) = FOREGROUND_SIGNAL.get() else {
        // No producer installed (or the cell poisoned): nothing can be
        // delivered — fail closed. The slot is already cleared, never
        // retried into a later boot phase.
        return false;
    };
    let _ = hook.deliver(owner, signal);
    true
}

/// The kernel-side producer of the `signal` syscall.
///
/// Authority and mechanism are separate halves of this contract: the
/// producer answers *which live task a pid names among the sender's own
/// children* ([`Self::resolve_child`]) and *how a signal reaches a target*
/// ([`Self::signal_task`]), and the syscall handler decides between the
/// widening rules (own child, same principal, `CAP_PROC_CONTROL`) in one
/// place. A producer therefore carries no policy of its own and can never
/// widen the target rule behind the handler's back.
///
/// Implementations must be [`Sync`]: the single installed producer is shared
/// by the per-CPU syscall handlers, exactly like the process-wait producer,
/// the spawn producer, and the console device.
pub trait ProcessSignal: Sync {
    /// Resolve `pid` to the **live** child of `sender` it names.
    ///
    /// `sender` is the kernel-attested identity of the calling task
    /// (supplied by the dispatcher, never by the caller). Only a child the
    /// sender spawned and that is still running resolves; a zombie awaiting
    /// reap, another parent's child, and an unknown `pid` all do not.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] when `pid` does not name a live child of
    /// `sender` — the answer that sends the handler on to its
    /// cross-principal rule. The default producer ([`NullProcessSignal`])
    /// returns [`Errno::NotImplemented`] to mark an inert interface.
    fn resolve_child(&self, sender: ProcessId, pid: i64) -> Result<ProcessId, Errno>;

    /// Deliver `signal` to an **already-authorised** `target`.
    ///
    /// Mechanism only: this performs no ownership or capability check, so
    /// the caller must have established the sender's authority over
    /// `target` first.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] when the scheduler no longer knows `target` (it
    /// exited between authorisation and delivery). The default producer
    /// ([`NullProcessSignal`]) returns [`Errno::NotImplemented`] to mark an
    /// inert interface.
    fn signal_task(&self, target: ProcessId, signal: Signal) -> Result<(), Errno>;

    /// Drive every thread of `process` **except** `keep` to its stopping point,
    /// so the process carries `status` and nothing of the group is still
    /// executing when it is reclaimed (`plans/THREADS.md` decision 10).
    ///
    /// The group-death half of `exit(code)` and of a fault kill. Each sibling
    /// either quiesces immediately — its per-thread state retired through the
    /// installed landing seam — or is still inside a syscall / executing in user
    /// mode, in which case the death is *deferred* against it carrying `status`,
    /// and whichever thread of the group lands last records that status and
    /// reclaims the process. That is why `status` is threaded through here
    /// rather than recorded by the calling thread up front: a `128 + n` written
    /// by a sibling's deferral would overwrite the real exit code.
    ///
    /// The default is a no-op: a single-threaded process has no siblings, and a
    /// producer with no thread-group table cannot see any.
    fn terminate_siblings(&self, process: ProcessId, keep: TaskId, status: i32) {
        let _ = (process, keep, status);
    }

    /// Whether this producer can drive a whole thread group to its stopping
    /// point.
    ///
    /// The prerequisite for admitting a *second* thread into a process: a group
    /// whose siblings cannot be stopped can never be torn down, so
    /// `thread_create` fails closed rather than building a process whose death
    /// would either strand its kernel state or reclaim its address space from
    /// under a running sibling. Defaults to `false` — an inert producer stops
    /// nothing.
    fn can_stop_group(&self) -> bool {
        false
    }
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
    fn resolve_child(&self, _sender: ProcessId, _pid: i64) -> Result<ProcessId, Errno> {
        Err(Errno::NotImplemented)
    }

    fn signal_task(&self, _target: ProcessId, _signal: Signal) -> Result<(), Errno> {
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
/// It carries **no bookkeeping of its own**: it resolves the sender's own
/// children and records a signalled termination through the one
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
    /// producer resolves and records against — never a second copy.
    wait: &'static KernelProcessWait<A>,
    /// The live scheduler this producer drives to deliver a signal
    /// (unpark / exit the target task).
    scheduler: &'static P,
    /// The authoritative thread-group table, so a process-directed signal
    /// reaches **every** thread of its target (`plans/THREADS.md` decision
    /// 10).
    ///
    /// A signal names a PID, but only threads are schedulable: parking,
    /// unparking, and exiting all name a thread. Delivering to the leader
    /// alone would leave a killed process's siblings running against a
    /// reclaimed address space — a wild-fault hazard, not a behavioural
    /// nuance — and would leave a stopped process still executing. [`None`]
    /// on a host fixture that wired no table; delivery then treats the
    /// target as the single-threaded process its PID names, which is what
    /// every process was before threads existed.
    caps: Option<&'static RwLock<CapTable>>,
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
    /// Build a producer that resolves children and records against `wait`
    /// and delivers through `scheduler`, fanning a process-directed signal out
    /// over the thread groups `caps` holds.
    #[must_use]
    pub const fn new(
        wait: &'static KernelProcessWait<A>,
        scheduler: &'static P,
        caps: &'static RwLock<CapTable>,
    ) -> Self {
        Self {
            wait,
            scheduler,
            caps: Some(caps),
            reclaim: OnceCell::new(),
        }
    }

    /// A producer with no thread-group table: every target is treated as the
    /// single-threaded process its PID names.
    ///
    /// The shape a host fixture of the signal bookkeeping alone needs — it
    /// registers no capability records, so there is no group to resolve.
    #[must_use]
    pub const fn without_thread_groups(
        wait: &'static KernelProcessWait<A>,
        scheduler: &'static P,
    ) -> Self {
        Self {
            wait,
            scheduler,
            caps: None,
            reclaim: OnceCell::new(),
        }
    }

    /// Every live thread of `process`, in ascending id order.
    ///
    /// A process whose group the table does not know — a boot principal, a
    /// kernel task, or a host fixture with no table — is its own single
    /// thread: a process id *is* its leader thread's id, so this degrades to
    /// exactly the pre-threads behaviour rather than delivering nothing.
    fn threads_of(&self, process: ProcessId) -> Vec<u64> {
        let listed = match self.caps {
            Some(caps) => caps
                .read()
                .threads_of(process)
                .map(|thread| thread.0)
                .collect(),
            None => Vec::new(),
        };
        if listed.is_empty() {
            alloc::vec![process.0]
        } else {
            listed
        }
    }

    /// Which process instance holds `process` right now.
    ///
    /// [`ProcId::KERNEL`] both for a principal that is not a distinct user
    /// process and for a record the table no longer holds, so a comparison
    /// against a real minted instance fails closed on a dead target. A
    /// producer wired with no table has no process instances to tell apart at
    /// all and answers the sentinel for every target, matching the sentinel a
    /// grant on such a build recorded.
    fn instance_of(&self, process: ProcessId) -> ProcId {
        self.caps
            .map_or(ProcId::KERNEL, |caps| caps.read().instance_of(process))
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
    fn resume(&self, child: ProcessId) -> Result<(), Errno> {
        let threads = self.threads_of(child);
        // Lift the stop overlay *before* the unpark, so the dispatch that
        // the unpark makes possible finds the task runnable rather than
        // re-parking it. Every thread of the group is lifted: a continue that
        // released only the leader would leave the process half-stopped.
        {
            let mut stopped = STOPPED_TASKS.lock();
            for thread in &threads {
                stopped.remove(thread);
            }
        }
        // The resume also clears any stop the parent never observed: a
        // stale "stopped" report after the child is running again would
        // mislead the job table.
        self.wait.record_continue(child);
        // The delivery succeeds when *some* thread of the group was resumable;
        // a group the scheduler no longer knows at all fails closed.
        let mut resumed = false;
        for thread in threads {
            match self.scheduler.unpark(thread) {
                Ok(()) | Err(SchedError::InvalidState) => resumed = true,
                Err(_) => {}
            }
        }
        if resumed {
            Ok(())
        } else {
            Err(Errno::NotFound)
        }
    }

    /// Stop a child without terminating it ([`Signal::Stop`]).
    ///
    /// Marks the child in the stop overlay *first* — so a wake racing the
    /// park cannot slip it back onto a CPU — then parks it and records the
    /// stop for a `WaitFlags::STOPPED` wait. A child the scheduler no
    /// longer knows fails closed with [`Errno::NotFound`] and leaves no
    /// overlay entry behind.
    fn stop(&self, child: ProcessId) -> Result<(), Errno> {
        let threads = self.threads_of(child);
        // Mark every thread of the group *first* — so a wake racing any park
        // cannot slip one back onto a CPU — then park them all. Stopping only
        // the leader would leave the rest of the process running, which is not
        // what "stopped" means to a job-control shell.
        {
            let mut stopped = STOPPED_TASKS.lock();
            for thread in &threads {
                stopped.insert(*thread);
            }
        }
        let mut parked = false;
        for thread in &threads {
            if self.scheduler.park(*thread).is_ok() {
                parked = true;
            }
        }
        if parked {
            self.wait.record_stop(child, Signal::Stop);
            Ok(())
        } else {
            let mut stopped = STOPPED_TASKS.lock();
            for thread in &threads {
                stopped.remove(thread);
            }
            Err(Errno::NotFound)
        }
    }

    /// Terminate a child ([`Signal::Terminate`] / [`Signal::Kill`]), the
    /// process to carry the signal's `128 + n` status for its parent's `wait`.
    ///
    /// A signal names a process, so every thread of the group is driven to its
    /// death, and the process teardown lands once, when the last of them is
    /// down (`plans/THREADS.md` decision 10). The delivery is complete once each
    /// death is recorded: a thread is never destroyed where that is unsafe —
    /// inside a kernel body, or while its code is still executing — so the
    /// death may land later, at the thread's boundary or where the scheduler
    /// retires it, and the parent's `wait` reaps the child when it does.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] when no thread of the child is left to reach;
    /// nothing is recorded for its parent then (never a fabricated zombie).
    fn terminate(&self, child: ProcessId, signal: Signal) -> Result<(), Errno> {
        // Only a terminating signal reaches here, so it names a `128 + n`
        // status; a non-terminating one is refused rather than assumed.
        let Some(status) = signal.termination_status() else {
            return Err(Errno::OutOfRange);
        };
        let teardown = DeferredTeardown::Exit {
            process: child,
            status,
        };
        if self.kill_group(teardown, None) {
            Ok(())
        } else {
            Err(Errno::NotFound)
        }
    }

    /// Record `teardown` against every thread of its process but `spare` and
    /// bring each to where its death lands, reporting whether the scheduler
    /// still knew any of them.
    fn kill_group(&self, teardown: DeferredTeardown, spare: Option<u64>) -> bool {
        let claims = claim_group_kill(self.caps, teardown, spare);
        // A stopped thread must still die: lifted, the wake or retire below
        // reaches it instead of the dispatch shim re-parking it forever.
        {
            let mut stopped = STOPPED_TASKS.lock();
            for claim in &claims {
                stopped.remove(&claim.thread);
            }
        }
        let mut reached = false;
        for claim in claims {
            reached |= self.stop_claimed(claim);
        }
        reached
    }

    /// Bring one thread whose death was just claimed to where that death
    /// lands, reporting whether the scheduler still knew the thread.
    fn stop_claimed(&self, claim: ClaimedKill) -> bool {
        match claim.site {
            // Every in-kernel park loop re-tests the gate after a wake and
            // unwinds; `InvalidState` is a thread already runnable, which
            // reaches its boundary by itself.
            KillSite::Boundary => matches!(
                self.scheduler.unpark(claim.thread),
                Ok(()) | Err(SchedError::InvalidState)
            ),
            KillSite::Retire => match self.scheduler.exit(claim.thread) {
                // Retired by this call, so this call lands whichever death the
                // thread owes: the first claim's, which need not be this one.
                Ok(ExitDisposition::Quiesced) => {
                    if let Some(owed) = take_owed_kill(claim.thread) {
                        self.land(owed.process(), claim.thread, owed.reaped_status());
                    }
                    true
                }
                // Still executing, or already dying: the retire the scheduler
                // owes it, or the teardown already under way, takes the death.
                Ok(ExitDisposition::Deferred | ExitDisposition::AlreadyExited) => true,
                // A member the scheduler does not know is either mid-teardown,
                // whose gate clear would drop this death anyway, or has no task
                // to retire at all; either way nothing would land it.
                Err(_) => {
                    if claim.recorded {
                        let _ = take_owed_kill(claim.thread);
                    }
                    false
                }
            },
        }
    }

    /// Retire one quiesced `thread` of `process` through the installed landing
    /// seam, which tears the process down — and records `status` for the
    /// parent's `wait` — once the group's last thread is down.
    fn land(&self, process: ProcessId, thread: u64, status: Option<i32>) {
        if let Ok(Some(hook)) = self.reclaim.get() {
            hook.land_thread_down(process, TaskId(thread), status);
            return;
        }
        // No landing seam installed. Such a build registered no kernel
        // resources to reclaim and holds no thread-group table, so this thread
        // *is* its group — but a parent's `wait` is still owed the status, and
        // dropping it would leave the parent blocked on a child that is gone.
        if let Some(status) = status {
            self.wait.record_exit(process, status);
        }
    }

    /// The one delivery engine: apply `signal` to an already-authorised
    /// `target`.
    ///
    /// Both entry points into this producer end here — the `signal`
    /// syscall's [`ProcessSignal::signal_task`] and the console line
    /// discipline's [`ForegroundSignal::deliver`] — so a `^C` and a
    /// parent's `Terminate` take exactly the same path and can never
    /// diverge. Authority is decided by each caller before it arrives:
    /// nothing here consults the parent/child table or a capability.
    fn deliver_signal(&self, target: ProcessId, signal: Signal) -> Result<(), Errno> {
        match signal {
            Signal::Continue => self.resume(target),
            // A termination *request* is observable: an opted-in target with
            // a free pending slot records it instead of dying (the recorded
            // delivery is complete — the target's waiter is woken to drain
            // it). A target that never opted in, or whose slot is already
            // occupied (the escalation rule), terminates by default.
            Signal::Terminate | Signal::Interrupt => {
                if try_intake(target.0, signal) {
                    Ok(())
                } else {
                    self.terminate(target, signal)
                }
            }
            // `Kill` is unconditionally fatal and unmaskable: it is never
            // offered to the intake.
            Signal::Kill => self.terminate(target, signal),
            Signal::Stop => self.stop(target),
        }
    }
}

impl<A, P> DeferredKillLander for KernelProcessSignal<A, P>
where
    A: SchedulerArch + Send + Sync + 'static,
    P: SchedulerPolicy<A> + Send + Sync + 'static,
{
    fn land_deferred_teardown(&self, thread: TaskId, teardown: DeferredTeardown) {
        // The thread has returned to the dispatch loop and executes nowhere,
        // so the teardown deferred at request time is now safe. The process
        // it owes is carried by the deferral itself.
        match teardown {
            // The very reap+reclaim the immediate terminate path runs, one
            // definition — and, for a multi-threaded victim, only once its last
            // thread is down.
            DeferredTeardown::Exit { process, status } => {
                self.land(process, thread.0, Some(status));
            }
            // A driver unload whose process was still executing: reclaim its
            // kernel resources only, with no `wait` reap (a driver is not a
            // waited-for child).
            DeferredTeardown::Plain { process } => {
                self.land(process, thread.0, None);
            }
        }
    }
}

impl<A, P> ProcessSignal for KernelProcessSignal<A, P>
where
    A: SchedulerArch + Send + Sync + 'static,
    P: SchedulerPolicy<A> + Send + Sync + 'static,
{
    fn terminate_siblings(&self, process: ProcessId, keep: TaskId, status: i32) {
        let _ = self.kill_group(DeferredTeardown::Exit { process, status }, Some(keep.0));
    }

    fn can_stop_group(&self) -> bool {
        // Without the thread-group table a fan-out sees no siblings at all, so
        // a second thread of a process could never be driven to its stopping
        // point.
        self.caps.is_some()
    }

    fn resolve_child(&self, sender: ProcessId, pid: i64) -> Result<ProcessId, Errno> {
        // The `wait` producer already owns the parent/child bookkeeping, so
        // who-parents-whom is answered in one place for both syscalls. A
        // live child of `sender` resolves; anything else is `NotFound`.
        self.wait.authorise_child(sender, pid)
    }

    fn signal_task(&self, target: ProcessId, signal: Signal) -> Result<(), Errno> {
        self.deliver_signal(target, signal)
    }
}

impl<A, P> ForegroundSignal for KernelProcessSignal<A, P>
where
    A: SchedulerArch + Send + Sync + 'static,
    P: SchedulerPolicy<A> + Send + Sync + 'static,
{
    fn deliver(&self, owner: ForegroundOwner, signal: Signal) -> Result<(), Errno> {
        // The ownership was granted to one *instance* of that pid, and an id
        // whose task is gone may be drawn again, so a target whose pid now
        // names a different instance is refused outright: a `^C` must never
        // land on whoever inherited the number.
        if self.instance_of(owner.process) != owner.instance {
            return Err(Errno::NotFound);
        }
        // No parent/child authorisation here: the authority was checked when
        // the parent marked the target foreground on its own console, and
        // the console line discipline is the kernel acting on the terminal
        // owner's standing instruction.
        //
        // The line discipline maps only `^C`/`^Z`; any other signal on this
        // path is a programming error refused outright. What it does map
        // goes through the one shared delivery engine, so a `^C` and a
        // parent's `Interrupt` behave identically (including the observable
        // intake and its escalation rule) and the delivery still fails
        // closed on a target the scheduler no longer knows.
        match signal {
            Signal::Interrupt | Signal::Stop => self.deliver_signal(owner.process, signal),
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

/// Serialises host tests that touch the process-global kill gate and the
/// once-set [`DEFERRED_KILL_LANDER`]: deaths are keyed by numeric task id and
/// share one lander, so two tests claiming or landing "their" task in
/// parallel would race each other's entries.
#[cfg(test)]
pub(crate) fn running_kill_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // A panicking holder does not corrupt the `()` state; continue.
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Test-only: consume the pending foreground slot, so the console
/// line-discipline tests can assert what the filter queued without invoking
/// whichever process-global hook another test may have installed.
#[cfg(test)]
pub(crate) fn take_pending_foreground_for_test() -> Option<(ForegroundOwner, Signal)> {
    FOREGROUND_QUEUED.store(false, Ordering::Release);
    PENDING_FOREGROUND.lock().take()
}

/// Test-only: whether some [`ForegroundSignal`] hook is installed, and if
/// not, install an inert one — so the console line-discipline tests always
/// run with the interception gate open, regardless of test ordering.
#[cfg(test)]
pub(crate) fn ensure_foreground_hook_for_test() {
    struct InertHook;
    impl ForegroundSignal for InertHook {
        fn deliver(&self, _owner: ForegroundOwner, _signal: Signal) -> Result<(), Errno> {
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
    use tairix_kernel_sched_api::{
        CpuId, Priority, SchedClass, SchedResult, SchedulerConfig, StepOutcome, TaskAction,
        TaskContext, TaskId as SchedTaskId, TaskState,
    };

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

    /// A landing seam that records the retirements the producer drove it with,
    /// so a test can assert the fan-out reached a thread without reimplementing
    /// the production landing rule
    /// (`KernelSyscallHandlers::land_thread_down`) in a double.
    ///
    /// A producer-level fixture installs one only when it asserts *that* the
    /// seam is driven. Without one the producer records the terminal status
    /// through its own wait producer — the degenerate path for a build that
    /// registered no kernel resources — which is what the other tests here
    /// observe.
    struct LandingRecorder {
        landed: SpinLock<Vec<(u64, Option<i32>)>>,
    }

    impl LandingRecorder {
        const fn new() -> Self {
            Self {
                landed: SpinLock::new(Vec::new()),
            }
        }

        /// Whether the producer retired `thread` carrying `status`.
        fn landed(&self, thread: u64, status: Option<i32>) -> bool {
            self.landed.lock().contains(&(thread, status))
        }
    }

    impl TaskReclaim for LandingRecorder {
        fn land_thread_down(&self, _process: ProcessId, thread: TaskId, status: Option<i32>) {
            self.landed.lock().push((thread.0, status));
        }
    }

    /// The foreground owner a producer with no capability table records and
    /// resolves: no distinct process instances exist on such a build, so both
    /// sides carry the kernel sentinel and the delivery gate lets the signal
    /// through to the mechanics under test.
    fn fg(process: u64) -> ForegroundOwner {
        ForegroundOwner {
            process: ProcessId(process),
            instance: ProcId::KERNEL,
        }
    }

    /// A live child task, as both the scheduler's `u64` id and the signed pid
    /// spelling the syscall surface names it by.
    fn spawn_child(scheduler: &Scheduler<TestArch>) -> (u64, i64) {
        let id = scheduler
            .spawn(0, Priority::Normal, |_ctx| TaskAction::Exit)
            .expect("task admitted");
        (id, id.cast_signed())
    }

    /// Drive the two halves of the producer in the order the syscall
    /// handler drives them for a signal aimed at the sender's own child:
    /// resolve the pid against the sender's children, then deliver.
    ///
    /// Every cross-principal rule is the handler's, so it is exercised
    /// against the handler; these producer-level tests cover the own-child
    /// path and the delivery mechanics.
    fn signal_child(
        signaller: &KernelProcessSignal<TestArch, Scheduler<TestArch>>,
        sender: ProcessId,
        pid: i64,
        signal: Signal,
    ) -> Result<(), Errno> {
        let target = signaller.resolve_child(sender, pid)?;
        signaller.signal_task(target, signal)
    }

    #[test]
    fn null_process_signal_fails_closed() {
        // Neither half of the inert default answers: it resolves no target
        // and delivers to none, rather than pretending either succeeded.
        assert_eq!(
            NULL_PROCESS_SIGNAL.resolve_child(ProcessId(1), 2),
            Err(Errno::NotImplemented)
        );
        // Every variant of the closed signal set fails closed on delivery.
        for signal in [
            Signal::Continue,
            Signal::Terminate,
            Signal::Kill,
            Signal::Interrupt,
            Signal::Stop,
        ] {
            assert_eq!(
                NULL_PROCESS_SIGNAL.signal_task(ProcessId(2), signal),
                Err(Errno::NotImplemented)
            );
        }
    }

    #[test]
    fn signalling_a_non_child_fails_closed() {
        let (wait, scheduler) = scaffold();
        let signaller = KernelProcessSignal::without_thread_groups(wait, scheduler);
        // A caller with no children resolves nothing to signal.
        assert_eq!(
            signal_child(&signaller, ProcessId(1), 2, Signal::Terminate),
            Err(Errno::NotFound)
        );
        // A live task that is not *this* caller's child is off-limits: task 9
        // may not reach task 7's child through the child rule.
        let (child, child_pid) = spawn_child(scheduler);
        wait.register_child(ProcessId(7), ProcessId(child));
        assert_eq!(
            signal_child(&signaller, ProcessId(9), child_pid, Signal::Kill),
            Err(Errno::NotFound)
        );
        // The child was untouched by the unresolved signal.
        assert_eq!(scheduler.live_task_count(), 1);
    }

    #[test]
    fn terminate_ends_the_child_and_records_its_signalled_status() {
        let _overlay = stopped_overlay_test_lock();
        let (wait, scheduler) = scaffold();
        let (child, child_pid) = spawn_child(scheduler);
        wait.register_child(ProcessId(7), ProcessId(child));
        let signaller = KernelProcessSignal::without_thread_groups(wait, scheduler);

        assert_eq!(
            signal_child(&signaller, ProcessId(7), child_pid, Signal::Terminate),
            Ok(())
        );
        // The child was terminated on the scheduler.
        assert_eq!(scheduler.live_task_count(), 0);
        // ... and reaps with Terminate's POSIX-familiar 143 status, exactly
        // as if it had exited with that code itself.
        let pid = child;
        assert_eq!(
            wait.wait(
                ProcessId(7),
                TaskId(7),
                tairix_abi::WAIT_PID_ANY,
                WaitFlags::empty()
            ),
            Ok(WaitedChild {
                pid,
                status: WaitStatus::Exited(143)
            })
        );
    }

    #[test]
    fn the_kill_gate_round_trips_enter_take_and_clear() {
        // Pure gate bookkeeping, on raw ids no scheduler-backed test uses.
        assert!(!kernel_enter(0x00de_ad01));
        assert!(!kill_pending(0x00de_ad01));
        assert_eq!(kernel_exit_take_kill(0x00de_ad01), None);
        // Clearing an open window leaves nothing behind.
        assert!(!kernel_enter(0x00de_ad02));
        clear_kill_gate(0x00de_ad02);
        assert_eq!(kernel_exit_take_kill(0x00de_ad02), None);
    }

    #[test]
    fn terminating_a_task_inside_a_syscall_defers_the_kill_to_its_boundary() {
        let _overlay = stopped_overlay_test_lock();
        let (wait, scheduler) = scaffold();
        let (child, child_pid) = spawn_child(scheduler);
        wait.register_child(ProcessId(7), ProcessId(child));
        let signaller = KernelProcessSignal::without_thread_groups(wait, scheduler);

        // The child is mid-syscall: its handler may hold kernel state only
        // its own unwind can release (a mount's `SleepLock`, an in-flight
        // block-I/O descriptor), so the kill must not land here — the
        // regression this pins down is a killed writer leaving its volume's
        // lock held forever, deadlocking every later filesystem call.
        assert!(!kernel_enter(child));
        assert_eq!(
            signal_child(&signaller, ProcessId(7), child_pid, Signal::Kill),
            Ok(())
        );
        // The child was not destroyed mid-handler: it is still live on the
        // scheduler, the kill is pending against it, and the parent cannot
        // reap it yet.
        assert_eq!(scheduler.live_task_count(), 1);
        assert!(kill_pending(child));
        assert_eq!(
            wait.poll(ProcessId(7), tairix_abi::WAIT_PID_ANY, WaitFlags::empty()),
            Err(Errno::WouldBlock)
        );
        // The syscall boundary takes the deferred kill exactly once.
        assert_eq!(
            kernel_exit_take_kill(child).and_then(DeferredTeardown::reaped_status),
            Signal::Kill.termination_status()
        );
        assert_eq!(kernel_exit_take_kill(child), None);
    }

    #[test]
    fn a_deferred_kill_keeps_the_first_termination_request() {
        let _overlay = stopped_overlay_test_lock();
        let (wait, scheduler) = scaffold();
        let (child, child_pid) = spawn_child(scheduler);
        wait.register_child(ProcessId(7), ProcessId(child));
        let signaller = KernelProcessSignal::without_thread_groups(wait, scheduler);

        assert!(!kernel_enter(child));
        assert_eq!(
            signal_child(&signaller, ProcessId(7), child_pid, Signal::Terminate),
            Ok(())
        );
        // A follow-up `Kill` against the already-doomed child changes
        // nothing: it dies at the same boundary, with the first request's
        // status — matching the immediate path, where a second signal finds
        // the child already gone.
        assert_eq!(
            signal_child(&signaller, ProcessId(7), child_pid, Signal::Kill),
            Ok(())
        );
        assert_eq!(
            kernel_exit_take_kill(child).and_then(DeferredTeardown::reaped_status),
            Signal::Terminate.termination_status()
        );
        assert_eq!(scheduler.live_task_count(), 1);
    }

    /// A terminating signal drives the installed landing seam with the
    /// victim's thread and its `128 + n` status — the kill-path half of the one
    /// teardown the `exit` handler runs (regression: before the seam existed a
    /// killed task leaked its capability record, IRQ bindings, endpoints, and
    /// open files, and a pipe peer parked forever).
    #[test]
    fn terminate_drives_the_installed_landing_seam() {
        let _overlay = stopped_overlay_test_lock();
        let (wait, scheduler) = scaffold();
        let (child, child_pid) = spawn_child(scheduler);
        wait.register_child(ProcessId(7), ProcessId(child));
        let signaller = KernelProcessSignal::without_thread_groups(wait, scheduler);
        let landed: &'static LandingRecorder = Box::leak(Box::new(LandingRecorder::new()));
        signaller
            .install_task_reclaim(landed)
            .expect("first install on this producer");

        assert_eq!(
            signal_child(&signaller, ProcessId(7), child_pid, Signal::Kill),
            Ok(())
        );
        assert!(landed.landed(child, Some(137)));
    }

    /// A death against the single-threaded process `task` names, as a build
    /// with no group table claims it.
    fn claim_one(teardown: DeferredTeardown) -> ClaimedKill {
        let claims = claim_group_kill(None, teardown, None);
        assert_eq!(claims.len(), 1, "a table-less process is its leader");
        claims[0]
    }

    fn exit_of(task: u64, status: i32) -> DeferredTeardown {
        DeferredTeardown::Exit {
            process: ProcessId(task),
            status,
        }
    }

    /// The owed count moves with the register under the gate lock, so the two
    /// agree whenever it is held, whatever other tests do to the shared gate.
    fn owed_count_is_exact() -> bool {
        let gate = KILL_GATE.lock();
        OWED_KILLS.load(Ordering::Relaxed) == gate.owed.len()
    }

    #[test]
    fn a_claimed_death_is_recorded_once_and_taken_once() {
        let _g = running_kill_test_lock();
        let a = 0x00c0_ffe1;
        clear_kill_gate(a);

        let first = claim_one(exit_of(a, 137));
        assert_eq!(first.thread, a);
        assert_eq!(first.site, KillSite::Retire);
        assert!(first.recorded);
        assert!(
            !claim_one(exit_of(a, 143)).recorded,
            "the first claim wins; a later one records nothing"
        );
        assert!(owed_count_is_exact());
        assert_eq!(take_owed_kill(a), Some(exit_of(a, 137)));
        assert_eq!(take_owed_kill(a), None, "taken exactly once");
        assert!(owed_count_is_exact());
    }

    #[test]
    fn a_death_claimed_inside_a_kernel_body_is_owed_at_its_boundary() {
        let _g = running_kill_test_lock();
        let task = 0x00c0_ffe8;
        clear_kill_gate(task);

        assert!(!kernel_enter(task));
        assert_eq!(claim_one(exit_of(task, 137)).site, KillSite::Boundary);
        assert!(kill_pending(task), "the body's park loops must unwind");
        assert_eq!(
            kernel_exit_take_kill(task).and_then(DeferredTeardown::reaped_status),
            Some(137)
        );
        assert!(!kill_pending(task));
    }

    /// The other order. A thread already owing a death may already have been
    /// told to die, and the scheduler retires such a thread at its next
    /// stopping point — inside a body, that frees a stack whose frames own
    /// kernel state. So it enters the kernel only to reach its boundary.
    #[test]
    fn a_thread_owing_a_death_enters_the_kernel_only_to_die() {
        let _g = running_kill_test_lock();
        let driver = 0x00c0_ffe4;
        clear_kill_gate(driver);
        let plain = DeferredTeardown::Plain {
            process: ProcessId(driver),
        };

        assert_eq!(claim_one(plain).site, KillSite::Retire);
        assert!(kernel_enter(driver), "the body must not run");
        assert_eq!(kernel_exit_take_kill(driver), Some(plain));
    }

    /// Tasks the deterministic test lander reclaimed, and the signalled
    /// statuses it reaped — kept as process-global sets so the landing tests
    /// share one installed lander regardless of test order (the global lander
    /// is set-once).
    static LAND_RECLAIMED: SpinLock<BTreeSet<u64>> = SpinLock::new(BTreeSet::new());
    static LAND_REAPED: SpinLock<BTreeMap<u64, i32>> = SpinLock::new(BTreeMap::new());

    /// A deterministic [`DeferredKillLander`] for the dispatch-loop landing
    /// tests: it records what it reclaimed and any status it reaped, mirroring
    /// the real producer's `Exit`-versus-`Plain` split.
    struct TestLander;
    impl DeferredKillLander for TestLander {
        fn land_deferred_teardown(&self, task: TaskId, teardown: DeferredTeardown) {
            if let Some(status) = teardown.reaped_status() {
                LAND_REAPED.lock().insert(task.0, status);
            }
            LAND_RECLAIMED.lock().insert(task.0);
        }
    }
    static TEST_LANDER: TestLander = TestLander;

    /// Ensure some deferred-kill lander is installed and report whether it is
    /// our [`TestLander`]. The lander is a process-global set-once cell, and
    /// another suite may have installed the real producer first; a landing
    /// still *takes* the death then, which every landing test asserts, but its
    /// side effects land elsewhere, so those assertions are gated on this.
    fn ensure_test_lander() -> bool {
        static TEST_LANDER_INSTALLED: AtomicUsize = AtomicUsize::new(0);
        if install_deferred_kill_lander(&TEST_LANDER).is_ok() {
            TEST_LANDER_INSTALLED.store(1, Ordering::Relaxed);
        }
        TEST_LANDER_INSTALLED.load(Ordering::Relaxed) == 1
    }

    /// A dispatch returns on a yield and a park as well as on a retire. A
    /// thread owing a death that its dispatch queued again may yet run, so its
    /// process is reclaimed only once the scheduler has retired it — and then
    /// exactly once.
    #[test]
    fn only_a_retired_thread_has_its_death_landed_by_the_dispatch_loop() {
        let _g = running_kill_test_lock();
        let ours = ensure_test_lander();
        let child = 0x00c0_ffe5;
        clear_kill_gate(child);
        LAND_RECLAIMED.lock().remove(&child);
        LAND_REAPED.lock().remove(&child);

        let _ = claim_one(exit_of(child, 137));
        land_retired_kill(child, || false);
        assert!(
            kill_pending(child),
            "a thread not retired keeps its death owed"
        );
        if ours {
            assert!(!LAND_RECLAIMED.lock().contains(&child));
        }

        land_retired_kill(child, || true);
        assert!(!kill_pending(child), "a retired thread's death is taken");
        if ours {
            assert!(LAND_RECLAIMED.lock().contains(&child));
            assert_eq!(LAND_REAPED.lock().get(&child), Some(&137));
        }

        LAND_RECLAIMED.lock().remove(&child);
        land_retired_kill(child, || true);
        if ours {
            assert!(!LAND_RECLAIMED.lock().contains(&child), "landed only once");
        }
    }

    /// A thread's own teardown clears the gate, so a death claimed while it
    /// was alive is never landed a second time on a process already reclaimed.
    #[test]
    fn clearing_the_gate_drops_a_death_without_landing_it() {
        let _g = running_kill_test_lock();
        let ours = ensure_test_lander();
        let child = 0x00c0_ffe6;
        clear_kill_gate(child);
        LAND_RECLAIMED.lock().remove(&child);

        let _ = claim_one(exit_of(child, 137));
        clear_kill_gate(child);
        land_retired_kill(child, || true);
        assert_eq!(take_owed_kill(child), None);
        if ours {
            assert!(!LAND_RECLAIMED.lock().contains(&child));
        }
    }

    /// An unloaded driver's death reclaims its resources and reaps nothing: a
    /// driver is not a waited-for child.
    #[test]
    fn a_plain_death_lands_without_a_reap() {
        let _g = running_kill_test_lock();
        let ours = ensure_test_lander();
        let driver = 0x00c0_ffe7;
        clear_kill_gate(driver);
        LAND_RECLAIMED.lock().remove(&driver);
        LAND_REAPED.lock().remove(&driver);

        let _ = claim_one(DeferredTeardown::Plain {
            process: ProcessId(driver),
        });
        land_retired_kill(driver, || true);
        assert!(!kill_pending(driver));
        if ours {
            assert!(LAND_RECLAIMED.lock().contains(&driver));
            assert!(!LAND_REAPED.lock().contains_key(&driver));
        }
    }

    /// How [`KillsElsewhere`] answers a kill.
    #[derive(Copy, Clone)]
    enum Victim {
        /// Executing on another CPU, which runs forward to the victim's retire
        /// — its dispatch loop included — before the answer reaches the killer.
        /// The interleaving in which a death recorded after the victim was told
        /// to die arrives after the only point that looks for it: the parent's
        /// `wait` never sees the exit and the process is never reclaimed
        /// (`stress-qemu-aarch64`, `plans/OPEN-DEFECTS.md` D296).
        RetiresBeforeTheKillerReturns,
        /// Executing on another CPU for as long as the test runs: the first
        /// kill is deferred to its retire and every later one finds it doomed.
        StillRunning,
    }

    /// A real scheduler whose `exit` answers as though each victim were on
    /// another CPU, as [`Victim`] says.
    struct KillsElsewhere {
        inner: &'static Scheduler<TestArch>,
        victim: Victim,
        doomed: SpinLock<BTreeSet<SchedTaskId>>,
    }

    impl SchedulerPolicy<TestArch> for KillsElsewhere {
        fn new(config: SchedulerConfig, arch: std::sync::Arc<TestArch>) -> SchedResult<Self> {
            let inner: &'static Scheduler<TestArch> =
                Box::leak(Box::new(Scheduler::new(config, arch)?));
            Ok(Self {
                inner,
                victim: Victim::StillRunning,
                doomed: SpinLock::new(BTreeSet::new()),
            })
        }
        fn cpu_count(&self) -> u32 {
            self.inner.cpu_count()
        }
        fn config(&self) -> SchedulerConfig {
            SchedulerPolicy::config(self.inner)
        }
        fn spawn<F>(&self, cpu: CpuId, priority: Priority, body: F) -> SchedResult<SchedTaskId>
        where
            F: FnMut(&mut TaskContext) -> TaskAction + Send + 'static,
        {
            SchedulerPolicy::spawn(self.inner, cpu, priority, body)
        }
        fn spawn_parked<F>(
            &self,
            cpu: CpuId,
            priority: Priority,
            body: F,
        ) -> SchedResult<SchedTaskId>
        where
            F: FnMut(&mut TaskContext) -> TaskAction + Send + 'static,
        {
            SchedulerPolicy::spawn_parked(self.inner, cpu, priority, body)
        }
        fn spawn_parked_as<F>(
            &self,
            id: SchedTaskId,
            cpu: CpuId,
            priority: Priority,
            body: F,
        ) -> SchedResult<SchedTaskId>
        where
            F: FnMut(&mut TaskContext) -> TaskAction + Send + 'static,
        {
            SchedulerPolicy::spawn_parked_as(self.inner, id, cpu, priority, body)
        }
        fn park(&self, id: SchedTaskId) -> SchedResult<()> {
            SchedulerPolicy::park(self.inner, id)
        }
        fn unpark(&self, id: SchedTaskId) -> SchedResult<()> {
            SchedulerPolicy::unpark(self.inner, id)
        }
        fn exit(&self, id: SchedTaskId) -> SchedResult<ExitDisposition> {
            match self.victim {
                Victim::RetiresBeforeTheKillerReturns => {
                    match SchedulerPolicy::exit(self.inner, id)? {
                        ExitDisposition::Quiesced => {
                            land_retired_kill(id, || self.inner.state_of(id) == TaskState::Exited);
                            Ok(ExitDisposition::Deferred)
                        }
                        other => Ok(other),
                    }
                }
                Victim::StillRunning if self.doomed.lock().insert(id) => {
                    Ok(ExitDisposition::Deferred)
                }
                Victim::StillRunning => Ok(ExitDisposition::AlreadyExited),
            }
        }
        fn on_timer_tick(&self, cpu: CpuId) -> SchedResult<()> {
            SchedulerPolicy::on_timer_tick(self.inner, cpu)
        }
        fn preemption_count(&self, cpu: CpuId) -> SchedResult<u64> {
            SchedulerPolicy::preemption_count(self.inner, cpu)
        }
        fn total_preemption_count(&self) -> u64 {
            SchedulerPolicy::total_preemption_count(self.inner)
        }
        fn step(&self, cpu: CpuId) -> SchedResult<StepOutcome> {
            SchedulerPolicy::step(self.inner, cpu)
        }
        fn run_count(&self, id: SchedTaskId) -> SchedResult<u64> {
            SchedulerPolicy::run_count(self.inner, id)
        }
        fn cpu_ticks_of(&self, id: SchedTaskId) -> SchedResult<u64> {
            SchedulerPolicy::cpu_ticks_of(self.inner, id)
        }
        fn cpu_busy_ticks(&self, cpu: CpuId) -> SchedResult<u64> {
            SchedulerPolicy::cpu_busy_ticks(self.inner, cpu)
        }
        fn cpu_switches(&self, cpu: CpuId) -> SchedResult<u64> {
            SchedulerPolicy::cpu_switches(self.inner, cpu)
        }
        fn queue_depth(&self, cpu: CpuId) -> SchedResult<u64> {
            SchedulerPolicy::queue_depth(self.inner, cpu)
        }
        fn has_ready_work(&self, cpu: CpuId) -> SchedResult<bool> {
            SchedulerPolicy::has_ready_work(self.inner, cpu)
        }
        fn state_of(&self, id: SchedTaskId) -> TaskState {
            SchedulerPolicy::state_of(self.inner, id)
        }
        fn live_task_count(&self) -> usize {
            SchedulerPolicy::live_task_count(self.inner)
        }
        fn current_task(&self, cpu: CpuId) -> Option<SchedTaskId> {
            SchedulerPolicy::current_task(self.inner, cpu)
        }
        fn set_sched_class(&self, id: SchedTaskId, class: SchedClass) -> SchedResult<()> {
            SchedulerPolicy::set_sched_class(self.inner, id, class)
        }
        fn sched_class(&self, id: SchedTaskId) -> SchedResult<SchedClass> {
            SchedulerPolicy::sched_class(self.inner, id)
        }
        fn set_priority(&self, id: SchedTaskId, priority: Priority) -> SchedResult<()> {
            SchedulerPolicy::set_priority(self.inner, id, priority)
        }
        fn priority(&self, id: SchedTaskId) -> SchedResult<Priority> {
            SchedulerPolicy::priority(self.inner, id)
        }
    }

    #[test]
    fn a_kill_whose_victim_retires_before_the_killer_returns_still_lands() {
        let _overlay = stopped_overlay_test_lock();
        let _g = running_kill_test_lock();
        let ours = ensure_test_lander();
        let (wait, inner) = scaffold();
        let scheduler: &'static KillsElsewhere = Box::leak(Box::new(KillsElsewhere {
            inner,
            victim: Victim::RetiresBeforeTheKillerReturns,
            doomed: SpinLock::new(BTreeSet::new()),
        }));
        let (child, child_pid) = spawn_child(inner);
        wait.register_child(ProcessId(7), ProcessId(child));
        let signaller = KernelProcessSignal::without_thread_groups(wait, scheduler);
        LAND_REAPED.lock().remove(&child);

        let target = signaller
            .resolve_child(ProcessId(7), child_pid)
            .expect("a live child");
        assert_eq!(signaller.signal_task(target, Signal::Terminate), Ok(()));

        assert!(!kill_pending(child), "no death left owed after its landing");
        if ours {
            assert_eq!(
                LAND_REAPED.lock().get(&child),
                Some(&143),
                "the retire landed the death the kill recorded"
            );
        }
    }

    /// A group exit that finds a sibling already dying — a kill still
    /// deferred to that sibling's retire — leaves its death to that retire. It
    /// used to land the sibling itself as well, so the retire landed it a
    /// second time: a thread the group table no longer held reads as the
    /// group's last, and the process was torn down twice.
    #[test]
    fn a_group_exit_leaves_a_dying_sibling_to_the_death_it_already_owes() {
        let _overlay = stopped_overlay_test_lock();
        let _g = running_kill_test_lock();
        let (wait, inner) = scaffold();
        let scheduler: &'static KillsElsewhere = Box::leak(Box::new(KillsElsewhere {
            inner,
            victim: Victim::StillRunning,
            doomed: SpinLock::new(BTreeSet::new()),
        }));
        let (leader, _) = spawn_child(inner);
        let (sibling, _) = spawn_child(inner);
        let sink: &'static crate::test_sink::TestSink =
            Box::leak(Box::new(crate::test_sink::TestSink::new()));
        let caps: &'static RwLock<CapTable> = Box::leak(Box::new(RwLock::new(CapTable::new())));
        caps.write()
            .insert(tairix_kernel_sec::TaskCapabilities::derive(
                ProcessId(leader),
                tairix_kernel_sec::UserId(0),
                tairix_caps::CapabilitySet::empty(),
                tairix_caps::CapabilitySet::empty(),
                sink,
            ));
        caps.write()
            .register_thread(TaskId(sibling), ProcessId(leader))
            .expect("the sibling joins the group");
        let signaller = KernelProcessSignal::new(wait, scheduler, caps);
        let landed: &'static LandingRecorder = Box::leak(Box::new(LandingRecorder::new()));
        signaller
            .install_task_reclaim(landed)
            .expect("first install on this producer");

        assert_eq!(
            signaller.signal_task(ProcessId(leader), Signal::Terminate),
            Ok(())
        );
        signaller.terminate_siblings(ProcessId(leader), TaskId(leader), 0);

        assert!(
            !landed.landed(sibling, Some(0)) && !landed.landed(sibling, Some(143)),
            "the dying sibling is landed by its own retire alone"
        );
        assert_eq!(take_owed_kill(sibling), Some(exit_of(leader, 143)));
        clear_kill_gate(leader);
    }

    /// The real [`KernelProcessSignal`] lander drives the *same* landing seam
    /// the immediate terminate path does (one definition), passing an `Exit`
    /// teardown's status through and a `Plain` teardown's absence of one — so a
    /// driver unload reclaims without minting a zombie no parent will reap.
    /// Whether a status then reaches the wait table is the landing rule's own
    /// contract, tested against the real implementation in `syscalls`.
    #[test]
    fn the_signal_producer_lander_lands_with_and_without_a_status() {
        let _overlay = stopped_overlay_test_lock();
        let (wait, scheduler) = scaffold();
        let (child, _pid) = spawn_child(scheduler);
        wait.register_child(ProcessId(7), ProcessId(child));
        let signaller = KernelProcessSignal::without_thread_groups(wait, scheduler);
        let landed: &'static LandingRecorder = Box::leak(Box::new(LandingRecorder::new()));
        signaller
            .install_task_reclaim(landed)
            .expect("first install on this producer");

        signaller.land_deferred_teardown(
            TaskId(child),
            DeferredTeardown::Exit {
                process: ProcessId(child),
                status: 137,
            },
        );
        assert!(landed.landed(child, Some(137)));

        // A driver unload whose process was still executing: a second,
        // unregistered id, landed with no status.
        let driver = scheduler
            .spawn(0, Priority::Normal, |_| TaskAction::Exit)
            .expect("driver task");
        signaller.land_deferred_teardown(
            TaskId(driver),
            DeferredTeardown::Plain {
                process: ProcessId(driver),
            },
        );
        assert!(landed.landed(driver, None));
    }

    #[test]
    fn kill_records_its_own_status() {
        let _overlay = stopped_overlay_test_lock();
        let (wait, scheduler) = scaffold();
        let (child, child_pid) = spawn_child(scheduler);
        wait.register_child(ProcessId(7), ProcessId(child));
        let signaller = KernelProcessSignal::without_thread_groups(wait, scheduler);

        assert_eq!(
            signal_child(&signaller, ProcessId(7), child_pid, Signal::Kill),
            Ok(())
        );
        let pid = child;
        // Kill surfaces as SIGKILL's familiar 137, distinct from Terminate.
        assert_eq!(
            wait.wait(
                ProcessId(7),
                TaskId(7),
                tairix_abi::WAIT_PID_ANY,
                WaitFlags::empty()
            ),
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
        wait.register_child(ProcessId(7), ProcessId(child));
        let signaller = KernelProcessSignal::without_thread_groups(wait, scheduler);

        assert_eq!(
            signal_child(&signaller, ProcessId(7), child_pid, Signal::Interrupt),
            Ok(())
        );
        assert_eq!(scheduler.live_task_count(), 0);
        let pid = child;
        // Interrupt surfaces as the `^C` 130 every POSIX shell reports.
        assert_eq!(
            wait.wait(
                ProcessId(7),
                TaskId(7),
                tairix_abi::WAIT_PID_ANY,
                WaitFlags::empty()
            ),
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
        wait.register_child(ProcessId(7), ProcessId(child));
        let signaller = KernelProcessSignal::without_thread_groups(wait, scheduler);

        assert_eq!(
            signal_child(&signaller, ProcessId(7), child_pid, Signal::Stop),
            Ok(())
        );
        // The stop overlay holds the child, so no broadcast wake can run it.
        assert!(task_is_stopped(child));
        // The child is still live (stopped, not terminated) …
        assert_eq!(scheduler.live_task_count(), 1);
        // … and a STOPPED wait observes the stop without reaping.
        assert_eq!(
            wait.poll(
                ProcessId(7),
                tairix_abi::WAIT_PID_ANY,
                WaitFlags::from_bits(WaitFlags::NONBLOCK.bits() | WaitFlags::STOPPED.bits())
                    .expect("defined bits")
            ),
            Ok(WaitedChild {
                pid: child,
                status: WaitStatus::Stopped(Signal::Stop)
            })
        );
        // Continue lifts the overlay and resumes the child.
        assert_eq!(
            signal_child(&signaller, ProcessId(7), child_pid, Signal::Continue),
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
        wait.register_child(ProcessId(7), ProcessId(child));
        let signaller = KernelProcessSignal::without_thread_groups(wait, scheduler);

        assert_eq!(
            signal_child(&signaller, ProcessId(7), child_pid, Signal::Stop),
            Ok(())
        );
        assert_eq!(
            signal_child(&signaller, ProcessId(7), child_pid, Signal::Continue),
            Ok(())
        );
        // The unobserved stop was cleared by the resume: nothing to report.
        assert_eq!(
            wait.poll(
                ProcessId(7),
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
        wait.register_child(ProcessId(7), ProcessId(child));
        let signaller = KernelProcessSignal::without_thread_groups(wait, scheduler);

        assert_eq!(
            signal_child(&signaller, ProcessId(7), child_pid, Signal::Stop),
            Ok(())
        );
        assert!(task_is_stopped(child));
        assert_eq!(
            signal_child(&signaller, ProcessId(7), child_pid, Signal::Kill),
            Ok(())
        );
        // The dead child leaves no stale overlay entry behind.
        assert!(!task_is_stopped(child));
        // The terminal exit superseded the unobserved stop.
        assert_eq!(
            wait.wait(
                ProcessId(7),
                TaskId(7),
                tairix_abi::WAIT_PID_ANY,
                WaitFlags::STOPPED
            ),
            Ok(WaitedChild {
                pid: child,
                status: WaitStatus::Exited(137)
            })
        );
    }

    #[test]
    fn foreground_deliver_maps_only_the_line_discipline_signals() {
        let _overlay = stopped_overlay_test_lock();
        let (wait, scheduler) = scaffold();
        let (child, _child_pid) = spawn_child(scheduler);
        wait.register_child(ProcessId(7), ProcessId(child));
        let signaller = KernelProcessSignal::without_thread_groups(wait, scheduler);

        // The console path never delivers Continue/Terminate/Kill.
        for signal in [Signal::Continue, Signal::Terminate, Signal::Kill] {
            assert_eq!(signaller.deliver(fg(child), signal), Err(Errno::OutOfRange));
        }
        // `^Z` stops the foreground task …
        assert_eq!(signaller.deliver(fg(child), Signal::Stop), Ok(()));
        assert!(task_is_stopped(child));
        assert_eq!(scheduler.live_task_count(), 1);
        // … and `^C` terminates it with the 130 status.
        assert_eq!(signaller.deliver(fg(child), Signal::Interrupt), Ok(()));
        assert_eq!(scheduler.live_task_count(), 0);
        assert_eq!(
            wait.wait(
                ProcessId(7),
                TaskId(7),
                tairix_abi::WAIT_PID_ANY,
                WaitFlags::empty()
            ),
            Ok(WaitedChild {
                pid: child,
                status: WaitStatus::Exited(130)
            })
        );
    }

    #[test]
    fn foreground_deliver_to_a_dead_target_fails_closed() {
        let (wait, scheduler) = scaffold();
        let signaller = KernelProcessSignal::without_thread_groups(wait, scheduler);
        // Task 9999 was never admitted: the delivery reaches no one.
        assert_eq!(
            signaller.deliver(fg(9999), Signal::Interrupt),
            Err(Errno::NotFound)
        );
        assert_eq!(
            signaller.deliver(fg(9999), Signal::Stop),
            Err(Errno::NotFound)
        );
        // A refused stop leaves no overlay entry behind.
        assert!(!task_is_stopped(9999));
    }

    /// Register `process` in `caps` as a live process instance, so the
    /// foreground delivery gate can resolve an instance for it.
    fn admit_instance(
        caps: &RwLock<CapTable>,
        process: ProcessId,
        instance: ProcId,
    ) -> ForegroundOwner {
        let sink = crate::test_sink::TestSink::new();
        let record = tairix_kernel_sec::TaskCapabilities::derive(
            process,
            tairix_kernel_sec::UserId(0),
            tairix_caps::CapabilitySet::empty(),
            tairix_caps::CapabilitySet::empty(),
            &sink,
        )
        .with_proc_id(instance);
        caps.write().insert(record);
        ForegroundOwner { process, instance }
    }

    /// A `^C` aimed at one process instance is refused once the pid names a
    /// different one — the id may be drawn again after its task is gone, and
    /// a mis-delivered kill would land on whoever inherited the number.
    #[test]
    fn foreground_deliver_refuses_a_stale_process_instance() {
        let _overlay = stopped_overlay_test_lock();
        let (wait, scheduler) = scaffold();
        let caps: &'static RwLock<CapTable> = Box::leak(Box::new(RwLock::new(CapTable::new())));
        let (child, _child_pid) = spawn_child(scheduler);
        wait.register_child(ProcessId(7), ProcessId(child));
        let signaller: &KernelProcessSignal<TestArch, Scheduler<TestArch>> =
            Box::leak(Box::new(KernelProcessSignal::new(wait, scheduler, caps)));

        // The keystroke was aimed at the instance the grant saw …
        let aimed_at = admit_instance(caps, ProcessId(child), ProcId::from_raw([0x11; 16]));
        // … but by delivery time the pid names a different one.
        admit_instance(caps, ProcessId(child), ProcId::from_raw([0x22; 16]));
        assert_eq!(
            signaller.deliver(aimed_at, Signal::Interrupt),
            Err(Errno::NotFound)
        );
        // The successor is untouched: neither killed nor stopped.
        assert_eq!(scheduler.live_task_count(), 1);
        assert!(!task_is_stopped(child));
        assert_eq!(
            signaller.deliver(aimed_at, Signal::Stop),
            Err(Errno::NotFound)
        );
        assert!(!task_is_stopped(child));

        // A record the table no longer holds is equally refused: an absent
        // instance never matches a real one.
        caps.write().remove(ProcessId(child));
        assert_eq!(
            signaller.deliver(aimed_at, Signal::Interrupt),
            Err(Errno::NotFound)
        );
        assert_eq!(scheduler.live_task_count(), 1);
    }

    /// The same gate passes the signal through when the pid still names the
    /// instance the ownership was granted at.
    #[test]
    fn foreground_deliver_reaches_the_instance_it_was_aimed_at() {
        let _overlay = stopped_overlay_test_lock();
        let (wait, scheduler) = scaffold();
        let caps: &'static RwLock<CapTable> = Box::leak(Box::new(RwLock::new(CapTable::new())));
        let (child, _child_pid) = spawn_child(scheduler);
        wait.register_child(ProcessId(7), ProcessId(child));
        let signaller: &KernelProcessSignal<TestArch, Scheduler<TestArch>> =
            Box::leak(Box::new(KernelProcessSignal::new(wait, scheduler, caps)));

        let aimed_at = admit_instance(caps, ProcessId(child), ProcId::from_raw([0x33; 16]));
        assert_eq!(signaller.deliver(aimed_at, Signal::Stop), Ok(()));
        assert!(task_is_stopped(child));
        assert_eq!(signaller.deliver(aimed_at, Signal::Interrupt), Ok(()));
        assert_eq!(scheduler.live_task_count(), 0);
    }

    #[test]
    fn queued_foreground_signal_round_trips_through_the_pending_slot() {
        // The pending slot is process-global, so serialise against the
        // console line-discipline tests that share it.
        let _guard = foreground_test_lock();
        queue_foreground_signal(fg(77), Signal::Interrupt);
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
        wait.register_child(ProcessId(7), ProcessId(child));
        let signaller = KernelProcessSignal::without_thread_groups(wait, scheduler);

        // The child is runnable, not stopped, so Continue succeeds as a no-op
        // (it neither terminates the child nor records an exit).
        assert_eq!(
            signal_child(&signaller, ProcessId(7), child_pid, Signal::Continue),
            Ok(())
        );
        assert_eq!(scheduler.live_task_count(), 1);
        // The child is still a live, signallable child (no status recorded).
        assert_eq!(
            wait.authorise_child(ProcessId(7), child_pid),
            Ok(ProcessId(child))
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
        wait.register_child(ProcessId(7), ProcessId(child));
        let signaller = KernelProcessSignal::without_thread_groups(wait, scheduler);

        intake_enable(child);
        // The interrupt is recorded, not delivered as a termination: the
        // child stays live and nothing becomes reapable.
        assert_eq!(
            signal_child(&signaller, ProcessId(7), child_pid, Signal::Interrupt),
            Ok(())
        );
        assert_eq!(scheduler.live_task_count(), 1);
        assert_eq!(
            wait.poll(ProcessId(7), tairix_abi::WAIT_PID_ANY, WaitFlags::NONBLOCK),
            Err(Errno::WouldBlock)
        );
        assert_eq!(intake_take(child), Ok(Signal::Interrupt));
        // `Kill` is unconditionally fatal regardless of the opt-in and
        // reaps with SIGKILL's familiar 137.
        assert_eq!(
            signal_child(&signaller, ProcessId(7), child_pid, Signal::Kill),
            Ok(())
        );
        assert_eq!(scheduler.live_task_count(), 0);
        assert_eq!(
            wait.wait(
                ProcessId(7),
                TaskId(7),
                tairix_abi::WAIT_PID_ANY,
                WaitFlags::empty()
            ),
            Ok(WaitedChild {
                pid: child,
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
        wait.register_child(ProcessId(7), ProcessId(child));
        let signaller = KernelProcessSignal::without_thread_groups(wait, scheduler);

        intake_enable(child);
        assert_eq!(
            signal_child(&signaller, ProcessId(7), child_pid, Signal::Interrupt),
            Ok(())
        );
        assert_eq!(scheduler.live_task_count(), 1);
        // The second interrupt finds the slot occupied and terminates the
        // child with the `^C` 130 — an unresponsive opted-in program stays
        // killable with plain `^C ^C`.
        assert_eq!(
            signal_child(&signaller, ProcessId(7), child_pid, Signal::Interrupt),
            Ok(())
        );
        assert_eq!(scheduler.live_task_count(), 0);
        assert_eq!(
            wait.wait(
                ProcessId(7),
                TaskId(7),
                tairix_abi::WAIT_PID_ANY,
                WaitFlags::empty()
            ),
            Ok(WaitedChild {
                pid: child,
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
        wait.register_child(ProcessId(7), ProcessId(child));
        let signaller = KernelProcessSignal::without_thread_groups(wait, scheduler);

        intake_enable(child);
        // The console `^C` is observed, not fatal …
        assert_eq!(signaller.deliver(fg(child), Signal::Interrupt), Ok(()));
        assert_eq!(scheduler.live_task_count(), 1);
        assert_eq!(intake_take(child), Ok(Signal::Interrupt));
        // … and `^Z` still stops the opted-in target (only termination
        // requests are observable; `Stop` stays scheduler-side).
        assert_eq!(signaller.deliver(fg(child), Signal::Stop), Ok(()));
        assert!(task_is_stopped(child));
        // Lift the overlay so the shared set holds no stale entry.
        assert_eq!(signaller.deliver(fg(child), Signal::Interrupt), Ok(()));
        assert_eq!(intake_take(child), Ok(Signal::Interrupt));
        STOPPED_TASKS.lock().remove(&child);
        clear_intake(child);
    }

    #[test]
    fn a_signalled_child_cannot_be_signalled_twice() {
        let _overlay = stopped_overlay_test_lock();
        let (wait, scheduler) = scaffold();
        let (child, child_pid) = spawn_child(scheduler);
        wait.register_child(ProcessId(7), ProcessId(child));
        let signaller = KernelProcessSignal::without_thread_groups(wait, scheduler);

        assert_eq!(
            signal_child(&signaller, ProcessId(7), child_pid, Signal::Terminate),
            Ok(())
        );
        // Once terminated the child is a zombie awaiting reap, not a live
        // process: a second signal fails closed rather than re-terminating it.
        assert_eq!(
            signal_child(&signaller, ProcessId(7), child_pid, Signal::Kill),
            Err(Errno::NotFound)
        );
    }
}
