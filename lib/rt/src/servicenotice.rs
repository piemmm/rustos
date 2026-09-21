//! Reporting this process's own service lifecycle to the service manager
//! (`plans/NEW-SERVICEMANAGER.md` SVC-5 / SVC-8).
//!
//! A supervised service says three things about itself, and all three go to
//! the same reserved endpoint as a [`ServiceNotice`]: that it has come up,
//! that it could not, and that it is still making progress. The manager
//! attributes each to the call's kernel-attested sender, so a notice carries
//! no identity and one service can never report another's.
//!
//! This is the one place in the runtime that knows how to say them, so every
//! service says them identically rather than hand-rolling the framing, the
//! reply decode, and — the part most easily got wrong — the renewal cadence.
//!
//! # A service is told its own cadence; it does not decide one
//!
//! The manager holds the watchdog interval, so it is the manager that
//! answers with it: every reply carries the interval the sender is being
//! held to, and [`Watchdog`] derives the renewal deadline from that. A
//! service therefore needs no copy of its own unit metadata, the two can
//! never disagree, and a watchdog the manager later disarms is learned about
//! on the next renewal rather than assumed to persist.
//!
//! # Not a busy-poll
//!
//! Renewal is a deadline folded into the park the service already performs
//! ([`Watchdog::timeout_ns`]), so the CPU sleeps between renewals and an
//! idle service still proves it is alive. A service that is not watched is
//! told so by a zero interval and arms nothing at all, which is why this
//! costs an unwatched service exactly one call at startup.

use tairix_abi::service_control::{decode_notice_reply, NOTICE_REPLY_LEN, SERVICE_NOTICE_ENDPOINT};
use tairix_abi::waitset::WAITSET_TIMEOUT_NONE;
use tairix_abi::{Duration64, Errno, LifecycleSignal, ServiceNotice, ServiceState};

/// Send one notice to the service manager and decode its answer.
///
/// # Errors
///
/// The manager's own refusal — [`Errno::NotFound`] when it can match the
/// sender to no service in the state the report requires — or the transport
/// error the call failed with.
fn send_notice(notice: ServiceNotice) -> Result<(ServiceState, Duration64), Errno> {
    let frame = notice.to_le_bytes();
    let mut reply = [0u8; NOTICE_REPLY_LEN];
    let len = crate::ipc_call(SERVICE_NOTICE_ENDPOINT, &frame, &mut reply)
        .map_err(Errno::from_syscall)?;
    decode_notice_reply(&reply[..len])
}

/// Announce that this service's endpoints are answerable, returning the
/// watchdog interval the manager is holding it to ([`Duration64::ZERO`] when
/// it is not watched).
///
/// Send this only *after* the bind, because that is what the announcement
/// means: a client the manager parked on this service's activation is
/// released by this call, so announcing any earlier hands one an endpoint
/// that does not exist yet.
///
/// # Errors
///
/// [`Errno::NotFound`] when the manager matches this process to no service
/// it is currently starting, or the transport error the call failed with. A
/// refusal is not fatal to the caller by itself: the service is answerable
/// either way, and only the manager's ordering is lost.
pub fn announce_ready() -> Result<Duration64, Errno> {
    send_notice(ServiceNotice::Lifecycle(LifecycleSignal::Ready)).map(|(_, watchdog)| watchdog)
}

/// Announce that this service has determined it cannot come up, so the
/// manager stops holding its dependents and its parked clients.
///
/// A service that cannot proceed reports this rather than exiting silently
/// or spinning.
///
/// # Errors
///
/// [`Errno::NotFound`] when the manager matches this process to no service
/// it is currently starting, or the transport error the call failed with.
pub fn announce_failed() -> Result<(), Errno> {
    send_notice(ServiceNotice::Lifecycle(LifecycleSignal::Failed)).map(|_| ())
}

/// Renew this service's liveness deadline, returning the interval the
/// manager is currently holding it to.
///
/// # Errors
///
/// [`Errno::NotFound`] when the manager matches this process to no running
/// service, or the transport error the call failed with.
pub fn heartbeat() -> Result<Duration64, Errno> {
    send_notice(ServiceNotice::Alive).map(|(_, watchdog)| watchdog)
}

/// A supervised service's renewal clock: the interval the manager reported
/// and the monotonic instant the next renewal is due.
///
/// Fold [`timeout_ns`](Self::timeout_ns) into the service's existing park
/// and call [`renew_if_due`](Self::renew_if_due) each time round its loop.
/// An unwatched service ([`Duration64::ZERO`]) arms nothing and renews
/// nothing, so wiring this up costs it only the call that told it so.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Watchdog {
    /// The manager's interval, or [`Duration64::ZERO`] when unwatched.
    interval: Duration64,
    /// Monotonic nanoseconds at which the next renewal falls due.
    due_ns: u64,
}

impl Watchdog {
    /// Renew at the halfway point, so a renewal and its reply have a whole
    /// further half-interval to complete before the manager would call the
    /// service wedged. Renewing on the deadline itself would make every
    /// scheduling delay a false kill.
    const RENEW_DIVISOR: u64 = 2;

    /// The unwatched clock: arms nothing, renews nothing.
    pub const UNWATCHED: Self = Self {
        interval: Duration64::ZERO,
        due_ns: 0,
    };

