//! Generic blocking wait-queue with true park + timed wake (Design D P-2,
//! `.junie/next-pi-prompt.md`).
//!
//! A reusable kernel wait primitive: a task registers on a [`WaitQueue`]
//! and *parks* (`RescheduleAction::Park`, off the run queue — no busy
//! yield), and is woken either by an **explicit event**
//! ([`WaitQueue::wake_all`]) or, when it registered with a finite deadline,
//! by the **timed wake** the architecture one-shot drives
//! ([`WaitQueue::sweep`], fed by the per-tick sweep the arch timer ISR
//! runs). The first consumer is the `hw_tree_wait` syscall, whose waiters
//! the [`HW_TREE_WAITQ`] holds and the [`crate::HwTreeSource`] store wakes
//! when the discovered hardware tree changes.
//!
//! # No lost wake-ups
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
//! depending on the concrete `Scheduler<A>` / arch port. The boot path installs one [`WaitQueueArch`] adapter over the
//! leaked `Scheduler` + arch, and every wake/sweep routes through
//! it. A build that never installs one (host tests of unrelated paths)
//! leaves the explicit-wake / timed-wake helpers as fail-safe no-ops.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use rustos_kernel_sched_api::{CpuId, TaskId};
use rustos_sync::once::OnceCell;
use rustos_sync::SpinLock;

/// Sentinel deadline meaning "no timeout": a waiter registered with this
/// value is only ever released by an explicit [`WaitQueue::wake_all`], never
/// by the timed [`WaitQueue::sweep`], and contributes no
/// [`WaitQueue::earliest_deadline`] arming (the one-shot
/// is armed only for a real pending deadline).
pub const NO_DEADLINE: u64 = u64::MAX;

/// The kernel-installed hook a [`WaitQueue`] uses to wake a parked waiter,
/// read the monotonic clock, and arm the timed-wake one-shot, without the
/// (global, `'static`) wait-queue naming the concrete `Scheduler<A>` / arch
/// port.
pub trait WaitQueueArch: Sync {
    /// Make the parked task `id` runnable again — the scheduler's
    /// cancellation-safe `Scheduler::unpark`, which records a
    /// wake-pending token if the task has not committed to park yet, so no
    /// wake is lost.
    fn unpark(&self, id: TaskId);

    /// Monotonic nanoseconds on the calling CPU (the same clock the
    /// `clock_get` syscall and the wait deadlines use).
    fn now_ns(&self) -> u64;

    /// Arm (or clear, with `None`) the calling CPU's timed-wake one-shot to
    /// the nearest pending deadline (`rustos_arch_api::SchedulerArch::set_wakeup`).
    fn set_wakeup(&self, deadline_ns: Option<u64>);

    /// The scheduler task currently switched in on `cpu`, or [`None`] if no
    /// task is running there (or `cpu` is out of range). Used by a blocking
    /// syscall handler that must register the *current* caller on a
    /// [`WaitQueue`] before parking it but is not itself handed the caller's
    /// id (the console-read backing, `crate::console::BlockingConsoleRead`).
    /// The default returns [`None`] so an uninstalled hook (host tests of
    /// unrelated paths) fails closed rather than parking an unknown task.
    fn current_task(&self, cpu: CpuId) -> Option<TaskId> {
        let _ = cpu;
        None
    }

