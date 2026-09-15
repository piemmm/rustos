//! The park/unpark handshake every policy shares.
//!
//! Parking is the task lifecycle, not scheduling policy: which CPU a woken
//! task lands on differs per policy, but the window between "decide to park"
//! and "actually parked" — and the token that closes it — is identical. One
//! definition here, so the three policies carry only their own placement.

use core::sync::atomic::{fence, Ordering};

use crate::{SchedError, SchedResult, TaskState};

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
}
