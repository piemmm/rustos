//! The kernel-side process-wait seam the `wait` (`abi-v1` number 16)
//! syscall uses (`plans/SPAWN.md` SP6).
//!
//! [`ProcessWait`] is the one object-safe boundary between the
//! arch-neutral syscall handler in `kernel/core` and the scheduler-side
//! producer that tracks the parent/child + exit-status bookkeeping, blocks
//! the caller until one of its children exits, reaps the zombie, and reports
//! the child's exit code. The producer's three responsibilities map onto the
//! trait's three methods:
//!
//! * [`ProcessWait::register_child`] — record a freshly spawned child against
//!   its parent (called from the `spawn` admit path).
//! * [`ProcessWait::record_exit`] — capture a child's exit code when it exits
//!   (called from the `exit` handler).
//! * [`ProcessWait::wait`] — block the parent until a matching child is
//!   reapable, reap it, and report it (called from the `wait` handler).
//!
//! Blocking the caller means cooperatively parking it back on the scheduler
//! until a child becomes reapable — work that belongs with the live scheduler
//! integration, not the decoupled handler — so, like the
//! [`ArchImageBuilder`](crate::spawn::ArchImageBuilder) and
//! [`MemMap`](crate::memmap::MemMap) producers, the concrete producer
//! ([`KernelProcessWait`]) is installed at boot through the
//! `with_process_wait` builder and the handler reaches it through this trait.
//!
//! Until a producer is installed the handler holds [`NULL_PROCESS_WAIT`],
//! which fails closed: every `wait` returns [`Errno::NotImplemented`] and the bookkeeping hooks are inert — exactly as
//! [`NULL_MEM_MAP`](crate::memmap::NULL_MEM_MAP) and
//! [`NULL_ARCH_IMAGE_BUILDER`](crate::spawn::NULL_ARCH_IMAGE_BUILDER) do for their
//! syscalls.

use alloc::collections::BTreeMap;

use tairix_abi::{Errno, Signal, WaitFlags, WaitStatus, WAIT_PID_ANY};
use tairix_kernel_sched_api::SchedulerArch;
use tairix_kernel_sec::ProcessId;
use tairix_sync::SpinLock;

use crate::dispatch_slot::RescheduleAction;
use crate::kthread::reschedule_current;

/// A child event reported by [`ProcessWait::wait`].
///
/// Carries the child's PID (the value the `wait` syscall returns to the
/// caller) and its typed status (the value the kernel encodes into the
/// caller's `status` out-pointer): an exited child was reaped and removed,
/// a stopped child (reported only under [`WaitFlags::STOPPED`]) stays
/// tracked and resumable.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct WaitedChild {
    /// PID of the reported child.
    pub pid: u32,
    /// What happened to it: exited (reaped) or stopped (still tracked).
    pub status: WaitStatus,
}

/// The kernel-side producer of the `wait` syscall.
///
/// Implemented by the scheduler-side producer that tracks each spawned
/// child against its parent, captures a child's exit code on exit, and
/// blocks `parent` until one of its children is reapable. The bookkeeping
/// methods carry default no-op bodies so the fail-closed default
/// ([`NullProcessWait`]) and the host-test doubles announce an inert
/// interface without restating them; the concrete [`KernelProcessWait`]
/// overrides all three.
///
/// Implementations must be [`Sync`]: the single installed producer is
/// shared by the per-CPU syscall handlers, exactly like the console device,
/// the spawn producer, and the anonymous-memory producer.
pub trait ProcessWait: Sync {
    /// Block `parent` until the child selected by `pid` exits, reap it, and
    /// return the reaped child's PID and exit code.
    ///
    /// `pid` is either a specific child's PID or [`tairix_abi::WAIT_PID_ANY`]
    /// to wait for whichever of `parent`'s children exits next. With
    /// [`WaitFlags::STOPPED`] set in `flags` the wait also completes for a
    /// child freshly stopped by [`Signal::Stop`] — reported without being
    /// reaped, each stop exactly once. The handler
    /// has already validated that the caller passed a non-null `status`
    /// pointer (the dispatcher rejects a null `UserPtr`); the implementation
    /// validates the parent/child relationship — a process may only reap its
    /// **own** children — and fails closed.
    ///
    /// # Errors
    ///
    /// Returns [`Errno::NotFound`] when `pid` does not name a child of
    /// `parent` (and `parent` has no children, for [`tairix_abi::WAIT_PID_ANY`]).
    /// The default producer ([`NullProcessWait`]) returns
    /// [`Errno::NotImplemented`] to mark an inert interface.
    fn wait(&self, parent: ProcessId, pid: i32, flags: WaitFlags) -> Result<WaitedChild, Errno>;

    /// Non-blocking counterpart to [`Self::wait`]: try to report a child of
    /// `parent` selected by `pid` **without ever parking the caller**.
    ///
    /// This backs `WaitFlags::NONBLOCK`, the poll the shell's job
    /// control uses to report finished background jobs before the next
    /// prompt; with [`WaitFlags::STOPPED`] also set the poll reports a
    /// pending stop the same way.
    ///
    /// # Errors
    ///
    /// * [`Errno::WouldBlock`] when a matching child exists but has nothing
    ///   to report yet — the `abi-v1` "nothing yet, retry" signal, so a
    ///   polling caller neither blocks nor floods the audit log.
    /// * [`Errno::NotFound`] when `pid` names no child of `parent`.
    ///
    /// The default fails closed with [`Errno::NotImplemented`] so a producer
    /// that predates the poll path — and the inert [`NullProcessWait`] —
    /// never fabricates a reaped child; [`KernelProcessWait`] overrides it.
    fn poll(&self, _parent: ProcessId, _pid: i32, _flags: WaitFlags) -> Result<WaitedChild, Errno> {
        Err(Errno::NotImplemented)
    }

    /// Record that `child` was spawned by `parent`.
    ///
    /// Called from the `spawn` admit path the instant a child is admitted,
    /// so a subsequent [`Self::wait`] can validate the parent/child
    /// relationship and reap it (a process may only reap
    /// its own children). The default is a no-op so the fail-closed default
    /// and the host-test doubles need not restate it.
    fn register_child(&self, _parent: ProcessId, _child: ProcessId) {}

    /// Record that `process` exited with `code`.
    ///
    /// Called from the `exit` handler for every exiting process; the producer
    /// keeps the code only for a process it is tracking as a child (every other
    /// process — PID 1, a kernel thread — is ignored), so the parent's
    /// [`Self::wait`] can read it back. The default is a no-op.
    fn record_exit(&self, _process: ProcessId, _code: i32) {}

    /// Record that `process` was stopped by `signal` ([`Signal::Stop`]), so a
    /// parent waiting with [`WaitFlags::STOPPED`] can observe it.
    ///
    /// Called from the signal producer after the child is parked. The
    /// default is a no-op, exactly like the other bookkeeping hooks.
    fn record_stop(&self, _process: ProcessId, _signal: Signal) {}

