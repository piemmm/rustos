//! Generic blocking wait-queue with true park + timed wake (Design D P-2,
//! `plans/PI.md`).
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

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use tairix_kernel_sched_api::{CpuId, TaskId};
use tairix_sync::once::OnceCell;
use tairix_sync::SpinLock;

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
    /// the nearest pending deadline (`tairix_arch_api::SchedulerArch::set_wakeup`).
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

/// Which condition on a queue a waiter is registered against.
///
/// A queue whose event releases *everyone* on it — a shared latch resolving, a
/// device line firing that every waiter re-checks — leaves every waiter on
/// [`WakeKey::NONE`] and wakes with [`WaitQueue::wake_all`]. A queue that holds
/// waiters of many independent objects instead keys each one, so
/// [`WaitQueue::wake_key`] releases only the waiters an event actually concerns
/// and unrelated objects' waiters stay parked: that is what keeps one shared
/// queue (one deadline index, one timed sweep) from becoming a machine-wide
/// thundering herd.
///
/// Keys are minted inside this crate from a monotonic counter, never supplied
/// by a caller, so two live objects can never collide on one.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct WakeKey(u64);

impl WakeKey {
    /// The unkeyed condition: the whole queue.
    pub const NONE: Self = Self(0);

    /// A keyed condition from a minted, never-reused identity.
    pub(crate) const fn new(id: u64) -> Self {
        Self(id)
    }
}

/// One registered waiter's bookkeeping: its FIFO arrival sequence and the
/// absolute monotonic-ns deadline at which the timed sweep releases it
/// ([`NO_DEADLINE`] = never by timeout).
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
struct Waiter {
    /// Monotonic arrival order. The oldest waiter (smallest `seq`) is the
    /// FIFO head that [`WaitQueue::wake_one`] releases, so repeated
    /// contention can never move an older task behind newer arrivals.
    seq: u64,
    deadline_ns: u64,
}

/// A registered waiter's identity: the condition it waits on and the task
/// waiting. Key-major, so one key's waiters are a contiguous range.
type WaiterId = (WakeKey, TaskId);

/// The registered-waiter set behind a [`WaitQueue`]'s lock.
///
/// A thin `Vec` scan was the P-2 slice; the complete primitive keeps three
/// cross-indices so every load-bearing per-park operation is O(log n), never a
/// linear scan under contended multi-user load:
///
/// - [`by_waiter`](Self::by_waiter): the canonical set, keyed by
///   [`WaiterId`], for O(log n) `register` / `deregister` /
///   `wake_waiter` membership and, because the key sorts first, an O(log n +
///   woken) [`WaitQueue::wake_key`] over one condition's waiters alone.
/// - [`order`](Self::order): arrival `seq` → waiter, so the FIFO head
///   ([`WaitQueue::wake_one`], [`WaitQueue::oldest_task`]) is the first key
///   — O(log n), a *stated* first-come-first-served fairness discipline with
///   no starvation (an older waiter is never overtaken).
/// - [`deadlines`](Self::deadlines): `(deadline_ns, seq)` → waiter, holding
///   only finite-deadline waiters, so [`WaitQueue::earliest_deadline`] is
///   the first key (O(log n)) and [`WaitQueue::sweep`] visits only the
///   already-expired prefix (O(log n + woken)) instead of scanning every
///   waiter on every timer expiry.
///
/// The three stay consistent: a waiter is in `by_waiter` and `order` always,
/// and in `deadlines` iff its deadline is finite.
struct WaitSet {
    /// Next FIFO arrival sequence to hand out. Monotonic; a fresh `register`
    /// takes and increments it, a re-`register` of a present waiter keeps its
    /// existing `seq` so its FIFO position is preserved.
    next_seq: u64,
    by_waiter: BTreeMap<WaiterId, Waiter>,
    order: BTreeMap<u64, WaiterId>,
    deadlines: BTreeMap<(u64, u64), WaiterId>,
}

impl WaitSet {
    /// An empty set. `const` so the enclosing [`WaitQueue`] stays
    /// `const`-constructible for a `static`.
    const fn new() -> Self {
        Self {
            next_seq: 0,
            by_waiter: BTreeMap::new(),
            order: BTreeMap::new(),
            deadlines: BTreeMap::new(),
        }
    }
}

/// A reusable blocking wait-queue.
///
/// Pure data: it holds only the registered waiters behind a [`SpinLock`]
/// and never itself parks or switches context — the *caller* (a syscall
/// handler) drives the park loop, registering here so a waker can find and
/// `unpark` it. This mirrors `kernel/irq`'s passive `IrqTable`:
/// one definition of the wait set, no threading concerns of its own.
pub struct WaitQueue {
    waiters: SpinLock<WaitSet>,
    /// Lock-free "an explicit wake was requested for this queue" flag.
    ///
    /// A wake delivered from **interrupt context** (a device-IRQ
    /// dispatcher, the timer ISR's sweep) must never take a lock a
    /// task interrupted on this CPU may already hold — the fully
    /// preemptive kernel runs in-kernel tasks with device IRQs enabled, so an ISR can fire while a task is inside
    /// [`Self::register`]. [`Self::request_wake`] therefore only sets
    /// this single atomic (it takes no lock and never blocks, exactly
    /// like `tairix_kernel_irq::IrqTable::fire`); the real
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
            waiters: SpinLock::new(WaitSet::new()),
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

    /// Non-consuming peek: whether a wake request is currently pending
    /// (awaiting its dispatcher-context drain). Read by the preemption
    /// gate so a timer tick never skips a reschedule a device-IRQ wake
    /// still needs — the woken task must reach [`drain_pending_wakes`].
    fn wake_is_pending(&self) -> bool {
        self.wake_pending.load(Ordering::Acquire)
    }

    /// Register `task` as waiting on the whole queue ([`WakeKey::NONE`]) with
    /// an absolute monotonic-ns `deadline_ns` ([`NO_DEADLINE`] for no
    /// timeout).
    pub fn register(&self, task: TaskId, deadline_ns: u64) {
        self.register_keyed(WakeKey::NONE, task, deadline_ns);
    }