    /// The CPU the caller is currently running on, or [`None`] before a hook
    /// is installed. A blocking primitive that is **not** handed a CPU id
    /// (the [`SleepLock`](crate::SleepLock), reached through a fixed-signature
    /// method that carries no caller context) resolves the current CPU here
    /// to then look up [`current_task`](Self::current_task) and park it. The
    /// default returns [`None`] so an uninstalled hook fails closed rather
    /// than acting on a guessed CPU.
    fn current_cpu(&self) -> Option<CpuId> {
        None
    }
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
/// one definition of the wait set, no threading concerns of its own.
pub struct WaitQueue {
    waiters: SpinLock<Vec<Waiter>>,
    /// Lock-free "an explicit wake was requested for this queue" flag.
    ///
    /// A wake delivered from **interrupt context** (a device-IRQ
    /// dispatcher, the timer ISR's sweep) must never take a lock a
    /// task interrupted on this CPU may already hold — the fully
    /// preemptive kernel runs in-kernel tasks with device IRQs enabled, so an ISR can fire while a task is inside
    /// [`Self::register`]. [`Self::request_wake`] therefore only sets
    /// this single atomic (it takes no lock and never blocks, exactly
    /// like `rustos_kernel_irq::IrqTable::fire`); the real
    /// [`Self::wake_all`] — which collects waiter ids under the lock and
    /// then calls the scheduler's `unpark` — runs later at a safe
    /// dispatcher-context point via [`drain_pending_wakes`]. A woken
    /// task cannot run until the current in-kernel task yields anyway
    /// (the kernel is non-preemptible), so deferring the *unpark* to
    /// that yield point costs no responsiveness while keeping every ISR
    /// lock-free.
    wake_pending: AtomicBool,
}

impl WaitQueue {
    /// An empty wait-queue. `const` so a consumer can place one in a
    /// `static` (the [`HW_TREE_WAITQ`] global below).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            waiters: SpinLock::new(Vec::new()),
            wake_pending: AtomicBool::new(false),
        }
    }

    /// Request an explicit wake of every waiter **from any context,
    /// including an interrupt handler**, without taking the wait-queue
    /// lock or the scheduler's locks.
    ///
    /// Lock-free: it only sets the `wake_pending` flag. The
    /// actual `unpark` is performed later by [`drain_pending_wakes`] in
    /// dispatcher context (ISRs stay lock-free; the
    /// scheduler is never locked with IRQs disabled). `Release` so the
    /// data the wake advertises (a byte pushed to a console queue, an
    /// `IrqTable` ready flag) is visible before the flag the drain
    /// observes with `Acquire`.
    pub fn request_wake(&self) {
        self.wake_pending.store(true, Ordering::Release);
    }

    /// Consume a pending wake request, returning whether one was set.
    /// `AcqRel` pairs with [`Self::request_wake`]'s `Release`.
    fn take_wake_pending(&self) -> bool {
        self.wake_pending.swap(false, Ordering::AcqRel)
    }

    /// Register `task` as waiting, with an absolute monotonic-ns
    /// `deadline_ns` ([`NO_DEADLINE`] for no timeout). Re-registering the
    /// same task updates its deadline rather than duplicating it, so a
    /// handler that loops (re-arming after each spurious wake) never grows
    /// the queue.
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
    /// while holding the wait-queue lock (no lock held
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
    /// the timed-wake one-shot is armed to (the nearest
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
/// rather than re-pointing the live hook.
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

/// The wait-queue holding the in-kernel driver-store **server** kthread
/// while it has no pending call to serve (Design D D2b-2c). Unlike
/// [`CALL_WAITQ`] (which holds the *callers* awaiting a reply), this holds
/// the bound *server* so it parks off the run queue between requests
/// instead of busy-yielding. It is woken by
/// [`serve_wake`] the instant the `ipc_call` handler posts a request to a
/// registered endpoint, so the server re-runs and drains it. The server
/// registers with [`NO_DEADLINE`] (it waits only for work, never a
/// timeout) and re-checks its endpoint after every wake, so the
/// check-then-park race is closed by the scheduler's wake-pending token.
pub static SERVE_WAITQ: WaitQueue = WaitQueue::new();

/// Wake every parked IPC-server kthread because a request was posted to a
/// registered call endpoint; each re-drains its endpoint and parks again
/// when empty. A fail-safe no-op before the arch hook is installed.
pub fn serve_wake() {
    if let Some(arch) = wait_arch() {
        SERVE_WAITQ.wake_all(arch);
    }
}

/// The wait-queue holding `stream_read` callers blocked on an empty
/// console (`crate::console::BlockingConsoleRead`). A login reading an
/// as-yet-silent console parks here off the run queue (**no** busy yield) so the CPU can idle and service device interrupts
/// (e.g. an interrupt-driven keyboard driver), and is woken either by
/// [`console_wake`] the instant input is pushed to a keyboard-backed
/// console's input queue, or by the timed [`WaitQueue::sweep`] re-poll its
/// bounded deadline arms (so a *polled* UART backing, which has no push, is
/// re-checked). Each woken reader re-polls its device and either returns
/// bytes or parks again, so a wake for a different reader is a harmless
/// spurious wake and the check-then-park race is closed
/// by the scheduler's wake-pending token (the same interlock `irq_wait` /
/// `hw_tree_wait` use).
pub static CONSOLE_WAITQ: WaitQueue = WaitQueue::new();

