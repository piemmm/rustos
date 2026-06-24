//! Stable [`rustos_log::EventId`] constants emitted by `login`.
//!
//! Per `lib/log` convention every subsystem owns a
//! 1 000-wide reserved range. Login occupies `10000..11000` (adjacent to
//! PID 1's `9000..10000`). Once shipped the numeric values must never be
//! re-used or re-numbered — external audit-log consumers rely on them.

use rustos_log::EventId;

/// Range start (inclusive) reserved for `login` event identifiers.
///
/// Exposed so audit consumers can filter by subsystem in O(1) instead of
/// matching on individual event identifiers.
pub const LOGIN_RANGE_START: u32 = 10_000;
/// Range end (exclusive) reserved for `login` event identifiers.
pub const LOGIN_RANGE_END: u32 = 11_000;

/// A user authenticated and a session was launched on their behalf. The
/// session inherits the user's capability ceiling.
pub const SESSION_STARTED: EventId = EventId(10_001);
/// An authentication attempt was rejected. A failed credential check is a
/// security-relevant decision in its own right; the cause is never disclosed to the caller, only audited.
pub const AUTH_FAILED: EventId = EventId(10_002);
/// The bounded attempt budget was exhausted without a successful
/// authentication; login fails closed and launches nothing.
pub const LOCKED_OUT: EventId = EventId(10_003);
/// A launched session returned (the user logged out or the session exited).
pub const SESSION_ENDED: EventId = EventId(10_004);
/// A user authenticated but the [`SessionLauncher`](crate::SessionLauncher)
/// refused to start their session; login fails closed rather than retry.
pub const SESSION_LAUNCH_FAILED: EventId = EventId(10_005);
/// The controlling terminal could not be read or written. Login cannot run
/// without a console, so it aborts (fail closed).
pub const CONSOLE_ERROR: EventId = EventId(10_006);

#[cfg(test)]
mod tests {
    use super::{
        AUTH_FAILED, CONSOLE_ERROR, LOCKED_OUT, LOGIN_RANGE_END, LOGIN_RANGE_START, SESSION_ENDED,
        SESSION_LAUNCH_FAILED, SESSION_STARTED,
    };

    const ALL: [u32; 6] = [
        SESSION_STARTED.0,
        AUTH_FAILED.0,
        LOCKED_OUT.0,
        SESSION_ENDED.0,
        SESSION_LAUNCH_FAILED.0,
        CONSOLE_ERROR.0,
    ];

    #[test]
    fn ids_are_inside_reserved_range() {
        for id in ALL {
            assert!((LOGIN_RANGE_START..LOGIN_RANGE_END).contains(&id));
        }
    }

    #[test]
    fn ids_are_unique() {
        let mut ids = ALL;
        ids.sort_unstable();
        for w in ids.windows(2) {
            assert_ne!(w[0], w[1], "duplicate login EventId");
        }
    }
}