    /// Record that `process` was resumed ([`Signal::Continue`]), clearing any
    /// not-yet-reported stop so a stale stop is never reported after the
    /// child is already running again.
    ///
    /// Called from the signal producer after the child is unparked. The
    /// default is a no-op.
    fn record_continue(&self, _process: ProcessId) {}

    /// Authorise `sender` over the **live** child selected by `pid`,
    /// returning the child's process id.
    ///
    /// The one parent/child authorisation the `signal` producer and the
    /// `console_foreground` handler share with `wait`, so who-owns-whom has
    /// a single definition. A zombie awaiting reap is not authorisable.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] when `pid` does not name a live child of
    /// `sender`. The default fails closed with [`Errno::NotImplemented`] so
    /// the inert [`NullProcessWait`] never authorises anyone.
    fn authorise_child(&self, _sender: ProcessId, _pid: i32) -> Result<ProcessId, Errno> {
        Err(Errno::NotImplemented)
    }

    /// Non-consuming readiness peek: classify the child of `parent` selected
    /// by `pid` **without reaping it** (never parking, never mutating).
    ///
    /// The wait-set `Child` source is built on this: the member-add
    /// owner-check refuses a specific `pid` that reports
    /// [`ChildPeek::NoChild`], and the readiness scan reports the member
    /// ready on [`ChildPeek::Reapable`]. The default fails closed with
    /// [`ChildPeek::NoChild`] so the inert [`NullProcessWait`] (and any
    /// producer that predates the wait-set) never fabricates a child.
    fn child_state(&self, _parent: ProcessId, _pid: i32) -> ChildPeek {
        ChildPeek::NoChild
    }

    /// Sever the exited `parent`'s link to every child row it owned.
    ///
    /// Called from the one shared process reclaim for every death path. A
    /// dead parent can never issue another `wait` (process ids are never
    /// reused), so its unreaped zombies are dropped outright and its
    /// running children become orphans whose rows are removed when they
    /// themselves exit — no row is ever stranded, and no still-running
    /// orphan is misreported dead. The default is a no-op, like the other
    /// bookkeeping hooks.
    fn parent_exited(&self, _parent: ProcessId) {}

    /// Whether `process` is a **live** tracked process (spawned, not yet
    /// exited).
    ///
    /// The console foreground gate uses this to self-heal a stale
    /// controlling-owner slot: a recorded owner this reports dead is
    /// cleared so the console is never wedged behind a process that can no
    /// longer read it. Task ids are never reused, so a `false` answer is
    /// final. The default reports **live** — the answer that keeps the
    /// gate denying — so a producer that predates the query (and the inert
    /// [`NullProcessWait`]) can never *widen* access by mistaking a live
    /// owner for a dead one; [`KernelProcessWait`] overrides it with the
    /// real bookkeeping.
    fn is_live(&self, _process: ProcessId) -> bool {
        true
    }
}

/// The process-wait producer installed before any real one exists.
///
/// Every wait fails closed with [`Errno::NotImplemented`] — the fail-closed
/// default require, so a `wait` issued before the
/// boot path installs the scheduler-side producer announces an inert
/// interface rather than fabricating a reaped child or an exit code. The
/// bookkeeping hooks inherit the no-op trait defaults.
#[derive(Debug, Default, Copy, Clone)]
pub struct NullProcessWait;

impl ProcessWait for NullProcessWait {
    fn wait(&self, _parent: ProcessId, _pid: i32, _flags: WaitFlags) -> Result<WaitedChild, Errno> {
        Err(Errno::NotImplemented)
    }
}

/// The shared [`NullProcessWait`] instance the syscall handler defaults to.
///
/// `KernelSyscallHandlers::new` points its `process_wait` borrow here so the
/// field is always valid without an `Option` branch on the hot path; the
/// boot path replaces it with [`KernelProcessWait`] through
/// `KernelSyscallHandlers::with_process_wait`.
pub static NULL_PROCESS_WAIT: NullProcessWait = NullProcessWait;

/// Narrow a scheduler process id to the `u32` PID the ABI carries.
///
/// Scheduler process ids stay well within `u32` for every supported
/// configuration; a value that would not fit saturates rather than wrapping
/// (never silently truncate).
fn narrow_task_id(id: u64) -> u32 {
    u32::try_from(id).unwrap_or(u32::MAX)
}

/// One child's entry in the [`ProcessTable`].
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
struct ChildEntry {
    /// Scheduler process id of the parent that spawned this child, or `None`
    /// once that parent has exited (the child is an orphan): no `wait`,
    /// signal, or peek can ever select it again, but the row keeps the
    /// liveness bookkeeping honest until the orphan itself exits.
    parent: Option<u64>,
    /// The child's exit code once it has exited (`Some`), or `None` while it
    /// is still running. A `Some` entry is a reapable zombie.
    exit: Option<i32>,
    /// A stop ([`Signal::Stop`]) not yet reported to a
    /// [`WaitFlags::STOPPED`] wait. Edge-triggered: set when the child is
    /// stopped, cleared when reported or when a continue resumes the child,
    /// so each stop is observed at most once and never after a resume.
    stop_pending: Option<Signal>,
}

/// Outcome of a non-consuming [`ProcessTable::peek`] /
/// [`ProcessWait::child_state`] readiness check.
///
/// The peek counterpart of [`Reap`]: it reports the same three-way
/// classification but never removes the zombie, so a wait-set scan can
/// observe "a child is reapable" without stealing the reap from the `wait`
/// syscall that follows.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ChildPeek {
    /// A matching child has exited and is waiting to be reaped.
    Reapable,
    /// A matching child exists but has not exited yet.
    Running,
    /// `pid` names no child of the calling parent (and the parent has no
    /// children at all, for [`tairix_abi::WAIT_PID_ANY`]).
    NoChild,
}

/// Outcome of a single [`ProcessTable::reap`] attempt.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum Reap {
    /// A matching child has something to report: an exited child (removed
    /// from the table) or, when stop reports were requested, a freshly
    /// stopped one (kept, its pending stop consumed).
    Ready(WaitedChild),
    /// A matching child exists but has not exited yet — the caller must
    /// block and retry.
    Blocked,
    /// `pid` names no child of the calling parent (and the parent has no
    /// children at all, for [`tairix_abi::WAIT_PID_ANY`]).
    NoChild,
}

