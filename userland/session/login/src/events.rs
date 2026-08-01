//! Stable [`tairix_log::EventId`] constants emitted by `login`.
//!
//! Per `lib/log` convention every subsystem owns a
//! 1 000-wide reserved range. Login occupies `10000..11000` (adjacent to
//! PID 1's `9000..10000`). Once shipped the numeric values must never be
//! re-used or re-numbered — external audit-log consumers rely on them.

use tairix_log::EventId;

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
/// A per-invocation elevation request re-authenticated and its command ran
/// to completion as the target account (`plans/CAPABILITY_USE.md` CU5).
pub const ELEVATE_GRANTED: EventId = EventId(10_007);
/// A per-invocation elevation request was refused — a foreign-console
/// caller, a malformed request, a failed re-authentication (cause never
/// disclosed to the caller, only audited), or a spawn refusal.
pub const ELEVATE_REFUSED: EventId = EventId(10_008);
/// This console's elevation endpoint could not be bound (no attested
/// console, the reserved id already taken, or no endpoint registry), so
/// sessions run without an elevation broker: an `elevate` request fails
/// closed at the missing rendezvous rather than being served unattested.
pub const ELEVATE_UNAVAILABLE: EventId = EventId(10_009);
/// The sandboxed OS font service (`fontd`) was started
/// (`plans/FONT-SERVICE.md`): the graphical desktop draws text through it, so
/// login brings it up (as the `fontd` service account) the first round this
/// machine is display-capable — never on a headless boot. Started once per
/// login process; a duplicate would fail closed on the reserved
/// `FONT_ENDPOINT` bind.
pub const FONTD_STARTED: EventId = EventId(10_010);
/// The font service could not be started for a graphical session (its spawn
/// was refused). Login degrades gracefully — the graphical session still
/// launches — but desktop text will not render until a font service is up,
/// so the refusal is audited loudly (`AGENTS.md` §2.24).
pub const FONTD_UNAVAILABLE: EventId = EventId(10_011);
/// A [`tairix_abi::elevate::ElevateRequest::Verify`] request re-authenticated
/// the caller's own attested account; nothing was run.
pub const VERIFY_GRANTED: EventId = EventId(10_012);
/// A [`tairix_abi::elevate::ElevateRequest::Verify`] request was refused — an
/// attested uid with no account, a failed re-authentication, or an
/// unattested caller (cause never disclosed to the caller, only audited).
pub const VERIFY_REFUSED: EventId = EventId(10_013);

#[cfg(test)]
mod tests {
    use super::{
        AUTH_FAILED, CONSOLE_ERROR, ELEVATE_GRANTED, ELEVATE_REFUSED, ELEVATE_UNAVAILABLE,
        FONTD_STARTED, FONTD_UNAVAILABLE, LOCKED_OUT, LOGIN_RANGE_END, LOGIN_RANGE_START,
        SESSION_ENDED, SESSION_LAUNCH_FAILED, SESSION_STARTED, VERIFY_GRANTED, VERIFY_REFUSED,
    };

    const ALL: [u32; 13] = [
        SESSION_STARTED.0,
        AUTH_FAILED.0,
        LOCKED_OUT.0,
        SESSION_ENDED.0,
        SESSION_LAUNCH_FAILED.0,
        CONSOLE_ERROR.0,
        ELEVATE_GRANTED.0,
        ELEVATE_REFUSED.0,
        ELEVATE_UNAVAILABLE.0,
        FONTD_STARTED.0,
        FONTD_UNAVAILABLE.0,
        VERIFY_GRANTED.0,
        VERIFY_REFUSED.0,
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
