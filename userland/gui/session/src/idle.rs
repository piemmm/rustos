//! The desktop's idle deadline: when the screensaver starts and when the
//! screen locks, both counted from the last input.
//!
//! A deadline is armed only while its action is still pending, so a desktop
//! whose screensaver is up and whose screen is locked arms no timer at all,
//! and one whose policy names neither never wakes for idleness.

use tairix_abi::time::Duration64;
use tairix_util::retry::RestartPacer;
use tairix_wallpaper::DesktopSettings;

use crate::switchuser::park_within;

/// How soon a refused lock is asked for again: a second, doubling while it
/// keeps failing, up to a minute, and never abandoned.
const LOCK_RETRY_BASE_NS: u64 = 1_000_000_000;
const LOCK_RETRY_CAP_NS: u64 = 60_000_000_000;

/// When the idle actions happen, as spans of idleness.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct IdlePolicy {
    /// How long before the screensaver starts, or `None` for never.
    pub screensaver: Option<Duration64>,
    /// How long before the screen locks, or `None` for never.
    pub lock: Option<Duration64>,
}

impl IdlePolicy {
    /// The policy `settings` name. `can_lock` is whether this session can
    /// verify a password at all: a lock nothing could open would strand the
    /// user, so without one the screen never locks on its own.
    #[must_use]
    pub fn of(settings: &DesktopSettings, can_lock: bool) -> Self {
        Self {
            screensaver: settings.screensaver_after.span(),
            lock: settings.lock_after.span().filter(|_| can_lock),
        }
    }
}

/// What an elapsed idle deadline asks the session to do.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum IdleAction {
    /// Lock the screen.
    Lock,
    /// Start the screensaver.
    StartScreensaver,
}

/// The last input, the policy, and which actions have happened since.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct IdleClock {
    policy: IdlePolicy,
    last_input_ns: u64,
    locked: bool,
    saving: bool,
    /// When a refused lock may be asked for again.
    lock_retry_at: Option<u64>,
    lock_retry: RestartPacer,
}

impl IdleClock {
    /// A clock whose last input was at `now_ns`, following no policy yet.
    #[must_use]
    pub const fn new(now_ns: u64) -> Self {
        Self {
            policy: IdlePolicy {
                screensaver: None,
                lock: None,
            },
            last_input_ns: now_ns,
            locked: false,
            saving: false,
            lock_retry_at: None,
            lock_retry: RestartPacer::new(LOCK_RETRY_BASE_NS, LOCK_RETRY_CAP_NS, LOCK_RETRY_CAP_NS),
        }
    }

    /// Follow `policy` from now on, counted from the same last input.
    pub fn set_policy(&mut self, policy: IdlePolicy) {
        self.policy = policy;
    }

    /// Input arrived at `now_ns`: every deadline starts again.
    pub fn input(&mut self, now_ns: u64) {
        self.last_input_ns = now_ns;
        self.locked = false;
        self.saving = false;
        self.lock_retry_at = None;
    }

    /// The lock [`due`](Self::due) asked for could not engage at `now_ns`:
    /// ask again on the retry pace, never at once, so a screen that must lock
    /// keeps trying without spinning.
    pub fn lock_refused(&mut self, now_ns: u64) {
        self.locked = false;
        self.lock_retry_at = Some(self.lock_retry.failed(now_ns).at);
    }

    /// The next action whose deadline has passed at `now_ns`, marked as done.
    ///
    /// A lock is answered before a screensaver due at the same moment, so the
    /// screensaver is raised over the lock rather than the reverse.
    pub fn due(&mut self, now_ns: u64) -> Option<IdleAction> {
        if !self.locked && self.lock_deadline().is_some_and(|at| now_ns >= at) {
            self.locked = true;
            if self.lock_retry_at.take().is_some() {
                self.lock_retry.started(now_ns);
            }
            return Some(IdleAction::Lock);
        }
        if !self.saving && self.passed(self.policy.screensaver, now_ns) {
            self.saving = true;
            return Some(IdleAction::StartScreensaver);
        }
        None
    }

    /// Whether an action is due at `now_ns`, without taking it.
    #[must_use]
    pub fn is_due(&self, now_ns: u64) -> bool {
        let lock = !self.locked && self.lock_deadline().is_some_and(|at| now_ns >= at);
        lock || !self.saving && self.passed(self.policy.screensaver, now_ns)
    }

    /// `park_ns` shortened to the nearest pending deadline, or left as it is
    /// when none is pending.
    #[must_use]
    pub fn park_deadline_ns(&self, now_ns: u64, park_ns: u64) -> u64 {
        let lock = self
            .lock_deadline()
            .filter(|_| !self.locked)
            .map(|at| at.saturating_sub(now_ns));
        let saver = self
            .policy
            .screensaver
            .filter(|_| !self.saving)
            .map(|span| self.deadline(span).saturating_sub(now_ns));
        park_within(park_within(park_ns, lock), saver)
    }

    /// When the lock is next due: its idle deadline, or a refused lock's
    /// retry if that is later.
    fn lock_deadline(&self) -> Option<u64> {
        let idle = self.deadline(self.policy.lock?);
        Some(self.lock_retry_at.map_or(idle, |retry| retry.max(idle)))
    }

    fn passed(&self, span: Option<Duration64>, now_ns: u64) -> bool {
        span.is_some_and(|span| now_ns >= self.deadline(span))
    }

    fn deadline(&self, span: Duration64) -> u64 {
        self.last_input_ns
            .saturating_add(span.saturating_total_nanos())
    }
}

#[cfg(test)]
mod tests {
    use tairix_abi::time::Duration64;
    use tairix_wallpaper::{DesktopSettings, IdleAfter};

