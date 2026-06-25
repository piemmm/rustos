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
//! [`ProcessSpawn`](crate::spawn::ProcessSpawn) and
//! [`MemMap`](crate::memmap::MemMap) producers, the concrete producer
//! ([`KernelProcessWait`]) is installed at boot through the
//! `with_process_wait` builder and the handler reaches it through this trait.
//!
//! Until a producer is installed the handler holds [`NULL_PROCESS_WAIT`],
//! which fails closed: every `wait` returns [`Errno::NotImplemented`] and the bookkeeping hooks are inert — exactly as
//! [`NULL_MEM_MAP`](crate::memmap::NULL_MEM_MAP) and
//! [`NULL_PROCESS_SPAWN`](crate::spawn::NULL_PROCESS_SPAWN) do for their
//! syscalls.

use alloc::collections::BTreeMap;

use rustos_abi::{Errno, WAIT_PID_ANY};
use rustos_kernel_sched_api::SchedulerArch;
use rustos_kernel_sec::TaskId;
use rustos_sync::SpinLock;

use crate::dispatch_slot::RescheduleAction;
use crate::kthread::reschedule_current;

/// A child process reaped by [`ProcessWait::wait`].
///
/// Carries the reaped child's PID (the value the `wait` syscall returns to
/// the caller) and its exit code (the value the kernel writes to the
/// caller's `status` out-pointer).
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct ReapedChild {
    /// PID of the child that was reaped.
    pub pid: u32,
    /// The exit code the child passed to `exit` (the
    /// program's terminating status).
    pub code: i32,
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
    /// `pid` is either a specific child's PID or [`rustos_abi::WAIT_PID_ANY`]
    /// to wait for whichever of `parent`'s children exits next. The handler
    /// has already validated that the caller passed a non-null `status`
    /// pointer (the dispatcher rejects a null `UserPtr`); the implementation
    /// validates the parent/child relationship — a process may only reap its
    /// **own** children — and fails closed.
    ///
    /// # Errors
    ///
    /// Returns [`Errno::NotFound`] when `pid` does not name a child of
    /// `parent` (and `parent` has no children, for [`rustos_abi::WAIT_PID_ANY`]).
    /// The default producer ([`NullProcessWait`]) returns
    /// [`Errno::NotImplemented`] to mark an inert interface.
    fn wait(&self, parent: TaskId, pid: i32) -> Result<ReapedChild, Errno>;

    /// Record that `child` was spawned by `parent`.
    ///
    /// Called from the `spawn` admit path the instant a child is admitted,
    /// so a subsequent [`Self::wait`] can validate the parent/child
    /// relationship and reap it (a process may only reap
    /// its own children). The default is a no-op so the fail-closed default
    /// and the host-test doubles need not restate it.
    fn register_child(&self, _parent: TaskId, _child: TaskId) {}

    /// Record that `task` exited with `code`.
    ///
    /// Called from the `exit` handler for every exiting task; the producer
    /// keeps the code only for a task it is tracking as a child (every other
    /// task — PID 1, a kernel thread — is ignored), so the parent's
    /// [`Self::wait`] can read it back. The default is a no-op.
    fn record_exit(&self, _task: TaskId, _code: i32) {}
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
    fn wait(&self, _parent: TaskId, _pid: i32) -> Result<ReapedChild, Errno> {
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

/// One child's entry in the [`ProcessTable`].
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
struct ChildEntry {
    /// Scheduler task id of the parent that spawned this child.
    parent: u64,
    /// The child's exit code once it has exited (`Some`), or `None` while it
    /// is still running. A `Some` entry is a reapable zombie.
    exit: Option<i32>,
}

/// Outcome of a single [`ProcessTable::reap`] attempt.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum Reap {
    /// A matching child had already exited; it has been removed from the
    /// table and is reported to the caller.
    Ready(ReapedChild),
    /// A matching child exists but has not exited yet — the caller must
    /// block and retry.
    Blocked,
    /// `pid` names no child of the calling parent (and the parent has no
    /// children at all, for [`rustos_abi::WAIT_PID_ANY`]).
    NoChild,
}

/// The parent/child + exit-status bookkeeping behind [`KernelProcessWait`].
///
/// Keyed by a child's scheduler task id, each entry records the child's
/// parent and its exit code once it exits. A child is registered when it is
/// spawned ([`Self::register`]), marked a reapable zombie when it exits
/// ([`Self::record_exit`]), and removed when its parent reaps it
/// ([`Self::reap`]). The map is intentionally tiny and append/remove only —
/// the scheduler owns task lifetimes; this only remembers the parent link
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
    pub fn register(&mut self, parent: TaskId, child: TaskId) {
        self.children.insert(
            child.0,
            ChildEntry {
                parent: parent.0,
                exit: None,
            },
        );
    }

    /// Mark `task` a reapable zombie carrying `code`, if it is a tracked
    /// child. A `task` the table does not track (PID 1, a kernel thread) is
    /// ignored.
    pub fn record_exit(&mut self, task: TaskId, code: i32) {
        if let Some(entry) = self.children.get_mut(&task.0) {
            entry.exit = Some(code);
        }
    }

    /// Try to reap a child of `parent` selected by `pid`.
    ///
    /// `pid` is [`rustos_abi::WAIT_PID_ANY`] for any child or a specific child's
    /// id. Among the matching children: the first (lowest-id, for
    /// determinism) that has already exited is removed
    /// and returned as [`Reap::Ready`]; if matching children exist but none
    /// has exited the result is [`Reap::Blocked`]; if no child matches it is
    /// [`Reap::NoChild`]. A negative `pid` other than [`rustos_abi::WAIT_PID_ANY`]
    /// names no child and fails closed with [`Reap::NoChild`].
    #[must_use]
    pub fn reap(&mut self, parent: TaskId, pid: i32) -> Reap {
        let target: Option<u64> = if pid == WAIT_PID_ANY {
            None
        } else {
            // A specific child id must be a valid non-negative task id; any
            // other negative selector (not WAIT_PID_ANY) names no child and fails
            // closed rather than matching anything.
            match u64::try_from(pid) {
                Ok(id) => Some(id),
                Err(_) => return Reap::NoChild,
            }
        };

        let mut any_match = false;
        let mut reapable: Option<(u64, i32)> = None;
        for (&child_id, entry) in &self.children {
            if entry.parent != parent.0 {
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
        }

        if let Some((child_id, code)) = reapable {
            self.children.remove(&child_id);
            // Scheduler task ids stay well within `u32` for every supported
            // configuration; a value that would not fit saturates rather
            // than wrapping (never silently truncate).
            let pid = u32::try_from(child_id).unwrap_or(u32::MAX);
            Reap::Ready(ReapedChild { pid, code })
        } else if any_match {
            Reap::Blocked
        } else {
            Reap::NoChild
        }
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

impl<A> ProcessWait for KernelProcessWait<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    fn register_child(&self, parent: TaskId, child: TaskId) {
        self.table.lock().register(parent, child);
    }

    fn record_exit(&self, task: TaskId, code: i32) {
        self.table.lock().record_exit(task, code);
        // Wake every parent parked in `wait`: the exiting task may be the
        // child one is blocked on (a real park woken by
        // the exit event). The lock is released above before the wake.
        crate::waitq::procwait_wake();
    }

    fn wait(&self, parent: TaskId, pid: i32) -> Result<ReapedChild, Errno> {
        loop {
            // Re-poll under the lock, then release it *before* parking so the
            // child whose exit we are waiting for can take the same lock from
            // its own `exit` (`record_exit`) while we are suspended. The lock
            // guard is a temporary dropped at the end of this statement.
            let reap = self.table.lock().reap(parent, pid);
            match reap {
                Reap::Ready(child) => return Ok(child),
                Reap::NoChild => return Err(Errno::NotFound),
                Reap::Blocked => {
                    // **Park** the caller off the run queue until a child
                    // exits (never a busy-yield): a
                    // re-enqueuing yield here would keep the run queue
                    // non-empty forever, so the dispatch loop could never
                    // reach its idle `wait_for_interrupt` and a device IRQ
                    // (e.g. an interrupt-driven driver PID 1 spawned) would be
                    // starved. Register on `PROCWAIT_WAITQ` *before* parking
                    // so an exit racing the park is not lost: `record_exit`'s
                    // `procwait_wake` unparks this task and the scheduler's
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
                }
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
            NULL_PROCESS_WAIT.wait(TaskId(7), 9),
            Err(Errno::NotImplemented)
        );
        // A WAIT_PID_ANY request announces the inert interface too, rather than
        // pretending a child was reaped.
        assert_eq!(
            NullProcessWait.wait(TaskId(1), rustos_abi::WAIT_PID_ANY),
            Err(Errno::NotImplemented)
        );
        // The bookkeeping hooks are inert no-ops on the null producer.
        NULL_PROCESS_WAIT.register_child(TaskId(1), TaskId(2));
        NULL_PROCESS_WAIT.record_exit(TaskId(2), 0);
    }

    #[test]
    fn reap_unknown_child_is_no_child() {
        let mut table = ProcessTable::new();
        assert_eq!(table.reap(TaskId(1), WAIT_PID_ANY), Reap::NoChild);
        assert_eq!(table.reap(TaskId(1), 9), Reap::NoChild);
    }

    #[test]
    fn registered_but_unexited_child_blocks() {
        let mut table = ProcessTable::new();
        table.register(TaskId(1), TaskId(2));
        assert_eq!(table.reap(TaskId(1), WAIT_PID_ANY), Reap::Blocked);
        // Selecting the specific child blocks the same way.
        assert_eq!(table.reap(TaskId(1), 2), Reap::Blocked);
    }

    #[test]
    fn exited_child_is_reaped_once_and_removed() {
        let mut table = ProcessTable::new();
        table.register(TaskId(1), TaskId(2));
        table.record_exit(TaskId(2), 7);
        assert_eq!(
            table.reap(TaskId(1), WAIT_PID_ANY),
            Reap::Ready(ReapedChild { pid: 2, code: 7 })
        );
        // A second reap finds nothing — the zombie was removed.
        assert_eq!(table.reap(TaskId(1), WAIT_PID_ANY), Reap::NoChild);
    }

    #[test]
    fn specific_pid_reaps_only_that_child() {
        let mut table = ProcessTable::new();
        table.register(TaskId(1), TaskId(2));
        table.register(TaskId(1), TaskId(3));
        table.record_exit(TaskId(3), 5);
        // Waiting on child 2 (still running) blocks even though 3 is a zombie.
        assert_eq!(table.reap(TaskId(1), 2), Reap::Blocked);
        // Waiting on child 3 reaps it.
        assert_eq!(
            table.reap(TaskId(1), 3),
            Reap::Ready(ReapedChild { pid: 3, code: 5 })
        );
    }

    #[test]
    fn a_process_cannot_reap_another_parents_child() {
        let mut table = ProcessTable::new();
        table.register(TaskId(1), TaskId(2));
        table.record_exit(TaskId(2), 0);
        // Task 9 is not the parent of child 2, so it sees no child.
        assert_eq!(table.reap(TaskId(9), WAIT_PID_ANY), Reap::NoChild);
        assert_eq!(table.reap(TaskId(9), 2), Reap::NoChild);
        // The real parent still reaps it.
        assert_eq!(
            table.reap(TaskId(1), 2),
            Reap::Ready(ReapedChild { pid: 2, code: 0 })
        );
    }

    #[test]
    fn record_exit_for_untracked_task_is_ignored() {
        let mut table = ProcessTable::new();
        // No panic, no entry created for an untracked task (PID 1, a kthread).
        table.record_exit(TaskId(42), 3);
        assert_eq!(table.reap(TaskId(0), WAIT_PID_ANY), Reap::NoChild);
    }

    #[test]
    fn wait_any_reaps_lowest_id_zombie_first() {
        let mut table = ProcessTable::new();
        table.register(TaskId(1), TaskId(5));
        table.register(TaskId(1), TaskId(3));
        table.record_exit(TaskId(5), 50);
        table.record_exit(TaskId(3), 30);
        // Deterministic: the lowest-id reapable child is returned first.
        assert_eq!(
            table.reap(TaskId(1), WAIT_PID_ANY),
            Reap::Ready(ReapedChild { pid: 3, code: 30 })
        );
        assert_eq!(
            table.reap(TaskId(1), WAIT_PID_ANY),
            Reap::Ready(ReapedChild { pid: 5, code: 50 })
        );
    }

    #[test]
    fn negative_non_wait_any_pid_is_no_child() {
        let mut table = ProcessTable::new();
        table.register(TaskId(1), TaskId(2));
        table.record_exit(TaskId(2), 0);
        // -2 is not WAIT_PID_ANY and not a valid child id: fail closed.
        assert_eq!(table.reap(TaskId(1), -2), Reap::NoChild);
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
        p.register_child(TaskId(1), TaskId(2));
        // The child has already exited by the time the parent waits, so the
        // reap is immediate — the blocking park path is never reached (it
        // would require a live scheduler and is proven by the `-M virt`
        // vertical).
        p.record_exit(TaskId(2), 9);
        assert_eq!(
            p.wait(TaskId(1), WAIT_PID_ANY),
            Ok(ReapedChild { pid: 2, code: 9 })
        );
        // The zombie was consumed; a second wait finds no child.
        assert_eq!(p.wait(TaskId(1), WAIT_PID_ANY), Err(Errno::NotFound));
    }

    #[test]
    fn producer_waiting_on_a_non_child_fails_closed() {
        let p = producer();
        p.register_child(TaskId(1), TaskId(2));
        p.record_exit(TaskId(2), 0);
        // Task 9 never spawned child 2: it may not reap it.
        assert_eq!(p.wait(TaskId(9), WAIT_PID_ANY), Err(Errno::NotFound));
        assert_eq!(p.wait(TaskId(9), 2), Err(Errno::NotFound));
    }

    #[test]
    fn producer_blocked_reap_without_a_published_kthread_fails_closed() {
        let p = producer();
        // A registered-but-unexited child would block. In a host test no
        // resumable user kthread is published, so the park cannot proceed and
        // the producer fails closed with `NotImplemented` rather than
        // busy-spinning forever.
        p.register_child(TaskId(1), TaskId(2));
        assert_eq!(p.wait(TaskId(1), WAIT_PID_ANY), Err(Errno::NotImplemented));
    }
}
