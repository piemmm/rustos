//! Generic blocking wait-queue with true park + timed wake (Design D P-2,
//! `.junie/next-pi-prompt.md`).
//!
//! A reusable kernel wait primitive: a task registers on a [`WaitQueue`]
//! and *parks* (`RescheduleAction::Park`, off the run queue — no busy
//! yield, `AGENTS.md` §2.1), and is woken either by an **explicit event**
//! ([`WaitQueue::wake_all`]) or, when it registered with a finite deadline,
//! by the **timed wake** the architecture one-shot drives
//! ([`WaitQueue::sweep`], fed by the per-tick sweep the arch timer ISR
//! runs). The first consumer is the `hw_tree_wait` syscall, whose waiters
//! the [`HW_TREE_WAITQ`] holds and the [`crate::HwTreeSource`] store wakes
//! when the discovered hardware tree changes (`AGENTS.md` §18.4).
//!
//! # No lost wake-ups (`AGENTS.md` §2.1)
//!
//! The park/unpark race — a wake delivered *after* the waiter last checked
//! its condition but *before* it commits to park — is closed in the
//! scheduler itself: `Scheduler::unpark` of a not-yet-parked task
//! records a wake-pending token that the dispatch loop's `Park` commit
//! consumes, re-readying the task instead of sleeping it. A waiter
//! therefore only ever sleeps through a wake it has *not* yet observed, and
//! always re-checks its condition after every wake, so a finished or
//! timed-out wait returns rather than parks forever.
//!
//! # Why an installed arch hook
//!
//! Waking a parked waiter needs the scheduler's `unpark`, the timed sweep
//! needs the monotonic clock, and arming the one-shot needs the arch timer
//! — none of which a global (`'static`) wait-queue can name without
//! depending on the concrete `Scheduler<A>` / arch port (`AGENTS.md` §17.4
//! / §2.2). The boot path installs one [`WaitQueueArch`] adapter over the
//! leaked `Scheduler` + arch, and every wake/sweep routes through
//! it. A build that never installs one (host tests of unrelated paths)
//! leaves the explicit-wake / timed-wake helpers as fail-safe no-ops.

use alloc::vec::Vec;

use rustos_kernel_sched_api::TaskId;
use rustos_sync::once::OnceCell;
use rustos_sync::SpinLock;

/// Sentinel deadline meaning "no timeout": a waiter registered with this
/// value is only ever released by an explicit [`WaitQueue::wake_all`], never
/// by the timed [`WaitQueue::sweep`], and contributes no
/// [`WaitQueue::earliest_deadline`] arming (`AGENTS.md` §17.1 — the one-shot
/// is armed only for a real pending deadline).
pub const NO_DEADLINE: u64 = u64::MAX;

/// The kernel-installed hook a [`WaitQueue`] uses to wake a parked waiter,
/// read the monotonic clock, and arm the timed-wake one-shot, without the
/// (global, `'static`) wait-queue naming the concrete `Scheduler<A>` / arch
/// port (`AGENTS.md` §17.4 / §2.2).
pub trait WaitQueueArch: Sync {
    /// Make the parked task `id` runnable again — the scheduler's
    /// cancellation-safe `Scheduler::unpark`, which records a
    /// wake-pending token if the task has not committed to park yet, so no
    /// wake is lost (`AGENTS.md` §2.1).
    fn unpark(&self, id: TaskId);

    /// Monotonic nanoseconds on the calling CPU (the same clock the
    /// `clock_get` syscall and the wait deadlines use).
    fn now_ns(&self) -> u64;

    /// Arm (or clear, with `None`) the calling CPU's timed-wake one-shot to
    /// the nearest pending deadline (`rustos_arch_api::SchedulerArch::set_wakeup`).
    fn set_wakeup(&self, deadline_ns: Option<u64>);
}

/// One registered waiter: the task to wake and the absolute monotonic-ns
/// deadline at which the timed sweep releases it ([`NO_DEADLINE`] = never by
/// timeout).
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
struct Waiter {
    task: TaskId,
    deadline_ns: u64,
}

/// A reusable blocking wait-queue.
///
/// Pure data: it holds only the registered waiters behind a [`SpinLock`]
/// and never itself parks or switches context — the *caller* (a syscall
/// handler) drives the park loop, registering here so a waker can find and
/// `unpark` it. This mirrors `kernel/irq`'s passive `IrqTable`:
/// one definition of the wait set, no threading concerns of its own
/// (`AGENTS.md` §2.2).
pub struct WaitQueue {
    waiters: SpinLock<Vec<Waiter>>,
}