/// The parent/child + exit-status bookkeeping behind [`KernelProcessWait`].
///
/// Keyed by a child's scheduler process id, each entry records the child's
/// parent and its exit code once it exits. A child is registered when it is
/// spawned ([`Self::register`]), marked a reapable zombie when it exits
/// ([`Self::record_exit`]), and removed when its parent reaps it
/// ([`Self::reap`]). The map is intentionally tiny and append/remove only —
/// the scheduler owns process lifetimes; this only remembers the parent link
/// and the terminal status the scheduler does not.
#[derive(Debug, Default)]
pub struct ProcessTable {
    children: BTreeMap<u64, ChildEntry>,
}

impl ProcessTable {
    /// Build an empty table.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            children: BTreeMap::new(),
        }
    }

    /// Record that `child` was spawned by `parent`.
    ///
    /// A fresh child id is never already present (the scheduler hands out
    /// monotonically increasing ids); a re-registration overwrites, which
    /// can only happen if an id were reused after a full reap, leaving the
    /// newer link — the correct value either way.
    pub fn register(&mut self, parent: ProcessId, child: ProcessId) {
        self.children.insert(
            child.0,
            ChildEntry {
                parent: Some(parent.0),
                exit: None,
                stop_pending: None,
            },
        );
    }

    /// Mark `process` a reapable zombie carrying `code`, if it is a tracked
    /// child. A `process` the table does not track (PID 1, a kernel thread) is
    /// ignored.
    pub fn record_exit(&mut self, process: ProcessId, code: i32) {
        if let Some(entry) = self.children.get_mut(&process.0) {
            // An orphan's exit has no parent left to reap it: drop the row
            // instead of minting a zombie no `wait` can ever collect.
            if entry.parent.is_none() {
                self.children.remove(&process.0);
                return;
            }
            entry.exit = Some(code);
            // A terminated child can no longer be "stopped": the exit report
            // supersedes any unobserved stop.
            entry.stop_pending = None;
        }
    }

    /// Mark a not-yet-reported stop by `signal` on `process`, if it is a
    /// tracked, still-live child. A `process` the table does not track — or a
    /// zombie awaiting reap — is ignored (a dead child cannot stop).
    pub fn record_stop(&mut self, process: ProcessId, signal: Signal) {
        if let Some(entry) = self.children.get_mut(&process.0) {
            if entry.exit.is_none() {
                entry.stop_pending = Some(signal);
            }
        }
    }

    /// Clear any not-yet-reported stop on `process` (the child was resumed), so
    /// a stale stop is never reported after the child is running again.
    pub fn record_continue(&mut self, process: ProcessId) {
        if let Some(entry) = self.children.get_mut(&process.0) {
            entry.stop_pending = None;
        }
    }

    /// Try to report a child of `parent` selected by `pid`.
    ///
    /// `pid` is [`tairix_abi::WAIT_PID_ANY`] for any child or a specific child's
    /// id. Among the matching children: the first (lowest-id, for
    /// determinism) that has already exited is removed
    /// and returned as [`Reap::Ready`]; otherwise, with `report_stopped`
    /// set, the first matching child carrying an unreported stop is returned
    /// as [`Reap::Ready`] with a stopped status — **kept in the table**,
    /// its pending stop consumed so it is reported exactly once; if matching
    /// children exist but none has anything to report the result is
    /// [`Reap::Blocked`]; if no child matches it is
    /// [`Reap::NoChild`]. A negative `pid` other than [`tairix_abi::WAIT_PID_ANY`]
    /// names no child and fails closed with [`Reap::NoChild`].
    #[must_use]
    pub fn reap(&mut self, parent: ProcessId, pid: i32, report_stopped: bool) -> Reap {
        let (any_match, reapable, stopped) = self.find(parent, pid, report_stopped);
        if let Some((child_id, code)) = reapable {
            self.children.remove(&child_id);
            Reap::Ready(WaitedChild {
                pid: narrow_task_id(child_id),
                status: WaitStatus::Exited(code),
            })
        } else if let Some((child_id, signal)) = stopped {
            if let Some(entry) = self.children.get_mut(&child_id) {
                // Consume the pending stop: it is reported exactly once.
                entry.stop_pending = None;
            }
            Reap::Ready(WaitedChild {
                pid: narrow_task_id(child_id),
                status: WaitStatus::Stopped(signal),
            })
        } else if any_match {
            Reap::Blocked
        } else {
            Reap::NoChild
        }
    }

    /// Non-consuming readiness peek: classify the child selected by `pid`
    /// without reaping it.
    ///
    /// The wait-set `Child` source scans through this, so observing "a child
    /// is reapable" never steals the reap from the `wait` syscall that
    /// follows. It shares the private `find` scan with [`Self::reap`], so
    /// the two can never disagree on which children match.
    #[must_use]
    pub fn peek(&self, parent: ProcessId, pid: i32) -> ChildPeek {
        let (any_match, reapable, _) = self.find(parent, pid, false);
        if reapable.is_some() {
            ChildPeek::Reapable
        } else if any_match {
            ChildPeek::Running
        } else {
            ChildPeek::NoChild
        }
    }

    /// The one matching scan behind [`Self::reap`] and [`Self::peek`]:
    /// resolve the `pid` selector (a specific child id, or
    /// [`tairix_abi::WAIT_PID_ANY`]) and report whether any child of `parent`
    /// matches, the first (lowest-id, for determinism) matching reapable
    /// zombie's `(process id, exit code)`, and — when `report_stopped` — the
    /// first matching child with an unreported stop. A reapable zombie wins
    /// over a pending stop: termination is the stronger, terminal report.
    ///
    /// A negative selector other than [`tairix_abi::WAIT_PID_ANY`] names no
    /// child and fails closed as no match.
    #[allow(clippy::type_complexity)] // Three named findings of one scan; a struct would restate them.
    fn find(
        &self,
        parent: ProcessId,
        pid: i32,
        report_stopped: bool,
    ) -> (bool, Option<(u64, i32)>, Option<(u64, Signal)>) {
        let target: Option<u64> = if pid == WAIT_PID_ANY {
            None
        } else {
            match u64::try_from(pid) {
                Ok(id) => Some(id),
                Err(_) => return (false, None, None),
            }
        };

        let mut any_match = false;
        let mut reapable: Option<(u64, i32)> = None;
        let mut stopped: Option<(u64, Signal)> = None;
        for (&child_id, entry) in &self.children {
            if entry.parent != Some(parent.0) {
                continue;
            }
            if let Some(want) = target {
                if child_id != want {
                    continue;
                }
            }
            any_match = true;
            if let Some(code) = entry.exit {
                reapable = Some((child_id, code));
                break;
            }
            if report_stopped && stopped.is_none() {
                if let Some(signal) = entry.stop_pending {
                    stopped = Some((child_id, signal));
                }
            }
        }
        (any_match, reapable, stopped)
    }

    /// The process id of a **live** (not-yet-exited) child of `parent` selected
    /// by `pid`, or `None`.
    ///
    /// This is the authorisation lookup the signal path uses: a process may
    /// signal only a child it spawned that is still running. `pid` must name
    /// a specific child (the `signal` syscall has no wildcard); a child that
    /// has already exited — a zombie awaiting reap — is **not** signallable
    /// and reports `None`, so a signal to a dead process fails closed rather
    /// than pretending to reach it. A negative or otherwise non-representable
    /// `pid` names no child.
    #[must_use]
    pub fn live_child(&self, parent: ProcessId, pid: i32) -> Option<ProcessId> {
        let want = u64::try_from(pid).ok()?;
        let entry = self.children.get(&want)?;
        if entry.parent == Some(parent.0) && entry.exit.is_none() {
            Some(ProcessId(want))
        } else {
            None
        }
    }

    /// Sever the exited `parent`'s link to every child row it owned.
    ///
    /// A dead parent can never reap, so its **zombie** rows are dropped
    /// outright (no `wait` will ever collect them) and its **running**
    /// children become orphans: their `parent` link is cleared so no
    /// selector can match them again, but the row itself survives so
    /// [`Self::is_live`] keeps answering honestly for a process that is
    /// still running (the console-foreground gate depends on that). An
    /// orphan's own exit then removes its row ([`Self::record_exit`])
    /// instead of minting an unreapable zombie, so the table stays
    /// bounded by the live process tree, never by history.
    pub fn parent_exited(&mut self, parent: ProcessId) {
        self.children
            .retain(|_, entry| entry.parent != Some(parent.0) || entry.exit.is_none());
        for entry in self.children.values_mut() {
            if entry.parent == Some(parent.0) {
                entry.parent = None;
                // An unobserved stop dies with the parent that could have
                // observed it.
                entry.stop_pending = None;
            }
        }
    }

    /// Whether `process` is tracked and still running, regardless of parent.
    ///
    /// The parentless liveness lookup behind [`ProcessWait::is_live`]: a
    /// tracked entry with no recorded exit is live; a zombie awaiting reap,
    /// a reaped (removed) entry, and a process the table never tracked all
    /// report dead. Every console foreground owner was authorised as a
    /// live tracked child when it was granted (process ids are never reused),
    /// so "untracked" can only mean the owner is gone.
    #[must_use]
    pub fn is_live(&self, process: ProcessId) -> bool {
        self.children
            .get(&process.0)
            .is_some_and(|entry| entry.exit.is_none())
    }
}