    use super::{IdleAction, IdleClock, IdlePolicy};

    const MIN: u64 = 60_000_000_000;

    fn policy(saver: Option<u64>, lock: Option<u64>) -> IdlePolicy {
        let minutes = |m: u64| Duration64::from_secs(i64::try_from(m * 60).expect("small"));
        IdlePolicy {
            screensaver: saver.map(minutes),
            lock: lock.map(minutes),
        }
    }

    #[test]
    fn nothing_is_due_or_armed_with_no_policy() {
        let mut clock = IdleClock::new(0);
        assert_eq!(clock.due(u64::MAX), None);
        assert_eq!(clock.park_deadline_ns(0, u64::MAX), u64::MAX);
    }

    #[test]
    fn the_screensaver_starts_once_its_span_has_passed_and_once_only() {
        let mut clock = IdleClock::new(0);
        clock.set_policy(policy(Some(5), None));
        assert_eq!(clock.park_deadline_ns(MIN, u64::MAX), 4 * MIN);
        assert_eq!(clock.due(5 * MIN - 1), None);
        assert_eq!(clock.due(5 * MIN), Some(IdleAction::StartScreensaver));
        assert_eq!(clock.due(6 * MIN), None);
        assert_eq!(
            clock.park_deadline_ns(6 * MIN, u64::MAX),
            u64::MAX,
            "an acted deadline arms nothing"
        );
    }

    #[test]
    fn input_starts_every_deadline_again() {
        let mut clock = IdleClock::new(0);
        clock.set_policy(policy(Some(5), Some(10)));
        assert_eq!(clock.due(5 * MIN), Some(IdleAction::StartScreensaver));
        clock.input(7 * MIN);
        assert_eq!(
            clock.due(11 * MIN),
            None,
            "the lock now counts from the input"
        );
        assert_eq!(clock.due(12 * MIN), Some(IdleAction::StartScreensaver));
        assert_eq!(clock.due(17 * MIN), Some(IdleAction::Lock));
    }

    #[test]
    fn a_lock_due_with_the_screensaver_is_answered_first() {
        let mut clock = IdleClock::new(0);
        clock.set_policy(policy(Some(5), Some(5)));
        assert_eq!(clock.due(5 * MIN), Some(IdleAction::Lock));
        assert_eq!(clock.due(5 * MIN), Some(IdleAction::StartScreensaver));
        assert_eq!(clock.due(5 * MIN), None);
    }

    #[test]
    fn the_park_is_the_nearest_pending_deadline() {
        let mut clock = IdleClock::new(0);
        clock.set_policy(policy(Some(10), Some(3)));
        assert_eq!(clock.park_deadline_ns(0, u64::MAX), 3 * MIN);
        assert_eq!(clock.park_deadline_ns(0, MIN), MIN, "a nearer park stands");
        assert_eq!(clock.due(3 * MIN), Some(IdleAction::Lock));
        assert_eq!(clock.park_deadline_ns(3 * MIN, u64::MAX), 7 * MIN);
    }

    #[test]
    fn a_session_that_cannot_verify_a_password_never_locks_on_its_own() {
        let settings = DesktopSettings {
            lock_after: IdleAfter::Minutes(1),
            screensaver_after: IdleAfter::Never,
            ..DesktopSettings::default()
        };
        assert_eq!(IdlePolicy::of(&settings, false).lock, None);
        assert_eq!(
            IdlePolicy::of(&settings, true).lock,
            Some(Duration64::from_secs(60))
        );
        assert_eq!(IdlePolicy::of(&settings, true).screensaver, None);
    }

    #[test]
    fn a_refused_lock_is_asked_for_again_on_a_paced_retry_never_at_once() {
        const SEC: u64 = 1_000_000_000;
        let mut clock = IdleClock::new(0);
        clock.set_policy(policy(None, Some(15)));
        assert_eq!(clock.due(15 * MIN), Some(IdleAction::Lock));

        clock.lock_refused(15 * MIN);
        assert_eq!(clock.due(15 * MIN), None, "no retry in the same instant");
        assert_eq!(clock.park_deadline_ns(15 * MIN, u64::MAX), SEC);
        assert_eq!(clock.due(15 * MIN + SEC), Some(IdleAction::Lock));

        // Refused again, it waits longer, and it never gives up.
        clock.lock_refused(15 * MIN + SEC);
        assert_eq!(clock.park_deadline_ns(15 * MIN + SEC, u64::MAX), 2 * SEC);
        assert_eq!(clock.due(15 * MIN + 2 * SEC), None);
        assert_eq!(clock.due(15 * MIN + 3 * SEC), Some(IdleAction::Lock));
    }

    #[test]
    fn is_due_says_what_due_would_take_without_taking_it() {
        let mut clock = IdleClock::new(0);
        clock.set_policy(policy(Some(5), Some(15)));
        assert!(!clock.is_due(5 * MIN - 1));
        assert!(clock.is_due(5 * MIN));
        assert!(clock.is_due(5 * MIN), "asking takes nothing");
        assert_eq!(clock.due(5 * MIN), Some(IdleAction::StartScreensaver));
        assert!(!clock.is_due(5 * MIN));
        assert!(clock.is_due(15 * MIN));
    }

    #[test]
    fn input_forgets_a_refused_lock() {
        let mut clock = IdleClock::new(0);
        clock.set_policy(policy(None, Some(15)));
        assert_eq!(clock.due(15 * MIN), Some(IdleAction::Lock));
        clock.lock_refused(15 * MIN);
        clock.input(16 * MIN);
        assert_eq!(clock.due(30 * MIN), None);
        assert_eq!(clock.due(31 * MIN), Some(IdleAction::Lock));
    }
}