/// Request a wake of every parked console reader because input was pushed
/// to a keyboard-backed console's input queue.
///
/// Called from the UART receive ISR (interrupt context) as well as the
/// input-focus arbiter (task context), so it is **lock-free**: it only
/// flags the queue ([`WaitQueue::request_wake`]); the real `unpark` runs
/// at the next dispatcher-context [`drain_pending_wakes`]. The woken
/// reader cannot run until the current in-kernel task yields anyway (the
/// kernel is non-preemptible), so deferring the unpark to
/// that point keeps the ISR lock-free without delaying delivery.
pub fn console_wake() {
    CONSOLE_WAITQ.request_wake();
}

/// The wait-queue holding `wait` (process-reap) callers blocked on a child
/// that has not yet exited (`crate::procwait::KernelProcessWait`). A parent
/// blocked in `wait` parks here off the run queue (**no**
/// busy yield) so the CPU can idle and service device interrupts; it is
/// woken by [`procwait_wake`] the instant any task records its exit, then
/// re-polls its child table and either reaps or parks again. Reaping is an
/// explicit event (a child exit), never a timeout, so every waiter
/// registers with [`NO_DEADLINE`]; the check-then-park race is closed by the
/// scheduler's wake-pending token (the same interlock `irq_wait` /
/// `hw_tree_wait` use).
pub static PROCWAIT_WAITQ: WaitQueue = WaitQueue::new();

/// Wake every parent parked in `wait` because a task recorded its exit;
/// each re-checks its child table and either reaps or parks again. A
/// fail-safe no-op before the arch hook is installed.
pub fn procwait_wake() {
    if let Some(arch) = wait_arch() {
        PROCWAIT_WAITQ.wake_all(arch);
    }
}

/// The wait-queue holding `irq_wait` callers (Design D — the user-space
/// device-driver IRQ path). A task that bound an IRQ line with `irq_bind`
/// and called `irq_wait` parks here off the run queue (no busy yield) and is woken by [`irq_wake`] the instant the device-IRQ
/// dispatch path runs [`rustos_kernel_irq::IrqTable::fire`] for *any* line,
/// or, with a finite timeout, by the timed [`WaitQueue::sweep`] below. Each
/// woken waiter re-checks its own bound line's ready flag through
/// [`rustos_kernel_irq::IrqTable::try_wait_step`] and either returns or
/// parks again, so a fire for a different line is a harmless spurious wake and the check-then-park race is closed by the
/// scheduler's wake-pending token (the same interlock `hw_tree_wait` uses).
pub static IRQ_WAITQ: WaitQueue = WaitQueue::new();

/// Request a wake of every parked `irq_wait` caller because a bound IRQ
/// line fired; each woken waiter re-checks its own line and either returns
/// [`Ready`] or parks again, so a fire for a different line is a harmless
/// spurious wake.
///
/// Called from the production device-IRQ dispatch path immediately after
/// [`rustos_kernel_irq::IrqTable::fire`] sets the per-line ready flag, so
/// it is **lock-free**: it only flags the queue
/// ([`WaitQueue::request_wake`]) and is safe to call from the device-IRQ
/// dispatcher while a task it interrupted holds the wait-queue or
/// scheduler locks. The real `unpark` runs at the next
/// dispatcher-context [`drain_pending_wakes`]; mask-before-wake still holds
/// because `fire` masked the line and set `ready` *before* this flag, and
/// the drain's `unpark` re-readies the waiter that then consumes `ready`.
///
/// [`Ready`]: rustos_kernel_irq::WaitOutcome::Ready
pub fn irq_wake() {
    IRQ_WAITQ.request_wake();
}

/// The wait-queue holding `hw_tree_wait` callers (Design D P-2). Woken by
/// the [`crate::HwTreeSource`] store on every change to the discovered
/// hardware tree and by the timed sweep below.
pub static HW_TREE_WAITQ: WaitQueue = WaitQueue::new();

/// Wake every `hw_tree_wait` caller because the discovered hardware tree
/// changed (the store's generation advanced). A fail-safe no-op before the
/// arch hook is installed.
pub fn hw_tree_wake() {
    if let Some(arch) = wait_arch() {
        HW_TREE_WAITQ.wake_all(arch);
    }
}

/// The wait-queue holding `users_db_wait` callers (`plans/PI.md` P11). A
/// `login` spawned before the encrypted root is unlocked parks here off the
/// run queue (**no** busy yield) instead of re-reading
/// `users_db_read` in a yield loop, which flooded the audit log with one
/// ERROR per poll. It is woken by [`users_db_wake`] the instant the unlock
/// reaches a terminal outcome — [`LateUsersDb::install`] published a
/// database, or [`LateUsersDb::resolve`] gave up with none — or, with a
/// finite timeout, by the timed [`WaitQueue::sweep`] below. Each woken
/// waiter re-checks whether the database is still pending and either returns
/// or parks again, so a wake is harmless if it was spurious and the check-then-park race is closed by the scheduler's
/// wake-pending token (the same interlock `hw_tree_wait` uses).
///
/// [`LateUsersDb::install`]: crate::users::LateUsersDb::install
/// [`LateUsersDb::resolve`]: crate::users::LateUsersDb::resolve
pub static USERS_DB_WAITQ: WaitQueue = WaitQueue::new();