impl WaitQueue {
    /// An empty wait-queue. `const` so a consumer can place one in a
    /// `static` (the [`HW_TREE_WAITQ`] global below).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            waiters: SpinLock::new(Vec::new()),
        }
    }

    /// Register `task` as waiting, with an absolute monotonic-ns
    /// `deadline_ns` ([`NO_DEADLINE`] for no timeout). Re-registering the
    /// same task updates its deadline rather than duplicating it, so a
    /// handler that loops (re-arming after each spurious wake) never grows
    /// the queue (`AGENTS.md` §2.3).
    pub fn register(&self, task: TaskId, deadline_ns: u64) {
        let mut waiters = self.waiters.lock();
        if let Some(existing) = waiters.iter_mut().find(|w| w.task == task) {
            existing.deadline_ns = deadline_ns;
        } else {
            waiters.push(Waiter { task, deadline_ns });
        }
    }

    /// Remove `task` from the wait set (it finished waiting). Idempotent:
    /// removing an absent task is a no-op.
    pub fn deregister(&self, task: TaskId) {
        self.waiters.lock().retain(|w| w.task != task);
    }

    /// Wake **every** waiter (an explicit event changed the condition they
    /// are blocked on). Each is `unpark`ed through `arch`; the woken
    /// handler re-checks its condition and deregisters itself.
    ///
    /// The task ids are collected under the lock and the lock released
    /// *before* any `unpark`, so the scheduler's own locks are never taken
    /// while holding the wait-queue lock (`AGENTS.md` §2.1 — no lock held
    /// across a hand-off).
    pub fn wake_all(&self, arch: &dyn WaitQueueArch) {
        let ids: Vec<TaskId> = self.waiters.lock().iter().map(|w| w.task).collect();
        for id in ids {
            arch.unpark(id);
        }
    }

    /// Wake every waiter whose finite deadline is at or before `now_ns`
    /// (the timed wake). A [`NO_DEADLINE`] waiter is never released here.
    ///
    /// As with [`Self::wake_all`], the expired ids are collected under the
    /// lock and `unpark`ed after it is dropped.
    pub fn sweep(&self, arch: &dyn WaitQueueArch, now_ns: u64) {
        let ids: Vec<TaskId> = self
            .waiters
            .lock()
            .iter()
            .filter(|w| w.deadline_ns != NO_DEADLINE && w.deadline_ns <= now_ns)
            .map(|w| w.task)
            .collect();
        for id in ids {
            arch.unpark(id);
        }
    }

    /// The soonest finite deadline among current waiters, or `None` if the
    /// queue is empty or every waiter is [`NO_DEADLINE`]. This is the value
    /// the timed-wake one-shot is armed to (`AGENTS.md` §17.1 — the nearest
    /// armed wakeup).
    #[must_use]
    pub fn earliest_deadline(&self) -> Option<u64> {
        self.waiters
            .lock()
            .iter()
            .map(|w| w.deadline_ns)
            .filter(|&d| d != NO_DEADLINE)
            .min()
    }

    /// `true` if no task is currently waiting. Diagnostic / test observer.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.waiters.lock().is_empty()
    }
}

impl Default for WaitQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// The boot-installed [`WaitQueueArch`] adapter (set-once per boot).
static WAIT_ARCH: OnceCell<&'static (dyn WaitQueueArch + 'static)> = OnceCell::new();

/// Error returned when [`install_wait_arch`] is called more than once.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct WaitArchAlreadyInstalled;

/// Publish the production [`WaitQueueArch`] adapter (the boot path's leaked
/// `Scheduler<A>` + arch). Set-once per boot: a second call fails closed
/// rather than re-pointing the live hook (`AGENTS.md` §2.1).
///
/// # Errors
/// [`WaitArchAlreadyInstalled`] if a hook was already installed.
pub fn install_wait_arch(
    arch: &'static (dyn WaitQueueArch + 'static),
) -> Result<(), WaitArchAlreadyInstalled> {
    WAIT_ARCH.set(arch).map_err(|_| WaitArchAlreadyInstalled)
}

/// The installed [`WaitQueueArch`], or `None` before a hook is published.
#[must_use]
pub fn wait_arch() -> Option<&'static (dyn WaitQueueArch + 'static)> {
    WAIT_ARCH.get().ok().flatten().copied()
}

/// The wait-queue holding `hw_tree_wait` callers (Design D P-2). Woken by
/// the [`crate::HwTreeSource`] store on every change to the discovered
/// hardware tree (`AGENTS.md` §18.4) and by the timed sweep below.
pub static HW_TREE_WAITQ: WaitQueue = WaitQueue::new();

/// Wake every `hw_tree_wait` caller because the discovered hardware tree
/// changed (the store's generation advanced). A fail-safe no-op before the
/// arch hook is installed (`AGENTS.md` §2.9).
pub fn hw_tree_wake() {
    if let Some(arch) = wait_arch() {
        HW_TREE_WAITQ.wake_all(arch);
    }
}

