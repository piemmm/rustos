//! The typed failure surface of the `stress` tool.

use core::fmt;

/// Everything that can end a `stress` run abnormally.
///
/// A worker's *typed refusal* (a resource limit, `ENOSPC`, a capability
/// denial) is deliberately **not** here: refusals are expected outcomes the
/// controller counts and reports (`plans/STRESSTEST.md` §7.2), not errors
/// that end the run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StressError {
    /// The command line is outside the closed option grammar — reported
    /// with the usage banner, exit 2.
    Usage,
    /// The run could not be set up or torn down: the reason is stated on
    /// stderr and the run exits 1 (fail loud, never silent).
    Fatal(&'static str),
}

impl fmt::Display for StressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str("usage error"),
            Self::Fatal(reason) => f.write_str(reason),
        }
    }
}