/// Wake every `users_db_wait` caller because the user database left its
/// pending state (a database was installed, or the unlock gave up); each
/// re-checks the pending condition and either returns or parks again. A
/// fail-safe no-op before the arch hook is installed.
pub fn users_db_wake() {
    if let Some(arch) = wait_arch() {
        USERS_DB_WAITQ.wake_all(arch);
    }
}

/// The wait-queue holding `ipc_call` callers (Design D D2b). A caller parks
/// here after posting its request to a [`rustos_kernel_ipc::call::CallEndpoint`]
/// and is woken by [`call_wake`] when the bound server replies (no busy yield). `ipc_call` carries no timeout, so every waiter
/// registers with [`NO_DEADLINE`] and is only ever released by an explicit
/// wake, never the timed [`WaitQueue::sweep`].
pub static CALL_WAITQ: WaitQueue = WaitQueue::new();

/// Wake every parked `ipc_call` caller because a [`CallEndpoint`] reply (or
/// cancellation) arrived; each re-checks its ticket and either claims the
/// reply or parks again. A fail-safe no-op before the arch hook is installed.
///
/// [`CallEndpoint`]: rustos_kernel_ipc::call::CallEndpoint
pub fn call_wake() {
    if let Some(arch) = wait_arch() {
        CALL_WAITQ.wake_all(arch);
    }
}

/// Lock-free "the timed-wake one-shot fired and a deadline sweep is owed"
/// flag, set by [`timed_wake_sweep`] in the timer ISR and consumed by
/// [`drain_pending_wakes`] in dispatcher context (the
/// ISR stays lock-free; the scheduler's `unpark` runs at a safe point).
static TIMED_SWEEP_PENDING: AtomicBool = AtomicBool::new(false);

/// Request a timed-wake sweep because the architecture one-shot fired.
///
/// Called from the arch timer ISR (every armed one-shot expiry) so a
/// finite-timeout wait is honoured even when the CPU has no runnable task
/// to preempt. **Lock-free**: it only sets
/// `TIMED_SWEEP_PENDING`; the real per-queue [`WaitQueue::sweep`] +
/// `unpark` + one-shot re-arm runs at the next dispatcher-context
/// [`drain_pending_wakes`], never in the ISR (which must not take the
/// wait-queue or scheduler locks a task it interrupted may hold).
pub fn timed_wake_sweep() {
    TIMED_SWEEP_PENDING.store(true, Ordering::Release);
}

/// Perform the actual deadline sweep across every timed wait-queue and
/// re-arm the one-shot to the next pending deadline. Runs only in
/// dispatcher context, out of [`drain_pending_wakes`].
fn run_timed_sweep(arch: &dyn WaitQueueArch) {
    let now = arch.now_ns();
    HW_TREE_WAITQ.sweep(arch, now);
    IRQ_WAITQ.sweep(arch, now);
    CONSOLE_WAITQ.sweep(arch, now);
    USERS_DB_WAITQ.sweep(arch, now);
    // Re-arm to the soonest pending deadline across *every* timed
    // wait-queue, so no finite timeout is dropped because another queue
    // armed a later one-shot (the nearest armed
    // wakeup).
    arch.set_wakeup(nearest_timed_deadline());
}

/// Perform every wake the interrupt handlers deferred, at a safe
/// dispatcher-context point.
///
/// The fully preemptive kernel runs in-kernel tasks with device IRQs
/// enabled, so an ISR must never take the wait-queue
/// or scheduler locks a task it interrupted may hold. Instead the ISR
/// flags a pending wake ([`WaitQueue::request_wake`] / [`timed_wake_sweep`])
/// and the dispatch loop calls this between scheduler steps and before it
/// idles, where taking those locks is safe. It performs the real
/// [`WaitQueue::wake_all`] for every edge-flagged queue and the deadline
/// `run_timed_sweep`, unparking the affected tasks.
///
/// Returns `true` if any wake was owed (a task may now be runnable), so
/// the caller re-steps the scheduler rather than idling. A fail-safe
/// no-op before the arch hook is installed.
pub fn drain_pending_wakes() -> bool {
    let Some(arch) = wait_arch() else {
        return false;
    };
    let mut woke = false;
    // Edge wakes flagged from interrupt context (device IRQ, UART receive).
    if CONSOLE_WAITQ.take_wake_pending() {
        CONSOLE_WAITQ.wake_all(arch);
        woke = true;
    }
    if IRQ_WAITQ.take_wake_pending() {
        IRQ_WAITQ.wake_all(arch);
        woke = true;
    }
    // Deadline sweep flagged by the timer one-shot.
    if TIMED_SWEEP_PENDING.swap(false, Ordering::AcqRel) {
        run_timed_sweep(arch);
        woke = true;
    }
    woke
}