    /// Register `task` as waiting on the condition `key`, with an absolute
    /// monotonic-ns `deadline_ns` ([`NO_DEADLINE`] for no timeout).
    /// Re-registering the same `(key, task)` updates its deadline rather than
    /// duplicating it *and preserves its FIFO position* (a handler that loops,
    /// re-arming after each spurious wake, keeps its place in line and never
    /// grows the queue). A task waiting on several conditions at once (a
    /// wait-set naming more than one stream) registers under each key and is
    /// released by whichever fires. O(log n) — no linear scan on this per-park
    /// path.
    pub fn register_keyed(&self, key: WakeKey, task: TaskId, deadline_ns: u64) {
        let mut set = self.waiters.lock();
        let id = (key, task);
        if let Some(existing) = set.by_waiter.get(&id).copied() {
            // Present: keep the FIFO `seq`, only re-index the deadline.
            if existing.deadline_ns != NO_DEADLINE {
                set.deadlines.remove(&(existing.deadline_ns, existing.seq));
            }
            let seq = existing.seq;
            set.by_waiter.insert(id, Waiter { seq, deadline_ns });
            if deadline_ns != NO_DEADLINE {
                set.deadlines.insert((deadline_ns, seq), id);
            }
        } else {
            let seq = set.next_seq;
            set.next_seq += 1;
            set.by_waiter.insert(id, Waiter { seq, deadline_ns });
            set.order.insert(seq, id);
            if deadline_ns != NO_DEADLINE {
                set.deadlines.insert((deadline_ns, seq), id);
            }
        }
    }

    /// Remove `task`'s unkeyed registration ([`WakeKey::NONE`]) from the wait
    /// set (it finished waiting).
    pub fn deregister(&self, task: TaskId) {
        self.deregister_keyed(WakeKey::NONE, task);
    }

    /// Remove `task`'s registration on `key` from the wait set. Idempotent:
    /// removing an absent waiter is a no-op. O(log n).
    pub fn deregister_keyed(&self, key: WakeKey, task: TaskId) {
        let mut set = self.waiters.lock();
        if let Some(w) = set.by_waiter.remove(&(key, task)) {
            set.order.remove(&w.seq);
            if w.deadline_ns != NO_DEADLINE {
                set.deadlines.remove(&(w.deadline_ns, w.seq));
            }
        }
    }

    /// Wake **every** waiter (an explicit event changed the condition they
    /// are blocked on). Each is `unpark`ed through `arch`; the woken
    /// handler re-checks its condition and deregisters itself.
    ///
    /// A genuine broadcast is O(n) in the number of waiters, by definition;
    /// this is the only linear path and is reserved for conditions that
    /// really do release everyone (cancellation, a shared latch resolving).
    /// The ids are collected in FIFO order under the lock and the lock
    /// released *before* any `unpark`, so the scheduler's own locks are
    /// never taken while holding the wait-queue lock (no lock held across a
    /// hand-off).
    pub fn wake_all(&self, arch: &dyn WaitQueueArch) {
        let ids: Vec<TaskId> = self
            .waiters
            .lock()
            .order
            .values()
            .map(|&(_, task)| task)
            .collect();
        for id in ids {
            arch.unpark(id);
        }
    }

    /// Wake every waiter registered on the condition `key`, returning how many
    /// there were. The one wake a queue that holds many independent objects'
    /// waiters uses: a waiter on another key stays parked, so an event never
    /// costs the machine a wake per unrelated object.
    ///
    /// Every waiter on one key is released, because a key names a condition
    /// they are all blocked on and all must re-check — a stream's bytes
    /// arriving, its space freeing, its peer closing terminally. That is a
    /// wake-all over a *single object's* waiters (in practice one), not the
    /// queue-wide broadcast [`Self::wake_all`] performs. The ids are collected
    /// under the lock and the lock released before any `unpark`, so the
    /// scheduler's locks are never taken while holding this one. An empty
    /// range allocates nothing. O(log n + woken).
    pub fn wake_key(&self, arch: &dyn WaitQueueArch, key: WakeKey) -> usize {
        let ids: Vec<TaskId> = self
            .waiters
            .lock()
            .by_waiter
            .range((key, TaskId::MIN)..=(key, TaskId::MAX))
            .map(|(&(_, task), _)| task)
            .collect();
        for &id in &ids {
            arch.unpark(id);
        }
        ids.len()
    }

    /// Wake the oldest registered waiter, returning whether one existed.
    ///
    /// Registration order is FIFO and re-registration keeps a waiter in
    /// place, so repeated contention cannot move an older task behind newer
    /// arrivals — a *stated* no-starvation guarantee. The waiter remains
    /// registered until it resumes and deregisters itself; this preserves
    /// the register-before-retest lost-wake discipline while avoiding a
    /// thundering herd. O(log n).
    pub fn wake_one(&self, arch: &dyn WaitQueueArch) -> bool {
        self.wake_n(arch, 1) == 1
    }

    /// Wake the `count` oldest registered waiters, returning how many were
    /// woken (fewer than `count` when fewer are waiting).
    ///
    /// The counted form of [`Self::wake_one`], and its one definition: a futex
    /// wake releases a caller-chosen number of waiters, and repeating
    /// `wake_one` would keep re-waking the same head (a waiter stays
    /// registered until it resumes and deregisters itself, which is what
    /// preserves the lost-wake discipline). The ids are collected in FIFO
    /// order under the lock and the lock released *before* any `unpark`, so
    /// the scheduler's locks are never taken while holding this one.
    /// O(log n + woken).
    pub fn wake_n(&self, arch: &dyn WaitQueueArch, count: usize) -> usize {
        let ids: Vec<TaskId> = self
            .waiters
            .lock()
            .order
            .values()
            .take(count)
            .map(|&(_, task)| task)
            .collect();
        for &id in &ids {
            arch.unpark(id);
        }
        ids.len()
    }

    /// The oldest registered task without waking or removing it.
    ///
    /// Used by [`SleepLock`](crate::SleepLock) to publish direct ownership
    /// handoff before waking the designated FIFO waiter. The waiter remains
    /// registered until it resumes, so the normal register-before-retest
    /// lost-wake discipline is preserved. O(log n).
    #[must_use]
    pub(crate) fn oldest_task(&self) -> Option<TaskId> {
        self.waiters
            .lock()
            .order
            .values()
            .next()
            .map(|&(_, task)| task)
    }