    /// Build a clock from the `interval` the manager reported, as of
    /// `now_ns` (monotonic nanoseconds). A zero interval is the manager's
    /// "I am not watching you", so it yields [`Self::UNWATCHED`] rather
    /// than a deadline that is perpetually due.
    #[must_use]
    pub fn new(interval: Duration64, now_ns: u64) -> Self {
        if interval.saturating_total_nanos() == 0 {
            return Self::UNWATCHED;
        }
        let mut clock = Self {
            interval,
            due_ns: 0,
        };
        clock.rearm(now_ns);
        clock
    }

    /// Announce readiness and adopt whatever interval the manager reports.
    ///
    /// The ordinary entry for a `notify`-ready service: one call that both
    /// releases the clients parked on this activation and establishes the
    /// renewal cadence. A refused announcement yields [`Self::UNWATCHED`] —
    /// a manager that is not tracking this process cannot be renewed to, and
    /// inventing a cadence for it would be guessing.
    #[must_use]
    pub fn announce_ready(now_ns: u64) -> Self {
        match announce_ready() {
            Ok(interval) => Self::new(interval, now_ns),
            Err(_) => Self::UNWATCHED,
        }
    }

    /// Ask the manager what this service's interval is, without announcing a
    /// readiness edge.
    ///
    /// The entry for an `immediate`-readiness service, which the manager
    /// already considers running: it has no readiness edge left to announce,
    /// so its first renewal is what tells it whether it is watched at all.
    #[must_use]
    pub fn attach(now_ns: u64) -> Self {
        match heartbeat() {
            Ok(interval) => Self::new(interval, now_ns),
            Err(_) => Self::UNWATCHED,
        }
    }

    /// Whether the manager is holding this service to a watchdog.
    #[must_use]
    pub fn is_watched(&self) -> bool {
        self.interval.saturating_total_nanos() > 0
    }

    /// The park timeout, in nanoseconds, that reaches the next renewal —
    /// or [`WAITSET_TIMEOUT_NONE`] when this service is unwatched, so an
    /// unwatched one still parks indefinitely and takes no timer.
    ///
    /// Combine with whatever other deadline the caller has by taking the
    /// smaller of the two. Call [`renew_if_due`](Self::renew_if_due) before
    /// this, not after: read the other way round, an already-lapsed
    /// deadline yields a zero timeout and the loop spins until it happens
    /// to renew.
    #[must_use]
    pub fn timeout_ns(&self, now_ns: u64) -> u64 {
        if !self.is_watched() {
            return WAITSET_TIMEOUT_NONE;
        }
        self.due_ns.saturating_sub(now_ns)
    }

    /// Renew if the deadline has fallen due, re-arming from the interval the
    /// manager reports back.
    ///
    /// Safe to call every time round the loop: a renewal that is not yet due
    /// costs one comparison. A refused renewal disarms the clock — the
    /// manager is no longer tracking this process, so there is nothing to
    /// renew and continuing to ask would be a poll.
    pub fn renew_if_due(&mut self, now_ns: u64) {
        if !self.is_watched() || now_ns < self.due_ns {
            return;
        }
        match heartbeat() {
            Ok(interval) => {
                self.interval = interval;
                self.rearm(now_ns);
            }
            Err(_) => *self = Self::UNWATCHED,
        }
    }

    /// Place the next renewal a half-interval after `now_ns`, saturating
    /// rather than wrapping so a clock near the end of its range cannot
    /// fold back into an immediately-due deadline.
    fn rearm(&mut self, now_ns: u64) {
        let half = self.interval.saturating_total_nanos() / Self::RENEW_DIVISOR;
        self.due_ns = now_ns.saturating_add(half);
    }
}

#[cfg(test)]
mod tests {
    use super::Watchdog;
    use tairix_abi::waitset::WAITSET_TIMEOUT_NONE;
    use tairix_abi::Duration64;

    #[test]
    fn an_unwatched_clock_arms_nothing() {
        let clock = Watchdog::UNWATCHED;
        assert!(!clock.is_watched());
        assert_eq!(clock.timeout_ns(0), WAITSET_TIMEOUT_NONE);
        assert_eq!(clock.timeout_ns(u64::MAX), WAITSET_TIMEOUT_NONE);
        // A zero interval is the manager's "I am not watching you", so it
        // must not read as "renew immediately, forever".
        assert_eq!(Watchdog::new(Duration64::ZERO, 10), Watchdog::UNWATCHED);
    }

    #[test]
    fn renewal_falls_due_at_half_the_interval() {
        let clock = Watchdog::new(Duration64::from_secs(30), 1_000);
        assert!(clock.is_watched());
        assert_eq!(clock.timeout_ns(1_000), 15 * 1_000_000_000);
        // Part-way through, the timeout is the remainder, never the whole
        // interval again — a park that re-armed from scratch each wake would
        // never renew.
        assert_eq!(clock.timeout_ns(1_000 + 10_000_000_000), 5_000_000_000);
    }

    #[test]
    fn a_lapsed_deadline_reports_a_zero_timeout_rather_than_wrapping() {
        let clock = Watchdog::new(Duration64::from_secs(4), 0);
        // Well past due: the park must return at once, not wrap to an
        // effectively infinite wait.
        assert_eq!(clock.timeout_ns(u64::MAX), 0);
    }

    #[test]
    fn rearming_near_the_end_of_the_clock_saturates() {
        let clock = Watchdog::new(Duration64::from_secs(30), u64::MAX - 1);
        assert_eq!(clock.timeout_ns(u64::MAX - 1), 1);
    }
}
