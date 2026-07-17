//! The error type surfaced by [`Login::run`](crate::Login::run).
//!
//! Every variant is a **fail-closed** outcome: login
//! launches a session only when a user authenticates *and* their session
//! starts. Anything else — a console that cannot be read, an exhausted
//! attempt budget, or a launcher that refuses the session — returns one of
//! these and starts nothing.

use core::fmt;

use tairix_abi::Errno;

/// Why [`Login::run`](crate::Login::run) returned without an active session.
///
/// A failed *credential* check is **not** represented here: an incorrect
/// password is an ordinary, expected step of the login loop that consumes
/// one attempt and re-prompts. These variants are the terminal, fail-closed
/// outcomes that end the loop without a session.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum LoginError {
    /// The bounded attempt budget was exhausted without a successful
    /// authentication. Login refuses to keep prompting forever and launches
    /// nothing.
    TooManyAttempts,
    /// The controlling terminal could not be read or written; the wrapped
    /// [`Errno`] is the [`LoginView`](crate::LoginView) seam's error verbatim.
    /// Login cannot operate without a console, so it aborts.
    Console(Errno),
    /// A user authenticated, but the [`SessionLauncher`](crate::SessionLauncher)
    /// refused to start their session; the wrapped [`Errno`] is the
    /// launcher's error verbatim.
    SessionLaunch(Errno),
}

impl fmt::Display for LoginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyAttempts => f.write_str("too many failed authentication attempts"),
            Self::Console(e) => write!(f, "console unavailable: {e}"),
            Self::SessionLaunch(e) => write!(f, "session launch failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LoginError;
    use tairix_abi::Errno;

    extern crate alloc;
    use alloc::format;

    #[test]
    fn display_is_stable() {
        assert_eq!(
            format!("{}", LoginError::TooManyAttempts),
            "too many failed authentication attempts",
        );
        assert_eq!(
            format!("{}", LoginError::Console(Errno::TimedOut)),
            "console unavailable: operation timed out",
        );
        assert_eq!(
            format!("{}", LoginError::SessionLaunch(Errno::NotFound)),
            "session launch failed: not found",
        );
    }
}