    /// Wake exactly `task`'s unkeyed registration ([`WakeKey::NONE`]) if it is
    /// currently waiting, returning whether it was.
    pub fn wake_task(&self, arch: &dyn WaitQueueArch, task: TaskId) -> bool {
        self.wake_waiter(arch, WakeKey::NONE, task)
    }

    /// Wake exactly the waiter `(key, task)` if it is currently registered,
    /// returning whether it was (the wake-one discipline — an addressed event
    /// such as a posted request or a ticket's reply wakes its one target,
    /// never the whole queue; a wake-all there is a thundering herd that
    /// keeps unrelated tasks runnable and distorts the load census).
    /// O(log n).
    ///
    /// An unregistered target is a benign no-op returning `false`: by the
    /// register-before-poll discipline every waiter registers *before* its
    /// first poll and stays registered until it is done, so a target absent
    /// from the queue is running and will observe the event on its own next
    /// poll. The `unpark` runs after the lock is released, exactly as
    /// [`Self::wake_all`].
    pub fn wake_waiter(&self, arch: &dyn WaitQueueArch, key: WakeKey, task: TaskId) -> bool {
        let registered = self.waiters.lock().by_waiter.contains_key(&(key, task));
        if registered {
            arch.unpark(task);
        }
        registered
    }

    /// Wake every waiter whose finite deadline is at or before `now_ns`
    /// (the timed wake). A [`NO_DEADLINE`] waiter is never released here.
    ///
    /// The deadline index is ordered, so only the already-expired prefix is
    /// visited — O(log n + woken), not a scan of every waiter on every timer
    /// expiry. The expired ids are collected under the lock and `unpark`ed
    /// after it is dropped, so the scheduler's locks are never taken while
    /// holding the wait-queue lock.
    ///
    /// A fired deadline is **consumed** here: its entry is removed from the
    /// deadline index and the waiter's stored deadline is reset to
    /// [`NO_DEADLINE`], while the waiter keeps its FIFO slot in `order` /
    /// `by_waiter` (so the register-before-retest lost-wake discipline holds and
    /// an edge [`Self::wake_all`] still finds it). Consuming it is what makes
    /// the timed wake single-shot per registration. Leaving the entry in place
    /// — relying on the woken waiter to deregister — pins the timer one-shot in
    /// the past forever when that waiter is instead released by another path
    /// (an edge wake) or exits without re-parking: `earliest_deadline` then
    /// keeps returning an already-elapsed time, the one-shot re-arms in the
    /// past and fires immediately, and the dispatch loop spins without ever
    /// idling — starving the console-transmit drain until the lockup watchdog
    /// trips. A waiter that is still blocked simply re-`register`s with a fresh
    /// deadline on its next park.
    pub fn sweep(&self, arch: &dyn WaitQueueArch, now_ns: u64) {
        let ids: Vec<TaskId> = {
            let mut set = self.waiters.lock();
            let expired: Vec<(u64, u64)> = set
                .deadlines
                .range(..=(now_ns, u64::MAX))
                .map(|(&key, _)| key)
                .collect();
            let mut ids = Vec::with_capacity(expired.len());
            for key in expired {
                if let Some(id) = set.deadlines.remove(&key) {
                    if let Some(waiter) = set.by_waiter.get_mut(&id) {
                        waiter.deadline_ns = NO_DEADLINE;
                    }
                    ids.push(id.1);
                }
            }
            ids
        };
        for id in ids {
            arch.unpark(id);
        }
    }

    /// The soonest finite deadline among current waiters, or `None` if the
    /// queue is empty or every waiter is [`NO_DEADLINE`]. This is the value
    /// the timed-wake one-shot is armed to (the nearest armed wakeup).
    /// O(log n) — the front of the ordered deadline index.
    #[must_use]
    pub fn earliest_deadline(&self) -> Option<u64> {
        self.waiters
            .lock()
            .deadlines
            .keys()
            .next()
            .map(|&(deadline, _seq)| deadline)
    }

    /// `true` if no task is currently waiting. Diagnostic / test observer.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.waiters.lock().by_waiter.is_empty()
    }
}

impl Default for WaitQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// The boot-installed [`WaitQueueArch`] adapter (set-once per boot).
#[cfg(not(test))]
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
    #[cfg(test)]
    {
        // The unit-test binary runs many independent boots in one process, so
        // each exercises the same set-once publication through a cell of its
        // own rather than contaminating the next boot's view (the same
        // treatment `crate::cpu_state::install` gives its table). A test that
        // needs a live hook claims one for itself (`crate::test_boot`).
        OnceCell::new()
            .set(arch)
            .map_err(|_| WaitArchAlreadyInstalled)
    }
    #[cfg(not(test))]
    {
        WAIT_ARCH.set(arch).map_err(|_| WaitArchAlreadyInstalled)
    }
}