/// The scheduler-side `wait` producer the boot path installs (`plans/SPAWN.md`
/// `SP6b`).
///
/// Owns the [`ProcessTable`] bookkeeping and blocks a waiting parent by
/// cooperatively parking it back on the scheduler — through
/// [`reschedule_current`] — until one of its children becomes reapable,
/// mirroring the `irq_wait` poll-and-yield loop (no
/// busy-spin). It needs only the arch handle (to read the current CPU for
/// the park) and the table; the park itself is the free
/// [`reschedule_current`] primitive, so the producer never touches the
/// scheduler handle directly.
pub struct KernelProcessWait<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    arch: &'static A,
    table: SpinLock<ProcessTable>,
}

impl<A> KernelProcessWait<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    /// Build a producer over `arch` with an empty bookkeeping table.
    #[must_use]
    pub const fn new(arch: &'static A) -> Self {
        Self {
            arch,
            table: SpinLock::new(ProcessTable::new()),
        }
    }
}

impl<A> KernelProcessWait<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    /// The one non-blocking report attempt behind both [`ProcessWait::wait`]
    /// (its re-poll loop) and [`ProcessWait::poll`], so the blocking and
    /// non-blocking paths can never diverge.
    fn try_report(
        &self,
        parent: ProcessId,
        pid: i32,
        flags: WaitFlags,
    ) -> Result<WaitedChild, Errno> {
        match self.table.lock().reap(parent, pid, flags.is_stopped()) {
            Reap::Ready(child) => Ok(child),
            Reap::Blocked => Err(Errno::WouldBlock),
            Reap::NoChild => Err(Errno::NotFound),
        }
    }
}

