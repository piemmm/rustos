//! The park/unpark handshake, and the settle after a body returns, every
//! policy shares.
//!
//! Parking is the task lifecycle, not scheduling policy: which CPU a woken
//! task lands on differs per policy, but the window between "decide to park"
//! and "actually parked" — and the token that closes it — is identical, as is
//! how a returning body's request is reconciled with whatever a remote park,
//! wake or kill did while it ran. One definition here, so the three policies
//! carry only their own placement.

use core::sync::atomic::{fence, AtomicBool, Ordering};

use crate::{CpuId, SchedError, SchedResult, SchedulerArch, TaskAction, TaskState};

/// The lifecycle state and wake token the handshake needs from a policy's own
/// per-task record.
pub trait ParkableTask {
    /// The task's current lifecycle state.
    fn load_state(&self) -> TaskState;

    /// Compare-exchange the state, reporting the observed value on failure.
    fn cas_state(&self, expected: TaskState, new: TaskState) -> Result<(), TaskState>;

    /// Record that a wake arrived before the task committed to park, so the
    /// next park is cancelled.
    fn set_wake_pending(&self);

    /// Consume the wake token, reporting whether one was set.
    fn take_wake_pending(&self) -> bool;

    /// Whether the task is in a CPU's competition: ready or running.
    fn is_runnable(&self) -> bool {
        matches!(self.load_state(), TaskState::Ready | TaskState::Running)
    }
}

/// Claim `task` out of [`TaskState::Parked`] and admit it, reporting whether
/// this call was the one that transitioned it.
///
/// The compare-exchange is what keeps the admission single when two wakers —
/// or a waker and the task's own park commit — reach it together.
fn claim_parked<T, F>(task: &T, admit: F) -> bool
where
    T: ParkableTask + ?Sized,
    F: FnOnce(&T),
{
    if task.cas_state(TaskState::Parked, TaskState::Ready).is_err() {
        return false;
    }
    admit(task);
    true
}

/// Make `task` runnable, admitting it through `admit` when this call is the
/// one that claimed it out of [`TaskState::Parked`].
///
/// Cancellation-safe: a wake of a task that has not yet committed to park
/// records a token rather than erroring, so it is never lost.
///
/// # Errors
/// [`SchedError::InvalidState`] only when the task is terminal, which is the
/// one answer meaning it can never run again. A wake that another waker (or
/// the task's own park commit) already satisfied is `Ok` — the task is
/// runnable, which is what the wake asked for. Reading that as a failure is
/// what let a wait-queue ownership handoff mistake a live waiter for a corpse
/// and delete its registration.
pub fn unpark_task<T, F>(task: &T, admit: F) -> SchedResult<()>
where
    T: ParkableTask + ?Sized,
    F: FnOnce(&T),
{
    match task.load_state() {
        TaskState::Exited => Err(SchedError::InvalidState),
        // Already committed to park: claim it directly.
        TaskState::Parked => {
            if claim_parked(task, admit) {
                return Ok(());
            }
            match task.load_state() {
                TaskState::Exited => Err(SchedError::InvalidState),
                _ => Ok(()),
            }
        }
        // Not yet committed (running its body, or already queued). Record the
        // token, then re-read the state: this store-then-load against
        // [`commit_park`]'s store-then-take, fenced on each side, forbids the
        // store-buffering outcome where the waker sees the task not-yet-parked
        // *and* the parker misses the token — one side always observes the
        // other.
        TaskState::Ready | TaskState::Running => {
            task.set_wake_pending();
            fence(Ordering::SeqCst);
            if task.load_state() == TaskState::Parked && task.take_wake_pending() {
                let _ = claim_parked(task, admit);
            }
            Ok(())
        }
    }
}

/// Complete a park the dispatcher has just published, re-admitting the task
/// through `admit` when a wake raced the commit.
///
/// The caller stores [`TaskState::Parked`] itself, together with whatever
/// per-CPU accounting the transition owes, and calls this to consume any token
/// a waker left behind.
pub fn commit_park<T, F>(task: &T, admit: F)
where
    T: ParkableTask + ?Sized,
    F: FnOnce(&T),
{
    fence(Ordering::SeqCst);
    if task.take_wake_pending() {
        let _ = claim_parked(task, admit);
    }
}