/// Release any timed waiters whose deadline has elapsed and re-arm the
/// one-shot to the next pending deadline. Called from the arch timer ISR's
/// per-tick sweep (every tick, EL0 or idle EL1) so a finite-timeout wait is
/// honoured even when the CPU has no runnable task to preempt (`AGENTS.md`
/// §17.1). A fail-safe no-op before the arch hook is installed.
pub fn timed_wake_sweep() {
    if let Some(arch) = wait_arch() {
        let now = arch.now_ns();
        HW_TREE_WAITQ.sweep(arch, now);
        arch.set_wakeup(HW_TREE_WAITQ.earliest_deadline());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use core::cell::RefCell;

    /// A mock [`WaitQueueArch`] recording every `unpark` and `set_wakeup`,
    /// with a settable monotonic clock, so the wait-queue logic is testable
    /// without a real scheduler or timer.
    struct MockArch {
        unparked: RefCell<Vec<TaskId>>,
        /// Number of [`WaitQueueArch::set_wakeup`] calls (`0` = never
        /// called), and the most recent argument. Split into a count plus
        /// an `Option<u64>` rather than an `Option<Option<u64>>` so the
        /// three states are distinguished without the `option_option` lint.
        wakeup_calls: RefCell<u32>,
        last_wakeup: RefCell<Option<u64>>,
        now: RefCell<u64>,
    }

    impl MockArch {
        fn new() -> Self {
            Self {
                unparked: RefCell::new(Vec::new()),
                wakeup_calls: RefCell::new(0),
                last_wakeup: RefCell::new(None),
                now: RefCell::new(0),
            }
        }
    }

    // SAFETY: the tests are single-threaded; `MockArch` is never shared
    // across threads. `WaitQueueArch: Sync` is satisfied structurally only
    // for the trait-object call, which never happens concurrently here.
    unsafe impl Sync for MockArch {}

    impl WaitQueueArch for MockArch {
        fn unpark(&self, id: TaskId) {
            self.unparked.borrow_mut().push(id);
        }
        fn now_ns(&self) -> u64 {
            *self.now.borrow()
        }
        fn set_wakeup(&self, deadline_ns: Option<u64>) {
            *self.wakeup_calls.borrow_mut() += 1;
            *self.last_wakeup.borrow_mut() = deadline_ns;
        }
    }

    #[test]
    fn register_is_idempotent_and_updates_the_deadline() {
        let q = WaitQueue::new();
        q.register(7, 100);
        q.register(7, 250);
        // One waiter, with the updated deadline.
        assert_eq!(q.earliest_deadline(), Some(250));
        assert!(!q.is_empty());
        q.deregister(7);
        assert!(q.is_empty());
        // Deregistering an absent task is a no-op.
        q.deregister(7);
    }

    #[test]
    fn wake_all_unparks_every_registered_waiter() {
        let arch = MockArch::new();
        let q = WaitQueue::new();
        q.register(1, NO_DEADLINE);
        q.register(2, 500);
        q.wake_all(&arch);
        let mut got = arch.unparked.borrow().clone();
        got.sort_unstable();
        assert_eq!(got, alloc::vec![1, 2], "both waiters woken");
    }

    #[test]
    fn sweep_releases_only_expired_finite_deadlines() {
        let arch = MockArch::new();
        let q = WaitQueue::new();
        q.register(1, NO_DEADLINE); // never by timeout
        q.register(2, 100); // expired at now=150
        q.register(3, 1000); // not yet
        q.sweep(&arch, 150);
        assert_eq!(
            *arch.unparked.borrow(),
            alloc::vec![2],
            "only the elapsed finite deadline is released"
        );
    }

    #[test]
    fn earliest_deadline_ignores_no_deadline_waiters() {
        let q = WaitQueue::new();
        q.register(1, NO_DEADLINE);
        assert_eq!(q.earliest_deadline(), None, "an infinite wait arms nothing");
        q.register(2, 900);
        q.register(3, 400);
        assert_eq!(q.earliest_deadline(), Some(400), "the soonest finite one");
    }

    #[test]
    fn an_empty_queue_arms_no_wakeup() {
        let q = WaitQueue::new();
        assert_eq!(q.earliest_deadline(), None);
        assert!(q.is_empty());
    }

    #[test]
    fn set_wakeup_records_the_latest_arming_through_the_arch() {
        let arch = MockArch::new();
        assert_eq!(*arch.wakeup_calls.borrow(), 0, "never called yet");
        arch.set_wakeup(Some(900));
        assert_eq!(*arch.last_wakeup.borrow(), Some(900));
        // Clearing the timed arming records `None`, distinguished from
        // "never called" by the call count.
        arch.set_wakeup(None);
        assert_eq!(*arch.last_wakeup.borrow(), None);
        assert_eq!(*arch.wakeup_calls.borrow(), 2);
    }
}