impl<A> ProcessWait for KernelProcessWait<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    fn register_child(&self, parent: ProcessId, child: ProcessId) {
        self.table.lock().register(parent, child);
    }

    fn parent_exited(&self, parent: ProcessId) {
        self.table.lock().parent_exited(parent);
    }

    fn record_exit(&self, process: ProcessId, code: i32) {
        self.table.lock().record_exit(process, code);
        // Wake every parent parked in `wait`: the exiting process may be the
        // child one is blocked on (a real park woken by
        // the exit event). The lock is released above before the wake.
        crate::waitq::procwait_wake();
    }

    fn record_stop(&self, process: ProcessId, signal: Signal) {
        self.table.lock().record_stop(process, signal);
        // A stop is a reportable event for a parent blocked in a
        // `WaitFlags::STOPPED` wait, so it wakes the parked parents exactly
        // as an exit does. The lock is released above before the wake.
        crate::waitq::procwait_wake();
    }

    fn record_continue(&self, process: ProcessId) {
        // Clearing a pending stop creates nothing to report, so no wake: a
        // parent parked in `wait` stays parked until a real event.
        self.table.lock().record_continue(process);
    }

    fn authorise_child(&self, sender: ProcessId, pid: i32) -> Result<ProcessId, Errno> {
        // The one bookkeeping lookup the signal producer and the
        // `console_foreground` handler share with `wait`, so the
        // parent/child relationship has a single definition. Only a
        // **live** child authorises; a zombie fails closed.
        self.table
            .lock()
            .live_child(sender, pid)
            .ok_or(Errno::NotFound)
    }

    fn poll(&self, parent: ProcessId, pid: i32, flags: WaitFlags) -> Result<WaitedChild, Errno> {
        // A single non-blocking report attempt — the same primitive the
        // blocking `wait` loop uses, so the two can never diverge. A matching
        // child with nothing to report yet is `WouldBlock` (the
        // caller decides whether to retry) rather than parking; no child at
        // all fails closed with `NotFound`.
        self.try_report(parent, pid, flags)
    }

    fn child_state(&self, parent: ProcessId, pid: i32) -> ChildPeek {
        self.table.lock().peek(parent, pid)
    }

    fn is_live(&self, process: ProcessId) -> bool {
        self.table.lock().is_live(process)
    }

    fn wait(&self, parent: ProcessId, pid: i32, flags: WaitFlags) -> Result<WaitedChild, Errno> {
        loop {
            // Re-poll under the lock, then release it *before* parking so the
            // child whose exit we are waiting for can take the same lock from
            // its own `exit` (`record_exit`) while we are suspended.
            match self.try_report(parent, pid, flags) {
                Ok(child) => return Ok(child),
                Err(Errno::NotFound) => return Err(Errno::NotFound),
                Err(Errno::WouldBlock) => {
                    // **Park** the caller off the run queue until a child
                    // exits (never a busy-yield): a
                    // re-enqueuing yield here would keep the run queue
                    // non-empty forever, so the dispatch loop could never
                    // reach its idle `wait_for_interrupt` and a device IRQ
                    // (e.g. an interrupt-driven driver PID 1 spawned) would be
                    // starved. Register on `PROCWAIT_WAITQ` *before* parking
                    // so an exit racing the park is not lost: `record_exit`'s
                    // `procwait_wake` unparks this process and the scheduler's
                    // wake-pending token converts a concurrent park into a
                    // re-ready (the same interlock `irq_wait` / `hw_tree_wait`
                    // use). Reaping is an explicit event, so the registration
                    // carries `NO_DEADLINE` (no timed wake). A `false`
                    // reschedule means no resumable user kthread is published
                    // on this CPU — fail closed rather than busy-spin.
                    let cpu = self.arch.current_cpu();
                    crate::waitq::PROCWAIT_WAITQ.register(parent.0, crate::waitq::NO_DEADLINE);
                    let parked = reschedule_current(cpu, RescheduleAction::Park);
                    crate::waitq::PROCWAIT_WAITQ.deregister(parent.0);
                    if !parked {
                        return Err(Errno::NotImplemented);
                    }
                    // A doomed waiter never re-parks: a termination deferred
                    // against this process unwinds the wait so the kill lands at
                    // the syscall boundary (the errno never reaches user
                    // space).
                    if crate::procsignal::kill_pending(parent.0) {
                        return Err(Errno::Interrupted);
                    }
                }
                // `try_report` yields only the three arms above; keep the
                // residual fail-closed rather than unreachable-panicking.
                Err(err) => return Err(err),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_process_wait_fails_closed() {
        assert_eq!(
            NULL_PROCESS_WAIT.wait(ProcessId(7), 9, WaitFlags::empty()),
            Err(Errno::NotImplemented)
        );
        // A WAIT_PID_ANY request announces the inert interface too, rather than
        // pretending a child was reaped.
        assert_eq!(
            NullProcessWait.wait(ProcessId(1), tairix_abi::WAIT_PID_ANY, WaitFlags::empty()),
            Err(Errno::NotImplemented)
        );
        // The bookkeeping hooks are inert no-ops on the null producer.
        NULL_PROCESS_WAIT.register_child(ProcessId(1), ProcessId(2));
        NULL_PROCESS_WAIT.record_exit(ProcessId(2), 0);
    }

    #[test]
    fn reap_unknown_child_is_no_child() {
        let mut table = ProcessTable::new();
        assert_eq!(table.reap(ProcessId(1), WAIT_PID_ANY, false), Reap::NoChild);
        assert_eq!(table.reap(ProcessId(1), 9, false), Reap::NoChild);
    }

    #[test]
    fn registered_but_unexited_child_blocks() {
        let mut table = ProcessTable::new();
        table.register(ProcessId(1), ProcessId(2));
        assert_eq!(table.reap(ProcessId(1), WAIT_PID_ANY, false), Reap::Blocked);
        // Selecting the specific child blocks the same way.
        assert_eq!(table.reap(ProcessId(1), 2, false), Reap::Blocked);
        // A stop-aware wait has nothing extra to report for a merely
        // running child.
        assert_eq!(table.reap(ProcessId(1), WAIT_PID_ANY, true), Reap::Blocked);
    }

    #[test]
    fn exited_child_is_reaped_once_and_removed() {
        let mut table = ProcessTable::new();
        table.register(ProcessId(1), ProcessId(2));
        table.record_exit(ProcessId(2), 7);
        assert_eq!(
            table.reap(ProcessId(1), WAIT_PID_ANY, false),
            Reap::Ready(WaitedChild {
                pid: 2,
                status: WaitStatus::Exited(7)
            })
        );
        // A second reap finds nothing — the zombie was removed.
        assert_eq!(table.reap(ProcessId(1), WAIT_PID_ANY, false), Reap::NoChild);
    }

    #[test]
    fn specific_pid_reaps_only_that_child() {
        let mut table = ProcessTable::new();
        table.register(ProcessId(1), ProcessId(2));
        table.register(ProcessId(1), ProcessId(3));
        table.record_exit(ProcessId(3), 5);
        // Waiting on child 2 (still running) blocks even though 3 is a zombie.
        assert_eq!(table.reap(ProcessId(1), 2, false), Reap::Blocked);
        // Waiting on child 3 reaps it.
        assert_eq!(
            table.reap(ProcessId(1), 3, false),
            Reap::Ready(WaitedChild {
                pid: 3,
                status: WaitStatus::Exited(5)
            })
        );
    }

    #[test]
    fn a_process_cannot_reap_another_parents_child() {
        let mut table = ProcessTable::new();
        table.register(ProcessId(1), ProcessId(2));
        table.record_exit(ProcessId(2), 0);
        // Task 9 is not the parent of child 2, so it sees no child.
        assert_eq!(table.reap(ProcessId(9), WAIT_PID_ANY, false), Reap::NoChild);
        assert_eq!(table.reap(ProcessId(9), 2, false), Reap::NoChild);
        // The real parent still reaps it.
        assert_eq!(
            table.reap(ProcessId(1), 2, false),
            Reap::Ready(WaitedChild {
                pid: 2,
                status: WaitStatus::Exited(0)
            })
        );
    }

    #[test]
    fn peek_classifies_without_consuming() {
        let mut table = ProcessTable::new();
        // No children at all: nothing to observe.
        assert_eq!(table.peek(ProcessId(1), WAIT_PID_ANY), ChildPeek::NoChild);
        assert_eq!(table.peek(ProcessId(1), 2), ChildPeek::NoChild);

        table.register(ProcessId(1), ProcessId(2));
        assert_eq!(table.peek(ProcessId(1), WAIT_PID_ANY), ChildPeek::Running);
        assert_eq!(table.peek(ProcessId(1), 2), ChildPeek::Running);

        table.record_exit(ProcessId(2), 7);
        assert_eq!(table.peek(ProcessId(1), WAIT_PID_ANY), ChildPeek::Reapable);
        assert_eq!(table.peek(ProcessId(1), 2), ChildPeek::Reapable);
        // The peek left the zombie in place: the reap that follows still
        // finds it.
        assert_eq!(table.peek(ProcessId(1), 2), ChildPeek::Reapable);
        assert_eq!(
            table.reap(ProcessId(1), 2, false),
            Reap::Ready(WaitedChild {
                pid: 2,
                status: WaitStatus::Exited(7)
            })
        );
        assert_eq!(table.peek(ProcessId(1), 2), ChildPeek::NoChild);
    }

    /// A dead parent's unreaped zombie is dropped outright: no `wait` can
    /// ever collect it, so keeping it would strand the row forever.
    #[test]
    fn parent_exited_drops_unreaped_zombies() {
        let mut table = ProcessTable::new();
        table.register(ProcessId(1), ProcessId(2));
        table.record_exit(ProcessId(2), 7);
        table.parent_exited(ProcessId(1));
        assert_eq!(table.peek(ProcessId(1), WAIT_PID_ANY), ChildPeek::NoChild);
        assert!(!table.is_live(ProcessId(2)));
    }

    /// A dead parent's running child becomes an orphan: no selector can
    /// match it again, but it still reports live — the console-foreground
    /// gate must never mistake a running orphan for a dead owner — and its
    /// own exit removes the row instead of minting an unreapable zombie.
    #[test]
    fn parent_exited_orphans_a_running_child_without_stranding_a_zombie() {
        let mut table = ProcessTable::new();
        table.register(ProcessId(1), ProcessId(2));
        table.parent_exited(ProcessId(1));
        // Unmatchable by any parent selector...
        assert_eq!(table.peek(ProcessId(1), 2), ChildPeek::NoChild);
        assert_eq!(table.live_child(ProcessId(1), 2), None);
        assert_eq!(table.reap(ProcessId(1), WAIT_PID_ANY, false), Reap::NoChild);
        // ...but still honestly live while it runs.
        assert!(table.is_live(ProcessId(2)));
        // The orphan's own exit removes the row: dead, and never a zombie.
        table.record_exit(ProcessId(2), 0);
        assert!(!table.is_live(ProcessId(2)));
        assert_eq!(table.peek(ProcessId(1), 2), ChildPeek::NoChild);
    }

    /// Severing one parent's links leaves every other parent's children
    /// untouched — running and reapable alike.
    #[test]
    fn parent_exited_leaves_other_parents_children_untouched() {
        let mut table = ProcessTable::new();
        table.register(ProcessId(1), ProcessId(2));
        table.register(ProcessId(9), ProcessId(3));
        table.register(ProcessId(9), ProcessId(4));
        table.record_exit(ProcessId(3), 5);
        table.parent_exited(ProcessId(1));
        assert_eq!(table.peek(ProcessId(9), 3), ChildPeek::Reapable);
        assert_eq!(table.peek(ProcessId(9), 4), ChildPeek::Running);
        assert_eq!(
            table.reap(ProcessId(9), 3, false),
            Reap::Ready(WaitedChild {
                pid: 3,
                status: WaitStatus::Exited(5)
            })
        );
    }

    /// An unobserved stop dies with the parent that could have observed
    /// it: a stop-aware reap by anyone reports nothing for an orphan.
    #[test]
    fn parent_exited_discards_an_orphans_pending_stop() {
        let mut table = ProcessTable::new();
        table.register(ProcessId(1), ProcessId(2));
        table.record_stop(ProcessId(2), Signal::Stop);
        table.parent_exited(ProcessId(1));
        assert_eq!(table.reap(ProcessId(1), WAIT_PID_ANY, true), Reap::NoChild);
        assert!(table.is_live(ProcessId(2)));
    }

    #[test]
    fn peek_never_reveals_another_parents_child() {
        let mut table = ProcessTable::new();
        table.register(ProcessId(1), ProcessId(2));
        table.record_exit(ProcessId(2), 0);
        // Task 9 is not the parent: the peek observes nothing, exactly as
        // reap matches nothing.
        assert_eq!(table.peek(ProcessId(9), WAIT_PID_ANY), ChildPeek::NoChild);
        assert_eq!(table.peek(ProcessId(9), 2), ChildPeek::NoChild);
        // A negative selector other than WAIT_PID_ANY names no child.
        assert_eq!(table.peek(ProcessId(1), -7), ChildPeek::NoChild);
    }

    #[test]
    fn null_child_state_fails_closed() {
        assert_eq!(
            NULL_PROCESS_WAIT.child_state(ProcessId(1), WAIT_PID_ANY),
            ChildPeek::NoChild
        );
        assert_eq!(
            NULL_PROCESS_WAIT.child_state(ProcessId(1), 2),
            ChildPeek::NoChild
        );
    }

    #[test]
    fn record_exit_for_untracked_task_is_ignored() {
        let mut table = ProcessTable::new();
        // No panic, no entry created for an untracked process (PID 1, a kthread).
        table.record_exit(ProcessId(42), 3);
        assert_eq!(table.reap(ProcessId(0), WAIT_PID_ANY, false), Reap::NoChild);
        // The stop/continue hooks are equally inert for untracked tasks.
        table.record_stop(ProcessId(42), Signal::Stop);
        table.record_continue(ProcessId(42));
        assert_eq!(table.reap(ProcessId(0), WAIT_PID_ANY, true), Reap::NoChild);
    }

    #[test]
    fn wait_any_reaps_lowest_id_zombie_first() {
        let mut table = ProcessTable::new();
        table.register(ProcessId(1), ProcessId(5));
        table.register(ProcessId(1), ProcessId(3));
        table.record_exit(ProcessId(5), 50);
        table.record_exit(ProcessId(3), 30);
        // Deterministic: the lowest-id reapable child is returned first.
        assert_eq!(
            table.reap(ProcessId(1), WAIT_PID_ANY, false),
            Reap::Ready(WaitedChild {
                pid: 3,
                status: WaitStatus::Exited(30)
            })
        );
        assert_eq!(
            table.reap(ProcessId(1), WAIT_PID_ANY, false),
            Reap::Ready(WaitedChild {
                pid: 5,
                status: WaitStatus::Exited(50)
            })
        );
    }

    #[test]
    fn negative_non_wait_any_pid_is_no_child() {
        let mut table = ProcessTable::new();
        table.register(ProcessId(1), ProcessId(2));
        table.record_exit(ProcessId(2), 0);
        // -2 is not WAIT_PID_ANY and not a valid child id: fail closed.
        assert_eq!(table.reap(ProcessId(1), -2, false), Reap::NoChild);
        assert_eq!(table.reap(ProcessId(1), -2, true), Reap::NoChild);
    }

    #[test]
    fn live_child_finds_only_a_running_child_of_the_asker() {
        let mut table = ProcessTable::new();
        table.register(ProcessId(1), ProcessId(2));
        // A live child of the real parent resolves.
        assert_eq!(table.live_child(ProcessId(1), 2), Some(ProcessId(2)));
        // Another process is not the parent, so it cannot signal the child.
        assert_eq!(table.live_child(ProcessId(9), 2), None);
        // An unknown pid, WAIT_PID_ANY (no wildcard for signal), and a
        // negative pid all name no child.
        assert_eq!(table.live_child(ProcessId(1), 5), None);
        assert_eq!(table.live_child(ProcessId(1), WAIT_PID_ANY), None);
        assert_eq!(table.live_child(ProcessId(1), -2), None);
    }

    #[test]
    fn live_child_does_not_find_a_zombie() {
        let mut table = ProcessTable::new();
        table.register(ProcessId(1), ProcessId(2));
        table.record_exit(ProcessId(2), 0);
        // A child that already exited is a zombie awaiting reap, not a
        // signallable process — fail closed.
        assert_eq!(table.live_child(ProcessId(1), 2), None);
    }

    #[test]
    fn is_live_reports_only_a_tracked_running_task() {
        let mut table = ProcessTable::new();
        table.register(ProcessId(1), ProcessId(2));
        // A tracked, running process is live; an untracked one is not.
        assert!(table.is_live(ProcessId(2)));
        assert!(!table.is_live(ProcessId(9)));
        // A zombie awaiting reap is dead …
        table.record_exit(ProcessId(2), 0);
        assert!(!table.is_live(ProcessId(2)));
        // … and so is a reaped (removed) entry.
        assert_eq!(
            table.reap(ProcessId(1), 2, false),
            Reap::Ready(WaitedChild {
                pid: 2,
                status: WaitStatus::Exited(0)
            })
        );
        assert!(!table.is_live(ProcessId(2)));
    }

    #[test]
    fn null_is_live_keeps_the_gate_denying() {
        // The inert default reports live, so a gate that cannot prove a
        // recorded owner dead keeps refusing rather than widening access.
        assert!(NullProcessWait.is_live(ProcessId(2)));
    }

    #[test]
    fn authorise_child_gates_the_signal_path() {
        let p = producer();
        p.register_child(ProcessId(1), ProcessId(2));
        // The parent may signal its live child.
        assert_eq!(p.authorise_child(ProcessId(1), 2), Ok(ProcessId(2)));
        // A non-parent, an unknown pid, and (after exit) a zombie all fail
        // closed with `NotFound`.
        assert_eq!(p.authorise_child(ProcessId(9), 2), Err(Errno::NotFound));
        assert_eq!(p.authorise_child(ProcessId(1), 3), Err(Errno::NotFound));
        p.record_exit(ProcessId(2), 0);
        assert_eq!(p.authorise_child(ProcessId(1), 2), Err(Errno::NotFound));
    }

    #[test]
    fn a_signalled_exit_makes_the_child_reapable_with_its_status() {
        let p = producer();
        p.register_child(ProcessId(1), ProcessId(2));
        // A signalled child becomes a reapable zombie carrying the signal's
        // termination status, indistinguishable from a self-exit to `reap`.
        p.record_exit(ProcessId(2), 130);
        assert_eq!(
            p.wait(ProcessId(1), WAIT_PID_ANY, WaitFlags::empty()),
            Ok(WaitedChild {
                pid: 2,
                status: WaitStatus::Exited(130)
            })
        );
    }

    /// Build a `'static` [`KernelProcessWait`] over a fresh single-CPU
    /// [`TestArch`] for the producer-level host tests.
    fn producer() -> &'static KernelProcessWait<crate::test_arch::TestArch> {
        let arch: &'static crate::test_arch::TestArch = std::boxed::Box::leak(
            std::boxed::Box::new(crate::test_arch::TestArch::with_cpus(1)),
        );
        std::boxed::Box::leak(std::boxed::Box::new(KernelProcessWait::new(arch)))
    }

    #[test]
    fn producer_reaps_an_already_exited_child_without_blocking() {
        let p = producer();
        p.register_child(ProcessId(1), ProcessId(2));
        // The child has already exited by the time the parent waits, so the
        // reap is immediate — the blocking park path is never reached (it
        // would require a live scheduler and is proven by the `-M virt`
        // vertical).
        p.record_exit(ProcessId(2), 9);
        assert_eq!(
            p.wait(ProcessId(1), WAIT_PID_ANY, WaitFlags::empty()),
            Ok(WaitedChild {
                pid: 2,
                status: WaitStatus::Exited(9)
            })
        );
        // The zombie was consumed; a second wait finds no child.
        assert_eq!(
            p.wait(ProcessId(1), WAIT_PID_ANY, WaitFlags::empty()),
            Err(Errno::NotFound)
        );
    }

    #[test]
    fn producer_waiting_on_a_non_child_fails_closed() {
        let p = producer();
        p.register_child(ProcessId(1), ProcessId(2));
        p.record_exit(ProcessId(2), 0);
        // Task 9 never spawned child 2: it may not reap it.
        assert_eq!(
            p.wait(ProcessId(9), WAIT_PID_ANY, WaitFlags::empty()),
            Err(Errno::NotFound)
        );
        assert_eq!(
            p.wait(ProcessId(9), 2, WaitFlags::empty()),
            Err(Errno::NotFound)
        );
    }

    #[test]
    fn producer_blocked_reap_without_a_published_kthread_fails_closed() {
        let p = producer();
        // A registered-but-unexited child would block. In a host test no
        // resumable user kthread is published, so the park cannot proceed and
        // the producer fails closed with `NotImplemented` rather than
        // busy-spinning forever.
        p.register_child(ProcessId(1), ProcessId(2));
        assert_eq!(
            p.wait(ProcessId(1), WAIT_PID_ANY, WaitFlags::empty()),
            Err(Errno::NotImplemented)
        );
    }

    #[test]
    fn producer_poll_reaps_an_exited_child_without_blocking() {
        let p = producer();
        p.register_child(ProcessId(1), ProcessId(2));
        p.record_exit(ProcessId(2), 9);
        assert_eq!(
            p.poll(ProcessId(1), WAIT_PID_ANY, WaitFlags::NONBLOCK),
            Ok(WaitedChild {
                pid: 2,
                status: WaitStatus::Exited(9)
            })
        );
        // The zombie was consumed; a second poll finds no child.
        assert_eq!(
            p.poll(ProcessId(1), WAIT_PID_ANY, WaitFlags::NONBLOCK),
            Err(Errno::NotFound)
        );
    }

    #[test]
    fn producer_poll_of_a_running_child_would_block_never_parks() {
        let p = producer();
        // A registered-but-unexited child: a *blocking* wait here would park
        // (and fail closed in a host test), but the poll reports `WouldBlock`
        // immediately without ever touching the scheduler.
        p.register_child(ProcessId(1), ProcessId(2));
        assert_eq!(
            p.poll(ProcessId(1), WAIT_PID_ANY, WaitFlags::NONBLOCK),
            Err(Errno::WouldBlock)
        );
        assert_eq!(
            p.poll(ProcessId(1), 2, WaitFlags::NONBLOCK),
            Err(Errno::WouldBlock)
        );
    }

    #[test]
    fn producer_poll_of_a_non_child_fails_closed() {
        let p = producer();
        p.register_child(ProcessId(1), ProcessId(2));
        p.record_exit(ProcessId(2), 0);
        // Task 9 never spawned child 2, and a caller with no children at all
        // sees `NotFound` — a poll grants no authority over another principal.
        assert_eq!(
            p.poll(ProcessId(9), WAIT_PID_ANY, WaitFlags::NONBLOCK),
            Err(Errno::NotFound)
        );
        assert_eq!(
            p.poll(ProcessId(9), 2, WaitFlags::NONBLOCK),
            Err(Errno::NotFound)
        );
    }

    #[test]
    fn null_producer_poll_is_not_implemented() {
        // The inert default announces an unwired interface rather than
        // fabricating a reaped child.
        assert_eq!(
            NULL_PROCESS_WAIT.poll(ProcessId(1), WAIT_PID_ANY, WaitFlags::NONBLOCK),
            Err(Errno::NotImplemented)
        );
    }

    #[test]
    fn a_pending_stop_is_reported_once_and_only_when_requested() {
        let mut table = ProcessTable::new();
        table.register(ProcessId(1), ProcessId(2));
        table.record_stop(ProcessId(2), Signal::Stop);
        // Without the stop-report request the stopped child is invisible:
        // the wait stays blocked exactly as for a running child.
        assert_eq!(table.reap(ProcessId(1), WAIT_PID_ANY, false), Reap::Blocked);
        // With it, the stop is reported — and the child is NOT removed.
        assert_eq!(
            table.reap(ProcessId(1), WAIT_PID_ANY, true),
            Reap::Ready(WaitedChild {
                pid: 2,
                status: WaitStatus::Stopped(Signal::Stop)
            })
        );
        // Edge-triggered: the same stop is never reported twice.
        assert_eq!(table.reap(ProcessId(1), WAIT_PID_ANY, true), Reap::Blocked);
        // The child is still tracked and later exits normally.
        table.record_exit(ProcessId(2), 0);
        assert_eq!(
            table.reap(ProcessId(1), WAIT_PID_ANY, true),
            Reap::Ready(WaitedChild {
                pid: 2,
                status: WaitStatus::Exited(0)
            })
        );
    }

    #[test]
    fn a_continue_clears_an_unreported_stop() {
        let mut table = ProcessTable::new();
        table.register(ProcessId(1), ProcessId(2));
        table.record_stop(ProcessId(2), Signal::Stop);
        // The child is resumed before the parent ever looked: the stale
        // stop must not be reported afterwards.
        table.record_continue(ProcessId(2));
        assert_eq!(table.reap(ProcessId(1), WAIT_PID_ANY, true), Reap::Blocked);
    }

    #[test]
    fn an_exit_supersedes_an_unreported_stop() {
        let mut table = ProcessTable::new();
        table.register(ProcessId(1), ProcessId(2));
        table.record_stop(ProcessId(2), Signal::Stop);
        // The child died while stopped (e.g. a kill): the terminal exit is
        // the report; the stale stop is gone.
        table.record_exit(ProcessId(2), 137);
        assert_eq!(
            table.reap(ProcessId(1), WAIT_PID_ANY, true),
            Reap::Ready(WaitedChild {
                pid: 2,
                status: WaitStatus::Exited(137)
            })
        );
        assert_eq!(table.reap(ProcessId(1), WAIT_PID_ANY, true), Reap::NoChild);
    }

    #[test]
    fn a_stop_on_a_zombie_is_ignored() {
        let mut table = ProcessTable::new();
        table.register(ProcessId(1), ProcessId(2));
        table.record_exit(ProcessId(2), 3);
        // A dead child cannot stop; the exit report stands untouched.
        table.record_stop(ProcessId(2), Signal::Stop);
        assert_eq!(
            table.reap(ProcessId(1), WAIT_PID_ANY, true),
            Reap::Ready(WaitedChild {
                pid: 2,
                status: WaitStatus::Exited(3)
            })
        );
    }

    #[test]
    fn a_reapable_zombie_wins_over_a_pending_stop() {
        let mut table = ProcessTable::new();
        table.register(ProcessId(1), ProcessId(2));
        table.register(ProcessId(1), ProcessId(3));
        table.record_stop(ProcessId(2), Signal::Stop);
        table.record_exit(ProcessId(3), 0);
        // Termination is the stronger, terminal report; the stop stays
        // pending for the next wait.
        assert_eq!(
            table.reap(ProcessId(1), WAIT_PID_ANY, true),
            Reap::Ready(WaitedChild {
                pid: 3,
                status: WaitStatus::Exited(0)
            })
        );
        assert_eq!(
            table.reap(ProcessId(1), WAIT_PID_ANY, true),
            Reap::Ready(WaitedChild {
                pid: 2,
                status: WaitStatus::Stopped(Signal::Stop)
            })
        );
    }

    #[test]
    fn peek_does_not_consume_or_reveal_a_pending_stop() {
        let mut table = ProcessTable::new();
        table.register(ProcessId(1), ProcessId(2));
        table.record_stop(ProcessId(2), Signal::Stop);
        // The wait-set readiness peek is about reapability; a stopped child
        // is still merely "running" to it, and the pending stop survives.
        assert_eq!(table.peek(ProcessId(1), 2), ChildPeek::Running);
        assert_eq!(
            table.reap(ProcessId(1), 2, true),
            Reap::Ready(WaitedChild {
                pid: 2,
                status: WaitStatus::Stopped(Signal::Stop)
            })
        );
    }

    #[test]
    fn producer_poll_reports_a_pending_stop_without_blocking() {
        let p = producer();
        p.register_child(ProcessId(1), ProcessId(2));
        p.record_stop(ProcessId(2), Signal::Stop);
        let flags = WaitFlags::from_bits(WaitFlags::NONBLOCK.bits() | WaitFlags::STOPPED.bits())
            .expect("defined bits");
        assert_eq!(
            p.poll(ProcessId(1), WAIT_PID_ANY, flags),
            Ok(WaitedChild {
                pid: 2,
                status: WaitStatus::Stopped(Signal::Stop)
            })
        );
        // Reported once; the second poll would block again.
        assert_eq!(
            p.poll(ProcessId(1), WAIT_PID_ANY, flags),
            Err(Errno::WouldBlock)
        );
        // The stopped child was not reaped: it is still a live,
        // signallable child.
        assert_eq!(p.authorise_child(ProcessId(1), 2), Ok(ProcessId(2)));
    }

    #[test]
    fn producer_wait_reports_an_already_pending_stop_without_parking() {
        let p = producer();
        p.register_child(ProcessId(1), ProcessId(2));
        p.record_stop(ProcessId(2), Signal::Stop);
        // The stop is already pending when the parent waits, so the report
        // is immediate — the blocking park path is never reached.
        assert_eq!(
            p.wait(ProcessId(1), WAIT_PID_ANY, WaitFlags::STOPPED),
            Ok(WaitedChild {
                pid: 2,
                status: WaitStatus::Stopped(Signal::Stop)
            })
        );
    }
}
