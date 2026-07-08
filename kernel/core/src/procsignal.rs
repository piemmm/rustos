//! The kernel-side process-signal seam the `signal` (`abi-v1` number 64)
//! syscall uses (`plans/SPAWN.md` SP7).
//!
//! [`ProcessSignal`] is the one object-safe boundary between the
//! arch-neutral syscall handler in `kernel/core` and the scheduler-side
//! producer that delivers a control signal to one of the sender's children.
//! Like the [`ProcessWait`],
//! [`ProcessSpawn`](crate::spawn::ProcessSpawn), and
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

use alloc::collections::BTreeSet;
use core::sync::atomic::{AtomicU64, Ordering};

use rustos_abi::{Errno, Signal};
use rustos_kernel_sched_api::{SchedError, SchedulerArch, SchedulerPolicy};
use rustos_kernel_sec::TaskId;
use rustos_sync::once::OnceCell;
use rustos_sync::SpinLock;

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
        Self { wait, scheduler }
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
    fn terminate(&self, child: TaskId, signal: Signal) -> Result<(), Errno> {
        match self.scheduler.exit(child.0) {
            Ok(()) => {
                // A stopped child can be killed: lift its overlay entry so
                // the set never accumulates entries for dead tasks.
                STOPPED_TASKS.lock().remove(&child.0);
                // `termination_status` is `Some` for every terminating
                // signal (Terminate/Kill/Interrupt); this arm is never
                // reached for Continue or Stop.
                if let Some(status) = signal.termination_status() {
                    self.wait.record_signalled_exit(child, status);
                }
                Ok(())
            }
            Err(_) => Err(Errno::NotFound),
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
            Signal::Terminate | Signal::Kill | Signal::Interrupt => self.terminate(child, signal),
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
            Signal::Interrupt => self.terminate(target, Signal::Interrupt),
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

    use rustos_abi::{WaitFlags, WaitStatus};
    use rustos_kernel_sched_api::{Priority, SchedulerConfig, TaskAction};

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
            wait.wait(TaskId(7), rustos_abi::WAIT_PID_ANY, WaitFlags::empty()),
            Ok(WaitedChild {
                pid,
                status: WaitStatus::Exited(143)
            })
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
            wait.wait(TaskId(7), rustos_abi::WAIT_PID_ANY, WaitFlags::empty()),
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
            wait.wait(TaskId(7), rustos_abi::WAIT_PID_ANY, WaitFlags::empty()),
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
                rustos_abi::WAIT_PID_ANY,
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
                rustos_abi::WAIT_PID_ANY,
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
            wait.wait(TaskId(7), rustos_abi::WAIT_PID_ANY, WaitFlags::STOPPED),
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
            wait.wait(TaskId(7), rustos_abi::WAIT_PID_ANY, WaitFlags::empty()),
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