/// What a dispatcher owes a task whose body has just returned.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Settled {
    /// The task is [`TaskState::Exited`]: retire it.
    Retire,
    /// The task is [`TaskState::Parked`], by its own request or a remote park:
    /// finish with [`commit_park`].
    Park,
    /// The task yielded and is [`TaskState::Ready`] again: enqueue it.
    Requeue,
    /// A remote park and wake made the task runnable and queued it again while
    /// its body ran, so nothing is owed.
    Readmitted,
}

/// Request a task's termination through its `doomed` mark, reporting whether
/// this call was the first to.
///
/// The caller probes the task's body lock only after this returns. The fence
/// pairs with [`observe_doom`]'s: either the dispatch holding the body reads
/// the mark when its body returns, or the probe finds the body already
/// released. Without the pairing the probe can see the body held while the
/// run reads the mark stale and queues the task again, so a kill reported as
/// deferred to that run is never carried out by it.
#[must_use]
pub fn doom(mark: &AtomicBool) -> bool {
    let first = !mark.swap(true, Ordering::AcqRel);
    fence(Ordering::SeqCst);
    first
}

/// Read a task's `doomed` mark for [`settle`], once its dispatch has released
/// the body lock; the other half of [`doom`]'s pairing.
#[must_use]
pub fn observe_doom(mark: &AtomicBool) -> bool {
    fence(Ordering::SeqCst);
    mark.load(Ordering::Acquire)
}

/// Preempt the CPU executing a task this caller has just doomed, so it reaches
/// its stopping point now: alone on a tickless core it may have no next
/// quantum to stop at.
///
/// `running` is the CPU whose current-task slot names the task. A dispatch
/// publishes that slot before it takes the body lock, but nothing orders the
/// two for a killer that saw only the lock, so the body can be owned while no
/// slot the killer reads names it; every CPU is signalled then, since missing
/// the one running the task leaves it running.
pub fn nudge_doomed<A: SchedulerArch + ?Sized>(arch: &A, running: Option<CpuId>, cpus: u32) {
    match running {
        Some(cpu) => arch.send_ipi(cpu),
        None => (0..cpus).for_each(|cpu| arch.send_ipi(cpu)),
    }
}

