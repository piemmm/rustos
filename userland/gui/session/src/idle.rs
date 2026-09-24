//! The desktop's idle deadline: when the screensaver starts and when the
//! screen locks, both counted from the last input.
//!
//! A deadline is armed only while its action is still pending, so a desktop
//! whose screensaver is up and whose screen is locked arms no timer at all,
//! and one whose policy names neither never wakes for idleness.

use tairix_abi::time::Duration64;
use tairix_wallpaper::DesktopSettings;

use crate::switchuser::park_within;

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
    }

    /// The next action whose deadline has passed at `now_ns`, marked as done.
    ///
    /// A lock is answered before a screensaver due at the same moment, so the
    /// screensaver is raised over the lock rather than the reverse.
    pub fn due(&mut self, now_ns: u64) -> Option<IdleAction> {
        if !self.locked && self.passed(self.policy.lock, now_ns) {
            self.locked = true;
            return Some(IdleAction::Lock);
        }
        if !self.saving && self.passed(self.policy.screensaver, now_ns) {
            self.saving = true;
            return Some(IdleAction::StartScreensaver);
        }
        None
    }

    /// `park_ns` shortened to the nearest pending deadline, or left as it is
    /// when none is pending.
    #[must_use]
    pub fn park_deadline_ns(&self, now_ns: u64, park_ns: u64) -> u64 {
        let pending = |span: Option<Duration64>, done: bool| {
            span.filter(|_| !done)
                .map(|span| self.deadline(span).saturating_sub(now_ns))
        };
        let lock = pending(self.policy.lock, self.locked);
        let saver = pending(self.policy.screensaver, self.saving);
        park_within(park_within(park_ns, lock), saver)
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
}
