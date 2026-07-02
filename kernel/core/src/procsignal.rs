//! The kernel-side process-signal seam the `signal` (`abi-v1` number 64)
//! syscall uses (`plans/SPAWN.md` SP7).
//!
//! [`ProcessSignal`] is the one object-safe boundary between the
//! arch-neutral syscall handler in `kernel/core` and the scheduler-side
//! producer that delivers a control signal to one of the sender's children.
//! Like the [`ProcessWait`](crate::procwait::ProcessWait),
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

use rustos_abi::{Errno, Signal};
use rustos_kernel_sched_api::{SchedError, SchedulerArch, SchedulerPolicy};
use rustos_kernel_sec::TaskId;

use crate::procwait::KernelProcessWait;

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
/// * [`Signal::Continue`] resumes a stopped child ([`SchedulerPolicy::unpark`]);
/// * [`Signal::Terminate`] / [`Signal::Kill`] terminate the child
///   ([`SchedulerPolicy::exit`]) and record the signal's `128 + n`
///   termination status so the parent's `wait` reaps it.
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
        match self.scheduler.unpark(child.0) {
            Ok(()) | Err(SchedError::InvalidState) => Ok(()),
            Err(_) => Err(Errno::NotFound),
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
                // `termination_status` is `Some` for every terminating signal
                // (Terminate/Kill); this arm is never reached for Continue.
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
            Signal::Terminate | Signal::Kill => self.terminate(child, signal),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::boxed::Box;

    use rustos_kernel_sched_api::{Priority, SchedulerConfig, TaskAction};

    use crate::procwait::{ProcessWait, ReapedChild};
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
        for signal in [Signal::Continue, Signal::Terminate, Signal::Kill] {
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
        // ... and reaps with Terminate's 128 + 2 = 130 status, exactly as if
        // it had exited with that code itself.
        let pid = u32::try_from(child).expect("host task id fits u32");
        assert_eq!(
            wait.wait(TaskId(7), rustos_abi::WAIT_PID_ANY),
            Ok(ReapedChild { pid, code: 130 })
        );
    }

    #[test]
    fn kill_records_its_own_status() {
        let (wait, scheduler) = scaffold();
        let (child, child_pid) = spawn_child(scheduler);
        wait.register_child(TaskId(7), TaskId(child));
        let signaller = KernelProcessSignal::new(wait, scheduler);

        assert_eq!(signaller.signal(TaskId(7), child_pid, Signal::Kill), Ok(()));
        let pid = u32::try_from(child).expect("host task id fits u32");
        // Kill (3) surfaces as 128 + 3 = 131, distinct from Terminate.
        assert_eq!(
            wait.wait(TaskId(7), rustos_abi::WAIT_PID_ANY),
            Ok(ReapedChild { pid, code: 131 })
        );
    }

    #[test]
    fn continue_of_a_running_child_is_a_harmless_success() {
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