/// The installed [`WaitQueueArch`], or `None` before a hook is published.
#[must_use]
pub fn wait_arch() -> Option<&'static (dyn WaitQueueArch + 'static)> {
    #[cfg(test)]
    {
        crate::test_boot::claimed_wait_arch()
    }
    #[cfg(not(test))]
    {
        WAIT_ARCH.get().ok().flatten().copied()
    }
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
///
/// This broadcast is the **fallback** for an endpoint whose server has not
/// yet recorded its scheduler id (never received); a post to an endpoint
/// with a recorded server uses the targeted [`serve_wake_task`] instead, so
/// unrelated parked servers stay parked (wake-one, not a thundering herd).
pub fn serve_wake() {
    if let Some(arch) = wait_arch() {
        SERVE_WAITQ.wake_all(arch);
    }
}

/// Wake exactly the IPC server `task` parked on [`SERVE_WAITQ`] because a
/// request was posted to *its* endpoint (the endpoint recorded its server's
/// scheduler id at first receive). A server that is not parked is running
/// and will drain the request on its own next poll, so the miss is benign.
/// A fail-safe no-op before the arch hook is installed.
pub fn serve_wake_task(task: TaskId) {
    if let Some(arch) = wait_arch() {
        let _ = SERVE_WAITQ.wake_task(arch, task);
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

/// The wait-queue holding every byte-stream reader and writer blocked on an
/// empty or full ring — pipes (`plans/SPAWN.md` SP10) and pseudo-terminals
/// (`plans/PTY.md`) alike. A reader whose ring is momentarily empty (with a
/// live peer) and a writer whose ring is momentarily full (with a live peer)
/// park here off the run queue (**no** busy yield) and are woken by
/// [`stream_wake`] when *their* ring produces bytes, frees space, or closes
/// terminally (EOF / broken pipe). Each woken task re-runs its non-blocking
/// step ([`crate::pipe::PipeEnd::try_read`] /
/// [`crate::pipe::PipeEnd::try_write`] and the pty equivalents) and either
/// progresses or parks again; the check-then-park race is closed by the
/// scheduler's wake-pending token (the same interlock `wait`/`irq_wait` use).
///
/// Every waiter is registered under the [`WakeKey`] of the one ring side it
/// blocked on ([`crate::pipe::RingWaits`]), so a chunk moved on one stream
/// wakes only that stream's waiters. One queue for every stream is what keeps
/// a timed `stream_read` on the single deadline index the timed sweep and
/// [`nearest_timed_deadline`] already fold over, rather than the per-key
/// queues [`crate::futex`] holds — whose table every sweep and every arming
/// has to scan, a cost a futex key (a bare user address, with no kernel object
/// to hang a queue on) has no way to avoid.
pub static STREAM_WAITQ: WaitQueue = WaitQueue::new();

/// Wake the tasks parked on the stream ring side `key` because *that* ring's
/// condition changed (bytes arrived, space freed, or its peer side closed);
/// each re-runs its step and either progresses or parks again. A fail-safe
/// no-op before the arch hook is installed.
pub fn stream_wake(key: WakeKey) {
    if let Some(arch) = wait_arch() {
        let _ = STREAM_WAITQ.wake_key(arch, key);
    }
}

/// The wait-queue holding `waitset_wait` callers whose set observes their
/// own **signal intake** (`plans/STRESSTEST.md` ST3 — the
/// `WaitSourceKind::Signal` member). A process that opted into signal
/// observation (`signal_intake`) parks here off the run queue (**no** busy
/// yield) until a termination-request signal is recorded as its pending
/// observable event; it is woken by [`signal_intake_wake`] with a
/// **targeted** wake (only its own intake can ever concern it, so
/// unrelated waiters sleep on — wake-one, never a thundering herd). The
/// woken owner's scan re-peeks its intake and drains through
/// `signal_intake(Take)`; the check-then-park race is closed by the
/// scheduler's wake-pending token, exactly as the sibling queues rely on.
/// Joined only by a set that actually holds a `Signal` member, so signal
/// traffic never touches an unrelated waitset waiter.
pub static SIGNAL_INTAKE_WAITQ: WaitQueue = WaitQueue::new();

/// Wake exactly the opted-in task `task` parked on [`SIGNAL_INTAKE_WAITQ`]
/// because a termination-request signal was just recorded as its pending
/// observable event. A target that is not parked is running and will
/// observe the pending signal on its own next wait/drain, so the miss is
/// benign. Runs in dispatcher/task context (the signal producer), never an
/// ISR, so the unpark is direct. A fail-safe no-op before the arch hook is
/// installed.
pub fn signal_intake_wake(task: TaskId) {
    if let Some(arch) = wait_arch() {
        let _ = SIGNAL_INTAKE_WAITQ.wake_task(arch, task);
    }
}

/// The wait-queue holding `irq_wait` callers (Design D — the user-space
/// device-driver IRQ path). A task that bound an IRQ line with `irq_bind`
/// and called `irq_wait` parks here off the run queue (no busy yield) and is woken by [`irq_wake`] the instant the device-IRQ
/// dispatch path runs [`tairix_kernel_irq::IrqTable::fire`] for *any* line,
/// or, with a finite timeout, by the timed [`WaitQueue::sweep`] below. Each
/// woken waiter re-checks its own bound line's ready flag through
/// [`tairix_kernel_irq::IrqTable::try_wait_step`] and either returns or
/// parks again, so a fire for a different line is a harmless spurious wake and the check-then-park race is closed by the
/// scheduler's wake-pending token (the same interlock `hw_tree_wait` uses).
pub static IRQ_WAITQ: WaitQueue = WaitQueue::new();

/// Request a wake of every parked `irq_wait` caller because a bound IRQ
/// line fired; each woken waiter re-checks its own line and either returns
/// [`Ready`] or parks again, so a fire for a different line is a harmless
/// spurious wake.
///
/// Called from the production device-IRQ dispatch path immediately after
/// [`tairix_kernel_irq::IrqTable::fire`] sets the per-line ready flag, so
/// it is **lock-free**: it only flags the queue
/// ([`WaitQueue::request_wake`]) and is safe to call from the device-IRQ
/// dispatcher while a task it interrupted holds the wait-queue or
/// scheduler locks. The real `unpark` runs at the next
/// dispatcher-context [`drain_pending_wakes`]; mask-before-wake still holds
/// because `fire` masked the line and set `ready` *before* this flag, and
/// the drain's `unpark` re-readies the waiter that then consumes `ready`.
///
/// [`Ready`]: tairix_kernel_irq::WaitOutcome::Ready
pub fn irq_wake() {
    IRQ_WAITQ.request_wake();
}

/// The wait-queue holding every task parked on a
/// [`WaitSourceKind::MemoryPressure`](tairix_abi::WaitSourceKind::MemoryPressure)
/// wait-set member.
///
/// There is one band for the whole machine, so one queue holds every
/// watcher; each woken waiter re-checks the band against the one its own
/// member last observed, and a waiter already up to date simply parks
/// again.
pub static PRESSURE_WAITQ: WaitQueue = WaitQueue::new();

/// Request a wake of every memory-pressure watcher because the published
/// band changed.
///
/// Called from the pressure gauge's band-change hook, which fires inside
/// whatever was spending memory at the time — a cache operation, a demand
/// fault, a direct-reclaim sweep, possibly with the frame allocator's own
/// lock held. It is therefore **lock-free**: it only flags the queue
/// ([`WaitQueue::request_wake`]), and the real `unpark` runs later at the
/// next dispatcher-context [`drain_pending_wakes`], exactly like a device
/// IRQ's wake. Taking the wait-queue lock here instead could re-enter a
/// lock the interrupted allocator already holds.
pub fn pressure_wake() {
    PRESSURE_WAITQ.request_wake();
}

/// The wait-queue holding the write-back flusher kthread — the one task that
/// publishes a volume whose open filesystem transaction has aged out
/// (`crate::fs::writeback`).
///
/// A filesystem that batches commits keeps a transaction open for the next
/// operation to join, and between operations nothing in the driver runs, so
/// the age bound is only real if something above it publishes a volume that
/// falls quiet. The flusher registers here with the soonest deadline any
/// mounted volume has published and parks off the run queue; the timed
/// [`WaitQueue::sweep`] releases it when that deadline arrives, and
/// [`writeback_wake`] releases it early when a volume takes on a *sooner*
/// one. A machine with no dirty volume registers [`NO_DEADLINE`], so it arms
/// nothing and takes no wakeup at all.
pub static WRITEBACK_WAITQ: WaitQueue = WaitQueue::new();

/// Request a wake of the write-back flusher because a volume published a
/// write-back deadline **sooner** than the one the flusher is parked on.
///
/// A later deadline needs no wake: the flusher recomputes the soonest
/// deadline every time it runs, so an already-armed earlier wake will pick
/// the new volume up. That is what keeps a sync-heavy workload — which opens
/// and closes a transaction per barrier — from costing a task switch per
/// commit.
///
/// Called from inside a filesystem driver, under the mount lock that
/// serialises it, so it is **lock-free** past the queue's own deadline read:
/// it only flags the queue ([`WaitQueue::request_wake`]) and the real
/// `unpark` runs at the next dispatcher-context [`drain_pending_wakes`].
pub fn writeback_wake(deadline_ns: Option<u64>) {
    let Some(deadline) = deadline_ns else {
        // Nothing to publish: whatever the flusher is armed for still
        // covers it, and it will re-park on `NO_DEADLINE` when it next runs.
        return;
    };
    match WRITEBACK_WAITQ.earliest_deadline() {
        Some(armed) if armed <= deadline => {}
        _ => WRITEBACK_WAITQ.request_wake(),
    }
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

/// The wait-queue holding `spawn` callers whose store-bundle path arrived
/// while the on-disk application store is still *pending* (the boot kthread
/// that publishes the `/System` mount has not reached a terminal state).
/// Woken by [`app_store_wake`] the instant the
/// [`crate::appspawn::AppStore`] readiness latch resolves — available or
/// unavailable — whereupon each waiter re-checks the latch and proceeds or
/// fails closed. The wait is untimed (registered with [`NO_DEADLINE`]): the
/// boot path always resolves the latch, so only an explicit wake releases a
/// waiter, never the timed sweep.
pub static APP_STORE_WAITQ: WaitQueue = WaitQueue::new();

/// Wake every store-bundle `spawn` caller because the application-store
/// readiness latch resolved; each re-checks the latch and either proceeds
/// or fails closed. A fail-safe no-op before the arch hook is installed.
pub fn app_store_wake() {
    if let Some(arch) = wait_arch() {
        APP_STORE_WAITQ.wake_all(arch);
    }
}

/// The wait-queue holding a desktop session parked on a `SeatInput`
/// wait-set member while its seat's keyboard and pointer channels are both
/// empty (`plans/DISPLAY.md` D7a). Only wait-sets that actually contain a
/// `SeatInput` member register here (`waitset_wait` checks the membership
/// first), so the pointer-rate wakes a drag produces never touch an
/// unrelated waitset waiter (no thundering herd). It is woken by
/// [`seat_input_wake`] when a record is routed to a held seat's desktop
/// channel **and** when a lease ends (release, revoke, seat destruction),
/// so a session that lost its seat wakes and observes the typed refusal on
/// its next drain instead of parking forever. Waiters carry their
/// wait-set's own deadline semantics; the check-then-park race is closed by
/// the scheduler's wake-pending token, exactly as the other queues.
pub static SEAT_INPUT_WAITQ: WaitQueue = WaitQueue::new();

/// Wake every desktop session parked on a `SeatInput` wait-set member
/// because a record was routed to a held seat's desktop channel or a seat
/// lease ended; each re-scans its members and parks again when nothing is
/// ready. A fail-safe no-op before the arch hook is installed.
///
/// A broadcast, not a wake-one: the registry does not track which waiter
/// observes which seat, and only seat-input waiters register on this queue,
/// so the blast radius is the (small) set of desktop sessions — one per
/// held seat.
pub fn seat_input_wake() {
    if let Some(arch) = wait_arch() {
        SEAT_INPUT_WAITQ.wake_all(arch);
    }
}

/// The wait-queue holding `ipc_call` callers (Design D D2b). A caller parks
/// here after posting its request to a [`tairix_kernel_ipc::call::CallEndpoint`]
/// and is woken by [`call_wake`] when the bound server replies (no busy yield). `ipc_call` carries no timeout, so every waiter
/// registers with [`NO_DEADLINE`] and is only ever released by an explicit
/// wake, never the timed [`WaitQueue::sweep`].
pub static CALL_WAITQ: WaitQueue = WaitQueue::new();

/// Wake every parked `ipc_call` caller because a [`CallEndpoint`] reply (or
/// cancellation) arrived; each re-checks its ticket and either claims the
/// reply or parks again. A fail-safe no-op before the arch hook is installed.
///
/// This broadcast remains for **cancellation** (endpoint destruction, whose
/// affected callers are not individually known) and as the fallback for a
/// poster that carried no scheduler identity; an ordinary reply uses the
/// targeted [`call_wake_task`] with the poster id the endpoint captured at
/// post time, so unrelated parked callers stay parked (wake-one, not a
/// thundering herd).
///
/// [`CallEndpoint`]: tairix_kernel_ipc::call::CallEndpoint
pub fn call_wake() {
    if let Some(arch) = wait_arch() {
        CALL_WAITQ.wake_all(arch);
    }
}

/// Wake exactly the `ipc_call` caller `task` parked on [`CALL_WAITQ`]
/// because *its* ticket was replied (the poster's scheduler id captured at
/// post time). A caller that is not parked is running and will claim the
/// reply on its own next poll, so the miss is benign. A fail-safe no-op
/// before the arch hook is installed.
pub fn call_wake_task(task: TaskId) {
    if let Some(arch) = wait_arch() {
        let _ = CALL_WAITQ.wake_task(arch, task);
    }
}

/// The wait-queue holding senders parked on a `PortRoom` wait-set member —
/// a task that has a message a full mailbox refused and waits for the
/// receiver to free a slot rather than dropping the message or polling for
/// capacity. Room carries no deadline of its own, so every waiter registers
/// with [`NO_DEADLINE`] and is released only by an explicit wake (the
/// wait-set's own timeout still bounds the wait through `IRQ_WAITQ`).
///
/// Joined only by a wait-set that actually holds a `PortRoom` member, so
/// ordinary mailbox traffic never disturbs a waiter that did not ask about
/// room.
pub static PORT_ROOM_WAITQ: WaitQueue = WaitQueue::new();

/// Wake every parked sender because a port whose room they may be waiting
/// on was torn down; each re-scans and observes that its destination has
/// gone (the peek reports a vanished port ready, so the woken sender fails
/// its send closed instead of parking on a mailbox that can never drain).
/// A fail-safe no-op before the arch hook is installed.
///
/// A broadcast, because a destroyed port takes its own record of who was
/// waiting with it; the blast radius is the small set of senders currently
/// holding an undeliverable message. Every ordinary drain uses the targeted
/// [`port_room_wake_task`] instead, so a busy mailbox never wakes an
/// unrelated waiter.
pub fn port_room_wake() {
    if let Some(arch) = wait_arch() {
        PORT_ROOM_WAITQ.wake_all(arch);
    }
}

/// Wake exactly the sender `task` parked on [`PORT_ROOM_WAITQ`] because the
/// mailbox *it* is waiting on freed a slot (the port records its room
/// waiters, so the drain names them). A sender that is not parked is
/// running and will retry on its own, so the miss is benign. A fail-safe
/// no-op before the arch hook is installed.
pub fn port_room_wake_task(task: TaskId) {
    if let Some(arch) = wait_arch() {
        let _ = PORT_ROOM_WAITQ.wake_task(arch, task);
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
    STREAM_WAITQ.sweep(arch, now);
    // `CALL_WAITQ` holds callers awaiting a reply. Most register with
    // `NO_DEADLINE` (`ipc_call`/`call_recv` — released only by the reply or
    // teardown, so the sweep never touches them), but the async `call_post`
    // path registers a finite per-request deadline: sweeping here releases a
    // caller whose device wedged so its `call_reap` observes the timeout,
    // rather than parking it forever.
    CALL_WAITQ.sweep(arch, now);
    // The write-back flusher's deadline is the soonest a mounted volume's
    // open transaction ages out, so the sweep is what turns the batching
    // window into a real bound on how stale a quiet volume may be.
    WRITEBACK_WAITQ.sweep(arch, now);
    // The futex queues are per-key and created on demand, so they are swept
    // through their own module rather than named here (`plans/THREADS.md`
    // decision 5): a timed `futex_wait` is released exactly like any other
    // timed wait.
    crate::futex::sweep(arch, now);
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
    // Edge wakes flagged from a context that could not take a lock: a
    // device IRQ, a UART receive, or the pressure gauge's band change
    // inside an allocation path.
    if CONSOLE_WAITQ.take_wake_pending() {
        CONSOLE_WAITQ.wake_all(arch);
        woke = true;
    }
    if IRQ_WAITQ.take_wake_pending() {
        IRQ_WAITQ.wake_all(arch);
        woke = true;
    }
    if PRESSURE_WAITQ.take_wake_pending() {
        PRESSURE_WAITQ.wake_all(arch);
        woke = true;
    }
    // A volume took on a sooner write-back deadline than the flusher is
    // armed for, flagged from inside the driver under its mount lock.
    if WRITEBACK_WAITQ.take_wake_pending() {
        WRITEBACK_WAITQ.wake_all(arch);
        woke = true;
    }
    // Deadline sweep flagged by the timer one-shot.
    if TIMED_SWEEP_PENDING.swap(false, Ordering::AcqRel) {
        run_timed_sweep(arch);
        woke = true;
    }
    woke
}

/// Non-consuming peek: whether an *edge-flagged* interrupt-context
/// deferred wake (console RX, a device-IRQ [`irq_wake`], or a
/// memory-pressure band change [`pressure_wake`]) is awaiting its
/// dispatcher-context [`drain_pending_wakes`].
///
/// The preemption gate consults this so a timer tick on a CPU whose only
/// task is the one about to be preempted still reschedules when a wake is
/// owed — the woken task must reach `drain_pending_wakes`, which only runs
/// after the dispatch loop regains control. Without it, gating the tick
/// purely on "is there another runnable task" would strand a just-flagged
/// device wake until the next tick (or forever, on a lone-task CPU).
///
/// It deliberately excludes the timed-sweep-pending flag: the per-tick
/// timer callback sets that flag on **every** fired one-shot, so it is set
/// again the instant after each drain and would make the gate perpetually
/// true — defeating the whole point. Whether a timed sweep genuinely owes
/// a reschedule is answered by [`timed_wake_due`] (a deadline has actually
/// elapsed), not by the flag alone.
#[must_use]
pub fn has_pending_deferred_wake() -> bool {
    CONSOLE_WAITQ.wake_is_pending()
        || IRQ_WAITQ.wake_is_pending()
        || PRESSURE_WAITQ.wake_is_pending()
        || WRITEBACK_WAITQ.wake_is_pending()
}

/// Whether a timed waiter's finite deadline has already elapsed, so the
/// dispatcher-context timed sweep ([`drain_pending_wakes`]) owes it a wake.
///
/// The preemption gate consults this: a fired quantum tick on a lone-task
/// CPU must still reschedule when a sleeping task's timeout has come due
/// (the sweep — which releases it and makes it a competitor — only runs
/// once the dispatch loop regains control). A timed waiter whose deadline
/// is still in the future does **not** owe a reschedule, so a lone task
/// keeps running until the deadline actually arrives. `false` before the
/// arch clock hook is installed (nothing can be parked with a deadline).
#[must_use]
pub fn timed_wake_due() -> bool {
    match (nearest_timed_deadline(), wait_now_ns()) {
        (Some(deadline), Some(now)) => deadline <= now,
        _ => false,
    }
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
/// (`HW_TREE_WAITQ`, `IRQ_WAITQ`, `CONSOLE_WAITQ`, `USERS_DB_WAITQ`,
/// `STREAM_WAITQ`, `CALL_WAITQ`, `WRITEBACK_WAITQ`, and the per-key futex
/// queues), or [`None`] if none has one. A park site arms the one-shot to
/// this so registering a *later* deadline never delays an already-pending
/// earlier wake.
#[must_use]
pub fn nearest_timed_deadline() -> Option<u64> {
    [
        HW_TREE_WAITQ.earliest_deadline(),
        IRQ_WAITQ.earliest_deadline(),
        CONSOLE_WAITQ.earliest_deadline(),
        USERS_DB_WAITQ.earliest_deadline(),
        STREAM_WAITQ.earliest_deadline(),
        // A finite per-request deadline armed by `call_post` (the async
        // block transport). Infinite (`ipc_call`) registrations arm nothing.
        CALL_WAITQ.earliest_deadline(),
        // A timed `futex_wait` — a condition variable's bounded wait — over
        // the per-key queues created on demand.
        crate::futex::earliest_deadline(),
        // The soonest write-back deadline any mounted volume published.
        WRITEBACK_WAITQ.earliest_deadline(),
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
    fn wake_one_preserves_fifo_registration_order() {
        let arch = MockArch::new();
        let q = WaitQueue::new();
        q.register(7, NO_DEADLINE);
        q.register(3, NO_DEADLINE);

        assert!(q.wake_one(&arch));
        assert_eq!(*arch.unparked.borrow(), alloc::vec![7]);
        q.deregister(7);
        assert!(q.wake_one(&arch));
        assert_eq!(*arch.unparked.borrow(), alloc::vec![7, 3]);
        q.deregister(3);
        assert!(!q.wake_one(&arch));
    }

    #[test]
    fn wake_task_unparks_only_the_named_registered_waiter() {
        let arch = MockArch::new();
        let q = WaitQueue::new();
        q.register(1, NO_DEADLINE);
        q.register(2, NO_DEADLINE);
        // The addressed wake releases its one target; the other waiter
        // stays parked (wake-one, never a thundering herd).
        assert!(q.wake_task(&arch, 2));
        assert_eq!(*arch.unparked.borrow(), alloc::vec![2]);
        // An unregistered target is a benign no-op: the task is running
        // and will observe the event on its own next poll.
        assert!(!q.wake_task(&arch, 9));
        assert_eq!(*arch.unparked.borrow(), alloc::vec![2]);
    }

    /// The keyed wake is what keeps one shared queue from being a
    /// machine-wide broadcast: an event on one object releases that object's
    /// waiters and leaves every other object's parked.
    #[test]
    fn wake_key_releases_one_conditions_waiters_and_no_others() {
        let arch = MockArch::new();
        let q = WaitQueue::new();
        let (mine, theirs) = (WakeKey::new(1), WakeKey::new(2));
        q.register_keyed(mine, 1, NO_DEADLINE);
        q.register_keyed(mine, 2, NO_DEADLINE);
        q.register_keyed(theirs, 3, NO_DEADLINE);

        assert_eq!(q.wake_key(&arch, mine), 2);
        let mut got = arch.unparked.borrow().clone();
        got.sort_unstable();
        assert_eq!(got, alloc::vec![1, 2], "the other condition stayed parked");

        // A key nobody waits on wakes nobody, and the queue-wide broadcast
        // still reaches every key.
        assert_eq!(q.wake_key(&arch, WakeKey::new(99)), 0);
        arch.unparked.borrow_mut().clear();
        q.wake_all(&arch);
        let mut got = arch.unparked.borrow().clone();
        got.sort_unstable();
        assert_eq!(got, alloc::vec![1, 2, 3]);
    }

    /// A key scopes membership as well as the wake: the same task waiting on
    /// two conditions holds two registrations, each addressable and removable
    /// on its own, and the unkeyed forms are just the [`WakeKey::NONE`] one.
    #[test]
    fn a_registration_is_the_task_and_its_key_together() {
        let arch = MockArch::new();
        let q = WaitQueue::new();
        let (a, b) = (WakeKey::new(11), WakeKey::new(12));
        q.register_keyed(a, 5, NO_DEADLINE);
        q.register_keyed(b, 5, NO_DEADLINE);
        q.register(5, NO_DEADLINE);

        assert!(q.wake_waiter(&arch, a, 5));
        assert!(q.wake_waiter(&arch, b, 5));
        assert!(q.wake_task(&arch, 5), "the unkeyed registration is its own");
        assert!(!q.wake_waiter(&arch, WakeKey::new(13), 5));

        q.deregister_keyed(a, 5);
        assert!(!q.wake_waiter(&arch, a, 5), "only that key was released");
        assert!(q.wake_waiter(&arch, b, 5));
        assert!(q.wake_task(&arch, 5));
        q.deregister_keyed(b, 5);
        q.deregister(5);
        assert!(q.is_empty());
    }

    /// A keyed waiter's finite deadline joins the one deadline index every
    /// timed wait shares, so a timed `stream_read` needs no per-object queue
    /// for the sweep to find it.
    #[test]
    fn a_keyed_waiter_is_swept_by_its_deadline_like_any_other() {
        let arch = MockArch::new();
        let q = WaitQueue::new();
        let key = WakeKey::new(21);
        q.register_keyed(key, 8, 400);
        assert_eq!(q.earliest_deadline(), Some(400));
        q.sweep(&arch, 399);
        assert!(arch.unparked.borrow().is_empty(), "not yet due");
        q.sweep(&arch, 400);
        assert_eq!(*arch.unparked.borrow(), alloc::vec![8]);
        // The fired deadline is consumed but the waiter keeps its place, so an
        // edge wake on its key still finds it.
        assert_eq!(q.earliest_deadline(), None);
        assert_eq!(q.wake_key(&arch, key), 1);
        q.deregister_keyed(key, 8);
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
    fn sweep_consumes_the_fired_deadline_so_it_cannot_re_arm_in_the_past() {
        // A fired timed deadline must be consumed by the sweep, not left in
        // the index for the woken waiter to clear. A waiter released by
        // timeout but then woken/retired by another path (or that exits)
        // never re-parks to deregister; if `sweep` left its entry,
        // `earliest_deadline` would keep returning an already-elapsed time,
        // the timer one-shot would re-arm in the past and fire immediately,
        // and the dispatch loop would spin without ever idling — the Pi 4
        // console-starving hard-lockup this regresses.
        let arch = MockArch::new();
        let q = WaitQueue::new();
        q.register(1, 100);
        // The first sweep past the deadline releases the waiter exactly once.
        q.sweep(&arch, 150);
        assert_eq!(*arch.unparked.borrow(), alloc::vec![1]);
        // The deadline is consumed: nothing is left to perpetually re-arm the
        // one-shot in the past.
        assert_eq!(
            q.earliest_deadline(),
            None,
            "the fired deadline is removed from the index"
        );
        // A second sweep (the waiter never re-parked) releases nobody — no
        // stale entry, so no perpetual re-arm and no dispatch-loop spin.
        q.sweep(&arch, 200);
        assert_eq!(
            *arch.unparked.borrow(),
            alloc::vec![1],
            "a consumed deadline is not swept again"
        );
        // The waiter keeps its FIFO slot (register-before-retest / edge wakes).
        assert!(!q.is_empty(), "the waiter itself stays registered");
        assert_eq!(q.oldest_task(), Some(1));
        // On its next park it re-registers a fresh deadline cleanly.
        q.register(1, 500);
        assert_eq!(q.earliest_deadline(), Some(500));
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
    fn re_registration_preserves_fifo_position() {
        // A waiter that re-registers (re-arming after a spurious wake) keeps
        // its place in line: the older task is still the FIFO head, never
        // overtaken by a task that arrived later — the stated no-starvation
        // guarantee.
        let arch = MockArch::new();
        let q = WaitQueue::new();
        q.register(7, NO_DEADLINE);
        q.register(3, NO_DEADLINE);
        // 7 re-arms with a new (finite) deadline; its FIFO seq is retained.
        q.register(7, 500);
        assert_eq!(q.oldest_task(), Some(7), "re-register keeps FIFO head");
        assert!(q.wake_one(&arch));
        assert_eq!(*arch.unparked.borrow(), alloc::vec![7]);
    }

    #[test]
    fn re_registration_re_indexes_the_deadline() {
        // Updating a present waiter's deadline moves it in the ordered
        // deadline index rather than leaving a stale entry behind.
        let q = WaitQueue::new();
        q.register(1, 900);
        assert_eq!(q.earliest_deadline(), Some(900));
        // Tighten it, then relax it: the index always reflects the current
        // value, with no duplicate stale (900, _) entry lingering.
        q.register(1, 300);
        assert_eq!(q.earliest_deadline(), Some(300));
        q.register(1, NO_DEADLINE);
        assert_eq!(
            q.earliest_deadline(),
            None,
            "relaxing to no-deadline clears the arming"
        );
        assert!(!q.is_empty(), "the waiter itself is still registered");
    }

    #[test]
    fn sweep_visits_only_the_expired_prefix_in_deadline_order() {
        let arch = MockArch::new();
        let q = WaitQueue::new();
        q.register(1, 300);
        q.register(2, 100);
        q.register(3, 200);
        q.register(4, NO_DEADLINE);
        q.sweep(&arch, 250);
        // Exactly the finite deadlines at or before 250, in ascending
        // deadline order (100 then 200); 300 and the untimed waiter stay.
        assert_eq!(*arch.unparked.borrow(), alloc::vec![2, 3]);
    }

    /// The pressure gauge's band-change hook must be usable from inside
    /// an allocation path, so it only *flags* the queue; the flag is what
    /// the preemption gate sees and what the dispatcher-context drain
    /// consumes.
    ///
    /// Every assertion here is monotone in the shared flag (set, then
    /// observe set), never "observe clear": the flag is process-global
    /// and the test binary runs concurrently, so asserting it is clear
    /// would be a race, not a test.
    #[test]
    fn a_pressure_band_change_flags_a_deferred_wake_without_unparking() {
        pressure_wake();

        assert!(
            PRESSURE_WAITQ.wake_is_pending(),
            "the band change owes a wake"
        );
        assert!(
            has_pending_deferred_wake(),
            "a lone-task CPU must still reschedule so the drain can run"
        );
        assert!(
            PRESSURE_WAITQ.take_wake_pending(),
            "the drain consumes the owed wake"
        );
    }

    /// The one-shot flag semantics the pressure hook relies on, proved on
    /// a private queue so no other test's band change can perturb it: a
    /// wake requested once is reported once, and requesting it does not
    /// itself unpark anybody.
    #[test]
    fn a_flagged_wake_is_reported_exactly_once() {
        let arch = MockArch::new();
        let q = WaitQueue::new();
        q.register(11, NO_DEADLINE);

        q.request_wake();
        assert!(
            arch.unparked.borrow().is_empty(),
            "flagging must not unpark from the flagging context"
        );
        assert!(q.take_wake_pending());
        assert!(!q.take_wake_pending(), "one shot");

        q.wake_all(&arch);
        assert_eq!(*arch.unparked.borrow(), alloc::vec![11]);
    }

    #[test]
    fn deregister_removes_from_every_index() {
        let arch = MockArch::new();
        let q = WaitQueue::new();
        q.register(1, 100);
        q.register(2, 200);
        q.deregister(1);
        // Gone from the deadline index (earliest is now 2's), from the FIFO
        // order (oldest is now 2), and from membership.
        assert_eq!(q.earliest_deadline(), Some(200));
        assert_eq!(q.oldest_task(), Some(2));
        assert!(!q.wake_task(&arch, 1), "no longer a member");
        assert!(q.wake_task(&arch, 2));
    }

    #[test]
    fn wake_one_round_robins_without_starving_under_repeated_contention() {
        // Model the FIFO service loop a wake-one consumer drives: wake the
        // head, it resumes and deregisters, the next-oldest becomes head.
        // Every waiter is served exactly once, in arrival order — no task is
        // starved however many rounds run.
        let arch = MockArch::new();
        let q = WaitQueue::new();
        for id in [10, 20, 30, 40] {
            q.register(id, NO_DEADLINE);
        }
        for _ in 0..4 {
            let head = q.oldest_task().expect("a waiter remains");
            assert!(q.wake_one(&arch));
            q.deregister(head);
        }
        assert!(!q.wake_one(&arch), "queue drained");
        assert_eq!(*arch.unparked.borrow(), alloc::vec![10, 20, 30, 40]);
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