/// The installed arch hook's monotonic clock, for a consumer that times a
/// wait (the console readers' secret-feedback animation ticks), or [`None`]
/// before the hook is installed — on such a build nothing can park, so no
/// deadline is ever awaited against a missing clock.
#[must_use]
pub fn wait_now_ns() -> Option<u64> {
    wait_arch().map(WaitQueueArch::now_ns)
}

/// Re-point the timed-wake one-shot at the soonest deadline any waiter
/// still needs (or clear it when none does). Called by a park site after
/// registering a finite deadline — so the wake fires even on an
/// otherwise-idle CPU — and after deregistering one, so a finished timed
/// wait never leaves a stale arming behind. A fail-safe no-op before the
/// arch hook is installed.
pub fn rearm_timed_wakeup() {
    if let Some(arch) = wait_arch() {
        arch.set_wakeup(nearest_timed_deadline());
    }
}

/// Deregister `task` from [`CONSOLE_WAITQ`] and — only when its wait had
/// registered a finite `deadline_ns` — re-point the timed one-shot at
/// whatever any remaining waiter needs, so a finished animated console wait
/// (a secret-feedback tick) never leaves a stale arming behind while an
/// ordinary untimed read pays nothing extra. The one definition both
/// blocking console readers (`BlockingConsoleRead` and the unlock
/// kthread's reader) share.
pub fn console_deregister(task: TaskId, deadline_ns: u64) {
    CONSOLE_WAITQ.deregister(task);
    if deadline_ns != NO_DEADLINE {
        rearm_timed_wakeup();
    }
}

/// The soonest finite deadline pending across **every** timed wait-queue
/// (`HW_TREE_WAITQ`, `IRQ_WAITQ`, `CONSOLE_WAITQ`, `USERS_DB_WAITQ`), or
/// [`None`] if none has one. A park site arms the one-shot to this so
/// registering a *later* deadline never delays an already-pending earlier
/// wake.
#[must_use]
pub fn nearest_timed_deadline() -> Option<u64> {
    [
        HW_TREE_WAITQ.earliest_deadline(),
        IRQ_WAITQ.earliest_deadline(),
        CONSOLE_WAITQ.earliest_deadline(),
        USERS_DB_WAITQ.earliest_deadline(),
    ]
    .into_iter()
    .flatten()
    .min()
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

    #[test]
    fn request_wake_is_a_lock_free_one_shot_flag() {
        // The interrupt-context wake request sets a flag without touching
        // the waiter lock; the dispatcher consumes it exactly once
        // (the ISR is lock-free, the unpark deferred).
        let q = WaitQueue::new();
        assert!(!q.take_wake_pending(), "fresh queue owes no wake");
        q.request_wake();
        // Idempotent set: a second request before a drain does not stack.
        q.request_wake();
        assert!(q.take_wake_pending(), "the flagged wake is observed once");
        assert!(
            !q.take_wake_pending(),
            "the flag is cleared by the consuming drain"
        );
    }

    #[test]
    fn request_wake_does_not_itself_unpark() {
        // Requesting a wake only flags it; no waiter is unparked until a
        // dispatcher-context drain runs `wake_all`. (Here we observe that
        // `request_wake` performs no `unpark` by checking it leaves the
        // waiter set untouched and only sets the flag.)
        let arch = MockArch::new();
        let q = WaitQueue::new();
        q.register(5, NO_DEADLINE);
        q.request_wake();
        assert!(
            arch.unparked.borrow().is_empty(),
            "the request defers the unpark"
        );
        // The deferred drain (modelled here by the take + wake_all the
        // real `drain_pending_wakes` performs) does the actual unpark.
        assert!(q.take_wake_pending());
        q.wake_all(&arch);
        assert_eq!(*arch.unparked.borrow(), alloc::vec![5]);
    }
}