/// Move a task whose body just returned `action` out of [`TaskState::Running`].
///
/// Each transition is a compare-exchange from the state it was decided on, so
/// a remote park, wake or kill landing while the body unwinds is honoured
/// rather than overwritten: a plain store there loses the wake, or queues the
/// task a second time. `doomed` — a termination requested while the task ran,
/// read through [`observe_doom`] — wins over anything but its own park, since
/// a task parked inside a syscall holds kernel state only its own unwind can
/// release. `depart` performs a transition out of the ready set together with
/// whatever competing-weight accounting the policy keeps, and returns the
/// transition's result.
pub fn settle<T, D>(task: &T, action: TaskAction, doomed: bool, depart: D) -> Settled
where
    T: ParkableTask + ?Sized,
    D: Fn(&dyn Fn() -> bool) -> bool,
{
    let exit = action == TaskAction::Exit || (doomed && action != TaskAction::Park);
    loop {
        let observed = task.load_state();
        let settled = match observed {
            TaskState::Exited => return Settled::Retire,
            _ if exit => depart(&|| task.cas_state(observed, TaskState::Exited).is_ok())
                .then_some(Settled::Retire),
            TaskState::Ready => return Settled::Readmitted,
            TaskState::Parked => return Settled::Park,
            TaskState::Running if action == TaskAction::Park => depart(&|| {
                task.cas_state(TaskState::Running, TaskState::Parked)
                    .is_ok()
            })
            .then_some(Settled::Park),
            TaskState::Running => task
                .cas_state(TaskState::Running, TaskState::Ready)
                .is_ok()
                .then_some(Settled::Requeue),
        };
        if let Some(settled) = settled {
            return settled;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use core::cell::Cell;

    /// A bare `ParkableTask` with no scheduler behind it: the handshake is
    /// pure state, so the model needs nothing more.
    struct Model {
        state: Cell<TaskState>,
        token: Cell<bool>,
        admits: Cell<usize>,
    }

    impl Model {
        fn new(state: TaskState) -> Self {
            Self {
                state: Cell::new(state),
                token: Cell::new(false),
                admits: Cell::new(0),
            }
        }

        fn admit(&self) {
            self.admits.set(self.admits.get() + 1);
        }
    }

    impl ParkableTask for Model {
        fn load_state(&self) -> TaskState {
            self.state.get()
        }

        fn cas_state(&self, expected: TaskState, new: TaskState) -> Result<(), TaskState> {
            let current = self.state.get();
            if current == expected {
                self.state.set(new);
                Ok(())
            } else {
                Err(current)
            }
        }

        fn set_wake_pending(&self) {
            self.token.set(true);
        }

        fn take_wake_pending(&self) -> bool {
            self.token.replace(false)
        }
    }

    #[test]
    fn only_a_ready_or_running_task_is_runnable() {
        assert!(Model::new(TaskState::Ready).is_runnable());
        assert!(Model::new(TaskState::Running).is_runnable());
        assert!(!Model::new(TaskState::Parked).is_runnable());
        assert!(!Model::new(TaskState::Exited).is_runnable());
    }

    #[test]
    fn a_parked_task_is_claimed_and_admitted_once() {
        let task = Model::new(TaskState::Parked);
        assert_eq!(unpark_task(&task, Model::admit), Ok(()));
        assert_eq!(task.load_state(), TaskState::Ready);
        assert_eq!(task.admits.get(), 1);
        // A second wake finds it runnable: nothing to admit, still success.
        assert_eq!(unpark_task(&task, Model::admit), Ok(()));
        assert_eq!(task.admits.get(), 1);
    }

    #[test]
    fn a_wake_before_the_park_commits_cancels_it() {
        let task = Model::new(TaskState::Running);
        assert_eq!(unpark_task(&task, Model::admit), Ok(()));
        assert!(task.token.get(), "the token is owed to the coming park");
        // The dispatcher publishes the park and consumes the token, so the
        // task is re-admitted rather than slept.
        task.state.set(TaskState::Parked);
        commit_park(&task, Model::admit);
        assert_eq!(task.load_state(), TaskState::Ready);
        assert_eq!(task.admits.get(), 1);
    }

    #[test]
    fn a_park_with_no_token_takes_effect() {
        let task = Model::new(TaskState::Parked);
        commit_park(&task, Model::admit);
        assert_eq!(task.load_state(), TaskState::Parked);
        assert_eq!(task.admits.get(), 0, "no wake was owed");
    }

    #[test]
    fn a_wake_whose_claim_lost_to_another_waker_still_reports_success() {
        // The defect this contract exists for: the task was `Parked` when the
        // state was read and `Ready` by the time the claim ran, because
        // another waker got there first. It is live and runnable, so the wake
        // landed — reporting an error here is what let an ownership handoff
        // delete a live waiter's wait-queue row.
        struct RacingClaim {
            state: Cell<TaskState>,
        }

        impl ParkableTask for RacingClaim {
            fn load_state(&self) -> TaskState {
                self.state.get()
            }

            fn cas_state(&self, _: TaskState, _: TaskState) -> Result<(), TaskState> {
                // Models the concurrent waker winning: by the time the claim
                // runs the task has already been re-readied.
                self.state.set(TaskState::Ready);
                Err(TaskState::Ready)
            }

            fn set_wake_pending(&self) {}

            fn take_wake_pending(&self) -> bool {
                false
            }
        }

        let task = RacingClaim {
            state: Cell::new(TaskState::Parked),
        };
        assert_eq!(
            unpark_task(&task, |_| unreachable!("the claim lost, so no admit")),
            Ok(())
        );
    }

    #[test]
    fn a_wake_whose_claim_lost_to_a_retirement_fails_closed() {
        struct RacingExit {
            state: Cell<TaskState>,
        }

        impl ParkableTask for RacingExit {
            fn load_state(&self) -> TaskState {
                self.state.get()
            }

            fn cas_state(&self, _: TaskState, _: TaskState) -> Result<(), TaskState> {
                self.state.set(TaskState::Exited);
                Err(TaskState::Exited)
            }

            fn set_wake_pending(&self) {}

            fn take_wake_pending(&self) -> bool {
                false
            }
        }

        let task = RacingExit {
            state: Cell::new(TaskState::Parked),
        };
        assert_eq!(
            unpark_task(&task, |_| unreachable!("a corpse is never admitted")),
            Err(SchedError::InvalidState)
        );
    }

    #[test]
    fn a_terminal_task_can_never_run_again() {
        let task = Model::new(TaskState::Exited);
        assert_eq!(
            unpark_task(&task, Model::admit),
            Err(SchedError::InvalidState)
        );
        assert_eq!(task.admits.get(), 0);
    }

    /// Settle `task` after a body returned `action`, counting the departures.
    fn settle_counting(task: &Model, action: TaskAction, doomed: bool) -> (Settled, usize) {
        let departures = Cell::new(0);
        let settled = settle(task, action, doomed, |transition: &dyn Fn() -> bool| {
            let departed = transition();
            if departed {
                departures.set(departures.get() + 1);
            }
            departed
        });
        (settled, departures.get())
    }

    #[test]
    fn a_body_that_owned_its_run_settles_as_it_asked() {
        let task = Model::new(TaskState::Running);
        assert_eq!(
            settle_counting(&task, TaskAction::Yield, false),
            (Settled::Requeue, 0)
        );
        assert_eq!(task.load_state(), TaskState::Ready);

        let task = Model::new(TaskState::Running);
        assert_eq!(
            settle_counting(&task, TaskAction::Park, false),
            (Settled::Park, 1)
        );
        assert_eq!(task.load_state(), TaskState::Parked);

        let task = Model::new(TaskState::Running);
        assert_eq!(
            settle_counting(&task, TaskAction::Exit, false),
            (Settled::Retire, 1)
        );
        assert_eq!(task.load_state(), TaskState::Exited);
    }

    /// The defect this exists for: a remote park and wake while the body
    /// unwound leave the task `Ready` and already queued, and a park or a
    /// yield applied over that either loses the wake or queues it twice.
    #[test]
    fn a_task_readmitted_while_its_body_ran_is_left_as_the_waker_left_it() {
        for action in [TaskAction::Park, TaskAction::Yield] {
            let task = Model::new(TaskState::Ready);
            assert_eq!(
                settle_counting(&task, action, false),
                (Settled::Readmitted, 0),
                "{action:?} over a readmission"
            );
            assert_eq!(task.load_state(), TaskState::Ready);
        }
    }

    #[test]
    fn a_remote_park_while_the_body_ran_is_honoured() {
        let task = Model::new(TaskState::Parked);
        assert_eq!(
            settle_counting(&task, TaskAction::Yield, false),
            (Settled::Park, 0),
            "the parker already took the task out of the competition"
        );
        assert_eq!(task.load_state(), TaskState::Parked);
    }

    #[test]
    fn a_kill_wins_over_everything_but_the_tasks_own_park() {
        let task = Model::new(TaskState::Running);
        assert_eq!(
            settle_counting(&task, TaskAction::Yield, true),
            (Settled::Retire, 1)
        );
        let task = Model::new(TaskState::Running);
        assert_eq!(
            settle_counting(&task, TaskAction::Park, true),
            (Settled::Park, 1),
            "a task parked in a syscall must unwind before it can die"
        );
        // An exit over a readmission retires it and takes the waker's count off.
        let task = Model::new(TaskState::Ready);
        assert_eq!(
            settle_counting(&task, TaskAction::Exit, false),
            (Settled::Retire, 1)
        );
        assert_eq!(task.load_state(), TaskState::Exited);
    }

    /// A compare-exchange that loses to a transition landing between the
    /// read and the exchange is decided again against the new state.
    #[test]
    fn a_transition_that_loses_a_race_is_decided_again() {
        struct ParkedUnderneath {
            state: Cell<TaskState>,
            lost: Cell<bool>,
        }

        impl ParkableTask for ParkedUnderneath {
            fn load_state(&self) -> TaskState {
                self.state.get()
            }

            fn cas_state(&self, expected: TaskState, new: TaskState) -> Result<(), TaskState> {
                if !self.lost.replace(true) {
                    // A remote park lands first.
                    self.state.set(TaskState::Parked);
                    return Err(TaskState::Parked);
                }
                if self.state.get() == expected {
                    self.state.set(new);
                    Ok(())
                } else {
                    Err(self.state.get())
                }
            }

            fn set_wake_pending(&self) {}

            fn take_wake_pending(&self) -> bool {
                false
            }
        }

        let task = ParkedUnderneath {
            state: Cell::new(TaskState::Running),
            lost: Cell::new(false),
        };
        let settled = settle(
            &task,
            TaskAction::Yield,
            false,
            |transition: &dyn Fn() -> bool| transition(),
        );
        assert_eq!(settled, Settled::Park, "the yield lost to a park");
        assert_eq!(task.load_state(), TaskState::Parked);
    }

    #[test]
    fn a_doomed_task_no_cpu_names_is_nudged_everywhere() {
        let arch = crate::TestArch::new(3).expect("three CPUs");
        nudge_doomed(&arch, Some(1), 3);
        assert_eq!(
            (arch.ipi_count(0), arch.ipi_count(1), arch.ipi_count(2)),
            (0, 1, 0)
        );
        nudge_doomed(&arch, None, 3);
        assert_eq!(
            (arch.ipi_count(0), arch.ipi_count(1), arch.ipi_count(2)),
            (1, 2, 1),
            "an owned body no slot names could be running on any CPU"
        );
        assert_eq!(arch.stray_ipi_count(), 0);
    }

    #[test]
    fn only_the_first_doom_is_reported_as_first() {
        let mark = AtomicBool::new(false);
        assert!(!observe_doom(&mark));
        assert!(doom(&mark));
        assert!(!doom(&mark), "a repeat owes no teardown");
        assert!(observe_doom(&mark));
    }

    /// A store-buffering litmus run of the kill against a returning body, on
    /// real threads and the real body lock. Each round a dispatch releases the
    /// body and reads the mark while a killer dooms and probes the body; the
    /// killer finding the body held while the dispatch reads the mark clear is
    /// the outcome that let a deferred kill be requeued instead of retired.
    #[test]
    fn a_kill_and_a_returning_body_never_both_miss_each_other() {
        extern crate std;

        use core::sync::atomic::AtomicUsize;
        use std::sync::Arc;
        use tairix_sync::SpinLock;

        const ROUNDS: usize = 400_000;

        struct Round {
            body: SpinLock<()>,
            mark: AtomicBool,
            armed: AtomicUsize,
            fired: AtomicUsize,
            probed: AtomicUsize,
        }

        let round = Arc::new(Round {
            body: SpinLock::new(()),
            mark: AtomicBool::new(false),
            armed: AtomicUsize::new(0),
            fired: AtomicUsize::new(0),
            probed: AtomicUsize::new(0),
        });

        let killer = {
            let round = Arc::clone(&round);
            std::thread::spawn(move || {
                let mut held = std::vec::Vec::with_capacity(ROUNDS);
                for r in 1..=ROUNDS {
                    while round.armed.load(Ordering::Acquire) != r {
                        core::hint::spin_loop();
                    }
                    round.fired.store(r, Ordering::Release);
                    let _ = doom(&round.mark);
                    held.push(round.body.try_lock().is_none());
                    round.probed.store(r, Ordering::Release);
                }
                held
            })
        };

        let mut saw = std::vec::Vec::with_capacity(ROUNDS);
        for r in 1..=ROUNDS {
            while round.probed.load(Ordering::Acquire) != r - 1 {
                core::hint::spin_loop();
            }
            round.mark.store(false, Ordering::Relaxed);
            let body = round.body.lock();
            round.armed.store(r, Ordering::Release);
            while round.fired.load(Ordering::Acquire) != r {
                core::hint::spin_loop();
            }
            drop(body);
            saw.push(observe_doom(&round.mark));
        }
        let held = killer.join().expect("the killer thread completes");

        let missed = held.iter().zip(&saw).filter(|(held, saw)| **held && !**saw);
        assert_eq!(
            missed.count(),
            0,
            "a killer saw the body held while the run read the mark clear"
        );
    }
}
