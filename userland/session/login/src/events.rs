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
/// The font service could not be started for a graphical session (its spawn was
/// refused). Login degrades gracefully — the graphical session still launches —
/// but desktop text will not render until a font service is up, so the refusal
/// is audited loudly.
pub const FONTD_UNAVAILABLE: EventId = EventId(10_011);
/// A [`tairix_abi::elevate::ElevateRequest::Verify`] request re-authenticated
/// the caller's own attested account; nothing was run.
pub const VERIFY_GRANTED: EventId = EventId(10_012);
/// A [`tairix_abi::elevate::ElevateRequest::Verify`] request was refused — an
/// attested uid with no account, a failed re-authentication, or an
/// unattested caller (cause never disclosed to the caller, only audited).
pub const VERIFY_REFUSED: EventId = EventId(10_013);
/// A `session-v1` authentication from the graphical login screen succeeded.
/// Nothing has been started yet: the authority acts on its own loop.
pub const SESSION_AUTH_GRANTED: EventId = EventId(10_014);
/// A `session-v1` authentication was refused — a wrong password, an unknown
/// or locked account, no database, or an account still inside its attempt
/// cooldown. The reply carries no reason; only this record distinguishes a
/// live cooldown from an adjudicated failure, and never which of the
/// credential faults it was.
pub const SESSION_AUTH_REFUSED: EventId = EventId(10_015);
/// A `session-v1` request was not served at all: a caller that is not the
/// greeter service account on this console, or a frame that did not decode.
pub const SESSION_REQUEST_REFUSED: EventId = EventId(10_016);
/// A page of the machine's login-able accounts was disclosed to the
/// graphical login screen. Names are not secret, but who asked for them
/// and when is worth an audit record.
pub const SESSION_ACCOUNTS_SENT: EventId = EventId(10_017);
/// The reserved `session-v1` endpoint could not be bound, so no graphical
/// login screen can be served; the round degrades to the text login rather
/// than claiming a rendezvous nothing answers.
pub const SESSION_ENDPOINT_UNAVAILABLE: EventId = EventId(10_018);
/// A greeter exited without an accepted verdict (it failed to start, was
/// dismissed, or died); the authority starts a fresh one.
pub const GREETER_FAILED: EventId = EventId(10_019);
/// Consecutive greeter failures exhausted the graphical round's budget, so
/// this round runs the text login instead. A broken login screen can never
/// leave the machine impossible to log in to.
pub const GREETER_DEGRADED: EventId = EventId(10_020);
/// An authenticated account's existing desktop session was brought back to
/// the foreground through its wake mailbox, rather than a second desktop
/// being started for it.
pub const SESSION_RESUMED: EventId = EventId(10_021);
/// The presenting desktop session gave up the screen and is now recorded as
/// a background one: it keeps running and stays resumable, and the login
/// screen comes back up. Honoured only from the session that held the
/// screen, so this record names the uid the kernel attested.
pub const SESSION_BACKGROUNDED: EventId = EventId(10_022);
/// A live desktop session was told to end because the authority itself is
/// exiting. Nothing would ever wake a background session again once the
/// authority is gone, so every entry is ended rather than stranded.
pub const SESSION_ENDED_ON_EXIT: EventId = EventId(10_023);
/// A [`tairix_abi::elevate::ElevateRequest::Launch`] request
/// re-authenticated and its program was started as the target account; the
/// broker did not wait for it. Recorded apart from [`ELEVATE_GRANTED`]
/// because no exit code is known at this point and the broker keeps a
/// child of its own to reap.
pub const LAUNCH_GRANTED: EventId = EventId(10_024);
/// A [`tairix_abi::elevate::ElevateRequest::Launch`] request was refused —
/// a failed re-authentication (cause never disclosed to the caller, only
/// audited) or a spawn refusal.
pub const LAUNCH_REFUSED: EventId = EventId(10_025);
/// A program started for a [`tairix_abi::elevate::ElevateRequest::Launch`]
/// request ended abnormally, and login is the only observer of how. Such a
/// child inherits login's console, which under a graphical session is the
/// framebuffer text console behind the desktop, so a reason it wrote to
/// `stderr` reaches nobody; the reaper states it here instead. A clean exit
/// records nothing.
pub const LAUNCH_ENDED_ABNORMALLY: EventId = EventId(10_026);

#[cfg(test)]
mod tests {
    use super::{
        AUTH_FAILED, CONSOLE_ERROR, ELEVATE_GRANTED, ELEVATE_REFUSED, ELEVATE_UNAVAILABLE,
        FONTD_STARTED, FONTD_UNAVAILABLE, GREETER_DEGRADED, GREETER_FAILED,
        LAUNCH_ENDED_ABNORMALLY, LAUNCH_GRANTED, LAUNCH_REFUSED, LOCKED_OUT, LOGIN_RANGE_END,
        LOGIN_RANGE_START, SESSION_ACCOUNTS_SENT, SESSION_AUTH_GRANTED, SESSION_AUTH_REFUSED,
        SESSION_BACKGROUNDED, SESSION_ENDED, SESSION_ENDED_ON_EXIT, SESSION_ENDPOINT_UNAVAILABLE,
        SESSION_LAUNCH_FAILED, SESSION_REQUEST_REFUSED, SESSION_RESUMED, SESSION_STARTED,
        VERIFY_GRANTED, VERIFY_REFUSED,
    };

    const ALL: [u32; 26] = [
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
        SESSION_AUTH_GRANTED.0,
        SESSION_AUTH_REFUSED.0,
        SESSION_REQUEST_REFUSED.0,
        SESSION_ACCOUNTS_SENT.0,
        SESSION_ENDPOINT_UNAVAILABLE.0,
        GREETER_FAILED.0,
        GREETER_DEGRADED.0,
        SESSION_RESUMED.0,
        SESSION_BACKGROUNDED.0,
        SESSION_ENDED_ON_EXIT.0,
        LAUNCH_GRANTED.0,
        LAUNCH_REFUSED.0,
        LAUNCH_ENDED_ABNORMALLY.0,
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
